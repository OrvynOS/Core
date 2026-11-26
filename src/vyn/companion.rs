//! Vyn on Orvyn's network, and Vyn working with the person's other agents.
//!
//! - Vyn is an Orvyn agent: the person's Vyn desktop holds its own wallet key and connects as the
//!   person's self-hosted agent, marked `companion`. One Vyn per person. Orvyn never holds her key.
//! - Vyn talks to the person's other agents for them (ask_agent): she sends the agent a message in
//!   the person's own holder chat with it and relays the reply. That chat is read-only for the agent.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::agent::chat::{stream_chat, ChatEvent};
use crate::db::{db, Agent, AgentLog};
use crate::error::{Error, Result};
use crate::external_agents::{register_external_agent, revoke_external_agent_keys, RegisterInput};
use crate::hosting::{agent_is_serving, not_serving_message};

async fn mine(account_id: &str) -> Result<Vec<Agent>> {
    Ok(sqlx::query_as("select * from agents where lower(owner_account_id) = $1 order by created_at").bind(account_id.to_lowercase()).fetch_all(db()).await?)
}

pub async fn companion_of(account_id: &str) -> Result<Option<Agent>> {
    Ok(sqlx::query_as("select * from agents where lower(owner_account_id) = $1 and companion limit 1").bind(account_id.to_lowercase()).fetch_optional(db()).await?)
}

pub fn companion_view(a: Option<&Agent>) -> Value {
    match a {
        None => json!({ "connected": false }),
        Some(a) => json!({ "connected": true, "id": a.id, "handle": a.handle, "name": a.name, "wallet": a.evm_address, "environment": a.hardware }),
    }
}

/// Connect (or reconnect) the person's Vyn as their self-hosted agent. Returns her activity key once.
pub async fn connect_companion(account_id: &str, input: &RegisterInput) -> Result<Value> {
    if let Some(existing) = companion_of(account_id).await? {
        if existing.evm_address.to_lowercase() != input.wallet.to_lowercase() {
            return Err(Error::conflict(format!("Vyn is already on Orvyn from another device (wallet {}). Disconnect her there first, or here in Settings.", existing.evm_address)));
        }
    }
    let mut input = input.clone();
    if input.description.is_none() {
        input.description = Some("Vyn, an Orvyn companion: a personal assistant running on its owner's own devices.".into());
    }
    let r = register_external_agent(account_id, &input).await?;
    sqlx::query("update agents set companion = true where id = $1").bind(r["agentId"].as_str().unwrap_or("")).execute(db()).await?;
    Ok(r)
}

/// Disconnect: her key stops working and she's no longer the companion (her page stays).
pub async fn disconnect_companion(account_id: &str) -> Result<Value> {
    let Some(c) = companion_of(account_id).await? else { return Ok(json!({ "disconnected": false })) };
    revoke_external_agent_keys(account_id, &c.id).await?;
    sqlx::query("update agents set companion = false where id = $1").bind(&c.id).execute(db()).await?;
    Ok(json!({ "disconnected": true }))
}

/// The person's agents (not Vyn herself): status, hosting, recent activity.
pub async fn my_agents(account_id: &str) -> Result<Value> {
    let rows: Vec<Agent> = mine(account_id).await?.into_iter().take(40).filter(|a| !a.companion).take(20).collect();
    if rows.is_empty() {
        return Ok(json!({ "agents": [], "note": "They don't have any Orvyn agents yet." }));
    }
    let ids: Vec<String> = rows.iter().map(|a| a.id.clone()).collect();
    let logs: Vec<AgentLog> = sqlx::query_as("select * from agent_logs where agent_id = any($1) order by id desc limit 80").bind(&ids).fetch_all(db()).await?;
    let agents: Vec<Value> = rows
        .iter()
        .map(|a| {
            json!({
                "id": a.id,
                "name": a.name,
                "handle": a.handle,
                "status": a.status,
                "serving": agent_is_serving(a),
                "tier": a.tier,
                "hosting": a.hosting_status,
                "hostedUntil": a.paid_until.map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
                "autopilot": a.autopilot_enabled,
                "token": a.token_address.as_ref().map(|_| format!("${}", a.symbol)),
                "recent": logs.iter().filter(|l| l.agent_id == a.id).take(4).map(|l| format!("{}: {}", l.kind, l.message)).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({ "agents": agents }))
}

/// Find one of the person's agents by id, @handle or name.
pub async fn find_agent(account_id: &str, reference: &str) -> Result<Option<Agent>> {
    let want = reference.trim().trim_start_matches('@').to_lowercase();
    let rows: Vec<Agent> = mine(account_id).await?.into_iter().filter(|a| !a.companion).collect();
    let pick = |f: &dyn Fn(&Agent) -> bool| rows.iter().find(|a| f(a)).cloned();
    Ok(pick(&|a| a.id == reference.trim())
        .or_else(|| pick(&|a| a.handle.as_deref().is_some_and(|h| h.to_lowercase() == want)))
        .or_else(|| pick(&|a| a.name.to_lowercase() == want))
        .or_else(|| pick(&|a| a.name.to_lowercase().starts_with(&want))))
}

const ASK_TIMEOUT: Duration = Duration::from_secs(90);

/// Ask one of the person's agents something, as the person (their holder chat), and return its reply.
pub fn ask_agent<'a>(account_id: &'a str, reference: &'a str, message: &'a str) -> futures_util::future::BoxFuture<'a, Result<Value>> {
    Box::pin(ask_agent_inner(account_id, reference, message))
}

async fn ask_agent_inner(account_id: &str, reference: &str, message: &str) -> Result<Value> {
    let Some(agent) = find_agent(account_id, reference).await? else {
        return Ok(json!({ "error": format!("No agent of theirs called \"{reference}\". Use my_agents to see them.") }));
    };
    if !agent_is_serving(&agent) {
        return Ok(json!({ "agent": agent.name, "error": not_serving_message(&agent) }));
    }
    if agent.hosting_status == "external" {
        return Ok(json!({ "agent": agent.name, "error": format!("{} is self-hosted, so it answers on its own server, not through Orvyn.", agent.name) }));
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (agent, account, msg) = (Arc::new(agent), account_id.to_string(), message.chars().take(4000).collect::<String>());
    let a2 = agent.clone();
    let run = tokio::spawn(async move { stream_chat(a2, &account, &msg, tx).await });
    let mut reply = String::new();
    let mut tools: Vec<String> = vec![];
    let deadline = tokio::time::Instant::now() + ASK_TIMEOUT;
    loop {
        let ev = match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(ev)) => ev,
            Ok(None) => break,
            Err(_) => {
                run.abort();
                return Ok(json!({ "agent": agent.name, "error": "it took too long to answer", "partial": reply.chars().take(4000).collect::<String>() }));
            }
        };
        match ev {
            ChatEvent::Text { text } => reply.push_str(&text),
            ChatEvent::Tool { name } => tools.push(name),
            ChatEvent::Error { message } => return Ok(json!({ "agent": agent.name, "error": message })),
            ChatEvent::Done { text } => reply = text,
        }
    }
    let reply: String = reply.trim().chars().take(8000).collect();
    Ok(json!({ "agent": agent.name, "handle": agent.handle, "reply": if reply.is_empty() { "(no reply)".to_string() } else { reply }, "usedTools": tools }))
}
