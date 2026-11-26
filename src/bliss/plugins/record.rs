//! Track Record (calls graded by code) and Venus Lending (supply-only).

use std::sync::Arc;

use alloy::primitives::{I256, U256};
use alloy::providers::Provider;
use alloy::sol;
use alloy::sol_types::SolCall;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::data::{fail, meta};
use crate::ai::{schema, Tool, ToolSet};
use crate::bliss::prices::{parse_ref, price_of, subject_ref, Subject};
use crate::bliss::settings::{list, number, SettingField, Settings};
use crate::bliss::treasury::{adjust_budget, spendable};
use crate::bliss::{input, json_text, record_trade, round, Ctx, Installed, JobFuture, Plugin, PluginJob, PluginMeta};
use crate::db::{db, log_agent, AgentCall, AgentLogKind};
use crate::error::{Error, Result};
use crate::evm;
use crate::tiers::Tier;

const MAX_OPEN: i64 = 20;
const MAX_PER_DAY: i64 = 30;
const MOVE_PCT: f64 = 1.0;
const FLAT_PCT: f64 = 2.0;
const VOID_AFTER_MS: i64 = 48 * 3_600_000;

pub fn grade_move(direction: &str, move_pct: f64) -> &'static str {
    let hit = match direction {
        "flat" => move_pct.abs() < FLAT_PCT,
        "up" => move_pct >= MOVE_PCT,
        _ => move_pct <= -MOVE_PCT,
    };
    if hit { "hit" } else { "miss" }
}

/// Grade this agent's calls whose horizon has passed. Returns how many were graded.
pub async fn grade_due(agent_id: &str) -> Result<u64> {
    let now = chrono::Utc::now();
    let due: Vec<AgentCall> = sqlx::query_as("select * from agent_calls where agent_id = $1 and graded_at is null and due_at <= $2 limit 50").bind(agent_id).bind(now).fetch_all(db()).await?;
    let mut graded = 0;
    for c in due {
        let r = match price_of(&parse_ref(&c.subject_kind, &c.subject_ref)).await {
            Ok(p) => {
                let m = (p.price_usd - c.entry_price) / c.entry_price * 100.0;
                sqlx::query("update agent_calls set graded_at = $2, exit_price = $3, move_pct = $4, outcome = $5 where id = $1 and graded_at is null")
                    .bind(c.id)
                    .bind(now)
                    .bind(p.price_usd)
                    .bind(round(m, 3))
                    .bind(grade_move(&c.direction, m))
                    .execute(db())
                    .await?
            }
            Err(_) => {
                if now.timestamp_millis() - c.due_at.timestamp_millis() < VOID_AFTER_MS {
                    continue;
                }
                sqlx::query("update agent_calls set graded_at = $2, outcome = 'void' where id = $1 and graded_at is null").bind(c.id).bind(now).execute(db()).await?
            }
        };
        graded += r.rows_affected();
    }
    Ok(graded)
}

