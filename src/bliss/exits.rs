//! Stop-loss and take-profit exits for spot holdings on BNB Chain. DEXes have no native trigger
//! orders, so Orvyn watches prices and sells through the plugin's own sell path when an exit
//! triggers. The creator's `stopLossPct` is a hard limit: every buy gets an automatic stop at most
//! that far below the entry; the agent can tighten it but never loosen or remove it.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};

use super::settings::{number, SettingField};
use super::{input, json_text, round};
use crate::ai::{schema, Tool, ToolSet};
use crate::db::{db, log_agent, AgentLogKind, TriggerOrder};
use crate::error::{Error, Result};

pub type BoxFut<T> = Pin<Box<dyn Future<Output = Result<T>> + Send>>;

/// How a plugin prices, checks and sells a market for its exits.
pub struct ExitAdapter {
    pub plugin_id: &'static str,
    /// "BNB" | "USD".
    pub unit: &'static str,
    pub price: Arc<dyn Fn(String) -> BoxFut<Option<f64>> + Send + Sync>,
    pub holds: Arc<dyn Fn(String) -> BoxFut<bool> + Send + Sync>,
    pub sell: Arc<dyn Fn(String, u32, String) -> BoxFut<(bool, Value)> + Send + Sync>,
}

#[derive(Debug, Clone, Copy)]
pub struct ExitLimits {
    pub stop_loss_pct: f64,
    pub take_profit_pct: f64,
}

const MAX_ATTEMPTS: i32 = 3;

fn label(kind: &str) -> &'static str {
    if kind == "stop_loss" { "Stop-loss" } else { "Take-profit" }
}

fn js(n: f64) -> String {
    crate::auth::js_number(n)
}

pub async fn open_exits(agent_id: &str, plugin_id: Option<&str>) -> Result<Vec<TriggerOrder>> {
    Ok(sqlx::query_as("select * from trigger_orders where agent_id = $1 and status = 'open' and ($2::text is null or plugin_id = $2) order by created_at desc")
        .bind(agent_id)
        .bind(plugin_id)
        .fetch_all(db())
        .await?)
}

pub async fn recent_exits(agent_id: &str, limit: i64) -> Result<Vec<TriggerOrder>> {
    Ok(sqlx::query_as("select * from trigger_orders where agent_id = $1 order by created_at desc limit $2").bind(agent_id).bind(limit).fetch_all(db()).await?)
}

pub struct NewExit<'a> {
    pub plugin_id: &'a str,
    pub market: &'a str,
    pub kind: &'a str,
    pub trigger_price: f64,
    pub unit: &'a str,
    pub sell_percent: Option<f64>,
    pub source: &'a str,
    pub note: Option<String>,
}

/// Place an exit. One open stop-loss and one take-profit per market: a new one replaces the old.
pub async fn place_exit(agent_id: &str, e: NewExit<'_>) -> Result<TriggerOrder> {
    if !(e.trigger_price > 0.0) || !e.trigger_price.is_finite() {
        return Err(Error::invalid("Trigger price must be a positive number."));
    }
    let sell_percent = e.sell_percent.unwrap_or(100.0).round() as i32;
    if !(1..=100).contains(&sell_percent) {
        return Err(Error::invalid("Sell percent must be 1-100."));
    }
    sqlx::query("update trigger_orders set status = 'cancelled', note = 'replaced' where agent_id = $1 and plugin_id = $2 and market = $3 and kind = $4 and status = 'open'")
        .bind(agent_id)
        .bind(e.plugin_id)
        .bind(e.market)
        .bind(e.kind)
        .execute(db())
        .await?;
    Ok(sqlx::query_as(
        "insert into trigger_orders (agent_id, plugin_id, market, kind, trigger_price, price_unit, sell_percent, source, note) values ($1,$2,$3,$4,$5,$6,$7,$8,$9) returning *",
    )
    .bind(agent_id)
    .bind(e.plugin_id)
    .bind(e.market)
    .bind(e.kind)
    .bind(e.trigger_price)
    .bind(e.unit)
    .bind(sell_percent)
    .bind(e.source)
    .bind(e.note)
    .fetch_one(db())
    .await?)
}

/// The lowest stop allowed for a holding now priced at `price`.
pub fn stop_floor(price: f64, l: &ExitLimits) -> f64 {
    if l.stop_loss_pct > 0.0 { price * (1.0 - l.stop_loss_pct / 100.0) } else { 0.0 }
}

