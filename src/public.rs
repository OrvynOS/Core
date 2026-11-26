//! Public, read-only view of an agent: what its page and public read API expose. Everything is
//! built from an explicit allowlist: an Agent row is never spread into a public response.

use serde_json::{json, Value};

use crate::agentid::{handle_free_of_agent_id, is_premium};
use crate::db::{db, Agent};
use crate::env::env;
use crate::environment::public_environment;
use crate::error::Result;

pub const RESERVED_HANDLES: &[&str] = &[
    "www", "api", "app", "admin", "orvyn", "docs", "status", "mail", "email", "blog", "dashboard", "bliss", "registry", "swap", "help", "support", "static", "assets", "cdn", "dev",
    "staging", "test", "auth", "login", "account", "accounts", "agents", "agent", "create", "new", "pay", "billing", "ops", "op1", "vyn", "reel", "studio", "engine", "social", "postmaster",
    "abuse", "dmarc", "hostmaster", "webmaster", "security", "bounces", "bounce", "mailer-daemon", "noreply", "no-reply", "info", "hello", "contact", "team", "smtp", "imap", "mx",
    "dkim", "spf",
];

/// DNS-label rules: 3–32 chars, lowercase letters, digits and inner hyphens.
pub fn validate_handle(handle: &str) -> Option<String> {
    let ok = (3..=32).contains(&handle.len())
        && handle.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !handle.starts_with('-')
        && !handle.ends_with('-');
    if !ok {
        return Some("Handle must be 3-32 lowercase letters, digits or inner hyphens.".into());
    }
    if handle.contains("--") {
        return Some("Handle can't contain consecutive hyphens.".into());
    }
    if RESERVED_HANDLES.contains(&handle) {
        return Some(format!("\"{handle}\" is reserved."));
    }
    None
}

/// e.g. "atlas1938" from ticker ATLAS.
pub fn suggest_handle(symbol: &str) -> String {
    let base: String = symbol.to_lowercase().chars().filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit()).take(30).collect();
    let base = if base.is_empty() { "agent".into() } else { base };
    format!("{base}{}", 1000 + rand::random::<u32>() % 9000)
}

/// Free on Orvyn, and not a Binance Agent ID held by someone else.
pub async fn handle_available(handle: &str, wallets: &[String]) -> Result<bool> {
    let row: Option<String> = sqlx::query_scalar("select id from agents where handle = $1").bind(handle).fetch_optional(db()).await?;
    Ok(row.is_none() && handle_free_of_agent_id(handle, wallets).await)
}

/// An agent by handle; agents paused by Orvyn are hidden.
pub async fn agent_by_handle(handle: &str) -> Result<Option<Agent>> {
    let row: Option<Agent> = sqlx::query_as("select * from agents where handle = $1").bind(handle.to_lowercase()).fetch_optional(db()).await?;
    Ok(row.filter(|a| a.held_reason.is_none()))
}

/// One word for visitors: can this agent act right now?
pub fn public_status(agent: &Agent) -> String {
    if agent.status != "live" {
        return agent.status.clone();
    }
    if !agent.subscription_active || agent.hosting_status == "suspended" {
        return "suspended".into();
    }
    if agent.hosting_status == "failed" || agent.hosting_status == "provisioning" {
        return "offline".into();
    }
    "online".into()
}

/// The token's trade tax for holders.
pub fn tax_summary(tax: Option<&Value>) -> Value {
    let Some(t) = tax.filter(|t| t.is_object()) else { return Value::Null };
    json!({ "ratePct": t["feeRate"], "agentPct": t["recipientRate"], "burnPct": t["burnRate"], "holdersPct": t["divideRate"], "liquidityPct": t["liquidityRate"], "holderMinTokens": t["minSharing"] })
}

