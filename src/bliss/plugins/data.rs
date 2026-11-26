//! Read-only data plugins: GoPlus Security, DexScreener, GeckoTerminal, Market Pulse, and the
//! creator's knowledge base.

use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::ai::{schema, Tool, ToolSet};
use crate::bliss::{input, json_text, round, Ctx, Plugin, PluginMeta};
use crate::data::{dexscreener, geckoterminal, goplus};
use crate::db::{db, KnowledgeDoc};
use crate::evm::EVM_CHAINS;

pub fn meta(id: &str, name: &str, category: &'static str, summary: &str, description: &str, permissions: &[&'static str]) -> PluginMeta {
    PluginMeta { id: id.into(), name: name.into(), version: "1.0.0".into(), author: "Orvyn".into(), category, summary: summary.into(), description: description.into(), permissions: permissions.to_vec() }
}

/// A tool error as the model sees it.
pub fn fail(e: crate::Error) -> String {
    e.message
}

fn liq(p: &Value) -> f64 {
    p["liquidity"]["usd"].as_f64().unwrap_or(0.0)
}

// ---- GoPlus ----

pub struct GoPlus(PluginMeta);

pub fn goplus() -> Arc<dyn Plugin> {
    Arc::new(GoPlus(meta(
        "goplus",
        "GoPlus Security",
        "data",
        "Token and wallet risk checks: honeypots, hidden owners, mint and tax tricks, malicious addresses.",
        "Adds GoPlus Security tools so the agent can vet any EVM token or address before discussing or trading it.",
        &["read-market-data"],
    )))
}

fn goplus_chain(key: &str) -> Option<&'static str> {
    EVM_CHAINS.iter().find(|c| c.key == key).map(|c| c.goplus)
}

#[async_trait]
impl Plugin for GoPlus {
    fn meta(&self) -> &PluginMeta {
        &self.0
    }
    async fn knowledge(&self, _ctx: &Ctx) -> Option<String> {
        Some("Before endorsing or discussing buying an unfamiliar EVM token, check it with goplus_token_security and report any risk flags plainly.".into())
    }
    fn read_tools(&self, _ctx: &Ctx) -> ToolSet {
        let chains: Vec<&str> = EVM_CHAINS.iter().map(|c| c.key).collect();
        let chain = json!({ "type": "string", "enum": chains });
        vec![
            Tool::new(
                "goplus_token_security",
                &format!("GoPlus security report for an EVM token: honeypot, buy/sell tax, mintable, owner privileges, proxy, blacklist, holder concentration, and a pass/fail against default safety policy. Chains: {}.", chains.join(", ")),
                schema(&[("chain", chain.clone(), true), ("token", json!({ "type": "string" }), true)]),
                |v| async move {
                    #[derive(Deserialize)]
                    struct I {
                        chain: String,
                        token: String,
                    }
                    let i: I = input(v)?;
                    let cid = goplus_chain(&i.chain).ok_or("unknown chain")?;
                    let report = goplus::token_security(cid, &i.token).await.map_err(fail)?;
                    let verdict = goplus::assess_token(report.as_ref(), goplus::DEFAULT_POLICY);
                    let mut out = goplus::summarize_security(report.as_ref());
                    if !out.is_object() {
                        out = json!({});
                    }
                    out["passes_default_policy"] = verdict.ok.into();
                    out["concerns"] = json!(verdict.reasons);
                    out["warnings"] = json!(verdict.warnings);
                    json_text(out)
                },
            ),
            Tool::new(
                "goplus_address_security",
                "GoPlus check on a wallet or contract address: phishing, sanctions, mixers, honeypot-related, stealing attacks and other malicious-activity flags.",
                schema(&[("address", json!({ "type": "string" }), true), ("chain", chain, false)]),
                |v| async move {
                    let address = v["address"].as_str().ok_or("invalid input: address")?.to_string();
                    let cid = v["chain"].as_str().and_then(goplus_chain);
                    let r = goplus::address_security(&address, cid).await.map_err(fail)?;
                    let flags: Vec<&String> = r.as_object().map(|o| o.iter().filter(|(k, v)| *v == "1" && *k != "contract_address").map(|(k, _)| k).collect()).unwrap_or_default();
                    json_text(json!({ "address": address, "is_contract": r["contract_address"] == "1", "risk_flags": flags, "clean": flags.is_empty() }))
                },
            ),
        ]
    }
}

