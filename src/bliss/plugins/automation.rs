//! Automation: standing orders the agent sets once and Orvyn carries out without the model: DCA buys
//! on a schedule, and price-triggered buys or sells. Every execution goes through the Trader plugin's
//! own buy and sell (its limits, budget, venue checks, simulation, and the wallet policy): there's
//! no second trading path. Rules live in this plugin's state.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::data::meta;
use crate::ai::{schema, Tool, ToolSet};
use crate::bliss::prices::{price_of, Subject};
use crate::bliss::settings::{number, SettingField, Settings};
use crate::bliss::{json_text, Ctx, Installed, JobFuture, Plugin, PluginJob, PluginMeta};
use crate::db::{db, log_agent, AgentLogKind};
use crate::error::{Error, Result};
use crate::evm;
use crate::tiers::Tier;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Rule {
    #[serde(rename_all = "camelCase")]
    Dca { id: String, token: String, amount_bnb: f64, every_hours: f64, next_at: String, runs_left: i64, note: String },
    #[serde(rename_all = "camelCase")]
    Trigger {
        id: String,
        token: String,
        when: String,
        price_usd: f64,
        action: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        amount_bnb: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sell_pct: Option<f64>,
        note: String,
    },
}

impl Rule {
    fn id(&self) -> &str {
        match self {
            Rule::Dca { id, .. } | Rule::Trigger { id, .. } => id,
        }
    }
}

fn rules_of(state: &Value) -> Vec<Rule> {
    state["rules"].as_array().map(|a| a.iter().filter_map(|r| serde_json::from_value(r.clone()).ok()).collect()).unwrap_or_default()
}

async fn save_rules(agent_id: &str, rules: &[Rule]) -> Result<()> {
    sqlx::query("update agent_plugins set state = state || jsonb_build_object('rules', $3::jsonb) where agent_id = $1 and plugin_id = $2")
        .bind(agent_id)
        .bind("automation")
        .bind(serde_json::to_value(rules)?)
        .execute(db())
        .await?;
    Ok(())
}

/// Carry out one rule through the Trader plugin's own tools. Returns Trader's result text.
async fn act(ctx: &Ctx, rule: &Rule, side: &str, why: &str) -> Result<String> {
    let trader = ctx.installed.iter().find(|p| p.plugin.meta().id == "trader" && p.enabled).ok_or_else(|| Error::invalid("the Trader plugin isn't installed or is off"))?;
    let tctx = crate::bliss::context_for(&ctx.agent, trader, &ctx.installed);
    let tools = trader.plugin.action_tools(&tctx);
    let call = |name: &str| tools.iter().find(|t| t.name == name).and_then(|t| t.execute.clone()).ok_or_else(|| Error::internal(format!("Trader has no {name}")));
    let (kind, note) = match rule {
        Rule::Dca { note, .. } => ("dca", note),
        Rule::Trigger { note, .. } => ("trigger", note),
    };
    let token = match rule {
        Rule::Dca { token, .. } | Rule::Trigger { token, .. } => token.clone(),
    };
    let text = |v: std::result::Result<Value, String>| match v {
        Ok(Value::String(s)) => s,
        Ok(other) => other.to_string(),
        Err(e) => e,
    };
    if side == "buy" {
        let amount = match rule {
            Rule::Dca { amount_bnb, .. } => *amount_bnb,
            Rule::Trigger { amount_bnb, .. } => amount_bnb.unwrap_or(0.0),
        };
        let rationale = crate::http::clip(&format!("Automation ({kind}): {why}. {note}"), 500);
        return Ok(text(call("buy_token")?(json!({ "token": token, "amount_bnb": amount, "rationale": rationale })).await));
    }
    let held = evm::erc20_balance(&token, &ctx.agent.evm_address).await?;
    let decimals = evm::token_decimals("bsc", &token).await?;
    let pct = match rule {
        Rule::Trigger { sell_pct, .. } => sell_pct.unwrap_or(100.0),
        _ => 100.0,
    };
    let amount = held * alloy::primitives::U256::from((pct * 100.0).round() as u64) / alloy::primitives::U256::from(10_000u64);
    if amount == alloy::primitives::U256::ZERO {
        return Ok("nothing to sell".into());
    }
    let shown = alloy::primitives::utils::format_units(amount, decimals).unwrap_or_default();
    let shown = if shown.contains('.') { shown.trim_end_matches('0').trim_end_matches('.').to_string() } else { shown };
    let rationale = crate::http::clip(&format!("Automation (trigger): {why}. {note}"), 500);
    Ok(text(call("sell_token")?(json!({ "token": token, "amount": shown, "rationale": rationale })).await))
}

