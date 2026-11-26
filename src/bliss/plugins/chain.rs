//! Onchain Flows (who's buying and selling a BNB Chain token, from its Transfer events) and
//! prediction markets on Opinion (read-only).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::Filter;
use alloy::sol;
use alloy::sol_types::SolEvent;
use async_trait::async_trait;
use serde_json::{json, Value};

use super::data::{fail, meta};
use crate::ai::{schema, Tool, ToolSet};
use crate::bliss::settings::{number, SettingField};
use crate::bliss::{json_text, round, Ctx, Plugin, PluginMeta};
use crate::env::env;
use crate::error::{Error, Result};
use crate::evm;

sol! {
    event Transfer(address indexed from, address indexed to, uint256 value);
}

const MAX_LOGS: usize = 20_000;

struct PoolInfo {
    pools: HashSet<String>,
    price_usd: Option<f64>,
    symbol: Option<String>,
}

async fn token_pools(token: &str) -> PoolInfo {
    let empty = PoolInfo { pools: HashSet::new(), price_usd: None, symbol: None };
    let Ok(res) = crate::http::client().get(format!("https://api.dexscreener.com/latest/dex/tokens/{token}")).timeout(std::time::Duration::from_secs(10)).send().await else { return empty };
    if !res.status().is_success() {
        return empty;
    }
    let data: Value = res.json().await.unwrap_or(Value::Null);
    let mine: Vec<Value> = data["pairs"].as_array().cloned().unwrap_or_default().into_iter().filter(|p| p["chainId"] == "bsc" && p["baseToken"]["address"].as_str().is_some_and(|a| a.eq_ignore_ascii_case(token))).collect();
    let best = mine.iter().max_by(|a, b| a["liquidity"]["usd"].as_f64().unwrap_or(0.0).partial_cmp(&b["liquidity"]["usd"].as_f64().unwrap_or(0.0)).unwrap_or(std::cmp::Ordering::Equal));
    PoolInfo {
        pools: mine.iter().filter_map(|p| p["pairAddress"].as_str().map(str::to_lowercase)).collect(),
        price_usd: best.and_then(|b| b["priceUsd"].as_str().and_then(|p| p.parse().ok())),
        symbol: best.and_then(|b| b["baseToken"]["symbol"].as_str().map(String::from)),
    }
}

