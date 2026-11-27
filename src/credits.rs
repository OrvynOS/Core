//! Orvyn credits: a dollar balance per signed-in account that pays for Orvyn inference (chat in the
//! app, /v1/chat/completions, and agents set to the "orvyn" provider).
//!
//! People top up by sending BNB, USDT or USDC on BNB Chain from their own wallet (the account's
//! address) to ORVYN_CREDITS_ADDRESS, then handing us the transaction hash: we read the transfer on
//! chain and credit its dollar value once. BNB is valued at the median of Orvyn's price sources at
//! that moment. Amounts are kept in micro-dollars (1 = $0.000001).
//!
//! Orvyn API keys (`orv_sk_…`) let agents and tools spend the same balance. Only a SHA-256 of each
//! key is stored; the key itself is shown once.

use std::sync::LazyLock;

use alloy::primitives::{B256, U256};
use alloy::providers::Provider;
use alloy::sol_types::{SolCall, SolEvent};
use regex::Regex;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::db::{db, CreditEntry, InferenceKey};
use crate::env::env;
use crate::error::{Error, Result};
use crate::evm::{self, BSC};

pub const MICRO: f64 = 1_000_000.0;
/// The smallest top-up we credit.
pub const MIN_DEPOSIT_USD: f64 = 1.0;
const MAX_KEYS: i64 = 10;

static TX_HASH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^0x[0-9a-fA-F]{64}$").unwrap());

pub fn usd(micro: i64) -> f64 {
    micro as f64 / MICRO
}

/// Where deposits go, if this server takes them.
pub fn deposit_address() -> Option<String> {
    env().orvyn_credits_address.clone().map(|a| a.to_lowercase())
}

pub async fn balance(account_id: &str) -> Result<i64> {
    Ok(sqlx::query_scalar("select micro_usd from credit_balances where account_id = $1").bind(account_id.to_lowercase()).fetch_optional(db()).await?.unwrap_or(0))
}

pub async fn history(account_id: &str, limit: i64) -> Result<Vec<CreditEntry>> {
    Ok(sqlx::query_as("select * from credit_ledger where account_id = $1 order by created_at desc, id desc limit $2").bind(account_id.to_lowercase()).bind(limit).fetch_all(db()).await?)
}

pub fn entry_view(e: &CreditEntry) -> Value {
    json!({
        "id": e.id,
        "kind": e.kind,
        "usd": usd(e.micro_usd),
        "txHash": e.tx_hash,
        "token": e.token,
        "amount": e.amount,
        "model": e.model,
        "source": e.source,
        "inputTokens": e.input_tokens,
        "outputTokens": e.output_tokens,
        "createdAt": e.created_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    })
}

/// Add (or, negative, take away) credit and log it, in one transaction. Returns the new balance.
async fn apply(account_id: &str, micro: i64, entry: Value) -> Result<i64> {
    let account = account_id.to_lowercase();
    let mut tx = db().begin().await?;
    let s = |k: &str| entry[k].as_str().map(String::from);
    let n = |k: &str| entry[k].as_i64().map(|v| v.min(i32::MAX as i64) as i32);
    sqlx::query("insert into credit_ledger (account_id, kind, micro_usd, tx_hash, token, amount, model, source, input_tokens, output_tokens) values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)")
        .bind(&account)
        .bind(s("kind").unwrap_or_else(|| "adjust".into()))
        .bind(micro)
        .bind(s("txHash"))
        .bind(s("token"))
        .bind(s("amount"))
        .bind(s("model"))
        .bind(s("source"))
        .bind(n("inputTokens"))
        .bind(n("outputTokens"))
        .execute(&mut *tx)
        .await
        .map_err(|e| if e.to_string().contains("credit_ledger_tx_idx") { Error::conflict("That transaction was already credited.") } else { e.into() })?;
    let balance: i64 = sqlx::query_scalar(
        "insert into credit_balances (account_id, micro_usd) values ($1, $2)
         on conflict (account_id) do update set micro_usd = credit_balances.micro_usd + excluded.micro_usd, updated_at = now()
         returning micro_usd",
    )
    .bind(&account)
    .bind(micro)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(balance)
}

/// Charge inference to an account. The balance can dip just below zero on the last reply; the next
/// request is then refused until they top up.
pub async fn charge(account_id: &str, micro: i64, model: &str, source: &str, input_tokens: u64, output_tokens: u64) -> Result<i64> {
    apply(account_id, -micro.max(0), json!({ "kind": "usage", "model": model, "source": source, "inputTokens": input_tokens, "outputTokens": output_tokens })).await
}