/// The agent's record, from its graded calls (also shown on its public page).
pub async fn track_record(agent_id: &str, limit: i64) -> Result<Value> {
    let rows: Vec<AgentCall> = sqlx::query_as("select * from agent_calls where agent_id = $1 and outcome in ('hit', 'miss') order by graded_at desc limit $2").bind(agent_id).bind(limit).fetch_all(db()).await?;
    let open: i64 = sqlx::query_scalar("select count(*) from agent_calls where agent_id = $1 and graded_at is null").bind(agent_id).fetch_one(db()).await?;
    let hit = |r: &AgentCall| r.outcome.as_deref() == Some("hit");
    let hits = rows.iter().filter(|r| hit(r)).count();
    let n = rows.len();
    let pct = |a: usize, b: usize| if b == 0 { Value::Null } else { json!(round(a as f64 / b as f64 * 100.0, 1)) };
    let brier = (n > 0).then(|| rows.iter().map(|r| (r.confidence - if hit(r) { 1.0 } else { 0.0 }).powi(2)).sum::<f64>() / n as f64);
    let by_direction: Vec<Value> = ["up", "down", "flat"]
        .iter()
        .map(|d| {
            let set: Vec<&AgentCall> = rows.iter().filter(|r| r.direction == *d).collect();
            json!({ "direction": d, "calls": set.len(), "hitRatePct": pct(set.iter().filter(|r| hit(r)).count(), set.len()) })
        })
        .collect();
    let confident: Vec<&AgentCall> = rows.iter().filter(|r| r.confidence >= 0.75).collect();
    let recent: Vec<Value> = rows
        .iter()
        .take(8)
        .map(|r| json!({ "call": format!("{} {} in {}h at {}%", r.label, r.direction, r.horizon_hours, (r.confidence * 100.0).round()), "outcome": r.outcome, "movePct": r.move_pct, "thesis": crate::http::clip(&r.thesis, 160) }))
        .collect();
    Ok(json!({
        "graded": n, "open": open, "hitRatePct": pct(hits, n), "brier": brier.map(|b| round(b, 3)),
        "whenConfidentHitRatePct": pct(confident.iter().filter(|r| hit(r)).count(), confident.len()),
        "byDirection": by_direction, "recent": recent,
    }))
}

fn describe_record(r: &Value) -> String {
    let js = |v: &Value| v.as_number().map(crate::environment::js_number_str).unwrap_or_else(|| "null".into());
    if r["graded"].as_u64().unwrap_or(0) == 0 {
        return format!("No graded calls yet ({} open).", r["open"]);
    }
    let mut lines = vec![format!("Graded calls: {} ({} open). Hit rate {}%. Brier {} (0.25 = coin flip at 50%; lower is better).", r["graded"], r["open"], js(&r["hitRatePct"]), js(&r["brier"]))];
    if !r["whenConfidentHitRatePct"].is_null() {
        lines.push(format!("When you said 75%+ you were right {}% of the time.", js(&r["whenConfidentHitRatePct"])));
    }
    for d in r["byDirection"].as_array().unwrap() {
        if d["calls"].as_u64().unwrap_or(0) > 0 {
            lines.push(format!("- {}: {} calls, {}% hit", d["direction"].as_str().unwrap(), d["calls"], js(&d["hitRatePct"])));
        }
    }
    let misses: Vec<String> = r["recent"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["outcome"] == "miss")
        .take(3)
        .map(|m| format!("- {}, moved {}%: \"{}\"", m["call"].as_str().unwrap_or(""), js(&m["movePct"]), m["thesis"].as_str().unwrap_or("")))
        .collect();
    if !misses.is_empty() {
        lines.push(format!("Recent misses (don't repeat the reasoning without new evidence):\n{}", misses.join("\n")));
    }
    lines.join("\n")
}

pub struct TrackRecord(PluginMeta);

pub fn track_record_plugin() -> Arc<dyn Plugin> {
    Arc::new(TrackRecord(meta(
        "track-record",
        "Track Record",
        "capability",
        "Every market call the agent makes is logged and graded by code against the real price: hit rate and calibration, public.",
        "The agent logs each view it forms (a token or coin, up, down or flat, over a horizon, with how sure it is and why). The entry price is taken at that moment; when the horizon ends, Orvyn prices it again and grades it. Hit rate, calibration (Brier score) and recent misses go back into the agent's instructions so it calibrates instead of repeating reasoning that failed, and the record is shown on its public page. The agent can't edit or delete a call.",
        &["read-market-data"],
    )))
}

#[derive(Deserialize)]
struct CallInput {
    token_address: Option<String>,
    chain: Option<String>,
    coin: Option<String>,
    direction: String,
    horizon_hours: i64,
    confidence: f64,
    thesis: String,
    evidence: Option<Vec<String>>,
}

