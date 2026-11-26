//! Perps on Aster: the plugin's BNB budget is swapped to USDT on PancakeSwap (checked like any Trader
//! swap) and deposited into Aster's vault for the agent's own account. The agent's main wallet is the
//! Aster account; it approves a second Privy wallet (the API wallet) to trade perps only. Every
//! position gets a stop-loss on Aster itself when it opens; if the stop can't be placed, the position
//! is closed at once.

use std::sync::Arc;

use alloy::primitives::{I256, U256};
use alloy::sol_types::SolCall;
use async_trait::async_trait;
use serde_json::{json, Value};

use super::data::{fail, meta};
use super::trader::{check_plan, mcp, Plan};
use crate::ai::{schema, Tool, ToolSet};
use crate::aster::{aster_approve_api_wallet, aster_mark, aster_public, aster_signed, aster_symbols, aster_withdraw_fee, aster_withdraw_usdt, to_step, Params};
use crate::bliss::settings::{list, number, SettingField, Settings};
use crate::bliss::treasury::{adjust_budget, spendable};
use crate::bliss::{json_text, recent_trades, record_trade, Ctx, Plugin, PluginMeta};
use crate::db::db;
use crate::error::{Error, Result};
use crate::evm;
use crate::wallet_policy::aster::{USDT, VAULT};

const APPROVAL_DAYS: i64 = 180;

async fn patch_state(agent_id: &str, patch: Value) -> Result<()> {
    crate::bliss::merge_state(agent_id, "aster", patch).await
}

async fn fresh_state(agent_id: &str) -> Result<Value> {
    let s: Option<Value> = sqlx::query_scalar("select state from agent_plugins where agent_id = $1 and plugin_id = 'aster'").bind(agent_id).fetch_optional(db()).await?;
    Ok(s.unwrap_or(json!({})))
}

/// The agent's Aster API wallet, created and approved on first use (and re-approved before it expires).
async fn api_wallet(ctx: &Ctx) -> Result<crate::wallet_policy::guard::GuardedAsterWallet> {
    let mut state = fresh_state(&ctx.agent.id).await?;
    if state["asterSigner"].is_null() {
        let meta = crate::wallets::create_aster_signer_wallet(&ctx.agent.id).await?;
        let mut signer = serde_json::to_value(&meta)?;
        signer["address"] = meta.address().to_lowercase().into();
        // Saved before approving: the wallet policy only lets the main wallet approve this address.
        patch_state(&ctx.agent.id, json!({ "asterSigner": signer })).await?;
        state["asterSigner"] = signer;
    }
    let signer = state["asterSigner"].clone();
    let now = chrono::Utc::now().timestamp_millis();
    if state["approvedUntil"].as_i64().is_none_or(|u| u < now + 7 * 86_400_000) {
        let until = now + APPROVAL_DAYS * 86_400_000;
        let main = crate::wallets::agent_wallet(&ctx.agent.id, &ctx.agent.evm_wallet)?;
        let name = crate::http::clip(&format!("orvyn-{}", ctx.agent.handle.clone().unwrap_or_else(|| ctx.agent.id.clone())), 32);
        aster_approve_api_wallet(&main, signer["address"].as_str().unwrap_or(""), until, &name).await?;
        patch_state(&ctx.agent.id, json!({ "approvedUntil": until })).await?;
    }
    crate::wallets::aster_wallet(&ctx.agent.id, &signer)
}

async fn signed(ctx: &Ctx, method: reqwest::Method, path: &str, params: Params) -> Result<Value> {
    let api = api_wallet(ctx).await?;
    aster_signed(method, path, params, &ctx.agent.evm_address, &api).await
}

fn p(pairs: &[(&str, String)]) -> Params {
    pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
}

fn n(v: &Value) -> f64 {
    v.as_str().and_then(|s| s.parse().ok()).or_else(|| v.as_f64()).unwrap_or(0.0)
}

