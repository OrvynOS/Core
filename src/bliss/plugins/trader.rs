//! Trader: the agent buys and sells BNB Chain tokens on autopilot through Orvyn's trading MCP
//! servers: four.meme and Flap on their bonding curves, PancakeSwap or Uniswap after graduation. The
//! servers only build unsigned transactions; every call is checked again here (chain, the agent's
//! wallet, only trading contracts, exact approvals, BNB within the limits) before the agent signs,
//! and the wallet policy checks each transaction again. Buys spend this plugin's budget; sells
//! credit the BNB back.

use std::sync::Arc;

use alloy::primitives::{Address, Bytes, I256, U256};
use alloy::sol_types::SolInterface;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::data::{fail, meta};
use crate::ai::{schema, Tool, ToolSet};
use crate::bliss::settings::{list, number, select, SettingField, Settings};
use crate::bliss::treasury::{adjust_budget, spendable};
use crate::bliss::{json_text, recent_trades, record_trade, Ctx, Installed, Plugin, PluginMeta};
use crate::db::db;
use crate::error::{Error, Result};
use crate::evm;
use crate::tiers::Tier;
use crate::wallet_policy::rules::Erc20;

pub const VENUES: [&str; 4] = ["fourmeme", "flap", "pancakeswap", "uniswap"];