async fn log_call(agent_id: &str, i: CallInput) -> Result<Value> {
    let open: i64 = sqlx::query_scalar("select count(*) from agent_calls where agent_id = $1 and graded_at is null").bind(agent_id).fetch_one(db()).await?;
    if open >= MAX_OPEN {
        return Ok(json!({ "logged": false, "reason": format!("{MAX_OPEN} calls are already open; wait for some to be graded") }));
    }
    let today: i64 = sqlx::query_scalar("select count(*) from agent_calls where agent_id = $1 and created_at >= now() - interval '1 day'").bind(agent_id).fetch_one(db()).await?;
    if today >= MAX_PER_DAY {
        return Ok(json!({ "logged": false, "reason": format!("at most {MAX_PER_DAY} calls a day") }));
    }
    let subject = match (&i.coin, &i.token_address) {
        (Some(c), _) => Subject::Coin { id: c.to_lowercase() },
        (None, Some(t)) => Subject::Token { chain: i.chain.clone().unwrap_or_else(|| "bsc".into()).to_lowercase(), address: t.clone() },
        _ => unreachable!(),
    };
    let price = match price_of(&subject).await {
        Ok(p) => p,
        Err(e) => return Ok(json!({ "logged": false, "reason": format!("couldn't price it now ({}); calls need a live price", e.message) })),
    };
    let kind = if matches!(subject, Subject::Coin { .. }) { "coin" } else { "token" };
    let due = chrono::Utc::now() + chrono::Duration::hours(i.horizon_hours);
    let id: i32 = sqlx::query_scalar(
        "insert into agent_calls (agent_id, subject_kind, subject_ref, label, direction, confidence, horizon_hours, thesis, evidence, entry_price, due_at) values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) returning id",
    )
    .bind(agent_id)
    .bind(kind)
    .bind(subject_ref(&subject))
    .bind(&price.label)
    .bind(&i.direction)
    .bind(i.confidence)
    .bind(i.horizon_hours as i32)
    .bind(&i.thesis)
    .bind(json!(i.evidence.unwrap_or_default()))
    .bind(price.price_usd)
    .bind(due)
    .fetch_one(db())
    .await?;
    Ok(json!({ "logged": true, "id": id, "entry_price_usd": price.price_usd, "graded_at": crate::public::iso(due) }))
}

fn grade_job(ctx: Ctx) -> JobFuture {
    Box::pin(async move { grade_due(&ctx.agent.id).await.map(|_| ()) })
}