async fn account(ctx: &Ctx) -> Result<Value> {
    let balances = signed(ctx, reqwest::Method::GET, "/fapi/v3/balance", vec![]).await?;
    let positions = signed(ctx, reqwest::Method::GET, "/fapi/v3/positionRisk", vec![]).await?;
    let usdt = balances.as_array().and_then(|a| a.iter().find(|b| b["asset"] == "USDT")).cloned().unwrap_or(json!({}));
    let open: Vec<Value> = positions.as_array().cloned().unwrap_or_default().into_iter().filter(|p| n(&p["positionAmt"]) != 0.0).collect();
    Ok(json!({ "usdt": { "balance": n(&usdt["balance"]), "available": n(&usdt["availableBalance"]) }, "positions": open }))
}

async fn trades_today(agent_id: &str) -> Result<i64> {
    Ok(sqlx::query_scalar("select count(*) from trades where agent_id = $1 and plugin_id = 'aster' and ok = true and created_at >= now() - interval '1 day'").bind(agent_id).fetch_one(db()).await?)
}

pub struct Perps(PluginMeta);

pub fn perps() -> Arc<dyn Plugin> {
    Arc::new(Perps(meta(
        "aster",
        "Perps on Aster",
        "capability",
        "Trades perpetual futures on Aster with leverage, a stop-loss on every position, and your limits.",
        "The agent funds an Aster futures account from this plugin's share of tax income (BNB swapped to USDT on PancakeSwap), then opens and closes long or short positions on the markets you allow. Every position gets a stop-loss on Aster itself the moment it opens. The agent's Aster API key is a separate Privy wallet that can trade perps but never withdraw; withdrawals only return USDT to the agent's own wallet. Leverage, position size, margin and trades per day stay within your limits, and Orvyn's wallet policy caps leverage and order size again before anything is signed.",
        &["trade", "spend-bnb", "read-market-data", "third-party"],
    )))
}

fn js(x: f64) -> String {
    crate::auth::js_number(x)
}