/// Buys vs sells, whales and net buyers/sellers for a token over the last `hours`.
pub async fn flows(token: &str, hours: f64) -> Result<Value> {
    let p = evm::bsc();
    let addr = evm::parse_address(token)?;
    let latest = p.get_block_number().await.map_err(evm::rpc_error)?;
    // Measure the block time instead of assuming it (it has changed with hard forks).
    let t_now = p.get_block_by_number(latest.into()).await.map_err(evm::rpc_error)?.map(|b| b.header.timestamp).unwrap_or(0);
    let t_back = p.get_block_by_number((latest - 1000).into()).await.map_err(evm::rpc_error)?.map(|b| b.header.timestamp).unwrap_or(0);
    let spb = (t_now.saturating_sub(t_back)) as f64 / 1000.0;
    let span = ((hours * 3600.0) / spb.max(0.001)).ceil() as u64;
    let from = latest.saturating_sub(span);
    let decimals = evm::token_decimals("bsc", token).await?;
    let meta = token_pools(token).await;
    let mut logs = vec![];
    // RPCs cap how many blocks one getLogs may cover: shrink the window when told so.
    let mut chunk: u64 = 1000;
    let mut start = from;
    while start <= latest && logs.len() < MAX_LOGS {
        let end = (start + chunk - 1).min(latest);
        let filter = Filter::new().address(addr).event_signature(Transfer::SIGNATURE_HASH).from_block(start).to_block(end);
        match p.get_logs(&filter).await {
            Ok(l) => {
                logs.extend(l);
                start = end + 1;
            }
            Err(e) => {
                let msg = e.to_string().to_lowercase();
                if chunk <= 25 || !["limit", "too many", "exceed", "range", "large"].iter().any(|w| msg.contains(w)) {
                    return Err(evm::rpc_error(e));
                }
                chunk /= 4;
            }
        }
    }
    let unit = 10f64.powi(decimals as i32);
    let usd = |n: f64| meta.price_usd.map(|p| round(n * p, 2));
    let zero = format!("{:#x}", Address::ZERO);
    let (mut buys, mut sells) = (0.0, 0.0);
    let mut net: HashMap<String, f64> = HashMap::new();
    let mut wallets: HashSet<String> = HashSet::new();
    let mut moves: Vec<(String, String, f64, &'static str, String)> = vec![];
    for l in &logs {
        let Ok(ev) = Transfer::decode_log(&l.inner) else { continue };
        let (f, t) = (format!("{:#x}", ev.from), format!("{:#x}", ev.to));
        let amount = ev.value.to_string().parse::<f64>().unwrap_or(0.0) / unit;
        let kind = if meta.pools.contains(&f) {
            "buy"
        } else if meta.pools.contains(&t) {
            "sell"
        } else if f == zero {
            "mint"
        } else if t == zero {
            "burn"
        } else {
            "transfer"
        };
        if kind == "buy" {
            buys += amount;
            *net.entry(t.clone()).or_default() += amount;
        }
        if kind == "sell" {
            sells += amount;
            *net.entry(f.clone()).or_default() -= amount;
        }
        for a in [&f, &t] {
            if !meta.pools.contains(a) && *a != zero {
                wallets.insert(a.clone());
            }
        }
        moves.push((f, t, amount, kind, l.transaction_hash.map(|h| format!("{h:#x}")).unwrap_or_default()));
    }
    let mut ranked: Vec<(String, f64)> = net.into_iter().collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let top_buyers: Vec<&(String, f64)> = ranked.iter().filter(|(_, v)| *v > 0.0).take(8).collect();
    let sellers: Vec<&(String, f64)> = ranked.iter().filter(|(_, v)| *v < 0.0).collect();
    let top_sellers: Vec<&(String, f64)> = sellers.iter().rev().take(8).copied().collect();
    let top5: f64 = top_buyers.iter().take(5).map(|(_, v)| v).sum();
    moves.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    Ok(json!({
        "token": token, "symbol": meta.symbol,
        "window": { "hours": hours, "blocks": span, "seconds_per_block": round(spb, 3), "truncated": logs.len() >= MAX_LOGS },
        "transfers": logs.len(), "wallets": wallets.len(), "dex_pools_seen": meta.pools.len(),
        "buys": { "tokens": round(buys, 2), "usd": usd(buys) },
        "sells": { "tokens": round(sells, 2), "usd": usd(sells) },
        "net_buy_pressure_pct": if buys + sells > 0.0 { json!(round((buys - sells) / (buys + sells) * 100.0, 1)) } else { Value::Null },
        "top5_buyers_share_of_buys_pct": if buys > 0.0 { json!(round(top5 / buys * 100.0, 1)) } else { Value::Null },
        "biggest_moves": moves.iter().take(10).map(|(f, t, a, k, tx)| json!({ "from": f, "to": t, "amount": round(*a, 2), "kind": k, "tx": tx, "usd": usd(*a) })).collect::<Vec<_>>(),
        "top_net_buyers": top_buyers.iter().map(|(w, v)| json!({ "wallet": w, "tokens": round(*v, 2), "usd": usd(*v) })).collect::<Vec<_>>(),
        "top_net_sellers": top_sellers.iter().map(|(w, v)| json!({ "wallet": w, "tokens": round(-v, 2), "usd": usd(-v) })).collect::<Vec<_>>(),
    }))
}

pub struct OnchainFlows(PluginMeta);

pub fn onchain_flows() -> Arc<dyn Plugin> {
    Arc::new(OnchainFlows(meta(
        "onchain-flows",
        "Onchain Flows",
        "data",
        "Who is really buying and selling a BNB Chain token: buys vs sells, whales, net buyers and sellers, read from the chain.",
        "Reads a token's Transfer events on BNB Chain over a recent window (up to 6 hours), recognises its DEX pools so pool outflows count as buys and inflows as sells, and reports buy/sell volume, net buy pressure, the biggest transfers, the top net buyers and sellers, and how much of the buying comes from five wallets. Straight from the chain, no indexer.",
        &["read-market-data"],
    )))
}

#[async_trait]
impl Plugin for OnchainFlows {
    fn meta(&self) -> &PluginMeta {
        &self.0
    }
    async fn knowledge(&self, _ctx: &Ctx) -> Option<String> {
        Some("## Reading flows\nBuying that comes from a handful of wallets is weaker than broad buying. Big transfers between wallets aren't buys. Combine flows with liquidity and security checks before trusting a move.".into())
    }
    fn read_tools(&self, _ctx: &Ctx) -> ToolSet {
        vec![Tool::new(
            "get_token_flows",
            "Buys vs sells, whales and net buyers/sellers for a BNB Chain token over the last 0.25 to 6 hours, read from its Transfer events.",
            schema(&[("token_address", json!({ "type": "string", "pattern": "^0x[0-9a-fA-F]{40}$" }), true), ("hours", json!({ "type": "number", "minimum": 0.25, "maximum": 6 }), false)]),
            |v| async move {
                let token = v["token_address"].as_str().filter(|t| evm::is_address(t)).ok_or("invalid input: token_address")?.to_string();
                let hours = v["hours"].as_f64().unwrap_or(1.0);
                if !(0.25..=6.0).contains(&hours) {
                    return Err("invalid input: hours".into());
                }
                match flows(&token, hours).await {
                    Ok(f) => json_text(f),
                    Err(e) => json_text(json!({ "error": format!("couldn't read the chain: {}", e.message) })),
                }
            },
        )]
    }
}

// ---- Opinion (prediction markets) ----

pub struct Opinion(PluginMeta);

pub fn prediction_markets() -> Arc<dyn Plugin> {
    Arc::new(Opinion(meta(
        "opinion",
        "Prediction markets (Opinion)",
        "data",
        "Live odds from Opinion, BNB Chain's prediction market: markets, prices, order books.",
        "Lets the agent browse Opinion's prediction markets on BNB Chain and read each market's rules, current Yes/No prices (the market's implied probability), order book and price history. Read-only: the agent doesn't trade on Opinion.",
        &["read-market-data", "third-party"],
    )))
}

async fn opinion(path: &str, query: &[(&str, String)]) -> Result<Value> {
    let e = env();
    let key = e.opinion_api_key.clone().ok_or_else(|| Error::unavailable("Opinion market data isn't set up on this Orvyn instance (OPINION_API_KEY)"))?;
    let res = crate::http::client()
        .get(format!("{}{path}", e.opinion_api_url.trim_end_matches('/')))
        .query(query)
        .header("apikey", key)
        .header("user-agent", "Orvyn/1.0")
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| Error::internal(format!("Opinion: {e}")))?;
    let status = res.status();
    let body: Value = res.json().await.unwrap_or(Value::Null);
    if !status.is_success() || body.is_null() {
        return Err(Error::internal(format!("Opinion: HTTP {}", status.as_u16())));
    }
    if body["errno"].as_i64() != Some(0) {
        let msg = body["errmsg"].as_str().filter(|m| !m.is_empty()).map(String::from).unwrap_or_else(|| format!("error {}", body["errno"]));
        return Err(Error::internal(format!("Opinion: {msg}")));
    }
    Ok(body["result"].clone())
}