#[async_trait]
impl Plugin for TrackRecord {
    fn meta(&self) -> &PluginMeta {
        &self.0
    }
    async fn knowledge(&self, ctx: &Ctx) -> Option<String> {
        let r = track_record(&ctx.agent.id, 200).await.ok()?;
        Some(format!("## Your track record\nWhen you form a directional view you'd act on or tell people about, log it with log_call first: the market, direction, horizon, your honest probability and the evidence. It's graded by code against the real price and shown publicly. Make fewer, better calls; say \"no clear view\" when the evidence is mixed.\n\n{}", describe_record(&r)))
    }
    fn read_tools(&self, ctx: &Ctx) -> ToolSet {
        let (a1, a2) = (ctx.agent.id.clone(), ctx.agent.id.clone());
        vec![
            Tool::new(
                "log_call",
                "Log a directional call before acting on it or sharing it: a token (by contract address) or a major coin, up/down/flat over a horizon, with your probability of being right and the evidence. It's priced now and graded automatically when the horizon ends. Calls can't be edited or deleted.",
                schema(&[
                    ("token_address", json!({ "type": "string", "pattern": "^0x[0-9a-fA-F]{40}$", "description": "Token contract address (for tokens)" }), false),
                    ("chain", json!({ "type": "string", "maxLength": 20, "description": "DexScreener chain id for token_address, e.g. bsc, ethereum, base, solana. Default bsc." }), false),
                    ("coin", json!({ "type": "string", "maxLength": 40, "description": "CoinGecko id for majors, e.g. bitcoin, ethereum, binancecoin" }), false),
                    ("direction", json!({ "type": "string", "enum": ["up", "down", "flat"] }), true),
                    ("horizon_hours", json!({ "type": "integer", "minimum": 1, "maximum": 720 }), true),
                    ("confidence", json!({ "type": "number", "minimum": 0.5, "maximum": 0.99, "description": "Your probability of being right" }), true),
                    ("thesis", json!({ "type": "string", "minLength": 20, "maxLength": 600, "description": "Why, in one or two sentences, citing the evidence" }), true),
                    ("evidence", json!({ "type": "array", "items": { "type": "string", "maxLength": 200 }, "maxItems": 6, "description": "The data points you relied on" }), false),
                ]),
                move |v| {
                    let a = a1.clone();
                    async move {
                        let i: CallInput = input(v)?;
                        if i.token_address.is_some() == i.coin.is_some() {
                            return Err("invalid input: give either token_address or coin".into());
                        }
                        let ok = ["up", "down", "flat"].contains(&i.direction.as_str()) && (1..=720).contains(&i.horizon_hours) && (0.5..=0.99).contains(&i.confidence) && (20..=600).contains(&i.thesis.chars().count());
                        if !ok || i.token_address.as_deref().is_some_and(|t| !evm::is_address(t)) {
                            return Err("invalid input".into());
                        }
                        json_text(log_call(&a, i).await.map_err(fail)?)
                    }
                },
            ),
            Tool::new("my_track_record", "Your graded calls so far: hit rate, calibration (Brier), by direction, and recent calls with their outcomes.", schema(&[]), move |_| {
                let a = a2.clone();
                async move { json_text(track_record(&a, 200).await.map_err(fail)?) }
            }),
        ]
    }
    fn jobs(&self) -> Vec<PluginJob> {
        vec![PluginJob { id: "grade", every: |_| 600.0, run: grade_job }]
    }
}

// ---- Venus ----

sol! {
    #[sol(rpc)]
    interface VenusRead {
        function supplyRatePerBlock() view returns (uint256);
        function exchangeRateStored() view returns (uint256);
        function balanceOf(address) view returns (uint256);
        function getCash() view returns (uint256);
        function totalBorrows() view returns (uint256);
    }
    interface VTokenW {
        function mint(uint256 mintAmount) returns (uint256);
        function redeem(uint256 redeemTokens) returns (uint256);
        function redeemUnderlying(uint256 redeemAmount) returns (uint256);
    }
    interface VBnbW {
        function mint() payable;
    }
}

fn market_of(asset: &str) -> &'static str {
    crate::wallet_policy::venus::MARKETS.iter().find(|m| m.symbol == asset).map(|m| m.vtoken).unwrap()
}
fn underlying_of(asset: &str) -> &'static str {
    crate::wallet_policy::venus::MARKETS.iter().find(|m| m.symbol == asset).and_then(|m| m.underlying).unwrap()
}

async fn blocks_per_year() -> Result<f64> {
    let p = evm::bsc();
    let latest = p.get_block_number().await.map_err(evm::rpc_error)?;
    let a = p.get_block_by_number(latest.into()).await.map_err(evm::rpc_error)?.map(|b| b.header.timestamp).unwrap_or(0);
    let b = p.get_block_by_number((latest - 2000).into()).await.map_err(evm::rpc_error)?.map(|b| b.header.timestamp).unwrap_or(0);
    Ok(365.0 * 24.0 * 3600.0 / ((a.saturating_sub(b)) as f64 / 2000.0))
}

fn units18(v: U256) -> f64 {
    evm::from_units(v, 18)
}

