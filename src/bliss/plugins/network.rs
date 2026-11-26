//! Orvyn's network plugins: Verifiable Randomness (drand and Orvyn RNG draws), the Orvyn network
//! node (the agent signs RNG rounds and price reports), and Price Feeds (one price from many sources).

use std::collections::HashSet;
use std::sync::{Arc, LazyLock, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};

use super::data::{fail, meta};
use crate::ai::{schema, Tool, ToolSet};
use crate::bliss::settings::{number, SettingField, Settings};
use crate::bliss::{json_text, Ctx, JobFuture, Plugin, PluginJob, PluginMeta};
use crate::price::sources::{asset_quotes, consensus, token_quotes, ASSET_SYMBOLS};
use crate::rng::{self, drand};

// ---------- Verifiable Randomness ----------

pub struct Randomness(PluginMeta);

pub fn randomness() -> Arc<dyn Plugin> {
    Arc::new(Randomness(meta(
        "randomness",
        "Verifiable Randomness",
        "capability",
        "Provably fair random numbers and picks from the drand beacon, checked in code and recomputable by anyone.",
        "For giveaways, raffles, fair picks, games and sampling. Numbers come from drand (the League of Entropy's public beacon), verified against the network's key before use. The agent can announce a future round in advance so nobody, the agent included, knows the result beforehand, and every draw lists its round, purpose and method so anyone can check it.",
        &["read-market-data", "third-party"],
    )))
}

fn refuse(reason: impl Into<String>) -> Result<Value, String> {
    json_text(json!({ "ok": false, "reason": reason.into() }))
}

