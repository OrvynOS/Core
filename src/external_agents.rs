//! Self-hosted agents on Orvyn's network. An agent running on its owner's server connects itself:
//! the owner approves it with Orvyn OAuth (device login, scope `agents`), and the agent proves it
//! controls its wallet by signing a registration message. Orvyn then gives it a public page, an
//! OrvynID and its activity feed, but never holds or uses its key (custody "external") and never
//! hosts or bills it (hosting status "external"). The agent gets a key (shown once) to post its
//! activity; the owner can revoke it.

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::db::{db, log_agent, Agent, AgentLogKind};
use crate::error::{Error, Result};
use crate::evm;

const MAX_AGE_MS: i64 = 10 * 60_000;

fn sha256(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// The exact message the agent's wallet signs to register.
pub fn registration_message(wallet: &str, owner: &str, name: &str, issued_at: &str) -> Result<String> {
    Ok([
        "Register a self-hosted agent on Orvyn".to_string(),
        format!("Wallet: {}", evm::parse_address(wallet)?),
        format!("Owner: {}", owner.to_lowercase()),
        format!("Name: {name}"),
        format!("Issued At: {issued_at}"),
    ]
    .join("\n"))
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterInput {
    pub wallet: String,
    pub name: String,
    pub symbol: Option<String>,
    pub description: Option<String>,
    pub handle: Option<String>,
    pub issued_at: String,
    pub signature: String,
}

/// Register (or reconnect) the owner's self-hosted agent. Re-registering the same wallet for the
/// same owner updates its details and issues a new key (the old one stops working).
pub async fn register_external_agent(owner_account_id: &str, input: &RegisterInput) -> Result<Value> {
    let owner = owner_account_id.to_lowercase();
    if !evm::is_address(&input.wallet) {
        return Err(Error::invalid("wallet must be a 0x address"));
    }
    let name: String = input.name.trim().chars().take(32).collect();
    if name.is_empty() {
        return Err(Error::invalid("name is required"));
    }
    let issued = chrono::DateTime::parse_from_rfc3339(&input.issued_at).map(|t| t.timestamp_millis()).unwrap_or(i64::MIN);
    let age = chrono::Utc::now().timestamp_millis().saturating_sub(issued);
    if !(-60_000..=MAX_AGE_MS).contains(&age) {
        return Err(Error::invalid("the registration signature expired; sign again"));
    }
    let message = registration_message(&input.wallet, &owner, &name, &input.issued_at)?;
    let sig = hex::decode(input.signature.trim_start_matches("0x")).unwrap_or_default();
    let signer = crate::evm::sig::recover(crate::evm::sig::message_hash(&message), &sig);
    if signer.is_none_or(|s| format!("{s:#x}") != input.wallet.to_lowercase()) {
        return Err(Error::forbidden("the registration must be signed by the agent's wallet"));
    }
    let wallet = input.wallet.to_lowercase();
    let existing: Option<Agent> = sqlx::query_as("select * from agents where evm_address = $1").bind(&wallet).fetch_optional(db()).await?;
    if let Some(e) = &existing {
        if e.owner_account_id.to_lowercase() != owner {
            return Err(Error::conflict("this wallet is already an agent of someone else"));
        }
        if e.hosting_status != "external" {
            return Err(Error::conflict("this wallet is an agent Orvyn hosts"));
        }
    }
    let from_name: String = name.chars().filter(|c| c.is_ascii_alphanumeric()).take(6).collect();
    let symbol = input.symbol.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(String::from).unwrap_or(if from_name.is_empty() { "AGENT".into() } else { from_name });
    let symbol: String = symbol.to_uppercase().chars().take(10).collect();
    let description: String = input.description.clone().unwrap_or_default().chars().take(500).collect();
    let agent: Agent = match existing {
        Some(e) => sqlx::query_as("update agents set name = $2, symbol = $3, description = $4 where id = $1 returning *").bind(&e.id).bind(&name).bind(&symbol).bind(&description).fetch_one(db()).await?,
        None => {
            let handle = crate::lifecycle::pick_handle(input.handle.as_deref(), &symbol, &[owner.clone(), wallet.clone()]).await.map_err(|e| Error::conflict(e.message))?;
            let custody = json!({ "custody": "external", "accountAddress": evm::parse_address(&wallet)?.to_string() });
            let a: Agent = sqlx::query_as(
                "insert into agents (id, handle, owner_account_id, name, symbol, description, ai_provider, ai_model, ai_key_encrypted, ai_key_hint, evm_address, evm_wallet, launchpad, status, hosting_status, subscription_active, autopilot_enabled)
                 values ($1, $2, $3, $4, $5, $6, 'self-hosted', 'self-hosted', '', '', $7, $8, 'none', 'live', 'external', true, false) returning *",
            )
            .bind(crate::lifecycle::uuid_v4())
            .bind(handle)
            .bind(&owner)
            .bind(&name)
            .bind(&symbol)
            .bind(&description)
            .bind(&wallet)
            .bind(custody)
            .fetch_one(db())
            .await?;
            log_agent(&a.id, AgentLogKind::Identity, "Connected to Orvyn as a self-hosted agent").await?;
            a
        }
    };
    // One active key per agent.
    sqlx::query("update external_agent_keys set revoked_at = now() where agent_id = $1 and revoked_at is null").bind(&agent.id).execute(db()).await?;
    let key = format!("orva_{}", B64URL.encode(rand::random::<[u8; 32]>()));
    sqlx::query("insert into external_agent_keys (key_hash, agent_id) values ($1, $2)").bind(sha256(&key)).bind(&agent.id).execute(db()).await?;
    Ok(json!({ "agentId": agent.id, "handle": agent.handle, "key": key }))
}

/// The self-hosted agent a key belongs to.
pub async fn agent_for_key(key: &str) -> Result<Agent> {
    if !key.starts_with("orva_") {
        return Err(Error::unauthorized("invalid agent key"));
    }
    let row: Option<(String, Option<chrono::DateTime<chrono::Utc>>)> = sqlx::query_as("select agent_id, revoked_at from external_agent_keys where key_hash = $1").bind(sha256(key)).fetch_optional(db()).await?;
    let Some((agent_id, None)) = row else { return Err(Error::unauthorized("invalid or revoked agent key")) };
    let agent: Option<Agent> = sqlx::query_as("select * from agents where id = $1").bind(&agent_id).fetch_optional(db()).await?;
    agent.filter(|a| a.hosting_status == "external").ok_or_else(|| Error::not_found("agent not found"))
}

const ACTIVITY_KINDS: &[(&str, AgentLogKind)] = &[("trade", AgentLogKind::Trade), ("payment", AgentLogKind::Payment), ("think", AgentLogKind::Think), ("plugin", AgentLogKind::Plugin), ("launch", AgentLogKind::Launch)];

/// Post a self-hosted agent's activity to its public feed (up to 20 entries per call).
pub async fn post_activity(key: &str, entries: &[Value]) -> Result<Value> {
    let agent = agent_for_key(key).await?;
    let mut posted = 0;
    for e in entries.iter().take(20) {
        let (Some(kind), Some(message)) = (e["kind"].as_str(), e["message"].as_str()) else { continue };
        let Some((_, k)) = ACTIVITY_KINDS.iter().find(|(name, _)| *name == kind) else { continue };
        let m = message.trim();
        if m.is_empty() {
            continue;
        }
        log_agent(&agent.id, *k, &m.chars().take(500).collect::<String>()).await?;
        posted += 1;
    }
    Ok(json!({ "posted": posted }))
}

/// A self-hosted agent reports the hardware it runs on (shown as self-reported).
pub async fn report_environment(key: &str, hardware: &Value) -> Result<Value> {
    let agent = agent_for_key(key).await?;
    let h: crate::environment::Hardware = serde_json::from_value(hardware.clone()).map_err(|_| Error::invalid("hardware report is malformed"))?;
    let changed = crate::environment::record_hardware(&agent.id, &h).await.map_err(|e| if e.kind == crate::error::Kind::Invalid { Error::invalid("hardware report is malformed") } else { e })?;
    Ok(json!({ "recorded": true, "changed": changed }))
}

/// The owner disconnects a self-hosted agent: its key stops working; its page stays.
pub async fn revoke_external_agent_keys(owner_account_id: &str, agent_id: &str) -> Result<()> {
    let agent: Option<Agent> = sqlx::query_as("select * from agents where id = $1").bind(agent_id).fetch_optional(db()).await?;
    if agent.is_none_or(|a| a.owner_account_id.to_lowercase() != owner_account_id.to_lowercase() || a.hosting_status != "external") {
        return Err(Error::not_found("no such self-hosted agent of yours"));
    }
    sqlx::query("update external_agent_keys set revoked_at = now() where agent_id = $1 and revoked_at is null").bind(agent_id).execute(db()).await?;
    Ok(())
}