#[async_trait]
impl Plugin for Perps {
    fn meta(&self) -> &PluginMeta {
        &self.0
    }
    fn settings(&self) -> Vec<SettingField> {
        vec![
            number("feeSharePct", "Share of tax income for perps", None, 20.0, 0.0, 100.0, Some(1.0), Some("%")),
            list("symbols", "Markets", Some("Aster perp symbols, e.g. BTCUSDT"), vec!["BTCUSDT", "ETHUSDT", "BNBUSDT"], 10, true),
            number("maxLeverage", "Max leverage", None, 3.0, 1.0, 10.0, Some(1.0), Some("x")),
            number("maxPositionUsd", "Max position size", Some("Notional per market"), 100.0, 10.0, 5000.0, Some(10.0), Some("USD")),
            number("maxMarginUsd", "Max USDT in Aster", None, 200.0, 10.0, 10000.0, Some(10.0), Some("USD")),
            number("stopLossPct", "Widest stop-loss", Some("Every position gets a stop at most this far from entry"), 10.0, 1.0, 50.0, Some(0.5), Some("%")),
            number("maxTradesPerDay", "Max trades per day", None, 10.0, 1.0, 50.0, Some(1.0), None),
        ]
    }
    fn fee_share(&self, c: &Settings) -> Option<f64> {
        c.get("feeSharePct").and_then(Value::as_f64)
    }
    async fn before_uninstall(&self, ctx: &Ctx) -> Option<String> {
        if ctx.state["asterSigner"].is_null() {
            return None;
        }
        match account(ctx).await {
            Ok(a) => {
                if a["positions"].as_array().is_some_and(|p| !p.is_empty()) {
                    return Some("Close the agent's Aster positions first.".into());
                }
                let bal = a["usdt"]["balance"].as_f64().unwrap_or(0.0);
                (bal > 1.0).then(|| format!("The agent has {bal:.2} USDT on Aster; withdraw it first."))
            }
            Err(e) => Some(format!("Couldn't check the Aster account: {}", e.message)),
        }
    }
    async fn knowledge(&self, _ctx: &Ctx) -> Option<String> {
        Some("Aster is a perpetual futures exchange. A long profits when the price rises, a short when it falls; leverage multiplies both gains and losses, and a position is liquidated if losses eat its margin. Funding is paid between longs and shorts every few hours. Prefer small positions, low leverage and tight stops; don't add to losing positions.".into())
    }
    fn mandate(&self, ctx: &Ctx) -> Option<String> {
        Some(format!(
            "Perps: only {}; at most {}x leverage, ${} per market, ${} in Aster, {} trades a day; every position needs a stop-loss within {}%.",
            ctx.list("symbols").join(", "),
            js(ctx.num("maxLeverage")),
            js(ctx.num("maxPositionUsd")),
            js(ctx.num("maxMarginUsd")),
            js(ctx.num("maxTradesPerDay")),
            js(ctx.num("stopLossPct"))
        ))
    }
    fn read_tools(&self, ctx: &Ctx) -> ToolSet {
        let (c1, a2) = (ctx.clone(), ctx.agent.id.clone());
        vec![
            Tool::new("perp_market", "Aster perp market data for a symbol: mark and index price, funding rate, 24h change and volume.", schema(&[("symbol", json!({ "type": "string", "description": "e.g. BTCUSDT" }), true)]), |v| async move {
                let s = v["symbol"].as_str().unwrap_or("").to_uppercase();
                let r: Result<Value> = async {
                    let mut mark = aster_mark(&s).await?;
                    let day = aster_public("/fapi/v3/ticker/24hr", &vec![("symbol".into(), s.clone())]).await?;
                    mark["symbol"] = s.clone().into();
                    mark["change24hPct"] = n(&day["priceChangePercent"]).into();
                    mark["high24h"] = n(&day["highPrice"]).into();
                    mark["low24h"] = n(&day["lowPrice"]).into();
                    mark["volume24hUsd"] = n(&day["quoteVolume"]).into();
                    Ok(mark)
                }
                .await;
                match r {
                    Ok(m) => json_text(m),
                    Err(e) => Ok(Value::String(format!("Market data failed: {}", e.message))),
                }
            }),
            Tool::new("perp_account", "The agent's Aster account: USDT margin and open positions.", schema(&[]), move |_| {
                let ctx = c1.clone();
                async move {
                    if ctx.state["asterSigner"].is_null() {
                        return json_text(json!({ "funded": false, "note": "No Aster account yet; fund it first." }));
                    }
                    match account(&ctx).await {
                        Ok(a) => json_text(a),
                        Err(e) => Ok(Value::String(format!("Account read failed: {}", e.message))),
                    }
                }
            }),
            Tool::new("recent_perp_trades", "The agent's recent Aster trades.", schema(&[]), move |_| {
                let a = a2.clone();
                async move { json_text(serde_json::to_value(recent_trades(&a, "aster", 10).await.map_err(fail)?).unwrap()) }
            }),
        ]
    }
    fn has_actions(&self) -> bool {
        true
    }
    fn action_tools(&self, ctx: &Ctx) -> ToolSet {
        let (c1, c2, c3, c4) = (ctx.clone(), ctx.clone(), ctx.clone(), ctx.clone());
        let rationale = json!({ "type": "string", "minLength": 10, "maxLength": 500 });
        vec![
            Tool::new(
                "fund_perps",
                "Move BNB from the perps budget into the agent's Aster account: swapped to USDT on PancakeSwap, then deposited.",
                schema(&[("amount_bnb", json!({ "type": "number", "exclusiveMinimum": 0 }), true), ("rationale", rationale.clone(), true)]),
                move |v| {
                    let ctx = c1.clone();
                    async move {
                        let amount = v["amount_bnb"].as_f64().unwrap_or(0.0);
                        let why = v["rationale"].as_str().unwrap_or("").to_string();
                        let r = fund(&ctx, amount).await;
                        let (ok, result) = match &r {
                            Ok(x) => (true, x.clone()),
                            Err(e) => (false, json!({ "error": e.message })),
                        };
                        let _ = record_trade(&ctx.agent.id, "aster", "aster", "USDT", "buy", amount, "BNB", None, None, &why, &result, ok).await;
                        match r {
                            Ok(mut x) => {
                                x["ok"] = true.into();
                                x["note"] = "Aster credits deposits after the transaction confirms.".into();
                                json_text(x)
                            }
                            Err(e) => Ok(Value::String(format!("Funding failed: {}", e.message))),
                        }
                    }
                },
            ),
            Tool::new(
                "open_perp",
                "Open (or add to) a long or short perp position on Aster at market, with a stop-loss placed on Aster immediately.",
                schema(&[
                    ("symbol", json!({ "type": "string" }), true),
                    ("side", json!({ "type": "string", "enum": ["long", "short"] }), true),
                    ("size_usd", json!({ "type": "number", "exclusiveMinimum": 0, "description": "Position notional in USD (margin = size / leverage)" }), true),
                    ("leverage", json!({ "type": "integer", "minimum": 1 }), true),
                    ("stop_loss_pct", json!({ "type": "number", "exclusiveMinimum": 0, "description": "Stop distance from the mark price, in %" }), true),
                    ("take_profit_pct", json!({ "type": "number", "exclusiveMinimum": 0 }), false),
                    ("rationale", rationale.clone(), true),
                ]),
                move |v| {
                    let ctx = c2.clone();
                    async move {
                        let symbol = v["symbol"].as_str().unwrap_or("").to_uppercase();
                        let side = v["side"].as_str().unwrap_or("").to_string();
                        let size = v["size_usd"].as_f64().unwrap_or(0.0);
                        let lev = v["leverage"].as_i64().unwrap_or(1);
                        let why = v["rationale"].as_str().unwrap_or("").to_string();
                        let r = open(&ctx, &symbol, &side, size, lev, v["stop_loss_pct"].as_f64().unwrap_or(0.0), v["take_profit_pct"].as_f64()).await;
                        let (ok, result, price) = match &r {
                            Ok(x) => (true, x.clone(), x["entry"].as_f64()),
                            Err(e) => (false, json!({ "error": e.message }), None),
                        };
                        let _ = record_trade(&ctx.agent.id, "aster", "aster", &symbol, &side, size, "USD", Some(lev as i32), price, &why, &result, ok).await;
                        match r {
                            Ok(mut x) => {
                                x["ok"] = true.into();
                                json_text(x)
                            }
                            Err(e) => Ok(Value::String(format!("Order refused or failed: {}", e.message))),
                        }
                    }
                },
            ),
            Tool::new(
                "close_perp",
                "Close the agent's whole position on an Aster market at market price, and cancel its stop and take-profit orders.",
                schema(&[("symbol", json!({ "type": "string" }), true), ("rationale", rationale.clone(), true)]),
                move |v| {
                    let ctx = c3.clone();
                    async move {
                        let symbol = v["symbol"].as_str().unwrap_or("").to_uppercase();
                        let why = v["rationale"].as_str().unwrap_or("").to_string();
                        let r: Result<Option<(Value, f64, f64)>> = async {
                            let a = account(&ctx).await?;
                            let Some(pos) = a["positions"].as_array().and_then(|ps| ps.iter().find(|x| x["symbol"] == symbol.as_str())).cloned() else { return Ok(None) };
                            let amt = n(&pos["positionAmt"]);
                            let order = signed(
                                &ctx,
                                reqwest::Method::POST,
                                "/fapi/v3/order",
                                p(&[("symbol", symbol.clone()), ("side", if amt > 0.0 { "SELL" } else { "BUY" }.into()), ("type", "MARKET".into()), ("quantity", js(amt.abs())), ("reduceOnly", "true".into()), ("newOrderRespType", "RESULT".into())]),
                            )
                            .await?;
                            let _ = signed(&ctx, reqwest::Method::DELETE, "/fapi/v3/allOpenOrders", p(&[("symbol", symbol.clone())])).await;
                            let exit = n(&order["avgPrice"]);
                            Ok(Some((json!({ "orderId": order["orderId"], "closed": pos["positionAmt"], "exit": exit, "pnlUsd": n(&pos["unRealizedProfit"]) }), amt.abs() * n(&pos["markPrice"]), exit)))
                        }
                        .await;
                        match r {
                            Ok(None) => Ok(Value::String("No open position on that market.".into())),
                            Ok(Some((mut result, size, exit))) => {
                                let _ = record_trade(&ctx.agent.id, "aster", "aster", &symbol, "close", size, "USD", None, Some(exit), &why, &result, true).await;
                                result["ok"] = true.into();
                                json_text(result)
                            }
                            Err(e) => {
                                let _ = record_trade(&ctx.agent.id, "aster", "aster", &symbol, "close", 0.0, "USD", None, None, &why, &json!({ "error": e.message }), false).await;
                                Ok(Value::String(format!("Close failed: {}", e.message)))
                            }
                        }
                    }
                },
            ),
            Tool::new(
                "withdraw_perps",
                "Withdraw USDT from the agent's Aster account back to its own wallet on BNB Chain (Aster charges a small fee).",
                schema(&[("amount_usdt", json!({ "type": "number", "exclusiveMinimum": 0 }), true), ("rationale", rationale, true)]),
                move |v| {
                    let ctx = c4.clone();
                    async move {
                        let amount = v["amount_usdt"].as_f64().unwrap_or(0.0);
                        let why = v["rationale"].as_str().unwrap_or("").to_string();
                        let r: Result<Value> = async {
                            let a = account(&ctx).await?;
                            let available = a["usdt"]["available"].as_f64().unwrap_or(0.0);
                            if amount > available {
                                return Err(Error::invalid(format!("${available:.2} is available to withdraw")));
                            }
                            let fee = aster_withdraw_fee().await?;
                            let shown = format!("{amount:.6}").trim_end_matches('0').trim_end_matches('.').to_string();
                            let main = crate::wallets::agent_wallet(&ctx.agent.id, &ctx.agent.evm_wallet)?;
                            let api = api_wallet(&ctx).await?;
                            let r = aster_withdraw_usdt(&main, &api, &shown, &fee).await?;
                            let state = fresh_state(&ctx.agent.id).await?;
                            let net = (n(&state["netDepositedUsd"]) - amount).max(0.0);
                            patch_state(&ctx.agent.id, json!({ "netDepositedUsd": js(net) })).await?;
                            let mut logged = r.clone();
                            logged["fee"] = fee.clone().into();
                            let _ = record_trade(&ctx.agent.id, "aster", "aster", "USDT", "sell", amount, "USD", None, None, &why, &logged, true).await;
                            Ok(json!({ "ok": true, "withdrawId": r["withdrawId"], "fee": fee, "note": "USDT arrives in the agent's wallet once Aster processes it." }))
                        }
                        .await;
                        match r {
                            Ok(x) => json_text(x),
                            Err(e) => {
                                let _ = record_trade(&ctx.agent.id, "aster", "aster", "USDT", "sell", amount, "USD", None, None, &why, &json!({ "error": e.message }), false).await;
                                Ok(Value::String(format!("Withdrawal failed: {}", e.message)))
                            }
                        }
                    }
                },
            ),
        ]
    }
}

