//! What may be signed for Aster perps (Futures API V3).
//!
//! - The agent's main wallet (the Aster account, "user") may only: approve its own Aster API wallet
//!   (perps only, no spot, no withdrawals, no IP list, at most a year); withdraw USDT from Aster to
//!   itself; deposit USDT into Aster's vault for itself (approve + depositFor), within a daily cap.
//! - Its Aster API wallet (a second Privy wallet) may only sign API requests for this agent's
//!   account: orders on allowed symbols within the per-order USD cap, leverage up to the cap,
//!   cancels, reads, and the envelope of a withdrawal to the agent itself.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;

use alloy::primitives::U256;
use alloy::sol;
use alloy::sol_types::SolCall;
use serde_json::Value;

use super::config::cfg;
use super::{add_spend, addr, refuse, usd_micros, Checked, EvmAgent, Spends, TypedData};
use crate::error::Result;

pub const VAULT: &str = "0x128463a60784c4d3f46c23af3f65ed859ba87974";
pub const USDT: &str = "0x55d398326f99059ff775485246999027b3197955";
pub const SIGNING_CHAIN_ID: u64 = 1666;
pub const ACCOUNT_CHAIN_ID: u64 = 56;
const ZERO: &str = "0x0000000000000000000000000000000000000000";

sol! {
    interface AsterVault {
        function depositFor(address currency, address forAddress, uint256 amount, uint256 broker) payable;
    }
    interface Erc20Approve {
        function approve(address spender, uint256 amount) returns (bool);
    }
}

pub const MESSAGE_TYPES: &[(&str, &[(&str, &str)])] = &[("Message", &[("msg", "string")])];
pub const WITHDRAW_TYPES: &[(&str, &[(&str, &str)])] = &[(
    "Action",
    &[
        ("type", "string"),
        ("destination", "address"),
        ("destination Chain", "string"),
        ("token", "string"),
        ("amount", "string"),
        ("fee", "string"),
        ("nonce", "uint256"),
        ("aster chain", "string"),
    ],
)];

pub fn is_aster_domain(typed: &TypedData) -> bool {
    matches!(typed.domain_str("name"), Some("AsterSignTransaction") | Some("Aster"))
}

/// The query string inside an Aster "Message" signature, as ordered pairs (no repeats).
fn message_params(typed: &TypedData, chain_id: u64) -> Result<Vec<(String, String)>> {
    let d = &typed.domain;
    let vc = d.get("verifyingContract").and_then(Value::as_str).unwrap_or("").to_lowercase();
    if typed.domain_str("name") != Some("AsterSignTransaction") || typed.domain_str("version") != Some("1") || typed.domain_chain_id() != Some(chain_id) || vc != ZERO {
        return Err(refuse("not an Aster request signature"));
    }
    if typed.primary_type != "Message" || !typed.has_types(MESSAGE_TYPES) {
        return Err(refuse("unexpected Aster typed data"));
    }
    let Some(msg) = typed.message.get("msg").and_then(Value::as_str).filter(|m| m.chars().count() <= 4000) else {
        return Err(refuse("bad Aster request"));
    };
    let params: Vec<(String, String)> = url::form_urlencoded::parse(msg.as_bytes()).map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    let keys: HashSet<&String> = params.iter().map(|(k, _)| k).collect();
    if keys.len() != params.len() {
        return Err(refuse("repeated parameters in an Aster request"));
    }
    Ok(params)
}

fn get<'a>(p: &'a [(String, String)], k: &str) -> Option<&'a str> {
    p.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str())
}