fn trade_contracts(venue: &str) -> Vec<String> {
    match venue {
        "pancakeswap" => vec!["0x10ed43c718714eb63d5aa57b78b54704e256024e".into(), "0x13f4ea83d0bd40e75c8222255bc855a974568dd4".into()],
        "uniswap" => vec!["0xb971ef87ede563556b2ed4b1c0b0019111dd85d2".into()],
        "fourmeme" => vec!["0x5c952063c7fc8610ffdb798152d69f0b9550762b".into()],
        "flap" => vec![std::env::var("FLAP_PORTAL").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "0xe2cE6ab80874Fa9Fa2aAE65D277Dd6B8e65C9De0".into()).to_lowercase()],
        _ => vec![],
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlanCall {
    pub to: String,
    pub data: String,
    pub value: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub chain_id: u64,
    pub account: String,
    pub calls: Vec<PlanCall>,
    #[serde(default)]
    pub quote: Value,
    #[serde(default)]
    pub simulation: Option<Value>,
}

/// Run one of Orvyn's trading tools (orvyn-mcp, in process: the same tools api.orvyn.cash/mcp
/// serves) and read its JSON answer.
pub async fn mcp(venue: &str, name: &str, args: Value) -> Result<Value> {
    debug_assert_eq!(orvyn_mcp::venue_of(name), Some(venue));
    orvyn_mcp::call_json(name, &args).await.map_err(Error::internal)
}

/// Where a token trades now: its launchpad curve, or a DEX once it has graduated.
async fn pick_venue(token: &str, allowed: &[String]) -> Result<String> {
    let has = |v: &str| allowed.iter().any(|a| a == v);
    if has("fourmeme") {
        if let Ok(info) = mcp("fourmeme", "fourmeme_token_info", json!({ "token": token })).await {
            if info["graduated"] == false && info["quoteToken"] == "BNB" {
                return Ok("fourmeme".into());
            }
        }
    }
    if has("flap") {
        if let Ok(info) = mcp("flap", "flap_token_info", json!({ "token": token })).await {
            if info["tradingOnCurve"] == true && info["quoteToken"] == "BNB" {
                return Ok("flap".into());
            }
        }
    }
    for v in ["pancakeswap", "uniswap"] {
        if has(v) {
            return Ok(v.into());
        }
    }
    Err(Error::invalid("No enabled venue trades this token"))
}

/// Check what the MCP server returned before anything is signed.
pub fn check_plan(plan: &Plan, venue: &str, agent: &str, max_value: U256) -> Result<()> {
    let bad = |m: &str| Err(Error::forbidden(m.to_string()));
    if plan.chain_id != 56 {
        return bad("the trade isn't on BNB Chain");
    }
    if !plan.account.eq_ignore_ascii_case(agent) {
        return bad("the trade isn't from the agent's wallet");
    }
    if plan.calls.is_empty() || plan.calls.len() > 2 {
        return bad("unexpected number of calls");
    }
    let contracts = trade_contracts(venue);
    let mut total = U256::ZERO;
    for (i, c) in plan.calls.iter().enumerate() {
        if !evm::is_address(&c.to) {
            return bad("bad call target");
        }
        let value = U256::from_str_radix(&c.value, 10).map_err(|_| Error::forbidden("bad call value"))?;
        total += value;
        let to = c.to.to_lowercase();
        if contracts.contains(&to) {
            if i != plan.calls.len() - 1 {
                return bad("the swap must be the last call");
            }
            continue;
        }
        let data: Bytes = c.data.parse().map_err(|_| Error::forbidden(format!("unexpected call to {}", c.to)))?;
        let Ok(call) = Erc20::Erc20Calls::abi_decode(&data) else { return Err(Error::forbidden(format!("unexpected call to {}", c.to))) };
        let Erc20::Erc20Calls::approve(a) = call else { return Err(Error::forbidden(format!("unexpected call on {}", c.to))) };
        if value != U256::ZERO {
            return Err(Error::forbidden(format!("unexpected approve on {}", c.to)));
        }
        if !contracts.contains(&format!("{:#x}", a.spender)) {
            return bad("approval to a contract outside the venue");
        }
        if a.amount == U256::MAX {
            return bad("unlimited approval");
        }
    }
    if total > max_value {
        return bad("the trade attaches more BNB than allowed");
    }
    Ok(())
}

async fn trades_today(agent_id: &str) -> Result<i64> {
    Ok(sqlx::query_scalar("select count(*) from trades where agent_id = $1 and plugin_id = 'trader' and ok = true and created_at >= now() - interval '1 day'").bind(agent_id).fetch_one(db()).await?)
}

async fn execute(ctx: &Ctx, plan: &Plan) -> Result<Vec<String>> {
    let mut hashes = vec![];
    for c in &plan.calls {
        let to: Address = evm::parse_address(&c.to)?;
        let data: Bytes = c.data.parse().map_err(|_| Error::invalid("bad call data"))?;
        let value = U256::from_str_radix(&c.value, 10).unwrap_or_default();
        hashes.push(format!("{:#x}", crate::wallets::agent_send(&ctx.agent.id, &ctx.agent.evm_wallet, to, data, value).await?));
    }
    Ok(hashes)
}

pub struct Trader(PluginMeta);

pub fn trader() -> Arc<dyn Plugin> {
    Arc::new(Trader(meta(
        "trader",
        "Trader",
        "capability",
        "Buys and sells BNB Chain tokens on four.meme, Flap, PancakeSwap and Uniswap, within your limits.",
        "The agent trades from its own wallet in copilot: four.meme and Flap while a token is on its bonding curve, PancakeSwap or Uniswap once it graduates. Transactions are built by Orvyn's open-source trading MCP servers and checked again before the agent signs: only the venue's contracts, output to the agent's wallet, exact approvals, and BNB within your per-trade and daily limits. Buys spend this plugin's budget (its share of tax income); sells credit the BNB back.",
        &["trade", "spend-bnb", "read-market-data"],
    )))
}

fn venues(c: &Settings) -> Vec<String> {
    c.get("venues").and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default()
}

#[async_trait]
impl Plugin for Trader {
    fn meta(&self) -> &PluginMeta {
        &self.0
    }
    fn settings(&self) -> Vec<SettingField> {
        vec![
            number("feeSharePct", "Share of tax income for trading", None, 40.0, 0.0, 100.0, Some(1.0), Some("%")),
            number("maxTradeBnb", "Max BNB per buy", None, 0.05, 0.001, 5.0, Some(0.001), Some("BNB")),
            number("maxTradesPerDay", "Max trades per day", None, 10.0, 1.0, 100.0, Some(1.0), None),
            number("maxSlippagePct", "Max slippage", Some("Tax tokens need their tax on top."), 5.0, 0.1, 30.0, Some(0.1), Some("%")),
            list("venues", "Venues", Some("fourmeme, flap, pancakeswap, uniswap"), VENUES.to_vec(), 4, false),
            select("ownToken", "The agent's own token", None, "never", &[("never", "Never trade it"), ("buy-only", "May buy it back, never sell")]),
        ]
    }
    fn fee_share(&self, c: &Settings) -> Option<f64> {
        c.get("feeSharePct").and_then(Value::as_f64)
    }
    fn validate(&self, c: &Settings, _: &[Installed], _: Tier) -> Option<String> {
        venues(c).iter().any(|v| !VENUES.contains(&v.as_str())).then(|| format!("venues must be among {}", VENUES.join(", ")))
    }
    fn mandate(&self, ctx: &Ctx) -> Option<String> {
        let js = crate::auth::js_number;
        let own = if ctx.text("ownToken") == "never" { "Never trade the agent's own token." } else { "The agent's own token may be bought back, never sold." };
        Some(format!(
            "Trader: at most {} BNB per buy, {} trades a day, {}% slippage, on {}. {own} Check a token (security, liquidity) before buying it.",
            js(ctx.num("maxTradeBnb")),
            js(ctx.num("maxTradesPerDay")),
            js(ctx.num("maxSlippagePct")),
            ctx.list("venues").join(", ")
        ))
    }
    fn read_tools(&self, ctx: &Ctx) -> ToolSet {
        let c1 = ctx.clone();
        let agent_id = ctx.agent.id.clone();
        vec![
            Tool::new(
                "trade_quote",
                "Quote a buy (spend BNB) or sell (tokens for BNB) of a BNB Chain token at its current venue. Nothing is traded.",
                schema(&[
                    ("token", json!({ "type": "string", "description": "Token address" }), true),
                    ("side", json!({ "type": "string", "enum": ["buy", "sell"] }), true),
                    ("amount", json!({ "type": "string", "description": "BNB to spend (buy) or tokens to sell" }), true),
                ]),
                move |v| {
                    let ctx = c1.clone();
                    async move {
                        let (token, side, amount) = (v["token"].as_str().unwrap_or("").to_string(), v["side"].as_str().unwrap_or("").to_string(), v["amount"].as_str().unwrap_or("").to_string());
                        let r: Result<Value> = async {
                            let venue = pick_venue(&token, &ctx.list("venues")).await?;
                            let args = if venue == "fourmeme" || venue == "flap" {
                                json!({ "token": token, "side": side, "amount": amount })
                            } else if side == "buy" {
                                json!({ "token_in": "BNB", "token_out": token, "amount_in": amount })
                            } else {
                                json!({ "token_in": token, "token_out": "BNB", "amount_in": amount })
                            };
                            let mut q = mcp(&venue, &format!("{venue}_quote"), args).await?;
                            let mut out = json!({ "venue": venue });
                            if let Some(o) = q.as_object_mut() {
                                for (k, val) in o {
                                    out[k] = val.clone();
                                }
                            }
                            Ok(out)
                        }
                        .await;
                        match r {
                            Ok(q) => json_text(q),
                            Err(e) => Ok(Value::String(format!("Quote failed: {}", e.message))),
                        }
                    }
                },
            ),
            Tool::new("recent_trades", "The agent's recent Trader trades.", schema(&[]), move |_| {
                let a = agent_id.clone();
                async move { json_text(serde_json::to_value(recent_trades(&a, "trader", 10).await.map_err(fail)?).unwrap()) }
            }),
        ]
    }
    fn has_actions(&self) -> bool {
        true
    }
    fn action_tools(&self, ctx: &Ctx) -> ToolSet {
        let (c1, c2) = (ctx.clone(), ctx.clone());
        vec![
            Tool::new(
                "buy_token",
                "Buy a BNB Chain token with BNB from the trading budget, at its current venue (curve or DEX). Within the per-buy and daily limits.",
                schema(&[
                    ("token", json!({ "type": "string", "description": "Token address" }), true),
                    ("amount_bnb", json!({ "type": "number", "exclusiveMinimum": 0, "description": "BNB to spend" }), true),
                    ("rationale", json!({ "type": "string", "minLength": 10, "maxLength": 500, "description": "Why, for the public trade log" }), true),
                ]),
                move |v| {
                    let ctx = c1.clone();
                    async move {
                        let token = v["token"].as_str().unwrap_or("").to_string();
                        let amount = v["amount_bnb"].as_f64().unwrap_or(0.0);
                        let rationale = v["rationale"].as_str().unwrap_or("").to_string();
                        let mut venue_used = "pancakeswap".to_string();
                        let r: Result<Value> = async {
                            if !evm::is_address(&token) {
                                return Err(Error::invalid("token must be an address"));
                            }
                            let own = ctx.agent.token_address.as_deref().is_some_and(|t| t.eq_ignore_ascii_case(&token));
                            if own && ctx.text("ownToken") == "never" {
                                return Err(Error::forbidden("the creator doesn't allow trading the agent's own token"));
                            }
                            if amount > ctx.num("maxTradeBnb") {
                                return Err(Error::forbidden(format!("at most {} BNB per buy", crate::auth::js_number(ctx.num("maxTradeBnb")))));
                            }
                            if trades_today(&ctx.agent.id).await? as f64 >= ctx.num("maxTradesPerDay") {
                                return Err(Error::forbidden(format!("the daily limit of {} trades is reached", crate::auth::js_number(ctx.num("maxTradesPerDay")))));
                            }
                            let wei = evm::to_wei(amount);
                            let budget = spendable(&ctx.agent, &ctx.installed, "trader").await?;
                            if wei > budget {
                                return Err(Error::invalid(format!("the trading budget has {} BNB", alloy::primitives::utils::format_ether(budget))));
                            }
                            let venue = pick_venue(&token, &ctx.list("venues")).await?;
                            venue_used = venue.clone();
                            let slippage_bps = (ctx.num("maxSlippagePct") * 100.0).round() as i64;
                            let account = alloy::primitives::Address::parse_checksummed(&ctx.agent.evm_address, None).map(|a| a.to_string()).unwrap_or_else(|_| evm::parse_address(&ctx.agent.evm_address).map(|a| a.to_string()).unwrap_or_default());
                            let amt = crate::auth::js_number(amount);
                            let plan_v = match venue.as_str() {
                                "fourmeme" => mcp(&venue, "fourmeme_build_buy", json!({ "token": token, "amount": amt, "account": account, "slippage_bps": slippage_bps })).await?,
                                "flap" => mcp(&venue, "flap_build_swap", json!({ "token": token, "side": "buy", "amount": amt, "account": account, "slippage_bps": slippage_bps })).await?,
                                _ => mcp(&venue, &format!("{venue}_build_swap"), json!({ "token_in": "BNB", "token_out": token, "amount_in": amt, "account": account, "slippage_bps": slippage_bps })).await?,
                            };
                            let plan: Plan = serde_json::from_value(plan_v).map_err(|e| Error::internal(format!("bad plan: {e}")))?;
                            check_plan(&plan, &venue, &account, wei)?;
                            if plan.simulation.as_ref().is_some_and(|s| s["status"] == "reverted") {
                                return Err(Error::invalid("the trade fails in simulation"));
                            }
                            // Reserve the budget first so concurrent sessions can't double-spend it.
                            adjust_budget(&ctx.agent.id, "trader", -I256::from_raw(wei)).await?;
                            let hashes = match execute(&ctx, &plan).await {
                                Ok(h) => h,
                                Err(e) => {
                                    adjust_budget(&ctx.agent.id, "trader", I256::from_raw(wei)).await?;
                                    return Err(e);
                                }
                            };
                            Ok(json!({ "venue": venue, "txs": hashes, "got": plan.quote["tokenOut"], "min": plan.quote["minAmountOut"] }))
                        }
                        .await;
                        match r {
                            Ok(result) => {
                                let _ = record_trade(&ctx.agent.id, "trader", &venue_used, &token, "buy", amount, "BNB", None, None, &rationale, &result, true).await;
                                let mut out = json!({ "ok": true });
                                for (k, val) in result.as_object().unwrap() {
                                    out[k] = val.clone();
                                }
                                json_text(out)
                            }
                            Err(e) => {
                                let _ = record_trade(&ctx.agent.id, "trader", "pancakeswap", &token, "buy", amount, "BNB", None, None, &rationale, &json!({ "error": e.message }), false).await;
                                Ok(Value::String(format!("Buy refused or failed: {}", e.message)))
                            }
                        }
                    }
                },
            ),
            Tool::new(
                "sell_token",
                "Sell a token the agent holds for BNB at its current venue. The BNB goes back into the trading budget.",
                schema(&[
                    ("token", json!({ "type": "string", "description": "Token address" }), true),
                    ("amount", json!({ "type": "string", "description": "Tokens to sell, in whole units" }), true),
                    ("rationale", json!({ "type": "string", "minLength": 10, "maxLength": 500, "description": "Why, for the public trade log" }), true),
                ]),
                move |v| {
                    let ctx = c2.clone();
                    async move {
                        let token = v["token"].as_str().unwrap_or("").to_string();
                        let amount = v["amount"].as_str().unwrap_or("").to_string();
                        let rationale = v["rationale"].as_str().unwrap_or("").to_string();
                        let size: f64 = amount.parse().unwrap_or(f64::NAN);
                        let mut venue_used = "pancakeswap".to_string();
                        let r: Result<Value> = async {
                            if !evm::is_address(&token) {
                                return Err(Error::invalid("token must be an address"));
                            }
                            if ctx.agent.token_address.as_deref().is_some_and(|t| t.eq_ignore_ascii_case(&token)) {
                                return Err(Error::forbidden("the agent never sells its own token"));
                            }
                            if trades_today(&ctx.agent.id).await? as f64 >= ctx.num("maxTradesPerDay") {
                                return Err(Error::forbidden(format!("the daily limit of {} trades is reached", crate::auth::js_number(ctx.num("maxTradesPerDay")))));
                            }
                            let venue = pick_venue(&token, &ctx.list("venues")).await?;
                            venue_used = venue.clone();
                            let slippage_bps = (ctx.num("maxSlippagePct") * 100.0).round() as i64;
                            let account = evm::parse_address(&ctx.agent.evm_address)?.to_string();
                            let plan_v = match venue.as_str() {
                                "fourmeme" => mcp(&venue, "fourmeme_build_sell", json!({ "token": token, "amount": amount, "account": account, "slippage_bps": slippage_bps })).await?,
                                "flap" => mcp(&venue, "flap_build_swap", json!({ "token": token, "side": "sell", "amount": amount, "account": account, "slippage_bps": slippage_bps })).await?,
                                _ => mcp(&venue, &format!("{venue}_build_swap"), json!({ "token_in": token, "token_out": "BNB", "amount_in": amount, "account": account, "slippage_bps": slippage_bps })).await?,
                            };
                            let plan: Plan = serde_json::from_value(plan_v).map_err(|e| Error::internal(format!("bad plan: {e}")))?;
                            check_plan(&plan, &venue, &account, U256::ZERO)?;
                            if plan.simulation.as_ref().is_some_and(|s| s["status"] == "reverted") {
                                return Err(Error::invalid("the trade fails in simulation"));
                            }
                            let before = evm::bnb_balance(&account).await?;
                            let hashes = execute(&ctx, &plan).await?;
                            let after = evm::bnb_balance(&account).await?;
                            // Net of gas: what the sale added to the wallet goes to the trading budget.
                            let gained = after.saturating_sub(before);
                            if gained > U256::ZERO {
                                adjust_budget(&ctx.agent.id, "trader", I256::from_raw(gained)).await?;
                            }
                            Ok(json!({ "venue": venue, "txs": hashes, "received": plan.quote["tokenOut"], "min": plan.quote["minAmountOut"], "bnbCredited": alloy::primitives::utils::format_ether(gained) }))
                        }
                        .await;
                        match r {
                            Ok(result) => {
                                let _ = record_trade(&ctx.agent.id, "trader", &venue_used, &token, "sell", size, "TOKEN", None, None, &rationale, &result, true).await;
                                let mut out = json!({ "ok": true });
                                for (k, val) in result.as_object().unwrap() {
                                    out[k] = val.clone();
                                }
                                json_text(out)
                            }
                            Err(e) => {
                                let _ = record_trade(&ctx.agent.id, "trader", "pancakeswap", &token, "sell", size, "TOKEN", None, None, &rationale, &json!({ "error": e.message }), false).await;
                                Ok(Value::String(format!("Sell refused or failed: {}", e.message)))
                            }
                        }
                    }
                },
            ),
        ]
    }
}
