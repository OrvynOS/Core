//! Binance: the agent works its owner's Binance Agentic sub-account through Binance's MCP server.
//! Read tools (market data, balances, positions) are offered in chat and autopilot; tools that act
//! (orders, cancels, transfers, leverage) only in autopilot, and every one is checked against the
//! owner's caps in code before it reaches Binance (crate::binance::check_agent_write). Withdrawals
//! don't exist on Binance's agentic server.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::data::meta;
use crate::ai::{Tool, ToolSet};
use crate::binance::{self, Caps};
use crate::bliss::settings::{boolean, list, number, SettingField, Settings};
use crate::bliss::{Ctx, Plugin, PluginMeta};
use crate::db::{log_agent, AgentLogKind};

pub struct Binance(PluginMeta);

pub fn binance() -> Arc<dyn Plugin> {
    Arc::new(Binance(meta(
        binance::PLUGIN_ID,
        "Binance",
        "capability",
        "Trade the owner's Binance Agentic sub-account (spot, futures, margin, convert) within limits the owner sets; never withdrawals.",
        "The owner connects a Binance Agentic sub-account (Binance's own sign-in; the owner picks what the agent may do) and funds it with what they choose. The agent reads markets, balances and positions, and in copilot can place and cancel orders. Orvyn checks every order against the owner's limits before it reaches Binance: size per order and per day in USD, which products and symbols, and maximum leverage. Binance's agentic server never allows withdrawals, and the owner can disconnect the agent from Binance at any time.",
        &["trade", "read-market-data"],
    )))
}

fn tools_of(state: &Value) -> Vec<Value> {
    if state["connected"] != true {
        return vec![];
    }
    state["tools"].as_array().cloned().unwrap_or_default()
}

fn text(s: String) -> std::result::Result<Value, String> {
    Ok(Value::String(s))
}

async fn conn_for(ctx_agent: &crate::db::Agent) -> std::result::Result<crate::db::BinanceConnection, String> {
    binance::connection(&ctx_agent.owner_account_id, Some(&ctx_agent.id)).await.map_err(|e| e.message)?.ok_or_else(|| "Binance isn't connected; the owner connects it in the Binance plugin's settings".to_string())
}