fn market_view(m: &Value) -> Value {
    let status = match m["status"].as_i64() {
        Some(1) => json!("created"),
        Some(2) => json!("trading"),
        Some(3) => json!("resolving"),
        Some(4) => json!("resolved"),
        Some(5) => json!("failed"),
        Some(6) => json!("deleted"),
        _ => if m["statusEnum"].is_null() { m["status"].clone() } else { m["statusEnum"].clone() },
    };
    let or = |v: &Value, d: &str| if v.is_null() { json!(d) } else { v.clone() };
    let mut out = json!({
        "id": m["marketId"], "title": m["marketTitle"], "status": status,
        "yes": { "label": or(&m["yesLabel"], "Yes"), "tokenId": m["yesTokenId"] },
        "no": { "label": or(&m["noLabel"], "No"), "tokenId": m["noTokenId"] },
        "volume": m["volume"], "closesAt": m["cutoffAt"],
    });
    let truthy = |v: &Value| !(v.is_null() || v == &json!(0) || v == &json!("") || v == &json!(false));
    if truthy(&m["resolvedAt"]) {
        out["resolvedAt"] = m["resolvedAt"].clone();
    }
    if truthy(&m["resultTokenId"]) {
        out["winningTokenId"] = m["resultTokenId"].clone();
    }
    if let Some(children) = m["childMarkets"].as_array() {
        out["outcomes"] = Value::Array(children.iter().map(market_view).collect());
    }
    out
}

