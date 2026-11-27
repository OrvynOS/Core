//! Agent subscriptions, paid in USDT, USDC or BNB to the on-chain Orvyn registry by agent id. The
//! registry is the source of truth; this module mirrors it, runs autopay, and tells hosting whether
//! an agent should be running. Self-hosted agents don't pay hosting.

use std::collections::BTreeMap;

use alloy::primitives::{Address, B256, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use serde::Serialize;
use serde_json::{json, Value};

use crate::db::{db, log_agent, Agent, AgentLogKind};
use crate::env::env;
use crate::environment::{environment_hash, environment_of_agent};
use crate::error::{Error, Result};
use crate::evm::{self, rpc_error, BSC};
use crate::tiers::{Tier, TIER_ORDER};

pub const NATIVE_TOKEN: &str = "0x0000000000000000000000000000000000000000";
const DAY_MS: i64 = 86_400_000;

sol! {
    #[sol(rpc)]
    interface OrvynRegistry {
        struct AgentView { string agentId; address owner; address agentWallet; uint64 paidUntil; uint64 registeredAt; uint32 payments; bool active; uint8 tier; bytes32 environmentHash; }
        struct TokenPrice { address token; uint256 monthlyPrice; }
        struct Payment { address payer; address token; uint256 amount; uint32 months; uint64 periodStart; uint64 paidUntil; uint64 paidAt; uint64 blockNumber; }
        function tierBps(uint8 tier) view returns (uint256);
        function priceFor(uint8 tier, address token) view returns (uint256);
        function changeTier(string agentId, uint8 newTier) returns (uint64 paidUntil);
        function attestEnvironment(string agentId, bytes32 hash);
        function registerAgent(string agentId, address owner, address agentWallet) returns (AgentView);
        function getAgent(string agentId) view returns (bool found, AgentView agent);
        function getPrices() view returns (TokenPrice[]);
        function getPayments(string agentId, uint256 from, uint256 limit) view returns (Payment[]);
        function pay(string agentId, address token, uint256 amount) payable returns (uint32 months, uint256 used);
        event TierChanged(bytes32 indexed agentKey, string agentId, uint8 fromTier, uint8 toTier, uint64 paidUntil);
        event SubscriptionPaid(bytes32 indexed agentKey, string agentId, uint256 paymentIndex, address indexed payer, address indexed token, uint256 amount, uint32 months, uint64 periodStart, uint64 paidUntil);
    }
}

pub fn billing_enabled() -> bool {
    env().orvyn_registry_contract.is_some()
}

pub fn registry_contract() -> Result<Address> {
    let c = env().orvyn_registry_contract.as_deref().ok_or_else(|| Error::unavailable("ORVYN_REGISTRY_CONTRACT is not set"))?;
    evm::parse_address(c)
}

fn registry() -> Result<OrvynRegistry::OrvynRegistryInstance<alloy::providers::DynProvider>> {
    Ok(OrvynRegistry::new(registry_contract()?, evm::bsc()))
}

#[derive(Debug, Clone)]
pub struct RegistryAgent {
    pub owner: Address,
    pub agent_wallet: Address,
    pub paid_until: u64,
    pub registered_at: u64,
    pub payments: u32,
    pub active: bool,
    pub tier: u8,
    pub environment_hash: B256,
}

pub async fn registry_agent(agent_id: &str) -> Result<Option<RegistryAgent>> {
    let r = registry()?.getAgent(agent_id.to_string()).call().await.map_err(rpc_error)?;
    if !r.found {
        return Ok(None);
    }
    let a = r.agent;
    Ok(Some(RegistryAgent { owner: a.owner, agent_wallet: a.agentWallet, paid_until: a.paidUntil, registered_at: a.registeredAt, payments: a.payments, active: a.active, tier: a.tier, environment_hash: a.environmentHash }))
}

/// Prices: token → monthly Basic price, and tier → token → monthly price (empty when not offered).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryConfig {
    pub contract: String,
    pub prices: BTreeMap<String, String>,
    pub tier_prices: BTreeMap<String, BTreeMap<String, String>>,
    pub tokens: Value,
    pub decimals: u8,
}