/// Validate an exit the agent asks for against the current price and the creator's limit.
pub fn check_exit(kind: &str, trigger: f64, price: f64, l: &ExitLimits) -> Result<()> {
    if kind == "stop_loss" {
        if trigger >= price {
            return Err(Error::invalid(format!("A stop-loss must be below the current price ({}).", js(round(price, 10)))));
        }
        let floor = stop_floor(price, l);
        if trigger < floor {
            return Err(Error::invalid(format!("The creator's limit is a stop at most {}% below the price, i.e. at or above {}.", js(l.stop_loss_pct), js(round(floor, 10)))));
        }
    } else if trigger <= price {
        return Err(Error::invalid(format!("A take-profit must be above the current price ({}).", js(round(price, 10)))));
    }
    Ok(())
}

/// After a buy: automatic exits from the creator's defaults, relative to the fill price.
pub async fn auto_exits(agent_id: &str, plugin_id: &str, market: &str, entry: f64, unit: &str, l: &ExitLimits) -> Result<()> {
    if !(entry > 0.0) {
        return Ok(());
    }
    let current = open_exits(agent_id, Some(plugin_id)).await?;
    let existing = |kind: &str| current.iter().find(|o| o.market == market && o.kind == kind).cloned();
    let stop = if l.stop_loss_pct > 0.0 { entry * (1.0 - l.stop_loss_pct / 100.0) } else { 0.0 };
    // Keep a tighter stop the agent already set; otherwise (re)place the default.
    let sl = existing("stop_loss");
    if stop > 0.0 && sl.as_ref().is_none_or(|s| s.trigger_price < stop) {
        place_exit(agent_id, NewExit { plugin_id, market, kind: "stop_loss", trigger_price: stop, unit, sell_percent: None, source: "auto", note: Some(format!("{}% below entry {}", js(l.stop_loss_pct), js(round(entry, 10)))) }).await?;
    }
    if l.take_profit_pct > 0.0 && existing("take_profit").is_none() {
        let tp = entry * (1.0 + l.take_profit_pct / 100.0);
        place_exit(agent_id, NewExit { plugin_id, market, kind: "take_profit", trigger_price: tp, unit, sell_percent: None, source: "auto", note: Some(format!("{}% above entry {}", js(l.take_profit_pct), js(round(entry, 10)))) }).await?;
    }
    Ok(())
}

/// Cancel an exit. The agent can't remove the stop that enforces the creator's limit.
pub async fn cancel_exit(agent_id: &str, id: i32, by: &str, limits: Option<&ExitLimits>) -> Result<TriggerOrder> {
    let order: Option<TriggerOrder> = sqlx::query_as("select * from trigger_orders where id = $1 and agent_id = $2").bind(id).bind(agent_id).fetch_optional(db()).await?;
    let Some(order) = order.filter(|o| o.status == "open") else { return Err(Error::not_found(format!("No open exit #{id}."))) };
    if by == "agent" && order.kind == "stop_loss" && limits.is_some_and(|l| l.stop_loss_pct > 0.0) {
        return Err(Error::forbidden("The creator requires a stop-loss on every holding. Place a tighter one instead of cancelling it."));
    }
    sqlx::query("update trigger_orders set status = 'cancelled', note = $2 where id = $1").bind(id).bind(format!("cancelled by {by}")).execute(db()).await?;
    Ok(order)
}