// ---- DexScreener ----

pub struct DexScreener(PluginMeta);

pub fn dexscreener_plugin() -> Arc<dyn Plugin> {
    Arc::new(DexScreener(meta(
        "dexscreener",
        "DexScreener",
        "data",
        "Live DEX pairs on 80+ chains: price, liquidity, volume, buys/sells and pair age for any token.",
        "Adds DexScreener search, per-token pair lookups and the top-boosted tokens list, so the agent can answer about tokens on BNB Chain and beyond and spot what's getting attention.",
        &["read-market-data"],
    )))
}

#[async_trait]
impl Plugin for DexScreener {
    fn meta(&self) -> &PluginMeta {
        &self.0
    }
    fn read_tools(&self, _ctx: &Ctx) -> ToolSet {
        vec![
            Tool::new(
                "dexscreener_search",
                "Search DexScreener pairs by token name, symbol or address. Returns the most liquid matches across chains.",
                schema(&[("query", json!({ "type": "string", "minLength": 2 }), true), ("limit", json!({ "type": "integer", "minimum": 1, "maximum": 20 }), false)]),
                |v| async move {
                    let q = v["query"].as_str().ok_or("invalid input: query")?.to_string();
                    let mut pairs = dexscreener::search(&q).await.map_err(fail)?;
                    pairs.sort_by(|a, b| liq(b).partial_cmp(&liq(a)).unwrap_or(std::cmp::Ordering::Equal));
                    if pairs.is_empty() {
                        return Ok("No pairs found.".into());
                    }
                    let n = v["limit"].as_u64().unwrap_or(8) as usize;
                    json_text(Value::Array(pairs.iter().take(n).map(dexscreener::summarize_pair).collect()))
                },
            ),
            Tool::new(
                "dexscreener_token_pairs",
                "All DEX pairs for one token on one chain (DexScreener chain ids, e.g. bsc), deepest first.",
                schema(&[("chain", json!({ "type": "string" }), true), ("token", json!({ "type": "string" }), true)]),
                |v| async move {
                    let (c, t) = (v["chain"].as_str().ok_or("invalid input: chain")?.to_string(), v["token"].as_str().ok_or("invalid input: token")?.to_string());
                    let mut pairs = dexscreener::token_pairs(&c, &t).await.map_err(fail)?;
                    pairs.sort_by(|a, b| liq(b).partial_cmp(&liq(a)).unwrap_or(std::cmp::Ordering::Equal));
                    if pairs.is_empty() {
                        return Ok("No pairs found for that token.".into());
                    }
                    json_text(Value::Array(pairs.iter().take(10).map(dexscreener::summarize_pair).collect()))
                },
            ),
            Tool::new(
                "dexscreener_boosted",
                "Tokens with the most active DexScreener boosts right now (paid promotion; a signal of attention, not quality).",
                schema(&[("chain", json!({ "type": "string", "description": "Filter to one DexScreener chain id" }), false)]),
                |v| async move {
                    let chain = v["chain"].as_str().map(String::from);
                    let rows = dexscreener::top_boosted().await.map_err(fail)?;
                    let out: Vec<Value> = rows
                        .iter()
                        .filter(|b| chain.as_ref().is_none_or(|c| b["chainId"] == c.as_str()))
                        .take(20)
                        .map(|b| json!({ "chain": b["chainId"], "token": b["tokenAddress"], "boost": b["totalAmount"], "description": b["description"].as_str().map(|d| crate::http::clip(d, 140)) }))
                        .collect();
                    json_text(Value::Array(out))
                },
            ),
        ]
    }
}

// ---- GeckoTerminal ----

pub struct GeckoTerminal(PluginMeta);

pub fn geckoterminal_plugin() -> Arc<dyn Plugin> {
    Arc::new(GeckoTerminal(meta(
        "geckoterminal",
        "GeckoTerminal",
        "data",
        "Trending pools, pool search and OHLCV candles for DEX pools on 200+ networks.",
        "Adds GeckoTerminal data: what's trending on each network, pool search, and candles for any pool, including BNB Chain (PancakeSwap, four.meme graduates) and other major chains.",
        &["read-market-data"],
    )))
}