#[async_trait]
impl Plugin for Binance {
    fn meta(&self) -> &PluginMeta {
        &self.0
    }
    fn settings(&self) -> Vec<SettingField> {
        vec![
            number("maxOrderUsd", "Max per order", Some("Largest order the agent may place, in USD (futures: the position's size, not its margin)."), 50.0, 5.0, 100_000.0, Some(5.0), Some("USD")),
            number("maxDailyUsd", "Max per day", Some("All the agent's orders in any 24 hours, in USD."), 200.0, 5.0, 1_000_000.0, Some(5.0), Some("USD")),
            list("products", "Products", Some("Which of spot, futures, margin and convert the agent may trade."), vec!["spot"], 4, false),
            number("maxLeverage", "Max leverage", Some("Highest leverage the agent may use or set on futures and margin."), 1.0, 1.0, 20.0, Some(1.0), Some("x")),
            list("symbols", "Only these symbols", Some("e.g. BTCUSDT, ETHUSDT. Empty: any symbol."), vec![], 30, true),
            boolean("allowTransfers", "Allow transfers between wallets", Some("Moving funds between the sub-account's own wallets (spot, futures, margin)."), false),
        ]
    }
    fn validate(&self, config: &Settings, _others: &[crate::bliss::Installed], _tier: crate::tiers::Tier) -> Option<String> {
        let products = config.get("products").and_then(Value::as_array).cloned().unwrap_or_default();
        let bad: Vec<String> = products.iter().filter_map(Value::as_str).map(str::to_lowercase).filter(|p| !["spot", "futures", "margin", "convert"].contains(&p.as_str())).collect();
        if !bad.is_empty() {
            return Some(format!("unknown products: {} (use spot, futures, margin, convert)", bad.join(", ")));
        }
        let (order, day) = (config.get("maxOrderUsd").and_then(Value::as_f64).unwrap_or(0.0), config.get("maxDailyUsd").and_then(Value::as_f64).unwrap_or(0.0));
        (order > day).then(|| "the per-order limit can't be above the daily limit".into())
    }
    async fn knowledge(&self, ctx: &Ctx) -> Option<String> {
        if ctx.state["connected"] != true {
            return Some("## Binance\nThe owner hasn't connected a Binance account yet; say so if asked to trade on Binance.".into());
        }
        let c = Caps::from_settings(&ctx.config);
        Some(format!(
            "## Binance\nYou trade the owner's Binance Agentic sub-account with the binance_* tools. Read balances and positions before acting. Orvyn refuses orders outside the owner's limits: at most ${} per order and ${} per 24 hours (in USD), products: {}, leverage up to {}x{}{}. Size orders with quoteOrderQty (USD) where you can. Never try to withdraw.",
            c.max_order_usd,
            c.max_daily_usd,
            if c.products.is_empty() { "none".into() } else { c.products.join(", ") },
            c.max_leverage,
            if c.symbols.is_empty() { String::new() } else { format!(", symbols: {}", c.symbols.join(", ")) },
            if c.allow_transfers { ", transfers between its wallets allowed" } else { ", no transfers" },
        ))
    }
    fn mandate(&self, ctx: &Ctx) -> Option<String> {
        let c = Caps::from_settings(&ctx.config);
        Some(format!("Binance: ≤ ${} per order, ≤ ${} per day, products {}, ≤ {}x leverage (enforced in code).", c.max_order_usd, c.max_daily_usd, c.products.join("/"), c.max_leverage))
    }
    fn read_tools(&self, ctx: &Ctx) -> ToolSet {
        tools_of(&ctx.state)
            .into_iter()
            .filter(binance::is_read_tool)
            .map(|t| {
                let name = t["name"].as_str().unwrap_or("").to_string();
                let agent = ctx.agent.clone();
                Tool::new(&binance::tool_name(&name), &format!("[Binance] {}", t["description"].as_str().unwrap_or("")), t["inputSchema"].clone(), move |args| {
                    let (agent, name) = (agent.clone(), name.clone());
                    async move {
                        let conn = conn_for(&agent).await?;
                        match binance::call_tool(&conn, &name, args).await {
                            Ok(r) => text(binance::result_text(&r)),
                            Err(e) => text(format!("Binance {name} failed: {}", e.message)),
                        }
                    }
                })
            })
            .collect()
    }
    fn has_actions(&self) -> bool {
        true
    }
    fn action_tools(&self, ctx: &Ctx) -> ToolSet {
        let caps = Caps::from_settings(&ctx.config);
        tools_of(&ctx.state)
            .into_iter()
            .filter(|t| !binance::is_read_tool(t))
            .map(|t| {
                let name = t["name"].as_str().unwrap_or("").to_string();
                let (agent, caps) = (ctx.agent.clone(), caps.clone());
                let description = format!("[Binance, acts on the account; checked against the owner's limits] {}", t["description"].as_str().unwrap_or(""));
                Tool::new(&binance::tool_name(&name), &description, t["inputSchema"].clone(), move |args| {
                    let (agent, caps, name) = (agent.clone(), caps.clone(), name.clone());
                    async move {
                        let usd = match binance::check_agent_write(&agent.id, &caps, &name, &args).await {
                            Ok(u) => u,
                            Err(why) => {
                                let _ = log_agent(&agent.id, AgentLogKind::Trade, &format!("Binance {name} refused: {why}")).await;
                                return text(json!({ "ok": false, "refused": why }).to_string());
                            }
                        };
                        let conn = conn_for(&agent).await?;
                        match binance::call_tool(&conn, &name, args.clone()).await {
                            Ok(r) => {
                                if let (Some(u), false) = (usd, r["isError"] == true) {
                                    binance::record_agent_order(&agent.id, &name, &args, u).await.map_err(|e| e.message)?;
                                }
                                let _ = log_agent(&agent.id, AgentLogKind::Trade, &crate::http::clip(&format!("Binance {}", binance::describe_call(&name, &args)), 300)).await;
                                text(binance::result_text(&r))
                            }
                            Err(e) => text(format!("Binance {name} failed: {}", e.message)),
                        }
                    }
                })
            })
            .collect()
    }
}