/// Check every open exit of one plugin and sell the ones that triggered.
pub async fn run_exits(agent_id: &str, adapter: &ExitAdapter) -> Result<()> {
    let open = open_exits(agent_id, Some(adapter.plugin_id)).await?;
    let mut prices: HashMap<String, Option<f64>> = HashMap::new();
    let mut closed: HashSet<String> = HashSet::new();
    for order in open {
        if closed.contains(&order.market) {
            continue;
        }
        let r: Result<()> = async {
            if !(adapter.holds)(order.market.clone()).await? {
                sqlx::query("update trigger_orders set status = 'cancelled', note = 'position closed' where agent_id = $1 and market = $2 and status = 'open'").bind(agent_id).bind(&order.market).execute(db()).await?;
                closed.insert(order.market.clone());
                return Ok(());
            }
            let price = match prices.get(&order.market) {
                Some(p) => *p,
                None => {
                    let p = (adapter.price)(order.market.clone()).await.ok().flatten();
                    prices.insert(order.market.clone(), p);
                    p
                }
            };
            let Some(price) = price else { return Ok(()) };
            let hit = if order.kind == "stop_loss" { price <= order.trigger_price } else { price >= order.trigger_price };
            if !hit {
                return Ok(());
            }
            // Claim it, so two workers never sell for the same exit.
            let claimed = sqlx::query("update trigger_orders set status = 'triggered', triggered_at = now() where id = $1 and status = 'open'").bind(order.id).execute(db()).await?;
            if claimed.rows_affected() == 0 {
                return Ok(());
            }
            let what = format!("{} at {} {} (price {})", label(&order.kind), js(round(order.trigger_price, 10)), order.price_unit, js(round(price, 10)));
            let sold = (adapter.sell)(order.market.clone(), order.sell_percent as u32, format!("{what}: sold {}%", order.sell_percent)).await.unwrap_or_else(|e| (false, json!(e.message)));
            if sold.0 {
                sqlx::query("update trigger_orders set status = 'filled', result = $2 where id = $1").bind(order.id).bind(&sold.1).execute(db()).await?;
                log_agent(agent_id, AgentLogKind::Trade, &format!("{what} on {}: sold {}%", order.market, order.sell_percent)).await?;
                if order.sell_percent == 100 {
                    sqlx::query("update trigger_orders set status = 'cancelled', note = $3 where agent_id = $1 and market = $2 and status = 'open'")
                        .bind(agent_id)
                        .bind(&order.market)
                        .bind(format!("{} #{} filled", label(&order.kind).to_lowercase(), order.id))
                        .execute(db())
                        .await?;
                    closed.insert(order.market.clone());
                }
            } else {
                let attempts = order.attempts + 1;
                let failed = attempts >= MAX_ATTEMPTS;
                sqlx::query("update trigger_orders set status = $2, attempts = $3, result = $4, triggered_at = null where id = $1")
                    .bind(order.id)
                    .bind(if failed { "failed" } else { "open" })
                    .bind(attempts)
                    .bind(json!({ "error": sold.1 }))
                    .execute(db())
                    .await?;
                if failed {
                    let detail = sold.1.as_str().map(String::from).unwrap_or_else(|| sold.1.to_string());
                    log_agent(agent_id, AgentLogKind::Error, &format!("{what} on {}: the sell failed {attempts} times ({detail}); the exit was dropped", order.market)).await?;
                }
            }
            Ok(())
        }
        .await;
        if let Err(e) = r {
            eprintln!("[exits] {} #{}: {e}", adapter.plugin_id, order.id);
        }
    }
    Ok(())
}

/// Creator settings every spot trading plugin shares.
pub fn exit_settings(default_stop_pct: f64) -> Vec<SettingField> {
    vec![
        number("stopLossPct", "Stop-loss on every buy", Some("Sell a holding once it falls this far below the entry. The agent can tighten it, never loosen or remove it. 0 = off."), default_stop_pct, 0.0, 95.0, Some(1.0), Some("%")),
        number("takeProfitPct", "Take-profit on every buy", Some("Sell once a holding rises this far above the entry. The agent can change it. 0 = off."), 0.0, 0.0, 10_000.0, Some(1.0), Some("%")),
    ]
}

pub fn exit_mandate(l: &ExitLimits) -> String {
    let stop = if l.stop_loss_pct > 0.0 { format!("every holding keeps a stop-loss at most {}% below its price (placed automatically on each buy)", js(l.stop_loss_pct)) } else { "no required stop-loss".into() };
    let tp = if l.take_profit_pct > 0.0 { format!(", default take-profit {}% above entry", js(l.take_profit_pct)) } else { String::new() };
    format!("{stop}{tp}")
}