/// (unit, aggregate) for a timeframe name.
pub fn gecko_timeframe(tf: &str) -> Option<(&'static str, u32)> {
    match tf {
        "15m" => Some(("minute", 15)),
        "1h" => Some(("hour", 1)),
        "4h" => Some(("hour", 4)),
        "1d" => Some(("day", 1)),
        _ => None,
    }
}

#[async_trait]
impl Plugin for GeckoTerminal {
    fn meta(&self) -> &PluginMeta {
        &self.0
    }
    fn read_tools(&self, _ctx: &Ctx) -> ToolSet {
        vec![
            Tool::new(
                "geckoterminal_trending_pools",
                "Trending DEX pools on a GeckoTerminal network (e.g. bsc), or across all networks if omitted.",
                schema(&[("network", json!({ "type": "string" }), false), ("limit", json!({ "type": "integer", "minimum": 1, "maximum": 20 }), false)]),
                |v| async move {
                    let pools = geckoterminal::trending_pools(v["network"].as_str()).await.map_err(fail)?;
                    json_text(Value::Array(pools.into_iter().take(v["limit"].as_u64().unwrap_or(10) as usize).collect()))
                },
            ),
            Tool::new(
                "geckoterminal_search_pools",
                "Search GeckoTerminal pools by token name, symbol, or address, optionally on one network.",
                schema(&[("query", json!({ "type": "string", "minLength": 2 }), true), ("network", json!({ "type": "string" }), false)]),
                |v| async move {
                    let pools = geckoterminal::search_pools(v["query"].as_str().ok_or("invalid input: query")?, v["network"].as_str()).await.map_err(fail)?;
                    if pools.is_empty() {
                        return Ok("No pools found.".into());
                    }
                    json_text(Value::Array(pools.into_iter().take(10).collect()))
                },
            ),
            Tool::new(
                "geckoterminal_ohlcv",
                "OHLCV candles for a GeckoTerminal pool (oldest first).",
                schema(&[
                    ("network", json!({ "type": "string" }), true),
                    ("pool", json!({ "type": "string", "description": "Pool address from search or trending results" }), true),
                    ("timeframe", json!({ "type": "string", "enum": ["15m", "1h", "4h", "1d"] }), true),
                    ("limit", json!({ "type": "integer", "minimum": 10, "maximum": 500 }), false),
                ]),
                |v| async move {
                    let (unit, agg) = gecko_timeframe(v["timeframe"].as_str().unwrap_or("")).ok_or("invalid input: timeframe")?;
                    let candles = geckoterminal::ohlcv(v["network"].as_str().ok_or("invalid input: network")?, v["pool"].as_str().ok_or("invalid input: pool")?, unit, agg, v["limit"].as_u64().unwrap_or(100) as u32).await.map_err(fail)?;
                    json_text(serde_json::to_value(candles).unwrap())
                },
            ),
        ]
    }
}

// ---- Market Pulse ----

pub struct MarketPulse(PluginMeta);

pub fn market_pulse() -> Arc<dyn Plugin> {
    Arc::new(MarketPulse(meta(
        "market-pulse",
        "Market Pulse",
        "data",
        "Whole-market context: total crypto market cap, BTC dominance, trending coins and the Fear & Greed Index.",
        "Adds a market overview tool backed by CoinGecko and alternative.me, so the agent can frame answers and trades in the broader market: risk-on or risk-off, what's trending, and where sentiment sits over the last week.",
        &["read-market-data"],
    )))
}

type Cache = LazyLock<Mutex<Option<(Instant, Value)>>>;

/// A public response, cached so many agents share one request.
pub async fn cached_json(cache: &'static Cache, ttl: Duration, url: &str) -> crate::Result<Value> {
    if let Some((at, v)) = cache.lock().unwrap().clone() {
        if at.elapsed() < ttl {
            return Ok(v);
        }
    }
    let res = crate::http::client().get(url).header("accept", "application/json").timeout(Duration::from_secs(10)).send().await.map_err(|e| crate::Error::internal(e.to_string()))?;
    if !res.status().is_success() {
        let host = url::Url::parse(url).ok().and_then(|u| u.host_str().map(String::from)).unwrap_or_default();
        return Err(crate::Error::internal(format!("{host} returned {}", res.status().as_u16())));
    }
    let v: Value = res.json().await.map_err(|e| crate::Error::internal(e.to_string()))?;
    *cache.lock().unwrap() = Some((Instant::now(), v.clone()));
    Ok(v)
}