/// An admin top-up or refund (orvyn-admin), logged like any other entry.
pub async fn adjust(account_id: &str, usd_amount: f64) -> Result<i64> {
    apply(account_id, (usd_amount * MICRO).round() as i64, json!({ "kind": "adjust" })).await
}

/// What the app shows on the credits page: balance, where to send, and recent entries.
pub async fn summary(account_id: &str) -> Result<Value> {
    let bal = balance(account_id).await?;
    let entries = history(account_id, 50).await?;
    let orvyn = crate::orvyn_token::contract();
    let mut tokens = vec![
        json!({ "symbol": "BNB", "address": null, "decimals": 18 }),
        json!({ "symbol": "USDT", "address": BSC.usdt, "decimals": 18 }),
        json!({ "symbol": "USDC", "address": BSC.usdc, "decimals": 18 }),
    ];
    if let Some(a) = &orvyn {
        tokens.push(json!({ "symbol": crate::orvyn_token::SYMBOL, "address": a, "decimals": crate::orvyn_token::DECIMALS, "discountBps": crate::orvyn_token::DISCOUNT_BPS }));
    }
    Ok(json!({
        "usd": usd(bal),
        "deposit": {
            "address": deposit_address(),
            "chain": "BNB Chain",
            "chainId": BSC.chain_id,
            "minUsd": MIN_DEPOSIT_USD,
            "tokens": tokens,
        },
        // Paying with $ORVYN costs 2.5% less; until it launches the app says so ("soon").
        "orvyn": { "live": orvyn.is_some(), "symbol": crate::orvyn_token::SYMBOL, "discountBps": crate::orvyn_token::DISCOUNT_BPS },
        "entries": entries.iter().map(entry_view).collect::<Vec<_>>(),
    }))
}

/// The wallet call for a stablecoin top-up (BNB is a plain send to the deposit address).
pub fn deposit_call(symbol: &str, amount: f64) -> Result<Value> {
    let to = deposit_address().ok_or_else(|| Error::unavailable("Credit top-ups aren't open yet."))?;
    // $ORVYN: `amount` is a number of tokens (its USD value is checked when the deposit lands).
    if symbol == crate::orvyn_token::SYMBOL {
        let token = crate::orvyn_token::contract().ok_or_else(|| Error::invalid("$ORVYN isn't live yet."))?;
        if !(amount.is_finite() && amount > 0.0 && amount <= 1e12) {
            return Err(Error::invalid("Enter how many $ORVYN to send."));
        }
        let data = evm::IERC20::transferCall { to: evm::parse_address(&to)?, amount: evm::to_units(amount, crate::orvyn_token::DECIMALS as u8) }.abi_encode();
        return Ok(json!({ "to": token, "data": format!("0x{}", hex::encode(data)), "value": "0" }));
    }
    let token = match symbol {
        "USDT" => BSC.usdt,
        "USDC" => BSC.usdc,
        _ => return Err(Error::invalid("Top up with USDT, USDC or $ORVYN here; BNB is a plain send.")),
    };
    if !(amount.is_finite() && amount >= MIN_DEPOSIT_USD && amount <= 100_000.0) {
        return Err(Error::invalid(format!("Top up at least ${MIN_DEPOSIT_USD}.")));
    }
    let data = evm::IERC20::transferCall { to: evm::parse_address(&to)?, amount: evm::to_units(amount, 18) }.abi_encode();
    Ok(json!({ "to": token, "data": format!("0x{}", hex::encode(data)), "value": "0" }))
}

