//! Orvyn Virtual Card: a Visa card for an agent, set up by its owner through Privy's card issuing.
//! Off by default. Orvyn records which agent it's for, whether the agent may use it (off until the
//! owner turns it on) and the owner's limits; it never sees the card number. The agent can't spend
//! with it on its own yet: Privy offers no server-side access to card details or payments.

use serde_json::{json, Value};

use crate::db::{db, log_agent, Agent, AgentCard, AgentLogKind};
use crate::error::{Error, Result};

fn card_error(m: impl Into<String>) -> Error {
    Error::invalid(m)
}

pub fn card_view(c: &AgentCard) -> Value {
    json!({
        "cardId": c.card_id,
        "environment": c.environment,
        "chainId": c.chain_id,
        "asset": c.asset,
        "enabled": c.enabled,
        "perPurchaseLimitUsd": c.per_purchase_limit_usd,
        "monthlyLimitUsd": c.monthly_limit_usd,
        "createdAt": c.created_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    })
}

fn owner_only(agent: &Agent, account_id: &str) -> Result<()> {
    if agent.owner_account_id.to_lowercase() != account_id.to_lowercase() {
        return Err(card_error("only the agent's owner can manage its card"));
    }
    Ok(())
}

pub async fn agent_card(agent_id: &str) -> Result<Option<Value>> {
    let row: Option<AgentCard> = sqlx::query_as("select * from agent_cards where agent_id = $1").bind(agent_id).fetch_optional(db()).await?;
    Ok(row.as_ref().map(card_view))
}

pub struct NewCard<'a> {
    pub card_id: &'a str,
    /// "sandbox" | "production".
    pub environment: &'a str,
    pub chain_id: &'a str,
    pub asset: &'a str,
}

/// Record the card the owner just created with Privy for this agent (off for the agent).
pub async fn link_card(agent: &Agent, account_id: &str, card: NewCard<'_>) -> Result<Value> {
    owner_only(agent, account_id)?;
    let ok = (4..=128).contains(&card.card_id.len()) && card.card_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if !ok {
        return Err(card_error("bad card id"));
    }
    if card.environment != "sandbox" && card.environment != "production" {
        return Err(card_error("environment must be sandbox or production"));
    }
    let row: AgentCard = sqlx::query_as(
        "insert into agent_cards (agent_id, owner_account_id, card_id, environment, chain_id, asset, enabled) values ($1, $2, $3, $4, $5, $6, false)
         on conflict (agent_id) do update set card_id = excluded.card_id, environment = excluded.environment, chain_id = excluded.chain_id, asset = excluded.asset, owner_account_id = excluded.owner_account_id, enabled = false
         returning *",
    )
    .bind(&agent.id)
    .bind(account_id.to_lowercase())
    .bind(card.card_id)
    .bind(card.environment)
    .bind(card.chain_id)
    .bind(card.asset)
    .fetch_one(db())
    .await?;
    log_agent(&agent.id, AgentLogKind::Payment, &format!("Owner set up an Orvyn Virtual Card ({}); off for the agent until turned on", card.environment)).await?;
    Ok(card_view(&row))
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CardUpdate {
    pub enabled: Option<bool>,
    pub per_purchase_limit_usd: Option<f64>,
    pub monthly_limit_usd: Option<f64>,
}

/// Turn the agent's use of the card on or off, and set its limits.
pub async fn update_card(agent: &Agent, account_id: &str, u: CardUpdate) -> Result<Value> {
    owner_only(agent, account_id)?;
    if u.per_purchase_limit_usd.is_some_and(|l| !(l > 0.0 && l <= 10_000.0)) {
        return Err(card_error("per-purchase limit must be $0-10,000"));
    }
    if u.monthly_limit_usd.is_some_and(|l| !(l > 0.0 && l <= 100_000.0)) {
        return Err(card_error("monthly limit must be $0-100,000"));
    }
    let row: Option<AgentCard> = sqlx::query_as(
        "update agent_cards set enabled = coalesce($2, enabled), per_purchase_limit_usd = coalesce($3, per_purchase_limit_usd), monthly_limit_usd = coalesce($4, monthly_limit_usd) where agent_id = $1 returning *",
    )
    .bind(&agent.id)
    .bind(u.enabled)
    .bind(u.per_purchase_limit_usd)
    .bind(u.monthly_limit_usd)
    .fetch_optional(db())
    .await?;
    let row = row.ok_or_else(|| card_error("this agent has no card"))?;
    if let Some(e) = u.enabled {
        log_agent(&agent.id, AgentLogKind::Payment, &format!("Owner turned the virtual card {} for the agent", if e { "on" } else { "off" })).await?;
    }
    Ok(card_view(&row))
}

/// Forget the link (cancel the card itself in Privy's card view).
pub async fn unlink_card(agent: &Agent, account_id: &str) -> Result<()> {
    owner_only(agent, account_id)?;
    sqlx::query("delete from agent_cards where agent_id = $1").bind(&agent.id).execute(db()).await?;
    log_agent(&agent.id, AgentLogKind::Payment, "Owner removed the virtual card").await?;
    Ok(())
}