static GLOBAL: Cache = LazyLock::new(Default::default);
static TRENDING: Cache = LazyLock::new(Default::default);
static FEAR_GREED: Cache = LazyLock::new(Default::default);

#[async_trait]
impl Plugin for MarketPulse {
    fn meta(&self) -> &PluginMeta {
        &self.0
    }
    fn read_tools(&self, _ctx: &Ctx) -> ToolSet {
        vec![Tool::new(
            "get_market_overview",
            "Broad crypto market snapshot: total market cap and 24h change, 24h volume, BTC/ETH dominance, top trending coins on CoinGecko, and the Fear & Greed Index for the last 7 days.",
            schema(&[]),
            |_| async move {
                let g = cached_json(&GLOBAL, Duration::from_secs(60), "https://api.coingecko.com/api/v3/global").await;
                let t = cached_json(&TRENDING, Duration::from_secs(300), "https://api.coingecko.com/api/v3/search/trending").await;
                let f = cached_json(&FEAR_GREED, Duration::from_secs(600), "https://api.alternative.me/fng/?limit=7").await;
                let market = match g {
                    Ok(g) => {
                        let d = &g["data"];
                        json!({
                            "total_market_cap_usd": d["total_market_cap"]["usd"].as_f64().unwrap_or(0.0).round(),
                            "market_cap_change_24h_pct": round(d["market_cap_change_percentage_24h_usd"].as_f64().unwrap_or(0.0), 2),
                            "volume_24h_usd": d["total_volume"]["usd"].as_f64().unwrap_or(0.0).round(),
                            "btc_dominance_pct": round(d["market_cap_percentage"]["btc"].as_f64().unwrap_or(0.0), 2),
                            "eth_dominance_pct": round(d["market_cap_percentage"]["eth"].as_f64().unwrap_or(0.0), 2),
                        })
                    }
                    Err(_) => json!("unavailable"),
                };
                let trending = match t {
                    Ok(t) => Value::Array(t["coins"].as_array().cloned().unwrap_or_default().iter().take(10).map(|c| json!({ "name": c["item"]["name"], "symbol": c["item"]["symbol"], "rank": c["item"]["market_cap_rank"] })).collect()),
                    Err(_) => json!("unavailable"),
                };
                let fear_greed = match f {
                    Ok(f) => Value::Array(
                        f["data"]
                            .as_array()
                            .cloned()
                            .unwrap_or_default()
                            .iter()
                            .map(|d| {
                                let ts: i64 = d["timestamp"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
                                let date = chrono::DateTime::from_timestamp(ts, 0).map(|t| t.format("%Y-%m-%d").to_string()).unwrap_or_default();
                                json!({ "date": date, "value": d["value"].as_str().and_then(|s| s.parse::<f64>().ok()), "label": d["value_classification"] })
                            })
                            .collect(),
                    ),
                    Err(_) => json!("unavailable"),
                };
                json_text(json!({ "market": market, "trending": trending, "fear_greed": fear_greed }))
            },
        )]
    }
}

// ---- Knowledge base ----

pub const KNOWLEDGE_DOCS: usize = 50;
pub const KNOWLEDGE_CHARS_PER_DOC: usize = 50_000;

pub struct KnowledgeBase(PluginMeta);

pub fn knowledge_base() -> Arc<dyn Plugin> {
    Arc::new(KnowledgeBase(meta(
        "knowledge-base",
        "Custom Knowledge Base",
        "knowledge",
        "Teach the agent your own material: project docs, roadmap, FAQs, trading playbooks.",
        "Upload text or Markdown documents. The agent sees their titles in its system knowledge and searches them when answering holders or planning trades, and it cites the document it used.",
        &["knowledge"],
    )))
}

fn tokenize(text: &str) -> Vec<String> {
    static RE: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"[a-z0-9$]{2,}").unwrap());
    RE.find_iter(&text.to_lowercase()).map(|m| m.as_str().to_string()).collect()
}

