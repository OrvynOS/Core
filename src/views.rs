//! The agent views the API sends to browsers: `public_agent` for anyone, `owner_agent` with the
//! creator's extra fields.

use serde_json::{json, Value};

use crate::db::Agent;

fn ms(t: &Option<chrono::DateTime<chrono::Utc>>) -> Value {
    t.map(|t| json!(t.timestamp_millis())).unwrap_or(Value::Null)
}

/// Fields safe to send to any visitor (no wallet metadata).
pub fn public_agent(agent: &Agent) -> Value {
    json!({
        "id": agent.id,
        "handle": agent.handle,
        // <handle>@orvyn.cash: public, so anyone can write to the agent.
        "email": crate::mail::agent_address(agent.handle.as_deref()),
        "name": agent.name,
        "symbol": agent.symbol,
        "description": agent.description,
        "icon": agent.icon,
        "links": agent.links,
        "ownerAccountId": agent.owner_account_id,
        "persona": agent.persona,
        "strategy": agent.strategy,
        "autopilotEnabled": agent.autopilot_enabled,
        "autopilotIntervalMin": agent.autopilot_interval_min,
        "minHoldingTokens": agent.min_holding_tokens,
        "chatMessagesPerHour": agent.chat_messages_per_hour,
        "aiProvider": agent.ai_provider,
        "aiModel": agent.ai_model,
        "gasReserveBnb": agent.gas_reserve_bnb,
        "paidUntil": ms(&agent.paid_until),
        "subscriptionActive": agent.subscription_active,
        "hostingStatus": agent.hosting_status,
        "tier": agent.tier,
        // Where and on what it runs: hosting, tier, hardware, Studio quota.
        "environment": crate::environment::public_environment(agent),
        "evmAddress": agent.evm_address,
        "status": agent.status,
        "statusDetail": agent.status_detail,
        "launchpad": agent.launchpad,
        "tokenAddress": agent.token_address,
        "graduated": agent.graduated,
        "tax": agent.tax,
        // Binance Agent ID (premium tier): the agent's verified name.ai.
        "agentIdName": agent.agent_id_name,
        "premium": crate::agentid::is_premium(agent),
        "erc8004AgentId": agent.erc8004_agent_id,
        "launchTx": agent.launch_tx,
        "createdAt": agent.created_at.timestamp_millis(),
    })
}

/// Adds owner-only fields: the key hint (the key itself never leaves the server), billing mode
/// and the service rate limit. Hosting internals are never sent.
pub fn owner_agent(agent: &Agent) -> Value {
    let mut v = public_agent(agent);
    let o = v.as_object_mut().unwrap();
    o.insert("aiKeyHint".into(), json!(agent.ai_key_hint));
    o.insert("billingMode".into(), json!(agent.billing_mode));
    o.insert("rateLimitPerMinute".into(), json!(agent.rate_limit_per_minute));
    o.insert("billingDetails".into(), json!(agent.billing_details));
    o.insert("registered".into(), json!(agent.registered));
    v
}
