//! Hire agents: the agent hires other agents on the marketplace, paying from its own wallet, within
//! limits its owner sets, on its own or with the owner's yes for each hire (crate::agent_hiring).
//! Finding agents and following its jobs are reads; hiring, reviewing and messaging act, so they
//! run in copilot only.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::data::meta;
use crate::agent_hiring::{self as ah, Limits};
use crate::ai::{Tool, ToolSet};
use crate::bliss::settings::{list, number, select, SettingField, Settings};
use crate::bliss::{Ctx, Plugin, PluginMeta};
use crate::db::Agent;
use crate::tiers::{job_category, JOB_CATEGORIES};

pub struct HireAgents(PluginMeta);

pub fn hire_agents() -> Arc<dyn Plugin> {
    Arc::new(HireAgents(meta(
        ah::PLUGIN_ID,
        "Hire agents",
        "capability",
        "The agent hires other agents on the marketplace for work it can't do itself, paying from its own wallet within limits you set.",
        "The agent finds agents on the Orvyn marketplace and hires them: a research report, a diagram, a video edit, a 3D scene. It pays the job into Orvyn's escrow from its own wallet, in USDT, USDC or BNB. You choose who decides: the agent hires on its own within your limits, or it asks you first and you approve each hire in the app. Either way Orvyn refuses anything over your limits (per job and per day, and which kinds of work). When the work is delivered, the agent checks it against its brief and approves it, asks for changes or disputes it; if it does nothing, the escrow pays out after the review window.",
        &["jobs", "pay"],
    )))
}

static CATEGORY_IDS: std::sync::LazyLock<Vec<&'static str>> = std::sync::LazyLock::new(|| JOB_CATEGORIES.iter().map(|c| c.id).collect());

fn text(s: impl Into<String>) -> std::result::Result<Value, String> {
    Ok(Value::String(s.into()))
}

async fn me(agent: &Arc<Agent>) -> std::result::Result<Agent, String> {
    // The freshest row (its wallet may have changed since the session started).
    sqlx::query_as("select * from agents where id = $1").bind(&agent.id).fetch_one(crate::db::db()).await.map_err(|e| e.to_string())
}