pub async fn registry_config() -> Result<RegistryConfig> {
    let r = registry()?;
    let prices = r.getPrices().call().await.map_err(rpc_error)?;
    let base: BTreeMap<String, U256> = prices.iter().map(|p| (format!("{:#x}", p.token), p.monthlyPrice)).collect();
    let mut tier_prices = BTreeMap::new();
    for tier in TIER_ORDER {
        let bps = r.tierBps(tier.index()).call().await.map_err(rpc_error)?;
        let per: BTreeMap<String, String> = if bps > U256::ZERO { base.iter().map(|(t, p)| (t.clone(), (*p * bps / U256::from(10_000u64)).to_string())).collect() } else { BTreeMap::new() };
        tier_prices.insert(tier.as_str().to_string(), per);
    }
    Ok(RegistryConfig {
        contract: registry_contract()?.to_string(),
        prices: base.iter().map(|(t, p)| (t.clone(), p.to_string())).collect(),
        tier_prices,
        tokens: {
            let mut t = json!({ "usdt": BSC.usdt, "usdc": BSC.usdc, "bnb": NATIVE_TOKEN });
            // $ORVYN, 2.5% off, once it's live and the registry prices it.
            if let Some(a) = crate::orvyn_token::contract() {
                t["orvyn"] = json!(a);
            }
            t
        },
        decimals: 18,
    })
}

/// One month of `tier` in `token` (0 = not payable).
pub fn month_price(config: &RegistryConfig, tier: Tier, token: &str) -> U256 {
    config.tier_prices.get(tier.as_str()).and_then(|p| p.get(&token.to_lowercase())).and_then(|v| U256::from_str_radix(v, 10).ok()).unwrap_or_default()
}

fn operator_key() -> Result<String> {
    env().orvyn_operator_private_key.clone().ok_or_else(|| Error::unavailable("ORVYN_OPERATOR_PRIVATE_KEY is not set"))
}

async fn operator_call(data: Vec<u8>) -> Result<B256> {
    crate::wallets::send_from_key(&operator_key()?, registry_contract()?, data.into(), U256::ZERO).await
}

fn ts(secs: u64) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(secs as i64, 0).unwrap_or_default()
}

async fn register_on_chain(agent: &Agent) -> Result<()> {
    let call = OrvynRegistry::registerAgentCall { agentId: agent.id.clone(), owner: evm::parse_address(&agent.owner_account_id)?, agentWallet: evm::parse_address(&agent.evm_address)? };
    operator_call(call.abi_encode()).await?;
    Ok(())
}

/// Register the agent in the registry. It starts unpaid (there is no trial). Idempotent.
pub async fn register_agent(agent: Agent) -> Result<Agent> {
    if agent.registered {
        return Ok(agent);
    }
    let mut record = registry_agent(&agent.id).await?;
    if record.is_none() {
        register_on_chain(&agent).await?;
        record = registry_agent(&agent.id).await?;
    }
    let paid_until = record.as_ref().map(|r| ts(r.paid_until));
    let active = record.as_ref().is_some_and(|r| r.payments > 0 && r.active);
    let updated: Agent = sqlx::query_as("update agents set registered = true, paid_until = $2, subscription_active = $3 where id = $1 returning *")
        .bind(&agent.id)
        .bind(paid_until)
        .bind(active)
        .fetch_one(db())
        .await?;
    let msg = if active {
        format!("Registered in the Orvyn registry; hosted until {}", updated.paid_until.map(iso).unwrap_or_else(|| "?".into()))
    } else {
        "Registered in the Orvyn registry. Hosting starts with the first month's payment.".into()
    };
    log_agent(&agent.id, AgentLogKind::Plugin, &msg).await?;
    Ok(updated)
}