/// One pass: run due DCA buys and fired triggers.
pub async fn run_automation(ctx: &Ctx, now: chrono::DateTime<chrono::Utc>) -> Result<(usize, usize)> {
    let rules = rules_of(&ctx.state);
    let mut keep: Vec<Rule> = vec![];
    let js = crate::auth::js_number;
    for rule in &rules {
        let r: Result<Option<Rule>> = async {
            match rule {
                Rule::Dca { amount_bnb, every_hours, next_at, runs_left, .. } => {
                    let due = chrono::DateTime::parse_from_rfc3339(next_at).map(|t| t.with_timezone(&chrono::Utc)).unwrap_or(now);
                    if due > now {
                        return Ok(Some(rule.clone()));
                    }
                    let out = act(ctx, rule, "buy", &format!("scheduled buy of {} BNB every {}h", js(*amount_bnb), js(*every_hours))).await?;
                    log_agent(&ctx.agent.id, AgentLogKind::Trade, &format!("Automation DCA: {}", crate::http::clip(&out, 200))).await?;
                    let left = runs_left - 1;
                    if left > 0 {
                        let mut next = rule.clone();
                        if let Rule::Dca { runs_left, next_at, .. } = &mut next {
                            *runs_left = left;
                            *next_at = crate::public::iso(now + chrono::Duration::milliseconds((every_hours * 3_600_000.0) as i64));
                        }
                        return Ok(Some(next));
                    }
                    Ok(None)
                }
                Rule::Trigger { token, when, price_usd, action, .. } => {
                    let p = price_of(&Subject::Token { chain: "bsc".into(), address: token.clone() }).await?.price_usd;
                    let fired = if when == "below" { p <= *price_usd } else { p >= *price_usd };
                    if !fired {
                        return Ok(Some(rule.clone()));
                    }
                    let out = act(ctx, rule, action, &format!("price ${} crossed {when} ${}", js(p), js(*price_usd))).await?;
                    log_agent(&ctx.agent.id, AgentLogKind::Trade, &format!("Automation trigger: {}", crate::http::clip(&out, 200))).await?;
                    // Triggers fire once.
                    Ok(None)
                }
            }
        }
        .await;
        match r {
            Ok(Some(k)) => keep.push(k),
            Ok(None) => {}
            Err(e) => {
                log_agent(&ctx.agent.id, AgentLogKind::Error, &format!("Automation rule {} failed: {}", rule.id(), e.message)).await?;
                keep.push(rule.clone());
            }
        }
    }
    if keep != rules {
        save_rules(&ctx.agent.id, &keep).await?;
    }
    Ok((rules.len(), keep.len()))
}

pub struct Automation(PluginMeta);

pub fn automation() -> Arc<dyn Plugin> {
    Arc::new(Automation(meta(
        "automation",
        "Automation",
        "capability",
        "Standing orders: DCA buys on a schedule and price-triggered buys or sells, executed through Trader within its limits.",
        "The agent sets rules once and Orvyn carries them out every minute without the model: buy a fixed amount every N hours (DCA), or buy, or sell a share of a holding, when a token crosses a price. Every execution goes through the Trader plugin, so its per-buy and daily limits, budget, simulation and the wallet policy all apply. Needs Trader installed.",
        &["trade", "spend-bnb", "read-market-data"],
    )))
}

fn short_id() -> String {
    crate::lifecycle::uuid_v4()[..8].to_string()
}

fn run_job(ctx: Ctx) -> JobFuture {
    Box::pin(async move { run_automation(&ctx, chrono::Utc::now()).await.map(|_| ()) })
}

