//! Binance Agent ID (`<name>.ai`) on BNB Smart Chain: an ERC-721 whose token id is
//! `uint256(keccak256(name))`. An agent whose wallet (or whose owner's wallet) holds `alice.ai`
//! gets the handle `alice` (alice.orvyn.cash, alice@orvyn.cash) with a verified badge. Agent ID
//! holders are Orvyn's premium tier.

use alloy::primitives::{keccak256, Address, U256};
use alloy::sol;

use crate::db::{db, log_agent, Agent, AgentLogKind};
use crate::error::{Error, Result};
use crate::evm;

sol! {
    #[sol(rpc)]
    interface AgentIdRegistry {
        function ownerOf(uint256 tokenId) view returns (address);
        function balanceOf(address owner) view returns (uint256);
    }
}

pub const ORVYN_AGENT_ID: &str = "orvyn.ai";

pub fn registry() -> Address {
    crate::x402::agent_id_registry().parse().expect("Agent ID registry")
}

/// "alice", "Alice.ai" or " alice.AI " → "alice"; None if it isn't a plausible label.
pub fn agent_id_label(input: &str) -> Option<String> {
    let l = input.trim().to_lowercase();
    let label = l.strip_suffix(".ai").unwrap_or(&l).to_string();
    let ok = !label.is_empty()
        && label.len() <= 63
        && label.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && label.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    ok.then_some(label)
}

pub fn agent_id_token_id(label: &str) -> U256 {
    U256::from_be_bytes(keccak256(label.as_bytes()).0)
}

/// The wallet holding `<label>.ai`, or None if it isn't minted (ownerOf reverts). RPC errors are
/// errors, so an outage is never mistaken for "not minted".
pub async fn agent_id_owner(label: &str) -> Result<Option<Address>> {
    match AgentIdRegistry::new(registry(), evm::bsc()).ownerOf(agent_id_token_id(label)).call().await {
        Ok(owner) => Ok(Some(owner)),
        Err(e) if e.to_string().to_lowercase().contains("revert") => Ok(None),
        Err(e) => Err(evm::rpc_error(e)),
    }
}

pub async fn holds_agent_id(wallet: &str) -> bool {
    let Ok(w) = evm::parse_address(wallet) else { return false };
    AgentIdRegistry::new(registry(), evm::bsc()).balanceOf(w).call().await.map(|b| b > U256::ZERO).unwrap_or(false)
}

/// Which of `wallets` holds `<label>.ai`.
pub async fn verify_agent_id(label: &str, wallets: &[String]) -> Result<String> {
    let owner = agent_id_owner(label).await?.ok_or_else(|| Error::invalid(format!("{label}.ai isn't a registered Agent ID.")))?;
    wallets.iter().find(|w| w.eq_ignore_ascii_case(&format!("{owner:#x}"))).cloned().ok_or_else(|| Error::invalid(format!("{label}.ai is held by {owner}, not by this agent or its owner.")))
}

/// Whether `handle` may be taken by an agent with these wallets: true unless it's a minted Agent
/// ID held by someone else. An RPC outage fails open (the name is rechecked on claim).
pub async fn handle_free_of_agent_id(handle: &str, wallets: &[String]) -> bool {
    match agent_id_owner(handle).await.ok().flatten() {
        None => true,
        Some(owner) => wallets.iter().any(|w| w.eq_ignore_ascii_case(&format!("{owner:#x}"))),
    }
}

fn wallets_of(agent: &Agent) -> Vec<String> {
    [agent.evm_address.clone(), agent.owner_account_id.clone()].into_iter().filter(|w| evm::is_address(w)).collect()
}

/// Make `<label>.ai` the agent's identity (verified on chain); its handle becomes the label.
pub async fn claim_agent_id(agent: &Agent, name: &str) -> Result<Agent> {
    let label = agent_id_label(name).ok_or_else(|| Error::invalid("Enter an Agent ID like alice.ai."))?;
    if let Some(why) = crate::public::validate_handle(&label) {
        return Err(Error::invalid(format!("{label}.ai can't be used as an Orvyn address: {why}")));
    }
    let holder = verify_agent_id(&label, &wallets_of(agent)).await?;
    let taken: Option<String> = sqlx::query_scalar("select id from agents where handle = $1 and id <> $2").bind(&label).bind(&agent.id).fetch_optional(db()).await?;
    if taken.is_some() {
        return Err(Error::conflict(format!("{label}.orvyn.cash is used by another agent; contact support to reclaim it with your Agent ID.")));
    }
    let updated: Agent = sqlx::query_as("update agents set handle = $2, agent_id_name = $2, agent_id_verified_at = now() where id = $1 returning *").bind(&agent.id).bind(&label).fetch_one(db()).await?;
    log_agent(&agent.id, AgentLogKind::Identity, &format!("Verified Agent ID {label}.ai (held by {holder}); address is now {label}@orvyn.cash.")).await?;
    Ok(updated)
}

/// Recheck an agent's Agent ID on chain. Moved away: the badge goes (handle and mailbox stay). An
/// RPC failure changes nothing. Returns whether it's still verified.
pub async fn recheck_agent_id(agent: &Agent) -> Result<bool> {
    let Some(label) = agent.agent_id_name.clone() else { return Ok(false) };
    let owner = match agent_id_owner(&label).await {
        Ok(o) => o,
        Err(_) => return Ok(is_premium(agent)),
    };
    if owner.is_some_and(|o| wallets_of(agent).iter().any(|w| w.eq_ignore_ascii_case(&format!("{o:#x}")))) {
        sqlx::query("update agents set agent_id_verified_at = now() where id = $1").bind(&agent.id).execute(db()).await?;
        return Ok(true);
    }
    sqlx::query("update agents set agent_id_name = null, agent_id_verified_at = null where id = $1").bind(&agent.id).execute(db()).await?;
    log_agent(&agent.id, AgentLogKind::Identity, &format!("Agent ID {label}.ai is no longer held by this agent or its owner; badge removed.")).await?;
    Ok(false)
}

pub async fn recheck_agent_ids() -> Result<(usize, usize)> {
    let rows: Vec<Agent> = sqlx::query_as("select * from agents where agent_id_name is not null").fetch_all(db()).await?;
    let mut revoked = 0;
    for a in &rows {
        if !recheck_agent_id(a).await? {
            revoked += 1;
        }
    }
    Ok((rows.len(), revoked))
}

/// Agents with a verified Binance Agent ID are Orvyn's premium tier.
pub fn is_premium(agent: &Agent) -> bool {
    agent.agent_id_name.is_some() && agent.agent_id_verified_at.is_some()
}

pub fn premium_required(feature: &str) -> Error {
    Error::forbidden(format!("{feature} is for agents with a verified Binance Agent ID. Claim yours (name.ai) to unlock it."))
}

/// Errors unless the agent has a verified Agent ID. Every exclusive service calls this.
pub fn require_premium(agent: &Agent, feature: &str) -> Result<()> {
    if is_premium(agent) { Ok(()) } else { Err(premium_required(feature)) }
}
