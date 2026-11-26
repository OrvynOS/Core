//! The capability plugins, on a throwaway database with a stand-in market (DexScreener prices the
//! test sets):
//! - Track Record: a call is priced at entry, graded by code when due from the real move; hit rate
//!   and Brier come out right; the record and the misses reach the agent's prompt;
//! - Automation: a DCA rule buys through Trader on schedule and counts down; a price trigger fires
//!   once when crossed and never before; without Trader nothing trades;
//! - Randomness: draws are deterministic, unique and in range; a future round can't be drawn early;
//! - Price Feeds: the consensus median, outliers and spread.
//!
//! `cargo test -p orvyn-core --test capabilities -- --ignored` also runs the live part: drand
//! rounds verify (a forged one doesn't), and derivatives, DeFi yields, Polymarket, Venus, price
//! sources and onchain flows answer with real data. The price-source check needs PYTH_HERMES_URL
//! and an RPC that isn't rate-limited (EVM_RPC_BSC): the public one answers the DEX TWAPs with 429.
mod support;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use orvyn_core::ai::{schema, Tool, ToolSet};
use orvyn_core::bliss::plugins::{automation, data::meta, markets, network, record};
use orvyn_core::bliss::{prices, Ctx, Installed, Plugin, PluginMeta, Settings};
use orvyn_core::db::Agent;
use orvyn_core::price::sources::{asset_quotes, consensus, Quote};
use orvyn_core::rng::{self, drand};
use orvyn_core::tiers::Tier;
use serde_json::{json, Value};

const TOKEN: &str = "0x1234567890abcdef1234567890abcdef12345678";

fn settings(v: Value) -> Settings {
    v.as_object().cloned().unwrap_or_default()
}

fn ctx(agent: &Arc<Agent>, config: Value, state: Value, installed: Vec<Installed>) -> Ctx {
    Ctx { agent: agent.clone(), config: settings(config), state, installed: Arc::new(installed) }
}

async fn run(tools: &ToolSet, name: &str, input: Value) -> Result<Value, String> {
    let tool = tools.iter().find(|t| t.name == name).unwrap_or_else(|| panic!("no tool {name}"));
    let out = tool.execute.clone().unwrap()(input).await?;
    Ok(match out {
        Value::String(s) => serde_json::from_str(&s).unwrap_or(Value::String(s)),
        v => v,
    })
}

/// A Trader stand-in that records the buys and sells Automation asks it for.
struct FakeTrader(PluginMeta, Arc<Mutex<Vec<Value>>>);

#[async_trait]
impl Plugin for FakeTrader {
    fn meta(&self) -> &PluginMeta {
        &self.0
    }
    fn has_actions(&self) -> bool {
        true
    }
    fn action_tools(&self, _ctx: &Ctx) -> ToolSet {
        ["buy_token", "sell_token"]
            .into_iter()
            .map(|name| {
                let trades = self.1.clone();
                Tool::new(name, "stand-in", schema(&[]), move |v| {
                    let trades = trades.clone();
                    async move {
                        trades.lock().unwrap().push(v);
                        Ok(json!("{\"ok\":true}"))
                    }
                })
            })
            .collect()
    }
}

#[derive(Clone)]
struct Market(Arc<Mutex<f64>>);

async fn pairs(State(m): State<Market>, Path(token): Path<String>) -> Json<Value> {
    let price = *m.0.lock().unwrap();
    Json(json!({ "pairs": [{ "chainId": "bsc", "pairAddress": "0xpool", "priceUsd": price.to_string(), "liquidity": { "usd": 50_000 }, "baseToken": { "address": token, "symbol": "TEST", "name": "Test" } }] }))
}