async fn fund(ctx: &Ctx, amount_bnb: f64) -> Result<Value> {
    let wei = evm::to_wei(amount_bnb);
    let budget = spendable(&ctx.agent, &ctx.installed, "aster").await?;
    if wei > budget {
        return Err(Error::invalid(format!("the perps budget has {} BNB", alloy::primitives::utils::format_ether(budget))));
    }
    let state = fresh_state(&ctx.agent.id).await?;
    let account = evm::parse_address(&ctx.agent.evm_address)?.to_string();
    let plan_v = mcp("pancakeswap", "pancakeswap_build_swap", json!({ "token_in": "BNB", "token_out": USDT, "amount_in": js(amount_bnb), "account": account, "slippage_bps": 100 })).await?;
    let plan: Plan = serde_json::from_value(plan_v).map_err(|e| Error::internal(format!("bad plan: {e}")))?;
    check_plan(&plan, "pancakeswap", &account, wei)?;
    let quoted = n(&plan.quote["tokenOut"]["amount"]);
    if n(&state["netDepositedUsd"]) + quoted > ctx.num("maxMarginUsd") {
        return Err(Error::forbidden(format!("that would put more than ${} into Aster", js(ctx.num("maxMarginUsd")))));
    }
    adjust_budget(&ctx.agent.id, "aster", -I256::from_raw(wei)).await?;
    let before = evm::erc20_balance(USDT, &account).await?;
    let mut txs = vec![];
    for c in &plan.calls {
        let sent = crate::wallets::agent_send(&ctx.agent.id, &ctx.agent.evm_wallet, evm::parse_address(&c.to)?, c.data.parse().map_err(|_| Error::invalid("bad data"))?, U256::from_str_radix(&c.value, 10).unwrap_or_default()).await;
        match sent {
            Ok(h) => txs.push(format!("{h:#x}")),
            Err(e) => {
                adjust_budget(&ctx.agent.id, "aster", I256::from_raw(wei)).await?;
                return Err(e);
            }
        }
    }
    let got = evm::erc20_balance(USDT, &account).await?.saturating_sub(before);
    if got == U256::ZERO {
        return Err(Error::internal("the swap returned no USDT"));
    }
    let (usdt, vault) = (evm::parse_address(USDT)?, evm::parse_address(VAULT)?);
    let approve = evm::IERC20::approveCall { spender: vault, amount: got }.abi_encode();
    txs.push(format!("{:#x}", crate::wallets::agent_send(&ctx.agent.id, &ctx.agent.evm_wallet, usdt, approve.into(), U256::ZERO).await?));
    let deposit = crate::wallet_policy::aster::AsterVault::depositForCall { currency: usdt, forAddress: evm::parse_address(&account)?, amount: got, broker: U256::ZERO }.abi_encode();
    txs.push(format!("{:#x}", crate::wallets::agent_send(&ctx.agent.id, &ctx.agent.evm_wallet, vault, deposit.into(), U256::ZERO).await?));
    let usd = evm::from_units(got, 18);
    patch_state(&ctx.agent.id, json!({ "netDepositedUsd": js(n(&state["netDepositedUsd"]) + usd) })).await?;
    // Set up the API wallet now, so trading can start right away.
    api_wallet(ctx).await?;
    Ok(json!({ "depositedUsdt": alloy::primitives::utils::format_units(got, 18).unwrap_or_default(), "txs": txs }))
}

