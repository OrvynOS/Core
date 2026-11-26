//! Creator withdrawals from an agent's wallet on BNB Chain: BNB and BEP-20 tokens. A signed-in
//! session isn't enough: every withdrawal must also be signed by the creator's wallet (EIP-191 over
//! `withdrawal_message`, fresh nonce), so a stolen session can't drain an agent. The gas reserve
//! can't be withdrawn, and each withdrawal is posted to the agent's activity.

use alloy::primitives::{Address, U256};
use alloy::sol_types::SolCall;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::db::{db, log_agent, Agent, AgentLogKind, Withdrawal};
use crate::error::{Error, Result};
use crate::evm::{self, BSC};
use crate::wallet_policy::grants::OwnerGrant;

const GRANT_MAX_AGE_MS: i64 = 10 * 60_000;

pub struct ResolvedAsset {
    pub key: String,
    pub symbol: String,
    pub decimals: u8,
    pub balance: U256,
    pub max: U256,
    pub note: String,
}

pub fn format_units(v: U256, decimals: u8) -> String {
    alloy::primitives::utils::format_units(v, decimals).map(|s| {
        // viem prints no trailing zeros ("1.5", "2").
        if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s }
    }).unwrap_or_default()
}

async fn bnb_asset(agent: &Agent) -> Result<ResolvedAsset> {
    let balance = evm::bnb_balance(&agent.evm_address).await?;
    let gas = evm::native_transfer_cost("bsc").await?;
    let locked = evm::to_wei(agent.gas_reserve_bnb) + gas;
    Ok(ResolvedAsset {
        key: "bsc:native".into(),
        symbol: "BNB".into(),
        decimals: 18,
        balance,
        max: balance.saturating_sub(locked),
        note: format!("Keeps {} BNB for gas. Plugin budgets shrink to fit.", crate::auth::js_number(agent.gas_reserve_bnb)),
    })
}

async fn token_asset(agent: &Agent, token: &str) -> Result<ResolvedAsset> {
    let t = evm::parse_address(token)?;
    let erc = evm::IERC20::new(t, evm::bsc());
    let balance = evm::erc20_balance(token, &agent.evm_address).await?;
    let decimals = erc.decimals().call().await.map_err(evm::rpc_error)?;
    let lower = token.to_lowercase();
    let symbol = if lower == BSC.usdt {
        "USDT".into()
    } else if lower == BSC.usdc {
        "USDC".into()
    } else if lower == BSC.wrapped {
        "WBNB".into()
    } else if agent.token_address.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(token)) {
        agent.symbol.clone()
    } else {
        erc.symbol().call().await.unwrap_or_else(|_| format!("{}…{}", &token[..6], &token[token.len() - 4..]))
    };
    Ok(ResolvedAsset { key: format!("bsc:{lower}"), symbol, decimals, balance, max: balance, note: "Gas is paid in BNB from the agent's wallet.".into() })
}

async fn resolve_asset(agent: &Agent, key: &str) -> Result<ResolvedAsset> {
    if key == "bsc:native" {
        return bnb_asset(agent).await;
    }
    match key.split_once(':') {
        Some(("bsc", token)) if evm::is_address(token) => token_asset(agent, token).await,
        _ => Err(Error::invalid(format!("Unknown asset: {key}"))),
    }
}

/// Every asset the agent may hold, with live balances: BNB, USDT, USDC, WBNB, its own token and
/// tokens it bought. Assets that couldn't be read are listed in `unavailable`.
pub async fn agent_assets(agent: &Agent) -> Result<(Vec<ResolvedAsset>, Vec<String>)> {
    let traded: Vec<String> = sqlx::query_scalar("select distinct symbol from trades where agent_id = $1 and side = 'buy' and ok = true").bind(&agent.id).fetch_all(db()).await?;
    let mut tokens: Vec<String> = vec![BSC.usdt.into(), BSC.usdc.into(), BSC.wrapped.into()];
    tokens.extend(agent.token_address.clone());
    tokens.extend(traded.into_iter().filter(|t| evm::is_address(t)));
    let mut keys = vec!["bsc:native".to_string()];
    for t in tokens {
        let k = format!("bsc:{}", t.to_lowercase());
        if !keys.contains(&k) {
            keys.push(k);
        }
    }
    let mut resolved = vec![];
    let mut unavailable = vec![];
    for k in keys {
        match resolve_asset(agent, &k).await {
            Ok(a) if a.balance > U256::ZERO => resolved.push(a),
            Ok(_) => {}
            Err(_) => unavailable.push(k),
        }
    }
    Ok((resolved, unavailable))
}

