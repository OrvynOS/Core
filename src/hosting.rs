//! Agent hosting. In `render` mode every live agent gets its own Render web service (orvyn-agent)
//! that runs its jobs and serves its holder chat. The orchestrator suspends that service when the
//! subscription lapses and resumes it when payment lands. In `shared` mode (local dev) agents run in
//! the shared runtime and "suspended" just pauses their jobs and chat.

use serde_json::{json, Value};

use crate::db::{db, log_agent, Agent, AgentLogKind};
use crate::env::{env, HostingMode};
use crate::environment::plan_for;
use crate::error::{Error, Result};
use crate::tiers::Tier;

pub fn is_render_hosting() -> bool {
    env().hosting_mode == HostingMode::Render
}

async fn render(method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value> {
    let e = env();
    let Some(key) = e.render_api_key.as_deref() else {
        eprintln!("[hosting] RENDER_API_KEY is not set");
        return Err(Error::unavailable("agent hosting isn't configured"));
    };
    let mut req = crate::http::client()
        .request(method.clone(), format!("{}{path}", e.render_api_url))
        .bearer_auth(key)
        .header("accept", "application/json")
        .timeout(std::time::Duration::from_secs(30));
    if let Some(b) = body {
        req = req.json(&b);
    }
    let res = req.send().await.map_err(|e| Error::internal(format!("Render: {e}")))?;
    let status = res.status();
    let text = res.text().await.unwrap_or_default();
    // Details stay in the server logs; what's returned can reach the agent's activity log.
    if !status.is_success() {
        eprintln!("[hosting] {method} {path}: {status} {}", crate::http::clip(&text, 300));
        return Err(Error::internal(format!("the agent's web service couldn't be updated (HTTP {}); Orvyn will retry", status.as_u16())));
    }
    Ok(if text.is_empty() { json!({}) } else { serde_json::from_str(&text).unwrap_or(json!({})) })
}

/// What each agent service is given from the orchestrator's environment.
const AGENT_ENV_KEYS: &[&str] = &[
    "DATABASE_URL", "PRIVY_APP_ID", "PRIVY_APP_SECRET", "PRIVY_AUTHORIZATION_KEY", "PRIVY_AUTHORIZATION_KEY_ID", "PRIVY_VERIFICATION_KEY", "PRIVY_AGENT_POLICY_ID",
    "PRIVY_ASTER_POLICY_ID", "ASTER_API_URL", "OPINION_API_KEY", "OPINION_API_URL", "ORVYN_SHIELDED_WASM", "ORVYN_SHIELDED_ZKEY", "ORVYN_SHIELDED_POOL_FROM_BLOCK",
    "AI_KEY_ENCRYPTION_SECRET", "SESSION_SECRET", "ORVYN_INTERNAL_API_KEY", "ORVYN_API_URL", "ORVYN_REGISTRY_CONTRACT", "ORVYN_PAY_CONTRACT", "ORVYN_SHIELDED_POOL",
    "ORVYN_AGENT_ID_REGISTRY", "ORVYN_MAIL_DOMAIN", "ORVYN_MAIL_DAILY_SEND_CAP", "BSC_RPC_URL", "FOURMEME_API_URL", "FOURMEME_TOKEN_MANAGER", "FOURMEME_HELPER",
    "FOURMEME_LAUNCH_FEE_BNB", "FLAP_PORTAL", "TAX_ALLOCATE_MIN_BNB", "X402_AGENT_MAX_PER_REQUEST_USD", "X402_AGENT_APPROVE_UP_TO_USD", "X402_PERMIT2_PROXY",
    "FEE_INTERVAL_MIN", "NODE_ENV",
];

fn agent_env_vars(agent: &Agent) -> Vec<Value> {
    let mut vars = vec![json!({ "key": "AGENT_ID", "value": agent.id }), json!({ "key": "HOSTING_MODE", "value": "render" })];
    let mut env: Vec<(String, String)> = std::env::vars().filter(|(_, v)| !v.is_empty()).collect();
    env.sort();
    for (key, value) in env {
        if AGENT_ENV_KEYS.contains(&key.as_str()) || key.starts_with("EVM_RPC_") || key.starts_with("WALLET_") {
            vars.push(json!({ "key": key, "value": value }));
        }
    }
    vars
}

fn service_name(agent: &Agent) -> String {
    let clean = |s: &str| s.to_lowercase().chars().filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit()).collect::<String>();
    format!("orvyn-agent-{}-{}", clean(&agent.symbol), clean(&agent.id).chars().take(8).collect::<String>())
}

async fn set_agent(agent_id: &str, sql: &str, binds: &[Option<String>]) -> Result<Agent> {
    let mut q = sqlx::query_as::<_, Agent>(sql).bind(agent_id);
    for b in binds {
        q = q.bind(b.clone());
    }
    Ok(q.fetch_one(db()).await?)
}

/// Move the agent's web service to its tier's plan (after a tier change). Render restarts it.
pub async fn apply_plan(agent: Agent) -> Result<Agent> {
    let Some(service) = agent.render_service_id.clone().filter(|_| is_render_hosting()) else { return Ok(agent) };
    let plan = plan_for(Tier::parse(&agent.tier));
    render(reqwest::Method::PATCH, &format!("/services/{service}"), Some(json!({ "serviceDetails": { "plan": plan } }))).await?;
    log_agent(&agent.id, AgentLogKind::Launch, &format!("Web service moved to the {plan} plan for {} hosting", agent.tier)).await?;
    Ok(agent)
}

