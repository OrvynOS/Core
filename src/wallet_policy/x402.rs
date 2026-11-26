//! x402 payments an agent's key may sign without its owner: the x402 `exact` scheme over Permit2 on
//! BNB Smart Chain, and the one-time Permit2 approval it needs.
//!
//! - Typed data: exactly a Permit2 `PermitWitnessTransferFrom` (no extra or altered types) whose
//!   spender is the x402 exact Permit2 proxy, for an accepted stablecoin, at most
//!   `x402_max_per_payment`, expiring within `x402_max_deadline_sec`. Each counts against the daily
//!   `x402-usd` cap when signed (whether or not the seller settles it).
//! - Transactions: `approve(Permit2, amount)` on an accepted stablecoin, at most `x402_max_approval`.

use alloy::primitives::U256;

use super::config::cfg;
use super::{add_spend, json_lower, json_u256, now_secs, refuse, usd_micros, Checked, EvmAgent, Spends, TypedData};
use crate::error::Result;

pub const PERMIT2: &str = "0x000000000022d473030f116ddee9f6b43ac78ba3";
const BSC: u64 = 56;

/// The x402 exact-scheme Permit2 proxy (X402_PERMIT2_PROXY overrides).
pub fn x402_permit2_proxy() -> String {
    std::env::var("X402_PERMIT2_PROXY").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "0x402085c248EeA27D92E8b30b2C58ed07f9E20001".into()).to_lowercase()
}

const EXPECTED_TYPES: &[(&str, &[(&str, &str)])] = &[
    ("PermitWitnessTransferFrom", &[("permitted", "TokenPermissions"), ("spender", "address"), ("nonce", "uint256"), ("deadline", "uint256"), ("witness", "Witness")]),
    ("TokenPermissions", &[("token", "address"), ("amount", "uint256")]),
    ("Witness", &[("to", "address"), ("validAfter", "uint256")]),
];

pub fn is_permit2_domain(typed: &TypedData) -> bool {
    typed.domain_str("name") == Some("Permit2")
}

pub fn check_x402_permit(typed: &TypedData, agent: &EvmAgent) -> Result<Checked> {
    let c = cfg();
    if json_lower(typed.domain.get("verifyingContract")) != PERMIT2 || typed.domain_chain_id() != Some(BSC) {
        return Err(refuse("only Permit2 on BNB Chain can be signed"));
    }
    if typed.primary_type != "PermitWitnessTransferFrom" || !typed.has_types(EXPECTED_TYPES) {
        return Err(refuse("only x402 Permit2 payments (PermitWitnessTransferFrom with the x402 witness) can be signed"));
    }
    let m = &typed.message;
    if json_lower(m.get("spender")) != x402_permit2_proxy() {
        return Err(refuse("the Permit2 spender must be the x402 Permit2 proxy"));
    }
    let permitted = m.get("permitted").cloned().unwrap_or_default();
    let token = json_lower(permitted.get("token"));
    if !c.x402_tokens.contains(&token) {
        return Err(refuse(format!("x402 payments may only be made in {}", c.x402_tokens.join(", "))));
    }
    let amount = json_u256(permitted.get("amount"), "Permit2 message")?;
    if amount == U256::ZERO || amount > c.x402_max_per_payment {
        return Err(refuse(format!("an x402 payment may be at most {} base units", c.x402_max_per_payment)));
    }
    let deadline = json_u256(m.get("deadline"), "Permit2 message")?;
    let now = U256::from(now_secs());
    if deadline <= now || deadline > now + U256::from(c.x402_max_deadline_sec) {
        return Err(refuse(format!("an x402 payment must expire within {}s", c.x402_max_deadline_sec)));
    }
    let to = json_lower(m.get("witness").and_then(|w| w.get("to")));
    if !(to.len() == 42 && to.starts_with("0x") && to[2..].chars().all(|ch| ch.is_ascii_hexdigit())) {
        return Err(refuse("the x402 payee is missing"));
    }
    if to == agent.address.to_lowercase() {
        return Err(refuse("an agent can't pay itself"));
    }
    let mut spends = Spends::new();
    add_spend(&mut spends, "x402-usd", usd_micros(amount));
    Ok(Checked::new(format!("x402 payment of {amount} {token} to {to}"), spends))
}

/// `approve(Permit2, amount)` for x402, or None if this isn't one.
pub fn check_permit2_approval(chain_id: u64, token: &str, spender: &str, amount: U256) -> Result<Option<String>> {
    if spender.to_lowercase() != PERMIT2 {
        return Ok(None);
    }
    let c = cfg();
    if chain_id != BSC || !c.x402_tokens.contains(&token.to_lowercase()) {
        return Err(refuse("Permit2 may only be approved for x402 stablecoins on BNB Chain"));
    }
    if amount > c.x402_max_approval {
        return Err(refuse(format!("Permit2 approvals are capped at {} base units", c.x402_max_approval)));
    }
    Ok(Some(format!("approve {amount} of {token} for Permit2 (x402)")))
}