/// Everything the creator could withdraw right now, with maximums.
pub async fn withdrawable_assets(agent: &Agent) -> Result<Value> {
    let (assets, unavailable) = agent_assets(agent).await?;
    let list: Vec<Value> = assets
        .iter()
        .map(|a| json!({ "key": a.key, "network": BSC.label, "symbol": a.symbol, "decimals": a.decimals, "balance": format_units(a.balance, a.decimals), "max": format_units(a.max, a.decimals), "note": a.note }))
        .collect();
    Ok(json!({ "assets": list, "unavailable": unavailable }))
}

#[derive(Debug, Clone, Deserialize)]
pub struct WithdrawInput {
    pub asset: String,
    /// Decimal amount, e.g. "12.5", or "max" when preparing.
    pub amount: String,
    pub to: String,
}

/// The exact text the creator's wallet signs to authorize one withdrawal.
pub fn withdrawal_message(agent: &Agent, input: &WithdrawInput, nonce: &str, issued_at: &str) -> String {
    [
        format!("Withdraw from Orvyn agent {} (${})", agent.name, agent.symbol),
        format!("Agent ID: {}", agent.id),
        "Network: BNB Chain".into(),
        format!("Asset: {}", input.asset),
        format!("Amount: {}", input.amount),
        format!("To: {}", input.to),
        format!("Nonce: {nonce}"),
        format!("Issued At: {issued_at}"),
    ]
    .join("\n")
}

fn field<'a>(message: &'a str, name: &str) -> Option<&'a str> {
    let prefix = format!("{name}: ");
    message.split('\n').find(|l| l.starts_with(&prefix)).map(|l| &l[prefix.len()..])
}

/// Check the creator's signed withdrawal (exact message, fresh, signed by the owner, unused nonce,
/// burned here).
pub async fn verify_withdrawal_grant(agent: &Agent, input: &WithdrawInput, grant: &OwnerGrant) -> Result<()> {
    let (Some(nonce), Some(issued)) = (field(&grant.message, "Nonce"), field(&grant.message, "Issued At")) else {
        return Err(Error::invalid("Malformed withdrawal message."));
    };
    if nonce.len() != 32 || !nonce.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()) {
        return Err(Error::invalid("Malformed withdrawal message."));
    }
    if grant.message != withdrawal_message(agent, input, nonce, issued) {
        return Err(Error::invalid("The signed message doesn't match this withdrawal."));
    }
    let age = chrono::DateTime::parse_from_rfc3339(issued).map(|t| chrono::Utc::now().timestamp_millis() - t.timestamp_millis());
    if !age.is_ok_and(|a| (-60_000..=GRANT_MAX_AGE_MS).contains(&a)) {
        return Err(Error::unauthorized("The signature expired; sign again."));
    }
    let owner = evm::parse_address(&agent.owner_account_id)?;
    let sig = hex::decode(grant.signature.trim_start_matches("0x")).unwrap_or_default();
    if !evm::sig::verify_hash(owner, evm::sig::message_hash(&grant.message), &sig).await? {
        return Err(Error::forbidden("The withdrawal must be signed by the agent's creator wallet."));
    }
    let burned = sqlx::query("insert into used_nonces (nonce) values ($1) on conflict do nothing").bind(format!("withdraw:{nonce}")).execute(db()).await?;
    if burned.rows_affected() == 0 {
        return Err(Error::conflict("This signature was already used."));
    }
    Ok(())
}

fn check_destination(agent: &Agent, to: &str) -> Result<()> {
    if !evm::is_address(to) {
        return Err(Error::invalid("Enter a valid 0x address."));
    }
    if to.eq_ignore_ascii_case(&agent.evm_address) {
        return Err(Error::invalid("That's the agent's own address."));
    }
    Ok(())
}

fn parse_amount(amount: &str, decimals: u8) -> Result<U256> {
    let ok = amount.split_once('.').map_or(!amount.is_empty() && amount.chars().all(|c| c.is_ascii_digit()), |(w, f)| !w.is_empty() && !f.is_empty() && w.chars().all(|c| c.is_ascii_digit()) && f.chars().all(|c| c.is_ascii_digit()));
    if !ok {
        return Err(Error::invalid("Enter an amount like 12.5."));
    }
    if amount.split_once('.').is_some_and(|(_, f)| f.len() > decimals as usize) {
        return Err(Error::invalid(format!("At most {decimals} decimal places.")));
    }
    crate::wallet_policy::grants::to_units(amount, decimals).map_err(|_| Error::invalid("Enter an amount like 12.5."))
}

fn check_amount(asset: &ResolvedAsset, amount: U256) -> Result<()> {
    if amount == U256::ZERO {
        return Err(Error::invalid("Amount must be more than zero."));
    }
    if amount > asset.max {
        return Err(Error::invalid(format!("At most {} {} can be withdrawn right now.", format_units(asset.max, asset.decimals), asset.symbol)));
    }
    Ok(())
}

async fn refresh_owed(agent: &Agent) -> Result<Agent> {
    let _ = crate::lifecycle::allocate_earnings(agent).await;
    Ok(sqlx::query_as("select * from agents where id = $1").bind(&agent.id).fetch_one(db()).await?)
}