/// Main wallet: Aster typed data (approving its API wallet, withdrawing to itself).
pub fn check_aster_main_typed_data(typed: &TypedData, agent: &EvmAgent) -> Result<Checked> {
    if typed.domain_str("name") == Some("Aster") {
        let vc = typed.domain.get("verifyingContract").and_then(Value::as_str).unwrap_or("").to_lowercase();
        if typed.domain_str("version") != Some("1") || typed.domain_chain_id() != Some(ACCOUNT_CHAIN_ID) || vc != ZERO {
            return Err(refuse("unexpected Aster withdrawal domain"));
        }
        if typed.primary_type != "Action" || !typed.has_types(WITHDRAW_TYPES) {
            return Err(refuse("unexpected Aster withdrawal types"));
        }
        let m = &typed.message;
        let s = |k: &str| m.get(k).and_then(Value::as_str);
        if s("type") != Some("Withdraw") {
            return Err(refuse("only Aster withdrawals can be signed"));
        }
        if s("destination").map(str::to_lowercase).as_deref() != Some(agent.address.to_lowercase().as_str()) {
            return Err(refuse("Aster withdrawals may only go to the agent's own wallet"));
        }
        if s("destination Chain") != Some("BSC") || s("token") != Some("USDT") || s("aster chain") != Some("Mainnet") {
            return Err(refuse("only USDT withdrawals to BNB Chain"));
        }
        let amount = m.get("amount").map(|a| a.as_str().map(String::from).unwrap_or_else(|| a.to_string())).unwrap_or_default();
        return Ok(Checked::new(format!("Aster withdrawal of {amount} USDT to the agent"), Spends::new()));
    }
    let p = message_params(typed, ACCOUNT_CHAIN_ID)?;
    let lower = |k: &str| get(&p, k).map(str::to_lowercase);
    if lower("user").as_deref() != Some(agent.address.to_lowercase().as_str()) {
        return Err(refuse("the Aster account must be the agent's own wallet"));
    }
    let Some(agent_address) = lower("agentAddress") else {
        return Err(refuse("the main wallet may only approve the agent's Aster API wallet"));
    };
    if agent.aster_signer.as_deref().map(str::to_lowercase) != Some(agent_address.clone()) {
        return Err(refuse("only the agent's own Aster API wallet can be approved"));
    }
    if get(&p, "canWithdraw") != Some("false") || get(&p, "canSpotTrade") != Some("false") || get(&p, "canPerpTrade") != Some("true") {
        return Err(refuse("the Aster API wallet may trade perps only (no spot, no withdrawals)"));
    }
    if get(&p, "ipWhitelist").unwrap_or("") != "" {
        return Err(refuse("no IP whitelist"));
    }
    if get(&p, "signatureChainId") != Some("56") {
        return Err(refuse("wrong Aster signature chain"));
    }
    let expired: f64 = get(&p, "expired").and_then(|v| v.parse().ok()).unwrap_or(f64::NAN);
    let limit = super::now_ms() as f64 + 366.0 * 86_400_000.0;
    if !expired.is_finite() || expired > limit {
        return Err(refuse("Aster API wallet approvals last at most a year"));
    }
    Ok(Checked::new(format!("approve Aster API wallet {} (perps only)", get(&p, "agentAddress").unwrap_or("")), Spends::new()))
}

/// The mark price of a symbol (for the per-order cap).
pub type MarkPrice = Box<dyn Fn(String) -> Pin<Box<dyn Future<Output = Result<f64>> + Send>> + Send + Sync>;

/// Aster's mark price from its public API.
pub fn aster_mark_price() -> MarkPrice {
    Box::new(|symbol| {
        Box::pin(async move {
            let base = crate::env::env().aster_api_url.trim_end_matches('/').to_string();
            let res = crate::http::client()
                .get(format!("{base}/fapi/v3/premiumIndex"))
                .query(&[("symbol", &symbol)])
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await;
            let price = match res {
                Ok(r) if r.status().is_success() => r.json::<Value>().await.ok().and_then(|b| b.get("markPrice").and_then(Value::as_str).and_then(|p| p.parse::<f64>().ok())),
                _ => None,
            };
            price.filter(|p| *p > 0.0).ok_or_else(|| refuse(format!("no Aster mark price for {symbol}")))
        })
    })
}

const ORDER_KEYS: &[&str] = &["symbol", "side", "positionSide", "type", "timeInForce", "quantity", "reduceOnly", "price", "newClientOrderId", "stopPrice", "closePosition", "workingType", "priceProtect", "newOrderRespType"];
const ENVELOPE: &[&str] = &["nonce", "user", "signer", "recvWindow", "timestamp"];

/// A number as JavaScript's Number() reads a parameter (NaN for junk).
fn js_num(v: Option<&str>) -> f64 {
    match v {
        None => f64::NAN,
        Some(s) if s.trim().is_empty() => 0.0,
        Some(s) => s.trim().parse().unwrap_or(f64::NAN),
    }
}