#[async_trait]
impl Plugin for Automation {
    fn meta(&self) -> &PluginMeta {
        &self.0
    }
    fn settings(&self) -> Vec<SettingField> {
        vec![number("maxRules", "Max active rules", None, 10.0, 1.0, 50.0, Some(1.0), None)]
    }
    fn validate(&self, _c: &Settings, others: &[Installed], _: Tier) -> Option<String> {
        (!others.iter().any(|p| p.plugin.meta().id == "trader")).then(|| "Automation needs the Trader plugin; install Trader first".into())
    }
    fn mandate(&self, ctx: &Ctx) -> Option<String> {
        Some(format!("Automation: at most {} active rules; every execution is a Trader trade under Trader's limits.", crate::auth::js_number(ctx.num("maxRules"))))
    }
    fn read_tools(&self, ctx: &Ctx) -> ToolSet {
        let state = ctx.state.clone();
        vec![Tool::new("list_automations", "The agent's active standing orders (DCA and price triggers).", schema(&[]), move |_| {
            let state = state.clone();
            async move { json_text(json!({ "rules": rules_of(&state) })) }
        })]
    }
    fn has_actions(&self) -> bool {
        true
    }
    fn action_tools(&self, ctx: &Ctx) -> ToolSet {
        let max = ctx.num("maxRules") as usize;
        let (c1, c2, c3) = (ctx.clone(), ctx.clone(), ctx.clone());
        vec![
            Tool::new(
                "add_dca",
                "Buy `amount_bnb` of a token every `every_hours`, `times` times, through Trader (within its limits).",
                schema(&[
                    ("token", json!({ "type": "string" }), true),
                    ("amount_bnb", json!({ "type": "number", "exclusiveMinimum": 0 }), true),
                    ("every_hours", json!({ "type": "number", "minimum": 1, "maximum": 720 }), true),
                    ("times", json!({ "type": "integer", "minimum": 1, "maximum": 365 }), true),
                    ("note", json!({ "type": "string", "minLength": 5, "maxLength": 200, "description": "Why, for the public log" }), true),
                ]),
                move |v| {
                    let ctx = c1.clone();
                    async move {
                        let token = v["token"].as_str().unwrap_or("").to_string();
                        if !evm::is_address(&token) {
                            return json_text(json!({ "ok": false, "reason": "token must be an address" }));
                        }
                        let rules = rules_of(&ctx.state);
                        if rules.len() >= max {
                            return json_text(json!({ "ok": false, "reason": format!("at most {max} rules") }));
                        }
                        let rule = Rule::Dca {
                            id: short_id(),
                            token,
                            amount_bnb: v["amount_bnb"].as_f64().ok_or("invalid input: amount_bnb")?,
                            every_hours: v["every_hours"].as_f64().ok_or("invalid input: every_hours")?,
                            runs_left: v["times"].as_i64().ok_or("invalid input: times")?,
                            next_at: crate::public::iso(chrono::Utc::now()),
                            note: v["note"].as_str().unwrap_or("").to_string(),
                        };
                        let mut all = rules;
                        all.push(rule.clone());
                        save_rules(&ctx.agent.id, &all).await.map_err(|e| e.message)?;
                        json_text(json!({ "ok": true, "rule": rule }))
                    }
                },
            ),
            Tool::new(
                "add_price_trigger",
                "When a BNB Chain token's price goes below/above `price_usd`, buy `amount_bnb` of it or sell `sell_pct`% of the holding, once, through Trader.",
                schema(&[
                    ("token", json!({ "type": "string" }), true),
                    ("when", json!({ "type": "string", "enum": ["below", "above"] }), true),
                    ("price_usd", json!({ "type": "number", "exclusiveMinimum": 0 }), true),
                    ("action", json!({ "type": "string", "enum": ["buy", "sell"] }), true),
                    ("amount_bnb", json!({ "type": "number", "exclusiveMinimum": 0 }), false),
                    ("sell_pct", json!({ "type": "number", "minimum": 1, "maximum": 100 }), false),
                    ("note", json!({ "type": "string", "minLength": 5, "maxLength": 200 }), true),
                ]),
                move |v| {
                    let ctx = c2.clone();
                    async move {
                        let token = v["token"].as_str().unwrap_or("").to_string();
                        if !evm::is_address(&token) {
                            return json_text(json!({ "ok": false, "reason": "token must be an address" }));
                        }
                        let action = v["action"].as_str().unwrap_or("").to_string();
                        if action == "buy" && v["amount_bnb"].as_f64().is_none() {
                            return json_text(json!({ "ok": false, "reason": "a buy trigger needs amount_bnb" }));
                        }
                        let rules = rules_of(&ctx.state);
                        if rules.len() >= max {
                            return json_text(json!({ "ok": false, "reason": format!("at most {max} rules") }));
                        }
                        let rule = Rule::Trigger {
                            id: short_id(),
                            token,
                            when: v["when"].as_str().unwrap_or("below").to_string(),
                            price_usd: v["price_usd"].as_f64().ok_or("invalid input: price_usd")?,
                            amount_bnb: v["amount_bnb"].as_f64(),
                            sell_pct: (action == "sell").then(|| v["sell_pct"].as_f64().unwrap_or(100.0)),
                            action,
                            note: v["note"].as_str().unwrap_or("").to_string(),
                        };
                        let mut all = rules;
                        all.push(rule.clone());
                        save_rules(&ctx.agent.id, &all).await.map_err(|e| e.message)?;
                        json_text(json!({ "ok": true, "rule": rule }))
                    }
                },
            ),
            Tool::new("cancel_automation", "Cancel a standing order by id.", schema(&[("id", json!({ "type": "string", "maxLength": 20 }), true)]), move |v| {
                let ctx = c3.clone();
                async move {
                    let id = v["id"].as_str().unwrap_or("").to_string();
                    let rules = rules_of(&ctx.state);
                    let left: Vec<Rule> = rules.iter().filter(|r| r.id() != id).cloned().collect();
                    if left.len() == rules.len() {
                        return json_text(json!({ "ok": false, "reason": "no such rule" }));
                    }
                    save_rules(&ctx.agent.id, &left).await.map_err(|e| e.message)?;
                    json_text(json!({ "ok": true }))
                }
            }),
        ]
    }
    fn jobs(&self) -> Vec<PluginJob> {
        vec![PluginJob { id: "run", every: |_| 60.0, run: run_job }]
    }
}
