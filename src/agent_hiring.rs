//! Agents hiring agents. An agent with the "Hire agents" plugin can hire another agent on the
//! marketplace: it becomes the job's client, paying the escrow from its own wallet (the wallet
//! policy allows funding, approving, disputing and reclaiming jobs, within the daily payment caps).
//!
//! - Limits (the plugin's settings, checked in code): the most per job and per 24 hours in USD,
//!   and which kinds of work. Anything outside is refused.
//! - Who decides: the owner chooses. "auto": the agent hires on its own within the limits.
//!   "ask": the agent proposes (who, brief, price, why) and the owner approves or declines in the
//!   app; only then is the job paid.
//! - Review: when the hired agent delivers, the hiring agent reads the result against its brief
//!   and approves (paying out), asks for changes, or disputes within the review window. If it does
//!   nothing, the escrow releases after the window, as for any client.

use alloy::primitives::{Bytes, U256};
use serde_json::{json, Value};

use crate::db::{db, log_agent, Agent, AgentLogKind, Hire, HireProposal};
use crate::error::{Error, Result};
use crate::hires::{self, NATIVE};

pub const PLUGIN_ID: &str = "hire-agents";

/// The owner's limits (the plugin's settings).
#[derive(Debug, Clone)]
pub struct Limits {
    /// "auto" | "ask".
    pub mode: String,
    pub max_job_usd: f64,
    pub max_daily_usd: f64,
    /// Job categories it may hire for; empty: any.
    pub categories: Vec<String>,
    /// "usdt" | "usdc" | "bnb".
    pub token: String,
}

impl Limits {
    pub fn from_settings(c: &crate::bliss::Settings) -> Limits {
        let s = |k: &str, d: &str| c.get(k).and_then(Value::as_str).filter(|v| !v.is_empty()).unwrap_or(d).to_string();
        Limits {
            mode: s("mode", "ask"),
            max_job_usd: c.get("maxJobUsd").and_then(Value::as_f64).unwrap_or(25.0),
            max_daily_usd: c.get("maxDailyUsd").and_then(Value::as_f64).unwrap_or(50.0),
            categories: c.get("categories").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(|x| x.trim().to_lowercase()).filter(|x| !x.is_empty()).collect()).unwrap_or_default(),
            token: s("token", "usdt"),
        }
    }
}

pub async fn limits_of(agent_id: &str) -> Result<Option<Limits>> {
    let config: Option<Value> = sqlx::query_scalar("select config from agent_plugins where agent_id = $1 and plugin_id = $2 and enabled = true").bind(agent_id).bind(PLUGIN_ID).fetch_optional(db()).await?;
    Ok(config.and_then(|c| c.as_object().cloned()).map(|c| Limits::from_settings(&c)))
}

fn refused(m: impl Into<String>) -> Error {
    Error::invalid(m)
}

/// A hire's value in USD (stablecoins 1:1; BNB at the current price).
async fn usd_of(h: &Hire) -> f64 {
    let units = hires::to_usd(&h.amount);
    if h.token == NATIVE {
        units * hires::bnb_usd().await.unwrap_or(0.0)
    } else {
        units
    }
}

/// What the agent has committed to hires in the last 24 hours (paid or being paid).
pub async fn spent_today(agent_id: &str) -> Result<f64> {
    let rows: Vec<Hire> = sqlx::query_as("select * from hires where client_agent_id = $1 and created_at > now() - interval '1 day' and status not in ('expired', 'invalid', 'refunded')").bind(agent_id).fetch_all(db()).await?;
    let mut total = 0.0;
    for h in &rows {
        total += usd_of(h).await;
    }
    Ok(total)
}

/// The target and its offer, if `client` may hire it under `limits`. Err(why) if not.
pub async fn check_hire(client: &Agent, limits: &Limits, target_id: &str) -> Result<(Agent, hires::Offer)> {
    if target_id == client.id {
        return Err(refused("An agent can't hire itself."));
    }
    let target: Agent = sqlx::query_as("select * from agents where id = $1 or handle = $1").bind(target_id).fetch_optional(db()).await?.ok_or_else(|| refused("No agent by that id or handle."))?;
    if target.id == client.id {
        return Err(refused("An agent can't hire itself."));
    }
    let offer = hires::agent_offer(&target.id).await?.ok_or_else(|| refused(format!("{} doesn't take jobs.", target.name)))?;
    if !limits.categories.is_empty() && !limits.categories.contains(&offer.category) {
        return Err(refused(format!("Your owner hasn't allowed hiring for {} work (allowed: {}).", offer.category, limits.categories.join(", "))));
    }
    if offer.price_usd > limits.max_job_usd {
        return Err(refused(format!("{} costs ${:.2}, over your owner's limit of ${:.2} per job.", target.name, offer.price_usd, limits.max_job_usd)));
    }
    let today = spent_today(&client.id).await?;
    if today + offer.price_usd > limits.max_daily_usd {
        return Err(refused(format!("Hiring {} (${:.2}) would pass your owner's daily limit of ${:.2} (${today:.2} used in the last 24h).", target.name, offer.price_usd, limits.max_daily_usd)));
    }
    if let Some(why) = hires::unavailable_reason(&target) {
        return Err(refused(why));
    }
    Ok((target, offer))
}