async fn open(ctx: &Ctx, symbol: &str, side: &str, size_usd: f64, leverage: i64, stop_pct: f64, tp_pct: Option<f64>) -> Result<Value> {
    let symbols = ctx.list("symbols");
    if !symbols.iter().any(|s| s == symbol) {
        return Err(Error::forbidden(format!("{symbol} isn't one of the allowed markets ({})", symbols.join(", "))));
    }
    if leverage as f64 > ctx.num("maxLeverage") {
        return Err(Error::forbidden(format!("at most {}x leverage", js(ctx.num("maxLeverage")))));
    }
    if stop_pct > ctx.num("stopLossPct") {
        return Err(Error::forbidden(format!("the stop-loss may be at most {}% away", js(ctx.num("stopLossPct")))));
    }
    if trades_today(&ctx.agent.id).await? as f64 >= ctx.num("maxTradesPerDay") {
        return Err(Error::forbidden(format!("the daily limit of {} trades is reached", js(ctx.num("maxTradesPerDay")))));
    }
    let info = aster_symbols().await?.get(symbol).cloned();
    let mark = aster_mark(symbol).await?["markPrice"].as_f64().unwrap_or(0.0);
    let acct = account(ctx).await?;
    let Some(info) = info.filter(|i| i.status == "TRADING") else { return Err(Error::invalid(format!("{symbol} isn't trading on Aster"))) };
    let existing = acct["positions"].as_array().and_then(|ps| ps.iter().find(|x| x["symbol"] == symbol)).cloned();
    let existing_usd = existing.as_ref().map(|e| n(&e["positionAmt"]).abs() * mark).unwrap_or(0.0);
    if existing.as_ref().is_some_and(|e| (n(&e["positionAmt"]) > 0.0) != (side == "long")) {
        return Err(Error::conflict("close the opposite position first"));
    }
    if existing_usd + size_usd > ctx.num("maxPositionUsd") {
        return Err(Error::forbidden(format!("at most ${} per market (already ${existing_usd:.2})", js(ctx.num("maxPositionUsd")))));
    }
    let available = acct["usdt"]["available"].as_f64().unwrap_or(0.0);
    if size_usd / leverage as f64 > available {
        return Err(Error::invalid(format!("needs ${:.2} margin; ${available:.2} available", size_usd / leverage as f64)));
    }
    let quantity = to_step(size_usd / mark, info.step_size);
    let q: f64 = quantity.parse().unwrap_or(0.0);
    if q < info.min_qty || q * mark < info.min_notional {
        return Err(Error::invalid(format!("the order is below Aster's minimum (${})", js(info.min_notional))));
    }
    signed(ctx, reqwest::Method::POST, "/fapi/v3/leverage", p(&[("symbol", symbol.into()), ("leverage", leverage.to_string())])).await?;
    let (side_s, exit) = if side == "long" { ("BUY", "SELL") } else { ("SELL", "BUY") };
    let order = signed(ctx, reqwest::Method::POST, "/fapi/v3/order", p(&[("symbol", symbol.into()), ("side", side_s.into()), ("type", "MARKET".into()), ("quantity", quantity.clone()), ("newOrderRespType", "RESULT".into())])).await?;
    let entry = Some(n(&order["avgPrice"])).filter(|e| *e != 0.0).unwrap_or(mark);
    let dir = if side == "long" { -1.0 } else { 1.0 };
    let stop_price = to_step(entry * (1.0 + dir * stop_pct / 100.0), info.tick_size);
    let executed = order["executedQty"].as_str().filter(|q| !q.is_empty()).map(String::from).unwrap_or_else(|| quantity.clone());
    let stop = signed(ctx, reqwest::Method::POST, "/fapi/v3/order", p(&[("symbol", symbol.into()), ("side", exit.into()), ("type", "STOP_MARKET".into()), ("stopPrice", stop_price.clone()), ("closePosition", "true".into()), ("workingType", "MARK_PRICE".into())])).await;
    if let Err(e) = stop {
        // No stop, no position.
        let _ = signed(ctx, reqwest::Method::POST, "/fapi/v3/order", p(&[("symbol", symbol.into()), ("side", exit.into()), ("type", "MARKET".into()), ("quantity", executed), ("reduceOnly", "true".into())])).await;
        return Err(Error::internal(format!("the stop-loss couldn't be placed, so the position was closed: {}", e.message)));
    }
    let mut take_profit: Option<String> = None;
    if let Some(tp) = tp_pct {
        let price = to_step(entry * (1.0 - dir * tp / 100.0), info.tick_size);
        if signed(ctx, reqwest::Method::POST, "/fapi/v3/order", p(&[("symbol", symbol.into()), ("side", exit.into()), ("type", "TAKE_PROFIT_MARKET".into()), ("stopPrice", price.clone()), ("closePosition", "true".into()), ("workingType", "MARK_PRICE".into())])).await.is_ok() {
            take_profit = Some(price);
        }
    }
    Ok(json!({ "orderId": order["orderId"], "quantity": order["executedQty"].as_str().filter(|q| !q.is_empty()).unwrap_or(&quantity), "entry": entry, "stopPrice": stop_price, "takeProfit": take_profit }))
}