async fn venus_market(asset: &str, owner: &str, bpy: f64) -> Result<Value> {
    let c = VenusRead::new(evm::parse_address(market_of(asset))?, evm::bsc());
    let rate = c.supplyRatePerBlock().call().await.map_err(evm::rpc_error)?;
    let exch = c.exchangeRateStored().call().await.map_err(evm::rpc_error)?;
    let bal = c.balanceOf(evm::parse_address(owner)?).call().await.map_err(evm::rpc_error)?;
    let cash = c.getCash().call().await.map_err(evm::rpc_error)?;
    let borrows = c.totalBorrows().call().await.map_err(evm::rpc_error)?;
    let per_block = units18(rate);
    let supplied = bal * exch / U256::from(10u64).pow(U256::from(18u8));
    let total = units18(cash + borrows);
    let util = if total > 0.0 { units18(borrows) / total } else { 0.0 };
    Ok(json!({
        "asset": asset, "supply_apy_pct": round(((1.0 + per_block).powf(bpy) - 1.0) * 100.0, 2), "utilisation_pct": round(util * 100.0, 1),
        "available_to_withdraw": units18(cash), "supplied": units18(supplied),
    }))
}

pub struct Venus(PluginMeta);

pub fn venus() -> Arc<dyn Plugin> {
    Arc::new(Venus(meta(
        "venus",
        "Venus Lending",
        "capability",
        "Earn interest on idle USDT, USDC or BNB by supplying it to Venus, and take it back any time. Supply only: no borrowing.",
        "The agent supplies idle USDT, USDC or BNB to Venus, BNB Chain's largest lending market, and withdraws it when it needs it. It never borrows and never enables collateral, so a position can't be liquidated. The wallet policy only allows supplying and withdrawing on these three markets, with daily caps; withdrawals always return to the agent. Stablecoins come from the agent's wallet; BNB from this plugin's share of income.",
        &["spend-bnb", "read-market-data"],
    )))
}

fn assets(c: &Settings) -> Vec<String> {
    c.get("assets").and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_uppercase)).collect()).unwrap_or_default()
}
fn cfg_num(c: &Settings, k: &str) -> f64 {
    c.get(k).and_then(Value::as_f64).unwrap_or(0.0)
}