async fn random_draw(v: Value) -> Result<Value, String> {
    let purpose = v["purpose"].as_str().unwrap_or("").to_string();
    if purpose.len() < 3 || purpose.len() > 120 {
        return Err("purpose must be 3 to 120 characters".into());
    }
    let round = v["round"].as_i64();
    let count = v["count"].as_u64().unwrap_or(1).clamp(1, 100) as usize;
    let unique = v["unique"].as_bool().unwrap_or(true);
    let pick_from: Option<Vec<String>> = v["pick_from"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect());
    if pick_from.as_ref().is_some_and(|p| p.len() < 2 || p.len() > 1000) {
        return Err("pick_from needs 2 to 1000 entries".into());
    }
    let (lo, hi) = match &pick_from {
        Some(p) => (0, p.len() as i64 - 1),
        None => (v["min"].as_i64().unwrap_or(1), v["max"].as_i64().unwrap_or(100)),
    };
    let pick = |values: Vec<i64>| -> Value {
        match &pick_from {
            Some(p) => values.iter().map(|i| Value::String(p[*i as usize].clone())).collect(),
            None => json!(values),
        }
    };
    if v["source"] == "orvyn" {
        if hi < lo {
            return refuse("max must be at least min");
        }
        if unique && count as i64 > hi - lo + 1 {
            return refuse("more unique values asked for than the range holds");
        }
        let Some(proof) = rng::round_proof(round).await.map_err(fail)? else {
            return refuse(match round {
                None => "no Orvyn RNG round is finalised yet".to_string(),
                Some(r) => format!("Orvyn RNG round {r} isn't finalised (yet)"),
            });
        };
        if proof["verification"]["valid"] != true {
            let problems: Vec<&str> = proof["verification"]["problems"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            return refuse(format!("round {} failed verification: {}", proof["round"], problems.join("; ")));
        }
        let values = rng::draw(proof["output"].as_str().unwrap_or(""), &purpose, count, lo, hi, unique);
        return json_text(json!({ "ok": true, "verified": true, "source": "orvyn-rng", "round": proof["round"], "purpose": purpose, "result": pick(values), "proof": proof }));
    }
    if let Some(r) = round {
        if drand::time_of_round(r) as f64 > rng::now_s() {
            return refuse(format!("round {r} is published at {}; draw after that", drand::iso(drand::time_of_round(r))));
        }
    }
    if hi < lo {
        return refuse("max must be at least min");
    }
    if unique && count as i64 > hi - lo + 1 {
        return refuse("more unique values asked for than the range holds");
    }
    match drand::beacon(round).await {
        Ok(b) => {
            let values = rng::draw(&b.randomness, &purpose, count, lo, hi, unique);
            json_text(json!({
                "ok": true,
                "verified": true,
                "round": b.round,
                "published_at": b.at,
                "purpose": purpose,
                "result": pick(values),
                "proof": { "chain": "drand quicknet", "chain_hash": drand::CHAIN, "signature": b.signature, "randomness": b.randomness, "method": "value_i = sha256(randomness|purpose|i) → rejection sampling into the range" },
            }))
        }
        Err(e) => refuse(format!("the beacon couldn't be verified right now: {}", e.message)),
    }
}

#[async_trait]
impl Plugin for Randomness {
    fn meta(&self) -> &PluginMeta {
        &self.0
    }
    async fn knowledge(&self, _ctx: &Ctx) -> Option<String> {
        Some("## Fair randomness\nNever pick 'randomly' yourself. For anything people should trust (a winner, an order, a sample), first announce the round you'll use (commit_round), then draw from it once it's out (random_draw with that round) and publish the round and purpose with the result.".into())
    }
    fn read_tools(&self, _ctx: &Ctx) -> ToolSet {
        vec![
            Tool::new(
                "commit_round",
                "Get the drand round that will be published N minutes from now, to announce in advance ('the winner comes from round R').",
                schema(&[("minutes_from_now", json!({ "type": "number", "minimum": 0.1, "maximum": 10080 }), true)]),
                |v| async move {
                    let minutes = v["minutes_from_now"].as_f64().unwrap_or(1.0);
                    let at = (rng::now_s() + minutes * 60.0).floor();
                    let round = drand::round_at(at);
                    let orvyn = rng::round_of(at);
                    json_text(json!({
                        "round": round,
                        "published_at": drand::iso(drand::time_of_round(round)),
                        "how_to_verify": format!("drand quicknet round {round}: randomness = sha256(signature); values = sha256(randomness|purpose|i)"),
                        "orvyn_rng_round": orvyn,
                        "orvyn_rng_ready_at": drand::iso(rng::round_close(orvyn) + 6),
                    }))
                },
            ),
            Tool::new(
                "random_draw",
                "Draw verifiable random integers in [min, max] (or pick from a list) from a drand round: the latest, or a round you committed to earlier. `purpose` is part of the derivation, so use a clear, unique label (e.g. 'giveaway #12 winner').",
                schema(&[
                    ("purpose", json!({ "type": "string", "minLength": 3, "maxLength": 120 }), true),
                    ("round", json!({ "type": "integer", "exclusiveMinimum": 0 }), false),
                    ("min", json!({ "type": "integer" }), false),
                    ("max", json!({ "type": "integer" }), false),
                    ("count", json!({ "type": "integer", "minimum": 1, "maximum": 100 }), false),
                    ("unique", json!({ "type": "boolean" }), false),
                    ("pick_from", json!({ "type": "array", "items": { "type": "string", "maxLength": 200 }, "minItems": 2, "maxItems": 1000, "description": "Pick from these entries instead of numbers" }), false),
                    ("source", json!({ "type": "string", "enum": ["drand", "orvyn"], "description": "drand (default), or Orvyn RNG: agents' signatures mixed with drand" }), false),
                ]),
                random_draw,
            ),
        ]
    }
}

// ---------- Orvyn network node ----------

pub struct RngNodePlugin(PluginMeta);

pub fn rng_node() -> Arc<dyn Plugin> {
    Arc::new(RngNodePlugin(meta(
        "orvyn-rng",
        "Orvyn network node",
        "capability",
        "The agent helps run Orvyn's network: verifiable randomness and signed price feeds. It signs every round, earns from what contracts use, and its uptime and accuracy are public.",
        "Joins the agent to Orvyn's network, with its own BLS key (a key just for this, which can't move funds). Randomness: every minute it signs the round, and the round's output combines all nodes' signatures with the drand beacon, so no one can predict or steer it. Price feeds: every minute it prices BTC, ETH, BNB, CAKE and SOL from many sources and signs the network's report only if it's within 0.5% of what it saw. With the contracts live, the node stakes from the agent's wallet, earns from what contracts use, and can be slashed for provable faults (signing ahead, or two different reports for one round). Self-hosted agents can join with their own key through the API.",
        &[],
    )))
}

/// Agents already told they need BNB to stake (once per process, not every pass).
static WARNED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);

fn join_job(ctx: Ctx) -> JobFuture {
    Box::pin(async move {
        if ctx.agent.hosting_status == "external" {
            return Ok(());
        }
        rng::join_hosted(&ctx.agent.id).await?;
        // With the contract live, the node stakes from the agent's wallet to count on chain.
        let r = rng::chain::register_on_chain(&ctx.agent).await?;
        if r["state"] == "needs-bnb" && WARNED.lock().unwrap().insert(ctx.agent.id.clone()) {
            crate::db::log_agent(
                &ctx.agent.id,
                crate::db::AgentLogKind::Plugin,
                &format!("Orvyn RNG: send about {} BNB to stake this agent's node (it earns a share of every round it signs).", r["needBnb"].as_str().unwrap_or("")),
            )
            .await?;
        }
        if r["state"] == "registered" {
            let _ = rng::chain::pay_out(&ctx.agent).await;
        }
        Ok(())
    })
}