/// Credit a deposit: the transaction must be from the account's own address to the deposit
/// address, in BNB, USDT or USDC, and each transaction counts once.
pub async fn record_deposit(account_id: &str, tx_hash: &str) -> Result<CreditEntry> {
    if !TX_HASH.is_match(tx_hash) {
        return Err(Error::invalid("That isn't a transaction hash."));
    }
    let to = deposit_address().ok_or_else(|| Error::unavailable("Credit top-ups aren't open yet."))?;
    let account = account_id.to_lowercase();
    if !evm::is_address(&account) {
        return Err(Error::invalid("Sign in with a wallet to top up."));
    }
    let hash = tx_hash.to_lowercase();
    if let Some(seen) = sqlx::query_as::<_, CreditEntry>("select * from credit_ledger where tx_hash = $1").bind(&hash).fetch_optional(db()).await? {
        return if seen.account_id == account { Ok(seen) } else { Err(Error::conflict("That transaction was already credited.")) };
    }
    let h: B256 = hash.parse().map_err(|_| Error::invalid("That isn't a transaction hash."))?;
    let receipt = crate::wallets::receipt_of(h).await?.ok_or_else(|| Error::conflict("The transaction isn't on chain yet. Try again in a few seconds."))?;
    if !receipt.status() {
        return Err(Error::invalid("That transaction failed on chain."));
    }
    let from = evm::parse_address(&account)?;
    let to_addr = evm::parse_address(&to)?;
    let (mut symbol, mut amount) = (None, U256::ZERO);
    let orvyn = crate::orvyn_token::contract();
    let accepted: Vec<(&str, &str)> = [(BSC.usdt, "USDT"), (BSC.usdc, "USDC")].into_iter().chain(orvyn.as_deref().map(|a| (a, crate::orvyn_token::SYMBOL))).collect();
    for log in receipt.inner.logs() {
        let Some(s) = accepted.iter().copied().find(|(a, _)| a.eq_ignore_ascii_case(&format!("{:#x}", log.address()))) else { continue };
        if let Ok(ev) = evm::IERC20::Transfer::decode_log_data(log.data()) {
            if ev.from == from && ev.to == to_addr {
                symbol = Some(s.1);
                amount += ev.value;
            }
        }
    }
    if symbol.is_none() {
        if let Ok(Some(tx)) = evm::bsc().get_transaction_by_hash(h).await {
            use alloy::consensus::Transaction as _;
            use alloy::network::TransactionResponse as _;
            if tx.from() == from && tx.to() == Some(to_addr) && tx.value() > U256::ZERO {
                symbol = Some("BNB");
                amount = tx.value();
            }
        }
    }
    let Some(symbol) = symbol.filter(|_| amount > U256::ZERO) else {
        return Err(Error::invalid("That transaction doesn't send BNB, USDT, USDC or $ORVYN from your wallet to Orvyn's deposit address."));
    };
    let units = evm::from_units(amount, 18);
    let usd_value = if symbol == "BNB" {
        let price = crate::price::sources_median("BNB / USD").await.ok_or_else(|| Error::unavailable("Can't price BNB right now. Try again in a minute; your deposit is safe."))?;
        units * price
    } else if symbol == crate::orvyn_token::SYMBOL {
        units * crate::orvyn_token::usd_price().await.map_err(|_| Error::unavailable("Can't price $ORVYN right now. Try again in a minute; your deposit is safe."))?
    } else {
        units
    };
    if usd_value + 1e-9 < MIN_DEPOSIT_USD {
        return Err(Error::invalid(format!("Deposits start at ${MIN_DEPOSIT_USD}. Send a little more and submit both.")));
    }
    // $ORVYN costs 2.5% less: its market value buys more credit.
    let credited = if symbol == crate::orvyn_token::SYMBOL { crate::orvyn_token::credits_for(usd_value) } else { usd_value };
    let micro = (credited * MICRO).floor() as i64;
    apply(&account, micro, json!({ "kind": "deposit", "txHash": hash, "token": symbol, "amount": format!("{units}") })).await?;
    Ok(sqlx::query_as("select * from credit_ledger where tx_hash = $1").bind(&hash).fetch_one(db()).await?)
}

// ---------- API keys ----------

