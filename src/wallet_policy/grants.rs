//! Owner grants: a withdrawal the agent's owner signed with their wallet (EIP-191 over the
//! withdrawal message in withdraw). withdraw checks it before sending; the wallet policy checks it
//! again against the exact transaction being signed (asset, recipient, amount), so no other code path
//! can move an agent's funds out. Each grant signs one transaction.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use alloy::primitives::U256;
use serde::{Deserialize, Serialize};

use super::config::cfg;
use crate::error::{Error, Result};
use crate::evm;

/// A withdrawal the agent's owner signed: the exact message and the owner wallet's signature.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnerGrant {
    pub message: String,
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Withdrawal {
    /// "bsc:native" or "bsc:0x<token>" (lowercase).
    pub asset: String,
    pub to: String,
    /// In the asset's smallest unit.
    pub amount: U256,
    pub nonce: String,
}

fn grant_error(m: impl Into<String>) -> Error {
    super::refuse(m)
}

fn field<'a>(lines: &[&'a str], name: &str) -> Result<&'a str> {
    let prefix = format!("{name}: ");
    lines
        .iter()
        .find(|l| l.starts_with(&prefix))
        .map(|l| l[prefix.len()..].trim())
        .ok_or_else(|| grant_error(format!("the signed withdrawal has no {name}")))
}

static DECIMALS: LazyLock<Mutex<HashMap<String, u8>>> = LazyLock::new(Default::default);

async fn decimals_of(asset: &str) -> Result<u8> {
    if asset == "bsc:native" {
        return Ok(18);
    }
    let token = asset.strip_prefix("bsc:").filter(|t| t.len() == 42 && t.starts_with("0x") && t[2..].chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    let Some(token) = token else { return Err(grant_error(format!("unsupported asset {asset}"))) };
    if let Some(d) = DECIMALS.lock().unwrap().get(asset) {
        return Ok(*d);
    }
    let d = evm::token_decimals("bsc", token).await?;
    DECIMALS.lock().unwrap().insert(asset.to_string(), d);
    Ok(d)
}

/// "1.25" → base units with `decimals`; refuses too many decimals.
pub fn to_units(amount: &str, decimals: u8) -> Result<U256> {
    let ok = !amount.is_empty()
        && amount.split_once('.').map_or(amount.chars().all(|c| c.is_ascii_digit()), |(w, f)| !w.is_empty() && !f.is_empty() && w.chars().all(|c| c.is_ascii_digit()) && f.chars().all(|c| c.is_ascii_digit()));
    if !ok {
        return Err(grant_error("the signed amount isn't a number"));
    }
    let (whole, frac) = amount.split_once('.').unwrap_or((amount, ""));
    if frac.len() > decimals as usize {
        return Err(grant_error("the signed amount has too many decimals"));
    }
    let frac = format!("{frac:0<width$}", width = decimals as usize);
    let whole = U256::from_str_radix(whole, 10).map_err(|_| grant_error("the signed amount isn't a number"))?;
    let frac = if frac.is_empty() { U256::ZERO } else { U256::from_str_radix(&frac, 10).map_err(|_| grant_error("the signed amount isn't a number"))? };
    Ok(whole * U256::from(10u64).pow(U256::from(decimals)) + frac)
}

/// Check a grant's text for `agent_id` at `now_ms` (no signature check): its nonce and fields.
pub fn check_grant_text(message: &str, agent_id: &str, now_ms: i64) -> Result<()> {
    let lines: Vec<&str> = message.split('\n').collect();
    if !lines.first().is_some_and(|l| l.starts_with("Withdraw from Orvyn agent ")) {
        return Err(grant_error("not an Orvyn withdrawal"));
    }
    if field(&lines, "Agent ID")? != agent_id {
        return Err(grant_error("the signed withdrawal is for another agent"));
    }
    if field(&lines, "Network")? != "BNB Chain" {
        return Err(grant_error("the signed withdrawal is for another network"));
    }
    let issued = chrono::DateTime::parse_from_rfc3339(field(&lines, "Issued At")?).map(|t| t.timestamp_millis());
    let fresh = issued.is_ok_and(|t| {
        let age = now_ms - t;
        (-60_000..=cfg().grant_max_age_ms).contains(&age)
    });
    if !fresh {
        return Err(grant_error("the owner's signature has expired"));
    }
    let nonce = field(&lines, "Nonce")?;
    if nonce.len() != 32 || !nonce.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()) {
        return Err(grant_error("bad nonce"));
    }
    Ok(())
}

/// Verify a grant for `agent_id` by its owner; returns the withdrawal it authorizes.
pub async fn verify_grant(grant: &OwnerGrant, agent_id: &str, owner: &str) -> Result<Withdrawal> {
    check_grant_text(&grant.message, agent_id, super::now_ms())?;
    let lines: Vec<&str> = grant.message.split('\n').collect();
    let owner_address = evm::parse_address(owner).map_err(|_| grant_error("the withdrawal wasn't signed by the agent's owner"))?;
    let sig = hex::decode(grant.signature.trim_start_matches("0x")).unwrap_or_default();
    let valid = evm::sig::verify_hash(owner_address, evm::sig::message_hash(&grant.message), &sig).await.unwrap_or(false);
    if !valid {
        return Err(grant_error("the withdrawal wasn't signed by the agent's owner"));
    }
    let asset = field(&lines, "Asset")?.to_lowercase();
    let decimals = decimals_of(&asset).await?;
    Ok(Withdrawal {
        to: field(&lines, "To")?.to_lowercase(),
        amount: to_units(field(&lines, "Amount")?, decimals)?,
        nonce: field(&lines, "Nonce")?.to_string(),
        asset,
    })
}