#[tokio::test]
async fn track_record_automation_randomness_and_consensus() {
    support::test_env(&[("BSC_RPC_URL", &support::zero_rpc())]);
    let db = support::shared_db().await;
    let market = Market(Arc::new(Mutex::new(1.0)));
    let url = support::serve_forever(Router::new().route("/latest/dex/tokens/{token}", get(pairs)).with_state(market.clone()));
    unsafe { std::env::set_var("DEXSCREENER_BASE_URL", &url) };
    let set_price = |p: f64| {
        *market.0.lock().unwrap() = p;
        prices::reset_prices();
    };
    sqlx::query("insert into agents (id, name, symbol, owner_account_id, ai_provider, ai_model, ai_key_encrypted, ai_key_hint, evm_address, evm_wallet, status) values ('agent-cap', 'Cap', 'CAP', '0x1', 'x', 'x', 'x', 'x', '0x00000000000000000000000000000000000000a1', '{}', 'live')")
        .execute(db)
        .await
        .unwrap();
    let agent: Arc<Agent> = Arc::new(sqlx::query_as("select * from agents where id = 'agent-cap'").fetch_one(db).await.unwrap());

    // ---- Track Record ----
    assert!(record::grade_move("up", 1.5) == "hit" && record::grade_move("up", 0.5) == "miss");
    assert!(record::grade_move("down", -2.0) == "hit" && record::grade_move("flat", 1.9) == "hit" && record::grade_move("flat", -2.5) == "miss");
    let track = record::track_record_plugin();
    let tools = track.read_tools(&ctx(&agent, json!({}), json!({}), vec![]));
    let a = run(&tools, "log_call", json!({ "token_address": TOKEN, "direction": "up", "horizon_hours": 24, "confidence": 0.8, "thesis": "Buyers absorbed every dip and funding is flat." })).await.unwrap();
    run(&tools, "log_call", json!({ "token_address": TOKEN, "direction": "down", "horizon_hours": 24, "confidence": 0.6, "thesis": "Top holders started distributing into strength." })).await.unwrap();
    assert!(a["logged"] == true && a["entry_price_usd"] == 1.0, "a call is priced at entry: {a}");
    let bad = run(&tools, "log_call", json!({ "direction": "up", "horizon_hours": 24, "confidence": 0.7, "thesis": "Neither a token nor a coin was given here." })).await;
    assert!(bad.is_err() || bad.unwrap()["logged"] != true, "a call needs a market");
    // A day later the price is up 10%: the up call hits, the down call misses.
    set_price(1.1);
    assert_eq!(record::grade_due("agent-cap").await.unwrap(), 0, "nothing is graded before it's due");
    sqlx::query("update agent_calls set due_at = now() - interval '1 second'").execute(db).await.unwrap();
    assert_eq!(record::grade_due("agent-cap").await.unwrap(), 2, "both are graded when due");
    let rec = record::track_record("agent-cap", 200).await.unwrap();
    assert_eq!(rec["hitRatePct"], 50.0);
    // Brier: up call (0.8, hit) → 0.04; down call (0.6, miss) → 0.36; mean 0.2.
    assert_eq!(rec["brier"], 0.2, "Brier score from stated confidence");
    let knowledge = track.knowledge(&ctx(&agent, json!({}), json!({}), vec![])).await.unwrap();
    assert!(knowledge.contains("Hit rate 50%") && knowledge.contains("distributing"), "the record and the miss reach the prompt: {knowledge}");
    let move_pct: Option<f64> = sqlx::query_scalar("select move_pct from agent_calls where id = $1").bind(a["id"].as_i64().unwrap() as i32).fetch_one(db).await.unwrap();
    assert!((move_pct.unwrap() - 10.0).abs() < 0.01, "the move is stored");

    // ---- Automation ----
    sqlx::query("insert into agent_plugins (agent_id, plugin_id, config, state) values ('agent-cap', 'automation', '{\"maxRules\": 10}', '{}')").execute(db).await.unwrap();
    let trades = Arc::new(Mutex::new(Vec::<Value>::new()));
    let trader = || Installed { plugin: Arc::new(FakeTrader(meta("trader", "Trader", "trading", "stand-in", "stand-in", &[]), trades.clone())), config: Settings::new(), state: json!({}), enabled: true };
    let auto = automation::automation();
    let cfg = settings(json!({ "maxRules": 10 }));
    assert!(auto.validate(&cfg, &[], Tier::Studio).is_some() && auto.validate(&cfg, &[trader()], Tier::Studio).is_none(), "Automation requires Trader");
    let state = || async { sqlx::query_scalar::<_, Value>("select state from agent_plugins where agent_id = 'agent-cap' and plugin_id = 'automation'").fetch_one(db).await.unwrap() };
    let with_trader = |s: Value| ctx(&agent, json!({ "maxRules": 10 }), s, vec![trader()]);
    let buys = || trades.lock().unwrap().clone();
    let now = chrono::Utc::now;

    run(&auto.action_tools(&with_trader(state().await)), "add_dca", json!({ "token": TOKEN, "amount_bnb": 0.01, "every_hours": 24, "times": 2, "note": "Accumulate slowly" })).await.unwrap();
    automation::run_automation(&with_trader(state().await), now()).await.unwrap();
    assert!(buys().len() == 1 && buys()[0]["amount_bnb"] == 0.01, "DCA buys through Trader right away");
    assert_eq!(state().await["rules"][0]["runsLeft"], 1, "and counts down to the next run");
    automation::run_automation(&with_trader(state().await), now()).await.unwrap();
    assert_eq!(buys().len(), 1, "it doesn't buy again before the interval");
    automation::run_automation(&with_trader(state().await), now() + chrono::Duration::hours(25)).await.unwrap();
    assert!(buys().len() == 2 && state().await["rules"].as_array().unwrap().is_empty(), "after the last run the rule is gone");

    set_price(1.0);
    run(&auto.action_tools(&with_trader(state().await)), "add_price_trigger", json!({ "token": TOKEN, "when": "below", "price_usd": 0.8, "action": "buy", "amount_bnb": 0.02, "note": "Buy the dip" })).await.unwrap();
    automation::run_automation(&with_trader(state().await), now()).await.unwrap();
    assert_eq!(buys().len(), 2, "a trigger doesn't fire above its price");
    set_price(0.75);
    automation::run_automation(&with_trader(state().await), now()).await.unwrap();
    assert!(buys().len() == 3 && buys()[2]["amount_bnb"] == 0.02, "it fires once when crossed");
    assert!(state().await["rules"].as_array().unwrap().is_empty(), "and is removed");

    let alone = |s: Value| ctx(&agent, json!({ "maxRules": 10 }), s, vec![]);
    run(&auto.action_tools(&alone(state().await)), "add_dca", json!({ "token": TOKEN, "amount_bnb": 0.01, "every_hours": 1, "times": 1, "note": "No trader installed" })).await.unwrap();
    automation::run_automation(&alone(state().await), now()).await.unwrap();
    assert!(buys().len() == 3 && state().await["rules"].as_array().unwrap().len() == 1, "without Trader nothing trades and the rule waits");

    // ---- Randomness ----
    let seed = "ab".repeat(32);
    let d1 = rng::draw(&seed, "giveaway #1", 5, 1, 10, true);
    let mut unique = d1.clone();
    unique.sort();
    unique.dedup();
    assert!(d1 == rng::draw(&seed, "giveaway #1", 5, 1, 10, true) && unique.len() == 5 && d1.iter().all(|n| (1..=10).contains(n)), "deterministic, unique and in range: {d1:?}");
    assert_ne!(rng::draw(&seed, "giveaway #2", 5, 1, 10, true), d1, "the purpose changes the draw");
    let tools = network::randomness().read_tools(&ctx(&agent, json!({}), json!({}), vec![]));
    let future = run(&tools, "commit_round", json!({ "minutes_from_now": 10 })).await.unwrap();
    let early = run(&tools, "random_draw", json!({ "purpose": "too early", "round": future["round"] })).await.unwrap();
    assert_eq!(early["ok"], false, "a future round can't be drawn early");

    // ---- Price Feeds: consensus ----
    let q = |source: &str, price: f64| Quote { source: source.into(), kind: "exchange", price, age_seconds: None, note: None };
    let c = consensus(&[q("a", 100.0), q("b", 100.2), q("c", 99.9), q("d", 100.1), q("e", 107.0)], 1.0);
    assert_eq!(c["median"], 100.1, "the median ignores one wild source");
    let e = c["quotes"].as_array().unwrap().iter().find(|x| x["source"] == "e").unwrap().clone();
    assert!(e["outlier"] == true && c["agreeing"] == 4, "a source 7% off is an outlier");
    assert!(c["spread_pct"].as_f64().unwrap() < 0.4, "the spread counts only agreeing sources");
    assert_eq!(consensus(&[q("a", 10.0), q("b", 20.0)], 50.0)["median"], 15.0, "an even count takes the middle two");
}