/// Aster API wallet: one API request (orders, leverage, cancels, reads, withdrawal envelopes).
pub async fn check_aster_signer_typed_data(typed: &TypedData, agent: &EvmAgent, signer: &str, mark_price: &MarkPrice) -> Result<Checked> {
    let p = message_params(typed, SIGNING_CHAIN_ID)?;
    let lower = |k: &str| get(&p, k).map(str::to_lowercase);
    if lower("signer").as_deref() != Some(signer.to_lowercase().as_str()) {
        return Err(refuse("the request must name this API wallet as its signer"));
    }
    if get(&p, "user").is_some() && lower("user").as_deref() != Some(agent.address.to_lowercase().as_str()) {
        return Err(refuse("the request is for another Aster account"));
    }
    let keys: Vec<&str> = p.iter().map(|(k, _)| k.as_str()).filter(|k| !ENVELOPE.contains(k)).collect();
    let symbol = get(&p, "symbol").map(str::to_uppercase);
    let c = cfg();
    let mut spends = Spends::new();

    // Withdrawal envelope: only to the agent.
    if get(&p, "receiver").is_some() {
        if lower("receiver").as_deref() != Some(agent.address.to_lowercase().as_str()) {
            return Err(refuse("Aster withdrawals may only go to the agent's own wallet"));
        }
        if get(&p, "asset") != Some("USDT") || get(&p, "chainId") != Some("56") {
            return Err(refuse("only USDT withdrawals to BNB Chain"));
        }
        return Ok(Checked::new(format!("Aster withdrawal request of {} USDT to the agent", get(&p, "amount").unwrap_or("null")), spends));
    }
    if let Some(s) = &symbol {
        if !c.aster_symbols.contains(s) {
            return Err(refuse(format!("{s} isn't an allowed Aster market")));
        }
    }
    // Leverage.
    if get(&p, "leverage").is_some() {
        let Some(symbol) = symbol.filter(|_| keys.iter().all(|k| *k == "symbol" || *k == "leverage")) else {
            return Err(refuse("unexpected leverage request"));
        };
        let leverage = js_num(get(&p, "leverage"));
        if leverage.fract() != 0.0 || !leverage.is_finite() || leverage < 1.0 || leverage > c.aster_max_leverage {
            return Err(refuse(format!("leverage may be at most {}x", crate::auth::js_number(c.aster_max_leverage))));
        }
        return Ok(Checked::new(format!("set {symbol} leverage to {}x", crate::auth::js_number(leverage)), spends));
    }
    // New order.
    if get(&p, "side").is_some() || get(&p, "type").is_some() {
        let Some(symbol) = symbol.filter(|_| keys.iter().all(|k| ORDER_KEYS.contains(k))) else {
            return Err(refuse("unexpected order parameters"));
        };
        let side = get(&p, "side").unwrap_or("");
        if side != "BUY" && side != "SELL" {
            return Err(refuse("bad order side"));
        }
        let kind = get(&p, "type").unwrap_or("");
        if !["MARKET", "LIMIT", "STOP_MARKET", "TAKE_PROFIT_MARKET"].contains(&kind) {
            return Err(refuse(format!("{kind} orders aren't allowed")));
        }
        if get(&p, "reduceOnly") == Some("true") || get(&p, "closePosition") == Some("true") {
            return Ok(Checked::new(format!("{kind} {side} {symbol} (reduce only)"), spends));
        }
        if kind != "MARKET" && kind != "LIMIT" {
            return Err(refuse("stop and take-profit orders must be reduce only"));
        }
        let quantity = js_num(get(&p, "quantity"));
        if quantity.is_nan() || quantity <= 0.0 {
            return Err(refuse("bad order quantity"));
        }
        let (price, mark) = if kind == "LIMIT" {
            (js_num(get(&p, "price")), mark_price(symbol.clone()).await?)
        } else {
            let m = mark_price(symbol.clone()).await?;
            (m, m)
        };
        if price.is_nan() || price <= 0.0 || mark.is_nan() || mark <= 0.0 {
            return Err(refuse("no price for the order"));
        }
        let usd = quantity * price.max(mark);
        if usd > c.aster_max_order_usd {
            return Err(refuse(format!("Aster orders may be at most ${} (this one is ${usd:.2})", crate::auth::js_number(c.aster_max_order_usd))));
        }
        add_spend(&mut spends, "aster-order-usd", U256::from((usd * 1e6).ceil() as u128));
        return Ok(Checked::new(format!("{kind} {side} {} {symbol} (~${usd:.2})", crate::auth::js_number(quantity)), spends));
    }
    // Cancels and reads: symbol, order ids, nothing else.
    if keys.iter().all(|k| ["symbol", "orderId", "origClientOrderId", "limit", "startTime", "endTime"].contains(k)) {
        let summary = if keys.is_empty() { "read".to_string() } else { format!("read or cancel ({})", keys.join(", ")) };
        return Ok(Checked::new(summary, spends));
    }
    Err(refuse(format!("unexpected Aster request ({})", keys.join(", "))))
}

/// Main wallet transactions: USDT approval of Aster's vault, and depositing for itself. None if not Aster.
pub fn check_aster_transaction(to: &str, data: &[u8], value: U256, agent: &EvmAgent, spends: &mut Spends) -> Result<Option<String>> {
    if to == VAULT {
        let call = AsterVault::depositForCall::abi_decode(data).map_err(|_| refuse("only depositFor can be called on Aster's vault"))?;
        if addr(&call.currency) != USDT || value != U256::ZERO {
            return Err(refuse("only USDT can be deposited to Aster"));
        }
        if addr(&call.forAddress) != agent.address.to_lowercase() {
            return Err(refuse("Aster deposits must be for the agent's own account"));
        }
        if call.broker == U256::from(1000u64) {
            return Err(refuse("Aster deposits go to the futures account"));
        }
        add_spend(spends, "aster-usd", usd_micros(call.amount));
        return Ok(Some(format!("deposit {} USDT to Aster for the agent", call.amount)));
    }
    if to == USDT && data.starts_with(&[0x09, 0x5e, 0xa7, 0xb3]) {
        let Ok(call) = Erc20Approve::approveCall::abi_decode(data) else { return Ok(None) };
        if addr(&call.spender) != VAULT {
            return Ok(None);
        }
        if usd_micros(call.amount) > cfg().daily_cap_aster_usd {
            return Err(refuse("approval above the daily Aster deposit cap"));
        }
        return Ok(Some(format!("approve {} USDT for Aster's vault", call.amount)));
    }
    Ok(None)
}