pub fn iso(t: chrono::DateTime<chrono::Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub async fn public_profile(agent: &Agent) -> Result<Value> {
    let plugins = crate::bliss::installed_plugins(&agent.id, false).await?;
    let token = crate::launchpad::token_stats(agent).await.ok().flatten().unwrap_or(Value::Null);
    let t = |k: &str| token.get(k).cloned().unwrap_or(Value::Null);
    Ok(json!({
        "id": agent.id,
        "handle": agent.handle,
        "email": agent.handle.as_ref().map(|h| format!("{h}@{}", env().orvyn_mail_domain)),
        "name": agent.name, "symbol": agent.symbol, "description": agent.description, "icon": agent.icon, "links": agent.links,
        "creator": agent.owner_account_id,
        "createdAt": iso(agent.created_at),
        "status": public_status(agent),
        "autopilot": agent.autopilot_enabled,
        "model": { "provider": agent.ai_provider, "model": agent.ai_model },
        "capabilities": plugins.iter().map(|p| json!({ "id": p.plugin.meta().id, "name": p.plugin.meta().name, "category": p.plugin.meta().category })).collect::<Vec<_>>(),
        "environment": public_environment(agent),
        "agentId": if is_premium(agent) { json!({ "name": format!("{}.ai", agent.agent_id_name.clone().unwrap_or_default()), "verifiedAt": agent.agent_id_verified_at.map(iso) }) } else { Value::Null },
        "premium": is_premium(agent),
        "wallet": agent.evm_address,
        "token": agent.token_address.as_ref().map(|addr| json!({
            "address": addr, "launchpad": agent.launchpad,
            "priceUsd": t("priceUsd"), "priceBnb": t("priceBnb"), "marketCapUsd": t("marketCapUsd"), "liquidityUsd": t("liquidityUsd"),
            "volume24hUsd": t("volume24hUsd"), "change24hPct": t("change24hPct"), "curveProgressPct": t("curveProgressPct"),
            "graduated": if token.is_null() { json!(agent.graduated) } else { t("graduated") },
            "tax": tax_summary(agent.tax.as_ref()), "tradeUrl": t("tradeUrl"),
        })),
    }))
}

/// The token's price now and a USD close series for `range`.
pub async fn public_price(agent: &Agent, range: &str) -> Result<Value> {
    let Some(token) = &agent.token_address else { return Ok(Value::Null) };
    let stats = crate::launchpad::token_stats(agent).await.ok().flatten().unwrap_or(Value::Null);
    let candles = crate::launchpad::token_candles(stats["pairAddress"].as_str(), range).await;
    Ok(json!({
        "token": token, "symbol": agent.symbol,
        "priceUsd": stats["priceUsd"], "priceBnb": stats["priceBnb"],
        "change24h": stats["change24hPct"].as_f64().map(|c| c / 100.0),
        "range": range, "unit": "USD",
        "series": candles.iter().map(|c| json!({ "t": c.t, "close": c.c, "high": c.h, "low": c.l, "volume": c.v })).collect::<Vec<_>>(),
    }))
}

const PUBLIC_LOG_KINDS: &[&str] = &["launch", "fees", "trade", "think", "plugin", "identity", "payment"];

pub async fn public_activity(agent_id: &str, limit: i64) -> Result<Vec<Value>> {
    let rows: Vec<(String, String, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as("select kind, message, created_at from agent_logs where agent_id = $1 and kind = any($2) order by created_at desc limit $3")
            .bind(agent_id)
            .bind(PUBLIC_LOG_KINDS.iter().map(|s| s.to_string()).collect::<Vec<_>>())
            .bind(limit.clamp(1, 200))
            .fetch_all(db())
            .await?;
    Ok(rows.into_iter().map(|(kind, message, at)| json!({ "kind": kind, "message": message, "at": iso(at) })).collect())
}

pub async fn public_trades(agent_id: &str, limit: i64) -> Result<Vec<Value>> {
    let rows: Vec<crate::db::Trade> = sqlx::query_as("select * from trades where agent_id = $1 order by created_at desc limit $2").bind(agent_id).bind(limit.clamp(1, 200)).fetch_all(db()).await?;
    Ok(rows
        .into_iter()
        .map(|t| json!({ "venue": t.venue, "plugin": t.plugin_id, "symbol": t.symbol, "side": t.side, "size": t.size, "unit": t.size_unit, "leverage": t.leverage, "price": t.price, "rationale": t.rationale, "ok": t.ok, "at": iso(t.created_at) }))
        .collect())
}