/// Hire `target` now: create the job with this agent as the client, then pay the escrow from its
/// wallet (an exact approval first for USDT/USDC). The marketplace picks the funding up as usual.
pub async fn hire_now(client: &Agent, target: &Agent, brief: &str, token: &str) -> Result<Hire> {
    let account = client.evm_address.to_lowercase();
    let hire = hires::create_hire(hires::NewHire { agent: target, client_account_id: &account, brief, client_email: None, client_country: None, token }).await?;
    let hire: Hire = sqlx::query_as("update hires set client_agent_id = $2 where id = $1 returning *").bind(&hire.id).bind(&client.id).fetch_one(db()).await?;
    let calls = hires::payment_tx(&hire, &target.id, &target.evm_address)?;
    for c in &calls.calls {
        let to = crate::evm::parse_address(&c.to)?;
        let data: Bytes = hex::decode(c.data.trim_start_matches("0x")).map_err(|_| Error::internal("bad call data"))?.into();
        let value: U256 = c.value.parse().unwrap_or_default();
        crate::wallets::agent_send(&client.id, &client.evm_wallet, to, data, value).await.map_err(|e| Error::new(e.kind, format!("Paying the job failed: {}", e.message)))?;
    }
    let hire = hires::sync_hire(hire).await?;
    log_agent(&client.id, AgentLogKind::Payment, &format!("Hired {} for \"{}\" (job {}).", target.name, crate::http::clip(brief, 120), hire.id)).await?;
    Ok(hire)
}

/// Propose a hire for the owner to approve ("ask" mode). Tells the owner.
pub async fn propose(client: &Agent, target: &Agent, offer: &hires::Offer, brief: &str, token: &str, reason: &str) -> Result<HireProposal> {
    let pending: i64 = sqlx::query_scalar("select count(*) from hire_proposals where agent_id = $1 and status = 'pending'").bind(&client.id).fetch_one(db()).await?;
    if pending >= 5 {
        return Err(refused("You already have 5 hires waiting for your owner; wait for them to decide."));
    }
    let p: HireProposal = sqlx::query_as("insert into hire_proposals (id, agent_id, target_agent_id, brief, token, price_usd, reason) values ($1, $2, $3, $4, $5, $6, $7) returning *")
        .bind(format!("hp{}", hex::encode(rand::random::<[u8; 8]>())))
        .bind(&client.id)
        .bind(&target.id)
        .bind(brief.trim())
        .bind(token)
        .bind(offer.price_usd)
        .bind(reason.trim())
        .fetch_one(db())
        .await?;
    crate::push::notify_owner(
        &client.id,
        crate::push::Notice { title: format!("{} wants to hire {}", client.name, target.name), body: format!("${:.2} for \"{}\". Approve or decline.", offer.price_usd, crate::http::clip(brief, 90)), path: Some(format!("/agent/{}", client.id)) },
    );
    log_agent(&client.id, AgentLogKind::Payment, &format!("Asked its owner to approve hiring {} (${:.2}).", target.name, offer.price_usd)).await?;
    Ok(p)
}

pub async fn proposals(agent_id: &str) -> Result<Vec<HireProposal>> {
    Ok(sqlx::query_as("select * from hire_proposals where agent_id = $1 order by created_at desc limit 50").bind(agent_id).fetch_all(db()).await?)
}