/// Split docs into paragraph-sized chunks and rank them with BM25 against the query.
pub fn search_chunks(docs: &[(String, String)], query: &str, limit: usize) -> Vec<(String, String, f64)> {
    static PARA: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"\n\s*\n").unwrap());
    let mut chunks: Vec<(String, String, Vec<String>)> = vec![];
    for (title, content) in docs {
        let mut acc: Vec<String> = vec![];
        for p in PARA.split(content).map(str::trim).filter(|p| !p.is_empty()) {
            // Merge short paragraphs so chunks carry enough context (~1,200 chars).
            match acc.last_mut() {
                Some(last) if last.encode_utf16().count() + p.encode_utf16().count() < 1200 => {
                    last.push_str("\n\n");
                    last.push_str(p);
                }
                _ => acc.push(p.to_string()),
            }
        }
        for text in acc {
            let terms = tokenize(&format!("{title} {text}"));
            chunks.push((title.clone(), text, terms));
        }
    }
    if chunks.is_empty() {
        return vec![];
    }
    let mut q: Vec<String> = vec![];
    for t in tokenize(query) {
        if !q.contains(&t) {
            q.push(t);
        }
    }
    let n = chunks.len() as f64;
    let avg = chunks.iter().map(|c| c.2.len() as f64).sum::<f64>() / n;
    let df: Vec<f64> = q.iter().map(|t| chunks.iter().filter(|c| c.2.contains(t)).count() as f64).collect();
    let (k1, b) = (1.2, 0.75);
    let mut scored: Vec<(String, String, f64)> = chunks
        .into_iter()
        .map(|(title, text, terms)| {
            let mut score = 0.0;
            for (i, t) in q.iter().enumerate() {
                let tf = terms.iter().filter(|x| *x == t).count() as f64;
                if tf == 0.0 {
                    continue;
                }
                let idf = (1.0 + (n - df[i] + 0.5) / (df[i] + 0.5)).ln();
                score += idf * tf * (k1 + 1.0) / (tf + k1 * (1.0 - b + b * terms.len() as f64 / avg));
            }
            (title, text, score)
        })
        .filter(|c| c.2 > 0.0)
        .collect();
    scored.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit);
    scored
}

async fn docs_for(agent_id: &str) -> crate::Result<Vec<KnowledgeDoc>> {
    Ok(sqlx::query_as("select * from knowledge_docs where agent_id = $1 order by id").bind(agent_id).fetch_all(db()).await?)
}

#[async_trait]
impl Plugin for KnowledgeBase {
    fn meta(&self) -> &PluginMeta {
        &self.0
    }
    async fn knowledge(&self, ctx: &Ctx) -> Option<String> {
        let docs = docs_for(&ctx.agent.id).await.ok()?;
        if docs.is_empty() {
            return Some(String::new());
        }
        let titles: Vec<String> = docs.iter().map(|d| format!("\"{}\"", d.title)).collect();
        Some(format!("Your creator gave you a knowledge base with these documents: {}. Search it with search_knowledge before answering questions it may cover, and name the document you used.", titles.join(", ")))
    }
    fn read_tools(&self, ctx: &Ctx) -> ToolSet {
        let agent_id = ctx.agent.id.clone();
        vec![Tool::new(
            "search_knowledge",
            "Search the creator's knowledge base documents. Returns the most relevant passages with their document titles.",
            schema(&[("query", json!({ "type": "string", "minLength": 2 }), true), ("limit", json!({ "type": "integer", "minimum": 1, "maximum": 10 }), false)]),
            move |v| {
                let agent_id = agent_id.clone();
                async move {
                    let query = v["query"].as_str().ok_or("invalid input: query")?.to_string();
                    let docs: Vec<(String, String)> = docs_for(&agent_id).await.map_err(fail)?.into_iter().map(|d| (d.title, d.content)).collect();
                    let hits = search_chunks(&docs, &query, v["limit"].as_u64().unwrap_or(5) as usize);
                    if hits.is_empty() {
                        return Ok("No matching passages in the knowledge base.".into());
                    }
                    json_text(Value::Array(hits.into_iter().map(|(t, p, _)| json!({ "document": t, "passage": p })).collect()))
                }
            },
        )]
    }
}