fn key_hash(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

pub fn is_orvyn_key(s: &str) -> bool {
    s.starts_with("orv_sk_") && s.len() > 20
}

pub fn key_view(k: &InferenceKey) -> Value {
    let t = |v: &chrono::DateTime<chrono::Utc>| v.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    json!({ "id": k.id, "name": k.name, "hint": k.hint, "createdAt": t(&k.created_at), "lastUsedAt": k.last_used_at.as_ref().map(t) })
}

pub async fn list_keys(account_id: &str) -> Result<Vec<InferenceKey>> {
    Ok(sqlx::query_as("select * from inference_keys where account_id = $1 and revoked_at is null order by created_at desc").bind(account_id.to_lowercase()).fetch_all(db()).await?)
}

/// A new key: (its row, the secret, shown once).
pub async fn create_key(account_id: &str, name: &str) -> Result<(InferenceKey, String)> {
    let account = account_id.to_lowercase();
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 60 {
        return Err(Error::invalid("Give the key a name (up to 60 characters)."));
    }
    let active: i64 = sqlx::query_scalar("select count(*) from inference_keys where account_id = $1 and revoked_at is null").bind(&account).fetch_one(db()).await?;
    if active >= MAX_KEYS {
        return Err(Error::conflict(format!("You can have up to {MAX_KEYS} keys. Revoke one first.")));
    }
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let body: String = (0..40).map(|_| ALPHABET[rand::random_range(0..ALPHABET.len())] as char).collect();
    let secret = format!("orv_sk_{body}");
    let hint = format!("orv_sk_…{}", &body[body.len() - 4..]);
    let row = sqlx::query_as("insert into inference_keys (id, account_id, name, key_hash, hint) values ($1, $2, $3, $4, $5) returning *")
        .bind(crate::lifecycle::uuid_v4())
        .bind(&account)
        .bind(name)
        .bind(key_hash(&secret))
        .bind(hint)
        .fetch_one(db())
        .await?;
    Ok((row, secret))
}

pub async fn revoke_key(account_id: &str, id: &str) -> Result<()> {
    let n = sqlx::query("update inference_keys set revoked_at = now() where id = $1 and account_id = $2 and revoked_at is null").bind(id).bind(account_id.to_lowercase()).execute(db()).await?.rows_affected();
    if n == 0 {
        return Err(Error::not_found("No such key."));
    }
    Ok(())
}

/// The account behind an Orvyn API key, if it's live.
pub async fn account_for_key(secret: &str) -> Result<Option<String>> {
    Ok(key_owner(secret).await?.map(|(a, _)| a))
}

/// The account behind an Orvyn API key and what the key is for ("vyn" for Vyn's built-in key).
pub async fn key_owner(secret: &str) -> Result<Option<(String, Option<String>)>> {
    if !is_orvyn_key(secret) {
        return Ok(None);
    }
    Ok(sqlx::query_as("update inference_keys set last_used_at = now() where key_hash = $1 and revoked_at is null returning account_id, purpose").bind(key_hash(secret)).fetch_optional(db()).await?)
}

/// Vyn's built-in Orvyn AI key for an account (not counted against the key limit; one live at a time).
pub async fn create_vyn_key(account_id: &str) -> Result<String> {
    let account = account_id.to_lowercase();
    sqlx::query("update inference_keys set revoked_at = now() where account_id = $1 and purpose = 'vyn' and revoked_at is null").bind(&account).execute(db()).await?;
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let body: String = (0..40).map(|_| ALPHABET[rand::random_range(0..ALPHABET.len())] as char).collect();
    let secret = format!("orv_sk_{body}");
    sqlx::query("insert into inference_keys (id, account_id, name, key_hash, hint, purpose) values ($1, $2, 'Vyn (built in)', $3, $4, 'vyn')")
        .bind(crate::lifecycle::uuid_v4())
        .bind(&account)
        .bind(key_hash(&secret))
        .bind(format!("orv_sk_…{}", &body[body.len() - 4..]))
        .execute(db())
        .await?;
    Ok(secret)
}

// ---- Vyn's free allowance: each month, Orvyn covers the first VYN_FREE_USD_PER_MONTH (default
// $1) of what Vyn's built-in key spends; after that it's Orvyn credits as usual. ----

fn this_month() -> String {
    chrono::Utc::now().format("%Y-%m").to_string()
}

pub fn vyn_allowance_micro() -> i64 {
    let usd: f64 = std::env::var("VYN_FREE_USD_PER_MONTH").ok().and_then(|v| v.parse().ok()).filter(|v: &f64| *v >= 0.0).unwrap_or(1.0);
    (usd * MICRO).round() as i64
}

/// What's left of this month's allowance, in micro-USD.
pub async fn vyn_allowance_left(account_id: &str) -> Result<i64> {
    let used: Option<i64> = sqlx::query_scalar("select used_micro from vyn_allowance where account_id = $1 and month = $2").bind(account_id.to_lowercase()).bind(this_month()).fetch_optional(db()).await?;
    Ok((vyn_allowance_micro() - used.unwrap_or(0)).max(0))
}

/// Settle a Vyn request: the allowance first, the rest from credits. Returns the part credits paid.
pub async fn charge_vyn(account_id: &str, micro: i64, model: &str, input_tokens: u64, output_tokens: u64) -> Result<i64> {
    let account = account_id.to_lowercase();
    let micro = micro.max(0);
    let free = micro.min(vyn_allowance_left(&account).await?);
    if free > 0 {
        sqlx::query("insert into vyn_allowance (account_id, month, used_micro) values ($1, $2, $3) on conflict (account_id, month) do update set used_micro = vyn_allowance.used_micro + excluded.used_micro")
            .bind(&account)
            .bind(this_month())
            .bind(free)
            .execute(db())
            .await?;
    }
    let paid = micro - free;
    if paid > 0 {
        charge(&account, paid, model, "vyn", input_tokens, output_tokens).await?;
    }
    Ok(paid)
}