pub fn iso(t: chrono::DateTime<chrono::Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub struct Synced {
    pub agent: Agent,
    pub active: bool,
    pub ever_paid: bool,
    pub tier_changed: bool,
}

/// Mirror the registry's paid-until and tier into the database.
pub async fn sync_subscription(agent: Agent) -> Result<Synced> {
    let Some(record) = registry_agent(&agent.id).await? else {
        return Ok(Synced { agent, active: false, ever_paid: false, tier_changed: false });
    };
    let paid_until = ts(record.paid_until);
    let tier = Tier::from_index(record.tier as u64);
    let grace_ms = (env().billing_grace_hours * 3_600_000.0) as i64;
    // The grace period is for late renewals only: an agent that has never paid isn't hosted.
    let active = record.payments > 0 && paid_until.timestamp_millis() + grace_ms > chrono::Utc::now().timestamp_millis();
    let tier_changed = agent.tier != tier.as_str();
    if agent.paid_until != Some(paid_until) || agent.subscription_active != active || tier_changed {
        let updated: Agent = sqlx::query_as("update agents set paid_until = $2, subscription_active = $3, tier = $4 where id = $1 returning *")
            .bind(&agent.id)
            .bind(paid_until)
            .bind(active)
            .bind(tier.as_str())
            .fetch_one(db())
            .await?;
        if tier_changed {
            let was = Tier::parse(&agent.tier).spec().name;
            log_agent(&agent.id, AgentLogKind::Plugin, &format!("Hosting tier is now {} (was {was}); paid until {}.", tier.spec().name, iso(paid_until))).await?;
        }
        return Ok(Synced { agent: updated, active, ever_paid: record.payments > 0, tier_changed });
    }
    Ok(Synced { agent, active, ever_paid: record.payments > 0, tier_changed })
}

/// Register a self-hosted agent so its owner can buy it a tier (Studio quota, job categories).
pub async fn register_self_hosted(agent: Agent) -> Result<Agent> {
    if agent.hosting_status != "external" {
        return Err(Error::invalid("Only self-hosted agents register this way."));
    }
    if agent.registered {
        return Ok(agent);
    }
    if registry_agent(&agent.id).await?.is_none() {
        register_on_chain(&agent).await?;
    }
    let updated: Agent = sqlx::query_as("update agents set registered = true where id = $1 returning *").bind(&agent.id).fetch_one(db()).await?;
    log_agent(&agent.id, AgentLogKind::Plugin, "Registered in the Orvyn registry: a hosting tier can now be bought for its Studio quota and job categories.").await?;
    Ok(updated)
}

/// Self-hosted agents' tier counts only while it's paid; otherwise Basic. Never suspends them.
pub async fn sync_self_hosted_tier(agent: Agent) -> Result<Agent> {
    if agent.hosting_status != "external" || !agent.registered {
        return Ok(agent);
    }
    let record = registry_agent(&agent.id).await?;
    let tier = record.as_ref().filter(|r| r.active).map(|r| Tier::from_index(r.tier as u64)).unwrap_or(Tier::Basic);
    let paid_until = record.as_ref().map(|r| ts(r.paid_until));
    if tier.as_str() == agent.tier && paid_until == agent.paid_until {
        return Ok(agent);
    }
    let updated: Agent = sqlx::query_as("update agents set tier = $2, paid_until = $3 where id = $1 returning *").bind(&agent.id).bind(tier.as_str()).bind(paid_until).fetch_one(db()).await?;
    if tier.as_str() != agent.tier {
        log_agent(&agent.id, AgentLogKind::Plugin, &format!("Tier is now {} (was {}).", tier.spec().name, Tier::parse(&agent.tier).spec().name)).await?;
    }
    Ok(updated)
}

/// An unsigned call: what a wallet should send.
#[derive(Debug, Clone, Serialize)]
pub struct Call {
    pub to: String,
    pub data: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Calls {
    pub chain_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount: Option<String>,
    pub calls: Vec<Call>,
}

fn hex0x(b: &[u8]) -> String {
    format!("0x{}", hex::encode(b))
}

/// The owner's unsigned transaction to move the agent to `tier` (nothing is charged to switch).
pub async fn tier_change_call(agent_id: &str, tier: Tier) -> Result<Option<Calls>> {
    let config = registry_config().await?;
    if config.tier_prices.get(tier.as_str()).is_none_or(|p| p.is_empty()) {
        return Ok(None);
    }
    let data = OrvynRegistry::changeTierCall { agentId: agent_id.into(), newTier: tier.index() }.abi_encode();
    Ok(Some(Calls { chain_id: BSC.chain_id, amount: None, calls: vec![Call { to: config.contract, data: hex0x(&data), value: "0".into() }] }))
}

/// Record an Orvyn-hosted agent's environment in the registry when it changed.
pub async fn attest_environment(agent: Agent) -> Result<Agent> {
    if agent.hosting_status == "external" || !agent.registered || agent.hardware.as_ref().is_none_or(Value::is_null) {
        return Ok(agent);
    }
    let hash = environment_hash(&environment_of_agent(&agent));
    if agent.environment_hash.as_deref() == Some(hash.as_str()) {
        return Ok(agent);
    }
    let Some(record) = registry_agent(&agent.id).await? else { return Ok(agent) };
    let hash_b: B256 = hash.parse().map_err(|_| Error::internal("bad hash"))?;
    if record.environment_hash != hash_b {
        operator_call(OrvynRegistry::attestEnvironmentCall { agentId: agent.id.clone(), hash: hash_b }.abi_encode()).await?;
    }
    Ok(sqlx::query_as("update agents set environment_hash = $2 where id = $1 returning *").bind(&agent.id).bind(&hash).fetch_one(db()).await?)
}

async fn attach_receipt(agent_id: &str, tx: B256) {
    if let Err(e) = crate::invoices::attach_payment_tx(agent_id, tx).await {
        eprintln!("[invoices] {e}");
    }
}

fn months_label(m: i32) -> String {
    format!("{m} month{}", if m > 1 { "s" } else { "" })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayOutcome {
    Paid,
    Insufficient,
    Skipped,
    NotDue,
    None,
}

/// The agent pays `months` of hosting from its own treasury: USDT or USDC if it holds enough,
/// otherwise BNB above its gas reserve.
pub async fn pay_from_treasury(agent: &Agent, months: i32, label: &str) -> Result<PayOutcome> {
    let config = registry_config().await?;
    // The registry charges the agent's tier price there; read it from the chain.
    let tier = Tier::from_index(registry_agent(&agent.id).await?.map(|r| r.tier as u64).unwrap_or(0));
    let owner = agent.evm_address.clone();
    let reg = registry_contract()?;
    // $ORVYN first (2.5% off), then USDT and USDC.
    let orvyn = crate::orvyn_token::contract();
    let tokens: Vec<(&str, &str)> = orvyn.as_deref().map(|a| (a, "$ORVYN")).into_iter().chain([(BSC.usdt, "USDT"), (BSC.usdc, "USDC")]).collect();
    for (token, symbol) in tokens {
        let price = month_price(&config, tier, token);
        if price == U256::ZERO {
            continue;
        }
        let amount = price * U256::from(months as u64);
        if evm::erc20_balance(token, &owner).await.unwrap_or_default() < amount {
            continue;
        }
        let token_addr = evm::parse_address(token)?;
        crate::wallets::ensure_allowance(&agent.id, &agent.evm_wallet, &owner, token_addr, reg, amount).await?;
        let data = OrvynRegistry::payCall { agentId: agent.id.clone(), token: token_addr, amount }.abi_encode();
        let tx = crate::wallets::agent_send(&agent.id, &agent.evm_wallet, reg, data.into(), U256::ZERO).await?;
        log_agent(&agent.id, AgentLogKind::Fees, &format!("{label}: the agent paid {} {symbol} for {} of hosting", crate::auth::js_number(evm::to_bnb(amount)), months_label(months))).await?;
        attach_receipt(&agent.id, tx).await;
        return Ok(PayOutcome::Paid);
    }
    let bnb_price = month_price(&config, tier, NATIVE_TOKEN);
    if bnb_price > U256::ZERO {
        let amount = bnb_price * U256::from(months as u64);
        let balance = evm::bnb_balance(&agent.evm_address).await?;
        let reserve = evm::to_wei(agent.gas_reserve_bnb);
        if balance > reserve && balance - reserve >= amount {
            let data = OrvynRegistry::payCall { agentId: agent.id.clone(), token: Address::ZERO, amount: U256::ZERO }.abi_encode();
            let tx = crate::wallets::agent_send(&agent.id, &agent.evm_wallet, reg, data.into(), amount).await?;
            log_agent(&agent.id, AgentLogKind::Fees, &format!("{label}: the agent paid {} BNB for {} of hosting", crate::auth::js_number(evm::to_bnb(amount)), months_label(months))).await?;
            attach_receipt(&agent.id, tx).await;
            return Ok(PayOutcome::Paid);
        }
    }
    log_agent(&agent.id, AgentLogKind::Error, &format!("{label}: not enough USDT, USDC or free BNB to pay {} of hosting. Top up the agent or pay from a wallet.", months_label(months))).await?;
    Ok(PayOutcome::Insufficient)
}

/// Autopay: renew one month when within BILLING_RENEW_DAYS of expiry.
pub async fn autopay(agent: &Agent) -> Result<PayOutcome> {
    if agent.billing_mode != "autopay" || !agent.registered {
        return Ok(PayOutcome::Skipped);
    }
    let renew_at = agent.paid_until.map(|t| t.timestamp_millis()).unwrap_or(0) - (env().billing_renew_days * DAY_MS as f64) as i64;
    if chrono::Utc::now().timestamp_millis() < renew_at {
        return Ok(PayOutcome::NotDue);
    }
    pay_from_treasury(agent, 1, "Autopay").await
}

/// The owner asked the agent to pay `months` now from its treasury.
pub async fn request_treasury_payment(agent: &Agent, months: i32) -> Result<PayOutcome> {
    let updated: Agent = sqlx::query_as("update agents set pending_treasury_months = $2 where id = $1 returning *").bind(&agent.id).bind(months).fetch_one(db()).await?;
    settle_treasury_payment(&updated).await
}

/// Complete a pending owner-requested treasury payment, if any.
pub async fn settle_treasury_payment(agent: &Agent) -> Result<PayOutcome> {
    if agent.pending_treasury_months <= 0 || !agent.registered {
        return Ok(PayOutcome::None);
    }
    let result = pay_from_treasury(agent, agent.pending_treasury_months, "Treasury payment").await;
    sqlx::query("update agents set pending_treasury_months = 0 where id = $1").bind(&agent.id).execute(db()).await?;
    result
}

/// Unsigned transactions that pay `months` of hosting for `agent_id` from any wallet. None if the
/// registry doesn't accept that token.
pub async fn payment_calls(agent_id: &str, token: &str, months: u32) -> Result<Option<Calls>> {
    let config = registry_config().await?;
    let tier = Tier::from_index(registry_agent(agent_id).await?.map(|r| r.tier as u64).unwrap_or(0));
    let amount = month_price(&config, tier, token) * U256::from(months);
    if amount == U256::ZERO {
        return Ok(None);
    }
    let native = token.to_lowercase() == NATIVE_TOKEN;
    let token_addr = evm::parse_address(token)?;
    let pay = Call {
        to: config.contract.clone(),
        data: hex0x(&OrvynRegistry::payCall { agentId: agent_id.into(), token: token_addr, amount: if native { U256::ZERO } else { amount } }.abi_encode()),
        value: if native { amount.to_string() } else { "0".into() },
    };
    let calls = if native {
        vec![pay]
    } else {
        let approve = evm::IERC20::approveCall { spender: registry_contract()?, amount }.abi_encode();
        vec![Call { to: token.into(), data: hex0x(&approve), value: "0".into() }, pay]
    };
    Ok(Some(Calls { chain_id: BSC.chain_id, amount: Some(amount.to_string()), calls }))
}