/// Tools for a plugin's exits: holder chat gets coverage only (trigger prices are private);
/// autopilot gets the levels, set and cancel.
pub fn exit_tools(prefix: &str, agent_id: &str, adapter: Arc<ExitAdapter>, limits: ExitLimits, market_help: &str) -> (ToolSet, ToolSet) {
    let aid = agent_id.to_string();
    let a = adapter.clone();
    let coverage = Tool::new(
        &format!("{prefix}_exit_coverage"),
        &format!("Which of this agent's {prefix} holdings have a stop-loss and a take-profit in place, and the creator's stop-loss rule. Trigger prices are private and not included."),
        schema(&[]),
        move |_| {
            let (aid, a) = (aid.clone(), a.clone());
            async move {
                let open = open_exits(&aid, Some(a.plugin_id)).await.map_err(|e| e.message)?;
                let mut markets: Vec<String> = vec![];
                for o in &open {
                    if !markets.contains(&o.market) {
                        markets.push(o.market.clone());
                    }
                }
                let holdings: Vec<Value> = markets
                    .iter()
                    .map(|m| json!({ "market": m, "stop_loss": open.iter().any(|o| &o.market == m && o.kind == "stop_loss"), "take_profit": open.iter().any(|o| &o.market == m && o.kind == "take_profit") }))
                    .collect();
                json_text(json!({ "required_stop_loss_pct": if limits.stop_loss_pct > 0.0 { json!(limits.stop_loss_pct) } else { Value::Null }, "holdings": holdings }))
            }
        },
    );
    let (aid, a) = (agent_id.to_string(), adapter.clone());
    let list = Tool::new(
        &format!("{prefix}_exits"),
        &format!("Open stop-loss and take-profit exits on this agent's {prefix} holdings, with trigger prices in {}. Private: never share the levels with holders.", adapter.unit),
        schema(&[]),
        move |_| {
            let (aid, a) = (aid.clone(), a.clone());
            async move {
                let open = open_exits(&aid, Some(a.plugin_id)).await.map_err(|e| e.message)?;
                json_text(Value::Array(open.iter().map(|o| json!({ "id": o.id, "market": o.market, "kind": o.kind, "trigger_price": o.trigger_price, "unit": o.price_unit, "sell_percent": o.sell_percent, "source": o.source })).collect()))
            }
        },
    );
    #[derive(Deserialize)]
    struct SetExit {
        market: String,
        kind: String,
        trigger_price: f64,
        sell_percent: Option<f64>,
        rationale: String,
    }
    let (aid, a) = (agent_id.to_string(), adapter.clone());
    let set_desc = format!(
        "Set a stop-loss or take-profit on a holding ({market_help}). Orvyn sells sell_percent of it when the price in {} crosses trigger_price. Replaces the existing exit of that kind.{}",
        adapter.unit,
        if limits.stop_loss_pct > 0.0 { format!(" Stops can't be more than {}% below the current price.", js(limits.stop_loss_pct)) } else { String::new() }
    );
    let set = Tool::new(
        &format!("{prefix}_set_exit"),
        &set_desc,
        schema(&[
            ("market", json!({ "type": "string", "description": market_help }), true),
            ("kind", json!({ "type": "string", "enum": ["stop_loss", "take_profit"] }), true),
            ("trigger_price", json!({ "type": "number", "exclusiveMinimum": 0, "description": format!("Price in {}", adapter.unit) }), true),
            ("sell_percent", json!({ "type": "integer", "minimum": 1, "maximum": 100 }), false),
            ("rationale", json!({ "type": "string", "description": "One sentence on why" }), true),
        ]),
        move |v| {
            let (aid, a) = (aid.clone(), a.clone());
            async move {
                let i: SetExit = input(v)?;
                let r: Result<Value> = async {
                    if !(a.holds)(i.market.clone()).await? {
                        return Err(Error::invalid(format!("The agent doesn't hold {}.", i.market)));
                    }
                    let price = (a.price)(i.market.clone()).await?.ok_or_else(|| Error::invalid("No live price for that token right now."))?;
                    check_exit(&i.kind, i.trigger_price, price, &limits)?;
                    let row = place_exit(&aid, NewExit { plugin_id: a.plugin_id, market: &i.market, kind: &i.kind, trigger_price: i.trigger_price, unit: a.unit, sell_percent: i.sell_percent, source: "agent", note: Some(i.rationale.clone()) }).await?;
                    Ok(json!({ "ok": true, "id": row.id, "current_price": price }))
                }
                .await;
                json_text(r.unwrap_or_else(|e| json!({ "ok": false, "error": e.message })))
            }
        },
    );
    let (aid, a, p) = (agent_id.to_string(), adapter.clone(), prefix.to_string());
    let cancel = Tool::new(
        &format!("{prefix}_cancel_exit"),
        "Cancel one of this agent's exits by id (the required stop-loss can only be replaced, not cancelled).",
        schema(&[("id", json!({ "type": "integer" }), true)]),
        move |v| {
            let (aid, a, p) = (aid.clone(), a.clone(), p.clone());
            async move {
                let id = v["id"].as_i64().ok_or("invalid input: id")? as i32;
                let r: Result<Value> = async {
                    if !open_exits(&aid, Some(a.plugin_id)).await?.iter().any(|o| o.id == id) {
                        return Err(Error::not_found(format!("No open {p} exit #{id}.")));
                    }
                    cancel_exit(&aid, id, "agent", Some(&limits)).await?;
                    Ok(json!({ "ok": true }))
                }
                .await;
                json_text(r.unwrap_or_else(|e| json!({ "ok": false, "error": e.message })))
            }
        },
    );
    (vec![coverage], vec![list, set, cancel])
}