#[async_trait]
impl Plugin for Venus {
    fn meta(&self) -> &PluginMeta {
        &self.0
    }
    fn settings(&self) -> Vec<SettingField> {
        vec![
            list("assets", "Assets", Some("USDT, USDC, BNB"), vec!["USDT", "USDC"], 3, false),
            number("maxSupplyUsd", "Max per stablecoin supply", None, 500.0, 1.0, 100_000.0, Some(1.0), Some("USD")),
            number("maxSupplyBnb", "Max per BNB supply", None, 0.1, 0.001, 50.0, Some(0.001), Some("BNB")),
            number("feeSharePct", "Share of tax income for lending BNB", None, 0.0, 0.0, 100.0, Some(1.0), Some("%")),
        ]
    }
    fn fee_share(&self, c: &Settings) -> Option<f64> {
        Some(cfg_num(c, "feeSharePct"))
    }
    fn validate(&self, c: &Settings, _: &[Installed], _: Tier) -> Option<String> {
        assets(c).iter().any(|a| !["USDT", "USDC", "BNB"].contains(&a.as_str())).then(|| "assets must be among USDT, USDC, BNB".into())
    }
    fn mandate(&self, ctx: &Ctx) -> Option<String> {
        let js = crate::auth::js_number;
        Some(format!(
            "Venus: supply only ({}), at most ${} per stablecoin supply and {} BNB per BNB supply. Never borrow. Keep enough stablecoins liquid for hosting and payments.",
            ctx.list("assets").join(", "),
            js(ctx.num("maxSupplyUsd")),
            js(ctx.num("maxSupplyBnb"))
        ))
    }
    fn read_tools(&self, ctx: &Ctx) -> ToolSet {
        let (owner, allowed) = (ctx.agent.evm_address.clone(), ctx.list("assets"));
        vec![Tool::new("venus_markets", "Venus supply rates (APY, from the chain), utilisation and liquidity for USDT, USDC and BNB, and what the agent has supplied.", schema(&[]), move |_| {
            let (owner, allowed) = (owner.clone(), allowed.clone());
            async move {
                let r: Result<Value> = async {
                    let bpy = blocks_per_year().await?;
                    let mut markets = vec![];
                    for a in ["USDT", "USDC", "BNB"] {
                        markets.push(venus_market(a, &owner, bpy).await?);
                    }
                    Ok(json!({ "markets": markets, "allowed": allowed }))
                }
                .await;
                json_text(r.unwrap_or_else(|e| json!({ "error": e.message })))
            }
        })]
    }
    fn has_actions(&self) -> bool {
        true
    }
    fn action_tools(&self, ctx: &Ctx) -> ToolSet {
        let c1 = ctx.clone();
        let c2 = ctx.clone();
        let asset_enum = json!({ "type": "string", "enum": ["USDT", "USDC", "BNB"] });
        vec![
            Tool::new(
                "venus_supply",
                "Supply USDT, USDC or BNB to Venus to earn interest (within this plugin's limits).",
                schema(&[("asset", asset_enum.clone(), true), ("amount", json!({ "type": "number", "exclusiveMinimum": 0 }), true), ("rationale", json!({ "type": "string", "minLength": 10, "maxLength": 500 }), true)]),
                move |v| {
                    let ctx = c1.clone();
                    async move {
                        let asset = v["asset"].as_str().ok_or("invalid input: asset")?.to_string();
                        let amount = v["amount"].as_f64().filter(|a| *a > 0.0).ok_or("invalid input: amount")?;
                        let rationale = v["rationale"].as_str().ok_or("invalid input: rationale")?.to_string();
                        let r = venus_supply(&ctx, &asset, amount).await;
                        let unit = if asset == "BNB" { "BNB" } else { "USD" };
                        let (ok, result) = match &r {
                            Ok(tx) => (true, json!({ "tx": tx })),
                            Err(e) => (false, json!({ "error": e.message })),
                        };
                        let _ = record_trade(&ctx.agent.id, "venus", "venus", &asset, "buy", amount, unit, None, None, &format!("Supply to Venus: {rationale}"), &result, ok).await;
                        match r {
                            Ok(tx) => {
                                let _ = log_agent(&ctx.agent.id, AgentLogKind::Trade, &format!("Supplied {} {asset} to Venus", crate::auth::js_number(amount))).await;
                                json_text(json!({ "ok": true, "tx": tx }))
                            }
                            Err(e) => Ok(Value::String(format!("Supply refused or failed: {}", e.message))),
                        }
                    }
                },
            ),
            Tool::new(
                "venus_withdraw",
                "Withdraw supplied USDT, USDC or BNB from Venus back to the agent's wallet: an amount, or everything.",
                schema(&[("asset", asset_enum, true), ("amount", json!({ "anyOf": [{ "type": "number", "exclusiveMinimum": 0 }, { "type": "string", "const": "all" }] }), true), ("rationale", json!({ "type": "string", "minLength": 10, "maxLength": 500 }), true)]),
                move |v| {
                    let ctx = c2.clone();
                    async move {
                        let asset = v["asset"].as_str().ok_or("invalid input: asset")?.to_string();
                        let all = v["amount"] == "all";
                        let amount = if all { None } else { Some(v["amount"].as_f64().filter(|a| *a > 0.0).ok_or("invalid input: amount")?) };
                        let rationale = v["rationale"].as_str().ok_or("invalid input: rationale")?.to_string();
                        let r = venus_withdraw(&ctx, &asset, amount).await;
                        let unit = if asset == "BNB" { "BNB" } else { "USD" };
                        let (ok, result) = match &r {
                            Ok(tx) => (true, json!({ "tx": tx })),
                            Err(e) => (false, json!({ "error": e.message })),
                        };
                        let _ = record_trade(&ctx.agent.id, "venus", "venus", &asset, "sell", amount.unwrap_or(0.0), unit, None, None, &format!("Withdraw from Venus: {rationale}"), &result, ok).await;
                        match r {
                            Ok(tx) => {
                                let shown = amount.map(crate::auth::js_number).unwrap_or_else(|| "all".into());
                                let _ = log_agent(&ctx.agent.id, AgentLogKind::Trade, &format!("Withdrew {shown} {asset} from Venus")).await;
                                json_text(json!({ "ok": true, "tx": tx }))
                            }
                            Err(e) => Ok(Value::String(format!("Withdrawal failed: {}", e.message))),
                        }
                    }
                },
            ),
        ]
    }
}