#[async_trait]
impl Plugin for RngNodePlugin {
    fn meta(&self) -> &PluginMeta {
        &self.0
    }
    async fn knowledge(&self, ctx: &Ctx) -> Option<String> {
        let s = rng::node_stats(&ctx.agent.id, 1440).await.ok().flatten();
        let status = match s {
            Some(s) => format!("You've signed {} rounds in the last day ({}% uptime).", s["roundsSigned"], crate::auth::js_number(s["uptimePct"].as_f64().unwrap_or(0.0))),
            None => "Your node starts with the next round.".into(),
        };
        Some(format!("## Orvyn RNG\nYou're a node of Orvyn RNG, a verifiable randomness network run by agents. {status} When you need randomness people should trust, Orvyn RNG rounds are verifiable by anyone."))
    }
    fn read_tools(&self, ctx: &Ctx) -> ToolSet {
        let (a1, a2) = (ctx.agent.id.clone(), ctx.agent.id.clone());
        vec![
            Tool::new("price_node_accuracy", "How close this node's price observations have been to the network's reports over the last day.", schema(&[]), move |_| {
                let a = a1.clone();
                async move { json_text(crate::price::node_accuracy(&a, 1440).await.map_err(fail)?) }
            }),
            Tool::new("rng_node_status", "This agent's Orvyn RNG node: public key, rounds signed and uptime, and the network's latest round.", schema(&[]), move |_| {
                let a = a2.clone();
                async move { json_text(json!({ "node": rng::node_stats(&a, 1440).await.map_err(fail)?, "network": rng::network_stats().await.map_err(fail)? })) }
            }),
        ]
    }
    // Membership follows the plugin: joined while installed, left on uninstall.
    fn jobs(&self) -> Vec<PluginJob> {
        vec![PluginJob { id: "join", every: |_| 300.0, run: join_job }]
    }
    async fn before_uninstall(&self, ctx: &Ctx) -> Option<String> {
        rng::leave(&ctx.agent.id).await.err().map(|e| e.message)
    }
}

// ---------- Price Feeds ----------

pub struct PriceFeeds(PluginMeta);

pub fn price_feeds() -> Arc<dyn Plugin> {
    Arc::new(PriceFeeds(meta(
        "price-feeds",
        "Price Feeds",
        "data",
        "One price from many sources side by side: Chainlink, Pyth, Binance, OKX, Coinbase, CoinGecko, and PancakeSwap and Uniswap TWAPs, with a median.",
        "Prices an asset from independent sources at once: the Chainlink and Pyth oracles, the Binance, OKX and Coinbase exchanges, CoinGecko, and PancakeSwap v3 and Uniswap v3 on BNB Chain, read from the pools as a 30-minute time-weighted average. It shows the median, how far each source is from it, outliers, staleness and the spread. Any BNB Chain token works by address (DEX TWAPs and DexScreener). Read-only.",
        &["read-market-data", "third-party"],
    )))
}

#[async_trait]
impl Plugin for PriceFeeds {
    fn meta(&self) -> &PluginMeta {
        &self.0
    }
    fn settings(&self) -> Vec<SettingField> {
        vec![number("outlierPct", "Outlier threshold", None, 1.0, 0.05, 20.0, Some(0.05), Some("%"))]
    }
    fn fee_share(&self, _c: &Settings) -> Option<f64> {
        None
    }
    async fn knowledge(&self, _ctx: &Ctx) -> Option<String> {
        Some("## Reading prices\nTrust a price when independent sources agree (small spread, few outliers). A DEX TWAP far from the oracles means a thin or manipulated pool; a stale oracle (old age) lags fast markets. Exchange prices are in USDT, which can drift from USD.".into())
    }
    fn read_tools(&self, ctx: &Ctx) -> ToolSet {
        let outlier = ctx.num("outlierPct");
        vec![Tool::new(
            "get_price",
            &format!("Price one asset from many sources at once, with the median, each source's deviation, outliers and staleness. Listed assets: {}; or any BNB Chain token by token_address.", ASSET_SYMBOLS.join(", ")),
            schema(&[("asset", json!({ "type": "string", "maxLength": 10 }), false), ("token_address", json!({ "type": "string", "pattern": "^0x[0-9a-fA-F]{40}$" }), false)]),
            move |v| async move {
                let asset = v["asset"].as_str().map(str::to_uppercase);
                let token = v["token_address"].as_str().map(String::from);
                if asset.is_none() == token.is_none() {
                    return json_text(json!({ "error": "give an asset symbol or a token_address" }));
                }
                let r = match (&asset, &token) {
                    (Some(a), _) => asset_quotes(a, true).await,
                    (None, Some(t)) => Ok(token_quotes(t).await),
                    _ => unreachable!(),
                };
                match r {
                    Ok((quotes, failed)) if quotes.is_empty() => json_text(json!({ "error": "no source answered", "failed": failed })),
                    Ok((quotes, failed)) => {
                        let mut out = json!({ "asset": asset.or(token) });
                        if let (Some(o), Value::Object(c)) = (out.as_object_mut(), consensus(&quotes, outlier)) {
                            o.extend(c);
                            o.insert("unavailable".into(), json!(failed));
                        }
                        json_text(out)
                    }
                    Err(e) => json_text(json!({ "error": e.message })),
                }
            },
        )]
    }
}