/// Check a withdrawal before the creator signs it: resolves "max" and issues the message to sign.
pub async fn prepare_withdrawal(agent: &Agent, input: &WithdrawInput) -> Result<Value> {
    let agent = if input.asset == "bsc:native" { refresh_owed(agent).await? } else { agent.clone() };
    let asset = resolve_asset(&agent, &input.asset).await?;
    check_destination(&agent, &input.to)?;
    let amount = if input.amount == "max" { asset.max } else { parse_amount(&input.amount, asset.decimals)? };
    check_amount(&asset, amount)?;
    let normalized = WithdrawInput { amount: format_units(amount, asset.decimals), ..input.clone() };
    let message = withdrawal_message(&agent, &normalized, &hex::encode(rand::random::<[u8; 16]>()), &crate::public::iso(chrono::Utc::now()));
    Ok(json!({ "input": { "asset": normalized.asset, "amount": normalized.amount, "to": normalized.to }, "symbol": asset.symbol, "network": BSC.label, "message": message }))
}

/// Execute a withdrawal the creator signed (the grant verified with `verify_withdrawal_grant`;
/// the wallet policy checks it again against the exact transaction). One per agent at a time.
pub async fn execute_withdrawal(agent: &Agent, requested_by: &str, input: &WithdrawInput, grant: Option<OwnerGrant>) -> Result<Withdrawal> {
    if !requested_by.eq_ignore_ascii_case(&agent.owner_account_id) {
        return Err(Error::forbidden("Only the agent's creator can withdraw."));
    }
    let mut lock = db().begin().await?;
    sqlx::query("select pg_advisory_xact_lock(hashtext($1))").bind(format!("withdraw:{}", agent.id)).execute(&mut *lock).await?;
    let current = if input.asset == "bsc:native" { refresh_owed(agent).await? } else { agent.clone() };
    let asset = resolve_asset(&current, &input.asset).await?;
    check_destination(&current, &input.to)?;
    let amount = parse_amount(&input.amount, asset.decimals)?;
    check_amount(&asset, amount)?;
    let shown = format!("{} {}", format_units(amount, asset.decimals), asset.symbol);
    let to: Address = evm::parse_address(&input.to)?;
    let sent: Result<String> = async {
        let mut wallet = crate::wallets::agent_wallet(&agent.id, &agent.evm_wallet)?;
        if let Some(g) = grant {
            wallet = wallet.with_grant(g);
        }
        let hash = if asset.key == "bsc:native" {
            crate::wallets::agent_send_with(wallet, to, Default::default(), amount).await?
        } else {
            if evm::bnb_balance(&agent.evm_address).await? == U256::ZERO {
                return Err(Error::invalid("The agent has no BNB for gas."));
            }
            let token = evm::parse_address(asset.key.trim_start_matches("bsc:"))?;
            let data = evm::IERC20::transferCall { to, amount }.abi_encode();
            crate::wallets::agent_send_with(wallet, token, data.into(), U256::ZERO).await?
        };
        Ok(format!("{hash:#x}"))
    }
    .await;
    let (tx, ok, error) = match &sent {
        Ok(h) => (Some(h.clone()), true, None),
        Err(e) => (None, false, Some(e.message.clone())),
    };
    let row: Withdrawal = sqlx::query_as(
        "insert into withdrawals (agent_id, requested_by, asset, symbol, amount, destination, tx, status, error) values ($1,$2,$3,$4,$5,$6,$7,$8,$9) returning *",
    )
    .bind(&agent.id)
    .bind(requested_by.to_lowercase())
    .bind(&asset.key)
    .bind(&asset.symbol)
    .bind(format_units(amount, asset.decimals))
    .bind(&input.to)
    .bind(&tx)
    .bind(if ok { "sent" } else { "failed" })
    .bind(&error)
    .fetch_one(db())
    .await?;
    lock.commit().await?;
    if let Err(e) = sent {
        log_agent(&agent.id, AgentLogKind::Error, &format!("Withdrawal of {shown} to {} failed: {}", input.to, e.message)).await?;
        return Err(Error::internal(format!("The withdrawal failed: {}", e.message)));
    }
    log_agent(&agent.id, AgentLogKind::Withdraw, &format!("Creator withdrew {shown} to {}", input.to)).await?;
    if asset.key == "bsc:native" {
        crate::bliss::treasury::fit_budgets(&current, &crate::bliss::installed_plugins(&agent.id, true).await?).await?;
    }
    Ok(row)
}

pub async fn withdrawal_history(agent_id: &str, limit: i64) -> Result<Vec<Withdrawal>> {
    Ok(sqlx::query_as("select * from withdrawals where agent_id = $1 order by created_at desc limit $2").bind(agent_id).bind(limit).fetch_all(db()).await?)
}