async fn send(ctx: &Ctx, to: &str, data: Vec<u8>, value: U256) -> Result<String> {
    let h = crate::wallets::agent_send(&ctx.agent.id, &ctx.agent.evm_wallet, evm::parse_address(to)?, data.into(), value).await?;
    Ok(format!("{h:#x}"))
}

async fn venus_supply(ctx: &Ctx, asset: &str, amount: f64) -> Result<String> {
    if !ctx.list("assets").iter().any(|a| a.eq_ignore_ascii_case(asset)) {
        return Err(Error::invalid(format!("{asset} isn't enabled for lending")));
    }
    let js = crate::auth::js_number;
    if asset == "BNB" {
        if amount > ctx.num("maxSupplyBnb") {
            return Err(Error::invalid(format!("at most {} BNB per supply", js(ctx.num("maxSupplyBnb")))));
        }
        let wei = evm::to_wei(amount);
        let budget = spendable(&ctx.agent, &ctx.installed, "venus").await?;
        if wei > budget {
            return Err(Error::invalid(format!("the lending budget has {} BNB", alloy::primitives::utils::format_ether(budget))));
        }
        adjust_budget(&ctx.agent.id, "venus", -I256::from_raw(wei)).await?;
        return match send(ctx, market_of("BNB"), VBnbW::mintCall {}.abi_encode(), wei).await {
            Ok(tx) => Ok(tx),
            Err(e) => {
                adjust_budget(&ctx.agent.id, "venus", I256::from_raw(wei)).await?;
                Err(e)
            }
        };
    }
    if amount > ctx.num("maxSupplyUsd") {
        return Err(Error::invalid(format!("at most ${} per supply", js(ctx.num("maxSupplyUsd")))));
    }
    let token = underlying_of(asset);
    let units = crate::wallet_policy::grants::to_units(&format!("{amount}"), 18)?;
    if evm::erc20_balance(token, &ctx.agent.evm_address).await? < units {
        return Err(Error::invalid(format!("the wallet doesn't hold {} {asset}", js(amount))));
    }
    crate::wallets::ensure_allowance(&ctx.agent.id, &ctx.agent.evm_wallet, &ctx.agent.evm_address, evm::parse_address(token)?, evm::parse_address(market_of(asset))?, units).await?;
    send(ctx, market_of(asset), VTokenW::mintCall { mintAmount: units }.abi_encode(), U256::ZERO).await
}

async fn venus_withdraw(ctx: &Ctx, asset: &str, amount: Option<f64>) -> Result<String> {
    let vt = market_of(asset);
    let before = if asset == "BNB" { evm::bnb_balance(&ctx.agent.evm_address).await? } else { U256::ZERO };
    let tx = match amount {
        None => {
            let vbal = evm::erc20_balance(vt, &ctx.agent.evm_address).await?;
            if vbal == U256::ZERO {
                return Err(Error::invalid(format!("nothing is supplied in {asset}")));
            }
            send(ctx, vt, VTokenW::redeemCall { redeemTokens: vbal }.abi_encode(), U256::ZERO).await?
        }
        Some(a) => send(ctx, vt, VTokenW::redeemUnderlyingCall { redeemAmount: crate::wallet_policy::grants::to_units(&format!("{a}"), 18)? }.abi_encode(), U256::ZERO).await?,
    };
    // Withdrawn BNB (net of gas) goes back to the lending budget.
    if asset == "BNB" {
        let after = evm::bnb_balance(&ctx.agent.evm_address).await?;
        if after > before {
            adjust_budget(&ctx.agent.id, "venus", I256::from_raw(after - before)).await?;
        }
    }
    Ok(tx)
}