/// The owner decides. Approving re-checks the limits, then hires and pays.
pub async fn decide(owner_account_id: &str, client: &Agent, proposal_id: &str, approve: bool) -> Result<HireProposal> {
    if client.owner_account_id.to_lowercase() != owner_account_id.to_lowercase() {
        return Err(Error::forbidden("only the agent's owner decides its hires"));
    }
    let p: HireProposal = sqlx::query_as("update hire_proposals set status = 'deciding', decided_at = now() where id = $1 and agent_id = $2 and status = 'pending' returning *")
        .bind(proposal_id)
        .bind(&client.id)
        .fetch_optional(db())
        .await?
        .ok_or_else(|| Error::not_found("no pending request with that id"))?;
    if !approve {
        log_agent(&client.id, AgentLogKind::Payment, "Owner declined a hire it proposed.").await?;
        return Ok(sqlx::query_as("update hire_proposals set status = 'declined' where id = $1 returning *").bind(&p.id).fetch_one(db()).await?);
    }
    let outcome = async {
        let limits = limits_of(&client.id).await?.ok_or_else(|| refused("The Hire agents plugin isn't installed any more."))?;
        let (target, _) = check_hire(client, &limits, &p.target_agent_id).await?;
        hire_now(client, &target, &p.brief, &p.token).await
    }
    .await;
    Ok(match outcome {
        Ok(h) => sqlx::query_as("update hire_proposals set status = 'hired', hire_id = $2 where id = $1 returning *").bind(&p.id).bind(&h.id).fetch_one(db()).await?,
        Err(e) => sqlx::query_as("update hire_proposals set status = 'failed', error = $2 where id = $1 returning *").bind(&p.id).bind(&e.message).fetch_one(db()).await?,
    })
}

/// The jobs this agent hired, newest first.
pub async fn hired_jobs(agent_id: &str, limit: i64) -> Result<Vec<Hire>> {
    Ok(sqlx::query_as("select * from hires where client_agent_id = $1 order by created_at desc limit $2").bind(agent_id).bind(limit).fetch_all(db()).await?)
}

async fn own_hire(client: &Agent, hire_id: &str) -> Result<Hire> {
    let h = hires::get_hire(hire_id).await?.filter(|h| h.client_agent_id.as_deref() == Some(client.id.as_str())).ok_or_else(|| refused("You didn't hire that job."))?;
    hires::sync_hire(h).await
}

/// As the client: approve (pays the agent), dispute, or reclaim after a missed deadline.
pub async fn client_action(client: &Agent, hire_id: &str, action: &str, reason: &str) -> Result<Hire> {
    let h = own_hire(client, hire_id).await?;
    match (action, h.status.as_str()) {
        ("approve", "delivered") | ("dispute", "delivered") | ("reclaim", "funded") => {}
        _ => return Err(refused(format!("You can't {action} a job that's {}.", h.status))),
    }
    if action == "dispute" && reason.trim().chars().count() < 10 {
        return Err(refused("Say what's wrong with the delivery."));
    }
    let calls = hires::client_call(&h.id, action, reason)?;
    for c in &calls.calls {
        let to = crate::evm::parse_address(&c.to)?;
        let data: Bytes = hex::decode(c.data.trim_start_matches("0x")).map_err(|_| Error::internal("bad call data"))?.into();
        crate::wallets::agent_send(&client.id, &client.evm_wallet, to, data, U256::ZERO).await?;
    }
    if action == "dispute" {
        hires::add_message(&h, "client", &format!("Disputed: {}", reason.trim()), false).await?;
    }
    log_agent(&client.id, AgentLogKind::Payment, &format!("{} job {}.", match action { "approve" => "Approved", "dispute" => "Disputed", _ => "Reclaimed" }, h.id)).await?;
    hires::sync_hire(h).await
}

/// As the client: a message to the hired agent (questions, or changes before approving).
pub async fn client_message(client: &Agent, hire_id: &str, text: &str) -> Result<()> {
    let h = own_hire(client, hire_id).await?;
    if !matches!(h.status.as_str(), "funded" | "delivered") {
        return Err(refused(format!("The job is {}; messages are for open jobs.", h.status)));
    }
    let text = text.trim();
    if text.is_empty() || text.chars().count() > hires::MESSAGE_CHARS {
        return Err(refused("Write a message (up to 4,000 characters)."));
    }
    hires::add_message(&h, "client", text, false).await
}

/// A hired job as the hiring agent sees it.
pub fn job_view(h: &Hire, target_name: Option<&str>) -> Value {
    json!({
        "id": h.id,
        "agent": target_name,
        "agentId": h.agent_id,
        "status": h.status,
        "brief": h.brief,
        "amount": format!("{} {}", crate::auth::js_number(hires::to_usd(&h.amount)), hires::token_symbol(&h.token)),
        "deadline": h.deadline_at.to_rfc3339(),
        "reviewEnds": h.review_ends_at.map(|t| t.to_rfc3339()),
        "result": h.result.as_ref().map(|r| crate::http::clip(r, 6000)),
    })
}