#[tokio::test]
#[ignore = "live: drand, Binance/Aster, DefiLlama, Polymarket, BNB Chain"]
async fn live_data() {
    support::test_env(&[]);
    let mut failed = vec![];
    let mut check = |name: &str, pass: bool, detail: String| {
        println!("{} live: {name} {detail}", if pass { "ok  " } else { "FAIL" });
        if !pass {
            failed.push(name.to_string());
        }
    };
    let db = support::shared_db().await;
    sqlx::query("insert into agents (id, name, symbol, owner_account_id, ai_provider, ai_model, ai_key_encrypted, ai_key_hint, evm_address, evm_wallet, status) values ('agent-live', 'Live', 'LIVE', '0x1', 'x', 'x', 'x', 'x', '0x00000000000000000000000000000000000000a1', '{}', 'live')")
        .execute(db)
        .await
        .unwrap();
    let agent: Arc<Agent> = Arc::new(sqlx::query_as("select * from agents where id = 'agent-live'").fetch_one(db).await.unwrap());
    let no_ctx = ctx(&agent, json!({}), json!({}), vec![]);

    match drand::beacon(None).await {
        Ok(b) => {
            let forged = format!("{}{}", &b.signature[..b.signature.len() - 2], if b.signature.ends_with("00") { "01" } else { "00" });
            check("a drand round verifies and a forged one doesn't", drand::verify_round(b.round, &b.signature) && !drand::verify_round(b.round, &forged) && !drand::verify_round(b.round + 1, &b.signature), String::new());
        }
        Err(e) => check("drand beacon", false, e.message),
    }
    let r = run(&network::randomness().read_tools(&no_ctx), "random_draw", json!({ "purpose": "check", "pick_from": ["a", "b", "c"] })).await;
    check("random_draw returns a verified pick", r.as_ref().is_ok_and(|r| r["ok"] == true && r["verified"] == true && ["a", "b", "c"].contains(&r["result"][0].as_str().unwrap_or(""))), format!("{r:?}").chars().take(200).collect());
    let r = run(&markets::derivatives().read_tools(&no_ctx), "get_derivatives", json!({ "symbol": "BTCUSDT", "venue": "both" })).await;
    check("derivatives: BTCUSDT funding and OI", r.as_ref().is_ok_and(|r| r.as_array().is_some_and(|a| a.iter().any(|v| !v["funding_now"].is_null()))), format!("{r:?}").chars().take(200).collect());
    let r = run(&markets::defi_yields().read_tools(&no_ctx), "find_yields", json!({ "chain": "BSC", "stablecoin_only": true, "limit": 5 })).await;
    check(
        "DeFi yields: stablecoin pools on BSC",
        r.as_ref().is_ok_and(|r| r["pools"].as_array().is_some_and(|p| !p.is_empty() && p.iter().all(|p| p["chain"] == "BSC" && p["stablecoin"] == true))),
        format!("{r:?}").chars().take(200).collect(),
    );
    let tools = markets::polymarket().read_tools(&no_ctx);
    let s = run(&tools, "polymarket_search", json!({ "query": "bitcoin", "limit": 3 })).await;
    let slug = s.as_ref().ok().and_then(|s| s["markets"][0]["slug"].as_str().map(String::from));
    let h = match &slug {
        Some(slug) => run(&tools, "polymarket_history", json!({ "slug": slug, "interval": "1w" })).await,
        None => Err("no markets".into()),
    };
    check("Polymarket: search and history", h.as_ref().is_ok_and(|h| h["history"].is_array()), format!("{h:?}").chars().take(200).collect());
    let venus_ctx = ctx(&agent, json!({ "assets": ["USDT", "USDC"], "maxSupplyUsd": 500, "maxSupplyBnb": 0.1, "feeSharePct": 0 }), json!({}), vec![]);
    let r = run(&record::venus().read_tools(&venus_ctx), "venus_markets", json!({})).await;
    check(
        "Venus: supply APY and liquidity for USDT, USDC and BNB",
        r.as_ref().is_ok_and(|r| r["markets"].as_array().is_some_and(|m| m.len() == 3 && m.iter().all(|m| m["supply_apy_pct"].as_f64().is_some_and(|a| (0.0..100.0).contains(&a)) && m["available_to_withdraw"].as_f64().is_some_and(|x| x > 0.0)))),
        format!("{r:?}").chars().take(300).collect(),
    );
    match asset_quotes("BTC", false).await {
        Ok((quotes, failed_sources)) => {
            let c = consensus(&quotes, 1.0);
            let sources: Vec<&str> = c["quotes"].as_array().unwrap().iter().filter_map(|q| q["source"].as_str()).collect();
            let dex = sources.contains(&"pancakeswap-v3") && sources.contains(&"uniswap-v3");
            check("Price Feeds: BTC from oracles, exchanges and both DEX TWAPs", c["agreeing"].as_u64().unwrap_or(0) >= 5 && dex && c["spread_pct"].as_f64().unwrap_or(99.0) < 1.0, format!("{} agree, spread {}%, failed: {failed_sources:?}", c["agreeing"], c["spread_pct"]));
        }
        Err(e) => check("Price Feeds: BTC", false, e.message),
    }
    let r = orvyn_core::bliss::plugins::chain::flows("0x0E09FaBB73Bd3Ade0a17ECC321fD13a19e81cE82", 0.25).await;
    check("onchain flows: CAKE over 15 minutes", r.as_ref().is_ok_and(|r| r["transfers"].as_u64().unwrap_or(0) > 0 && r["dex_pools_seen"].as_u64().unwrap_or(0) > 0), format!("{r:?}").chars().take(200).collect());
    assert!(failed.is_empty(), "live checks failed: {failed:?}");
}