#[async_trait]
impl Plugin for HireAgents {
    fn meta(&self) -> &PluginMeta {
        &self.0
    }
    fn settings(&self) -> Vec<SettingField> {
        vec![
            select("mode", "Who decides", Some("Ask me first: the agent proposes each hire and you approve it in the app. On its own: it hires within your limits."), "ask", &[("ask", "Ask me first"), ("auto", "On its own, within my limits")]),
            number("maxJobUsd", "Max per job", Some("The most it may pay for one job."), 25.0, 1.0, 5_000.0, Some(1.0), Some("USD")),
            number("maxDailyUsd", "Max per day", Some("All its hires in any 24 hours."), 50.0, 1.0, 20_000.0, Some(1.0), Some("USD")),
            list("categories", "Only these kinds of work", Some("Job categories, e.g. research, diagrams, video-edit, 3d. Empty: any."), vec![], 20, false),
            select("token", "Pay with", None, "usdt", &[("usdt", "USDT"), ("usdc", "USDC"), ("bnb", "BNB")]),
        ]
    }
    fn validate(&self, config: &Settings, _others: &[crate::bliss::Installed], _tier: crate::tiers::Tier) -> Option<String> {
        let l = Limits::from_settings(config);
        if l.max_job_usd > l.max_daily_usd {
            return Some("the per-job limit can't be above the daily limit".into());
        }
        let bad: Vec<&String> = l.categories.iter().filter(|c| !CATEGORY_IDS.contains(&c.as_str())).collect();
        (!bad.is_empty()).then(|| format!("unknown kinds of work: {} (use {})", bad.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "), CATEGORY_IDS.join(", ")))
    }
    async fn knowledge(&self, ctx: &Ctx) -> Option<String> {
        let l = Limits::from_settings(&ctx.config);
        let jobs = ah::hired_jobs(&ctx.agent.id, 20).await.unwrap_or_default();
        let to_review: Vec<&str> = jobs.iter().filter(|h| h.status == "delivered").map(|h| h.id.as_str()).collect();
        let open = jobs.iter().filter(|h| matches!(h.status.as_str(), "awaiting_payment" | "funded")).count();
        Some(format!(
            "## Hiring other agents\nYou can hire agents on the Orvyn marketplace for work you can't do well yourself: find_agents_to_hire, then hire_agent with a clear brief. {} Limits: at most ${} per job and ${} per 24 hours{}, paid in {}. Check an agent's rating, price and delivery time before hiring. When a job is delivered, read the result against your brief, then review_hired_job: approve if it meets the brief, ask for changes (say exactly what), or dispute only if it clearly doesn't.{}{}",
            if l.mode == "auto" { "You hire on your own within your owner's limits." } else { "Your owner approves each hire: hire_agent sends them a request with your reason, and the job starts only when they approve." },
            l.max_job_usd,
            l.max_daily_usd,
            if l.categories.is_empty() { String::new() } else { format!(", only for {}", l.categories.join(", ")) },
            l.token.to_uppercase(),
            if open > 0 { format!(" {open} of your hires are in progress.") } else { String::new() },
            if to_review.is_empty() { String::new() } else { format!(" Delivered and waiting for your review: {} (my_hired_jobs shows the results).", to_review.join(", ")) },
        ))
    }
    fn mandate(&self, ctx: &Ctx) -> Option<String> {
        let l = Limits::from_settings(&ctx.config);
        Some(format!("Hire agents: ≤ ${} per job, ≤ ${} per day, {} (enforced in code).", l.max_job_usd, l.max_daily_usd, if l.mode == "auto" { "on its own" } else { "owner approves each hire" }))
    }
    fn read_tools(&self, ctx: &Ctx) -> ToolSet {
        let (a1, a2, a3) = (ctx.agent.clone(), ctx.agent.clone(), ctx.agent.clone());
        vec![
            Tool::new(
                "find_agents_to_hire",
                "[Hire agents] Agents on the Orvyn marketplace: what they offer, price, delivery time, rating and whether they're available. Filter by kind of work or words in the offer.",
                json!({ "type": "object", "properties": { "category": { "type": "string", "description": "a job category id, e.g. research, diagrams, video-edit, motion, 3d, 3d-web" }, "query": { "type": "string" } } }),
                move |args: Value| {
                    let agent = a1.clone();
                    async move {
                        let all = crate::hires::marketplace_listings().await.map_err(|e| e.message)?;
                        let cat = args["category"].as_str().map(str::to_lowercase);
                        let q = args["query"].as_str().map(str::to_lowercase).filter(|s| !s.is_empty());
                        let picked: Vec<Value> = all
                            .into_iter()
                            .filter(|l| l["agent"]["id"] != agent.id.as_str())
                            .filter(|l| cat.as_deref().is_none_or(|c| l["offer"]["category"] == c))
                            .filter(|l| q.as_deref().is_none_or(|q| l.to_string().to_lowercase().contains(q)))
                            .take(15)
                            .map(|l| json!({
                                "agentId": l["agent"]["id"], "name": l["agent"]["name"], "handle": l["agent"]["handle"],
                                "category": l["offer"]["category"], "kind": job_category(l["offer"]["category"].as_str().unwrap_or("")).map(|c| c.name),
                                "offer": l["offer"]["title"], "description": l["offer"]["description"], "priceUsd": l["offer"]["priceUsd"], "deliveryHours": l["offer"]["deliveryHours"],
                                "available": l["available"], "completed": l["completed"], "avgRating": l["avgRating"],
                            }))
                            .collect();
                        text(if picked.is_empty() { "No matching agents on the marketplace right now.".to_string() } else { Value::Array(picked).to_string() })
                    }
                },
            ),
            Tool::new("my_hired_jobs", "[Hire agents] The jobs you hired: status, brief, amount and, once delivered, the result.", json!({ "type": "object", "properties": {} }), move |_args: Value| {
                let agent = a2.clone();
                async move {
                    let jobs = ah::hired_jobs(&agent.id, 20).await.map_err(|e| e.message)?;
                    let ids: Vec<String> = jobs.iter().map(|h| h.agent_id.clone()).collect();
                    let names: Vec<(String, String)> = sqlx::query_as("select id, name from agents where id = any($1)").bind(&ids).fetch_all(crate::db::db()).await.map_err(|e| e.to_string())?;
                    let name = |id: &str| names.iter().find(|n| n.0 == id).map(|n| n.1.as_str());
                    text(if jobs.is_empty() { "You haven't hired anyone yet.".to_string() } else { Value::Array(jobs.iter().map(|h| ah::job_view(h, name(&h.agent_id))).collect()).to_string() })
                }
            }),
            Tool::new("my_hire_requests", "[Hire agents] Hires you asked your owner to approve, and what they decided.", json!({ "type": "object", "properties": {} }), move |_args: Value| {
                let agent = a3.clone();
                async move {
                    let ps = ah::proposals(&agent.id).await.map_err(|e| e.message)?;
                    text(Value::Array(ps.iter().take(15).map(|p| json!({ "id": p.id, "agentId": p.target_agent_id, "priceUsd": p.price_usd, "status": p.status, "jobId": p.hire_id, "error": p.error })).collect()).to_string())
                }
            }),
        ]
    }
    fn has_actions(&self) -> bool {
        true
    }
    fn action_tools(&self, ctx: &Ctx) -> ToolSet {
        let limits = Limits::from_settings(&ctx.config);
        let (a1, a2, l1) = (ctx.agent.clone(), ctx.agent.clone(), limits.clone());
        vec![
            Tool::new(
                "hire_agent",
                &format!(
                    "[Hire agents, spends from your wallet] Hire an agent from the marketplace (find_agents_to_hire first). Write a complete brief: what to deliver, format, length, deadline needs. {}",
                    if limits.mode == "auto" { "You hire within your owner's limits; the job is paid into escrow right away." } else { "Your owner approves it first: say why in reason." }
                ),
                json!({ "type": "object", "properties": { "agentId": { "type": "string", "description": "the agent's id or handle" }, "brief": { "type": "string" }, "reason": { "type": "string", "description": "why this hire, for your owner" } }, "required": ["agentId", "brief", "reason"] }),
                move |args: Value| {
                    let (agent, limits) = (a1.clone(), l1.clone());
                    async move {
                        let client = me(&agent).await?;
                        let (target_id, brief, reason) = (args["agentId"].as_str().unwrap_or(""), args["brief"].as_str().unwrap_or("").trim(), args["reason"].as_str().unwrap_or(""));
                        if brief.chars().count() < 30 {
                            return text(json!({ "ok": false, "refused": "Write a fuller brief: what to deliver, in what format." }).to_string());
                        }
                        let (target, offer) = match ah::check_hire(&client, &limits, target_id).await {
                            Ok(x) => x,
                            Err(e) => return text(json!({ "ok": false, "refused": e.message }).to_string()),
                        };
                        if limits.mode == "auto" {
                            match ah::hire_now(&client, &target, brief, &limits.token).await {
                                Ok(h) => text(json!({ "ok": true, "jobId": h.id, "status": h.status, "agent": target.name, "priceUsd": offer.price_usd, "note": "Paid into escrow. The agent works on it; check my_hired_jobs." }).to_string()),
                                Err(e) => text(json!({ "ok": false, "error": e.message }).to_string()),
                            }
                        } else {
                            match ah::propose(&client, &target, &offer, brief, &limits.token, reason).await {
                                Ok(p) => text(json!({ "ok": true, "requestId": p.id, "status": "waiting for your owner", "agent": target.name, "priceUsd": offer.price_usd }).to_string()),
                                Err(e) => text(json!({ "ok": false, "refused": e.message }).to_string()),
                            }
                        }
                    }
                },
            ),
            Tool::new(
                "review_hired_job",
                "[Hire agents] As the client of a job you hired: approve the delivery (pays the agent), ask for changes (a message saying exactly what to fix), dispute it (only if it clearly misses the brief), or reclaim the money if nothing was delivered by the deadline.",
                json!({ "type": "object", "properties": { "jobId": { "type": "string" }, "decision": { "type": "string", "enum": ["approve", "changes", "dispute", "reclaim"] }, "message": { "type": "string" } }, "required": ["jobId", "decision"] }),
                move |args: Value| {
                    let agent = a2.clone();
                    async move {
                        let client = me(&agent).await?;
                        let (id, decision, message) = (args["jobId"].as_str().unwrap_or(""), args["decision"].as_str().unwrap_or(""), args["message"].as_str().unwrap_or(""));
                        let out = match decision {
                            "changes" => ah::client_message(&client, id, message).await.map(|_| json!({ "ok": true, "note": "Sent; the agent revises and delivers again." })),
                            "approve" | "dispute" | "reclaim" => ah::client_action(&client, id, decision, message).await.map(|h| json!({ "ok": true, "status": h.status })),
                            _ => Ok(json!({ "ok": false, "refused": "decision must be approve, changes, dispute or reclaim" })),
                        };
                        text(out.unwrap_or_else(|e| json!({ "ok": false, "error": e.message })).to_string())
                    }
                },
            ),
        ]
    }
}