#[async_trait]
impl Plugin for Opinion {
    fn meta(&self) -> &PluginMeta {
        &self.0
    }
    fn settings(&self) -> Vec<SettingField> {
        vec![number("maxResults", "Markets per search", None, 10.0, 1.0, 20.0, Some(1.0), None)]
    }
    async fn knowledge(&self, _ctx: &Ctx) -> Option<String> {
        Some("On a prediction market each outcome token pays $1 if it happens and $0 if not, so its price (0.01 to 0.99) is the market's implied probability. Prices move with news; thin order books move a lot on small trades. A market resolves by its stated rules, not by headlines.".into())
    }
    fn read_tools(&self, ctx: &Ctx) -> ToolSet {
        let max = ctx.num("maxResults") as u64;
        vec![
            Tool::new(
                "prediction_markets",
                "List Opinion prediction markets that are trading, sorted by 24h volume (default), total volume, newest or closing soon.",
                schema(&[("sort", json!({ "type": "string", "enum": ["volume_24h", "volume", "volume_7d", "newest", "closing_soon"] }), false), ("page", json!({ "type": "integer", "minimum": 1 }), false)]),
                move |v| async move {
                    let sort = match v["sort"].as_str().unwrap_or("volume_24h") {
                        "newest" => 1,
                        "closing_soon" => 2,
                        "volume" => 3,
                        "volume_7d" => 7,
                        _ => 5,
                    };
                    let page = v["page"].as_u64().unwrap_or(1);
                    let q = [("chainId", "56".to_string()), ("marketType", "2".into()), ("status", "activated".into()), ("sortBy", sort.to_string()), ("page", page.to_string()), ("limit", max.to_string())];
                    match opinion("/market", &q).await {
                        Ok(r) => json_text(json!({ "total": r["total"], "page": page, "markets": r["list"].as_array().map(|l| l.iter().map(market_view).collect::<Vec<_>>()).unwrap_or_default() })),
                        Err(e) => Ok(Value::String(format!("Couldn't list markets: {}", e.message))),
                    }
                },
            ),
            Tool::new(
                "prediction_market",
                "One Opinion market: its rules, outcomes (with token ids), status and the latest Yes/No prices.",
                schema(&[("market_id", json!({ "type": "integer", "exclusiveMinimum": 0 }), true)]),
                |v| async move {
                    let id = v["market_id"].as_u64().ok_or("invalid input: market_id")?;
                    let r: Result<Value> = async {
                        let data = opinion(&format!("/market/{id}"), &[]).await?["data"].clone();
                        let mut m = market_view(&data);
                        let price = |t: Value| async move {
                            if t.is_null() {
                                return Value::Null;
                            }
                            let id = t.as_str().map(String::from).unwrap_or_else(|| t.to_string());
                            opinion("/token/latest-price", &[("token_id", id)]).await.unwrap_or(Value::Null)
                        };
                        let yes = price(m["yes"]["tokenId"].clone()).await;
                        let no = price(m["no"]["tokenId"].clone()).await;
                        m["rules"] = data["rules"].clone();
                        m["prices"] = json!({ "yes": yes, "no": no });
                        Ok(m)
                    }
                    .await;
                    match r {
                        Ok(m) => json_text(m),
                        Err(e) => Ok(Value::String(format!("Couldn't read the market: {}", e.message))),
                    }
                },
            ),
            Tool::new(
                "prediction_orderbook",
                "The order book for one outcome token of an Opinion market (bids and asks as [price, shares]).",
                schema(&[("token_id", json!({ "type": "string" }), true)]),
                |v| async move {
                    let t = v["token_id"].as_str().ok_or("invalid input: token_id")?.to_string();
                    match opinion("/token/orderbook", &[("token_id", t)]).await {
                        Ok(b) => {
                            let take = |k: &str| b[k].as_array().map(|a| a.iter().take(10).cloned().collect::<Vec<_>>()).unwrap_or_default();
                            json_text(json!({ "lastPrice": b["lastPrice"], "bids": take("bids"), "asks": take("asks") }))
                        }
                        Err(e) => Ok(Value::String(format!("Couldn't read the order book: {}", e.message))),
                    }
                },
            ),
            Tool::new(
                "prediction_price_history",
                "Price history of one outcome token (its implied probability over time).",
                schema(&[("token_id", json!({ "type": "string" }), true), ("interval", json!({ "type": "string", "enum": ["1h", "1d", "1w", "max"] }), false)]),
                |v| async move {
                    let t = v["token_id"].as_str().ok_or("invalid input: token_id")?.to_string();
                    let interval = v["interval"].as_str().unwrap_or("1d").to_string();
                    match opinion("/token/price-history", &[("token_id", t), ("interval", interval)]).await {
                        Ok(h) => json_text(h),
                        Err(e) => Ok(Value::String(format!("Couldn't read price history: {}", e.message))),
                    }
                },
            ),
        ]
    }
}

#[allow(dead_code)]
fn _unused(_: U256) {
    let _ = fail;
}