/// Create the agent's Render web service (idempotent). In shared mode: mark it running.
pub async fn provision_host(agent: Agent) -> Result<Agent> {
    if !is_render_hosting() {
        if agent.hosting_status == "none" {
            return set_agent(&agent.id, "update agents set hosting_status = 'running' where id = $1 returning *", &[]).await;
        }
        return Ok(agent);
    }
    if agent.render_service_id.is_some() {
        return Ok(agent);
    }
    let e = env();
    let (Some(owner), Some(repo)) = (&e.render_owner_id, &e.render_repo_url) else {
        eprintln!("[hosting] RENDER_OWNER_ID and RENDER_REPO_URL are required");
        return Err(Error::unavailable("agent hosting isn't configured"));
    };
    sqlx::query("update agents set hosting_status = 'provisioning' where id = $1").bind(&agent.id).execute(db()).await?;
    let created = async {
        let body = json!({
            "type": "web_service",
            "name": service_name(&agent),
            "ownerId": owner,
            "repo": repo,
            "branch": e.render_branch,
            "autoDeployTrigger": "commit",
            "envVars": agent_env_vars(&agent),
            "serviceDetails": {
                // The Rust agent service (crates/agent), from the repo's Dockerfile; the image's
                // default command is self-host, so the hosted service runs plain `orvyn-agent`.
                "runtime": "docker",
                "plan": plan_for(Tier::parse(&agent.tier)),
                "region": e.render_region,
                "numInstances": 1,
                "healthCheckPath": "/health",
                "envSpecificDetails": { "dockerfilePath": "./Dockerfile", "dockerContext": ".", "dockerCommand": "orvyn-agent" },
            },
        });
        let created = render(reqwest::Method::POST, "/services", Some(body)).await?;
        let service = &created["service"];
        let id = service["id"].as_str().ok_or_else(|| Error::internal("Render returned no service"))?.to_string();
        if let Some(group) = &e.render_agent_env_group_id {
            render(reqwest::Method::POST, &format!("/env-groups/{group}/services/{id}"), None).await?;
        }
        let url = service["serviceDetails"]["url"].as_str().map(String::from);
        // Render web services listen on 10000 and are reachable by slug on the private network.
        let internal = format!("{}:10000", service["slug"].as_str().unwrap_or_default());
        let updated = set_agent(
            &agent.id,
            "update agents set render_service_id = $2, host_url = $3, host_internal = $4, hosting_status = 'running' where id = $1 returning *",
            &[Some(id), url, Some(internal)],
        )
        .await?;
        log_agent(&agent.id, AgentLogKind::Launch, "Agent web service started").await?;
        Ok::<_, Error>(updated)
    }
    .await;
    if created.is_err() {
        sqlx::query("update agents set hosting_status = 'failed' where id = $1").bind(&agent.id).execute(db()).await?;
    }
    created
}

pub async fn suspend_host(agent: Agent, reason: &str) -> Result<Agent> {
    if agent.hosting_status == "suspended" {
        return Ok(agent);
    }
    if let Some(service) = agent.render_service_id.as_ref().filter(|_| is_render_hosting()) {
        render(reqwest::Method::POST, &format!("/services/{service}/suspend"), None).await?;
    }
    let updated = set_agent(&agent.id, "update agents set hosting_status = 'suspended' where id = $1 returning *", &[]).await?;
    log_agent(&agent.id, AgentLogKind::Error, &format!("Agent suspended: {reason}")).await?;
    Ok(updated)
}

pub async fn resume_host(agent: Agent) -> Result<Agent> {
    if agent.hosting_status != "suspended" || agent.held_reason.is_some() {
        return Ok(agent);
    }
    if let Some(service) = agent.render_service_id.as_ref().filter(|_| is_render_hosting()) {
        render(reqwest::Method::POST, &format!("/services/{service}/resume"), None).await?;
    }
    let updated = set_agent(&agent.id, "update agents set hosting_status = 'running' where id = $1 returning *", &[]).await?;
    log_agent(&agent.id, AgentLogKind::Launch, "Agent resumed: subscription is active").await?;
    Ok(updated)
}

/// Remove the agent's Render service entirely (e.g. when an agent is deleted).
pub async fn destroy_host(agent: &Agent) -> Result<()> {
    if let Some(service) = agent.render_service_id.as_ref().filter(|_| is_render_hosting()) {
        render(reqwest::Method::DELETE, &format!("/services/{service}"), None).await?;
    }
    sqlx::query("update agents set render_service_id = null, host_url = null, host_internal = null, hosting_status = 'none' where id = $1").bind(&agent.id).execute(db()).await?;
    Ok(())
}

/// Whether the agent may act (jobs, autopilot, chat) right now.
pub fn agent_is_serving(agent: &Agent) -> bool {
    agent.subscription_active && agent.hosting_status != "suspended" && agent.held_reason.is_none()
}

/// Why an agent isn't serving, for holders who try to chat.
pub fn not_serving_message(agent: &Agent) -> &'static str {
    if agent.held_reason.is_some() { "This agent is paused by Orvyn." } else { "This agent is suspended: its hosting subscription has expired." }
}
