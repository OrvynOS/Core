//! Market data plugins: Technical Analysis, DeFi Yields, Derivatives Data, Polymarket.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::data::{cached_json, fail, gecko_timeframe, meta};
use crate::ai::{schema, Tool, ToolSet};
use crate::bliss::{json_text, round, Ctx, Plugin, PluginMeta};
use crate::data::geckoterminal::{self, Candle};

async fn get(url: &str) -> crate::Result<Value> {
    let res = crate::http::client().get(url).header("accept", "application/json").timeout(Duration::from_secs(12)).send().await.map_err(|e| crate::Error::internal(e.to_string()))?;
    if !res.status().is_success() {
        let host = url::Url::parse(url).ok().and_then(|u| u.host_str().map(String::from)).unwrap_or_default();
        return Err(crate::Error::internal(format!("{host} returned {}", res.status().as_u16())));
    }
    res.json().await.map_err(|e| crate::Error::internal(e.to_string()))
}

fn n(v: &Value) -> f64 {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok())).unwrap_or(0.0)
}

// ---- Technical analysis ----

fn ema(values: &[f64], period: usize) -> Vec<f64> {
    let k = 2.0 / (period as f64 + 1.0);
    let mut out: Vec<f64> = Vec::with_capacity(values.len());
    for (i, v) in values.iter().enumerate() {
        out.push(if i == 0 { *v } else { v * k + out[i - 1] * (1.0 - k) });
    }
    out
}

fn rsi(closes: &[f64], period: usize) -> Option<f64> {
    if closes.len() <= period {
        return None;
    }
    let (mut gain, mut loss) = (0.0, 0.0);
    for i in 1..=period {
        let d = closes[i] - closes[i - 1];
        if d >= 0.0 { gain += d } else { loss -= d }
    }
    gain /= period as f64;
    loss /= period as f64;
    for i in period + 1..closes.len() {
        let d = closes[i] - closes[i - 1];
        gain = (gain * (period as f64 - 1.0) + d.max(0.0)) / period as f64;
        loss = (loss * (period as f64 - 1.0) + (-d).max(0.0)) / period as f64;
    }
    Some(if loss == 0.0 { 100.0 } else { 100.0 - 100.0 / (1.0 + gain / loss) })
}

fn atr(c: &[Candle], period: usize) -> Option<f64> {
    if c.len() <= period {
        return None;
    }
    let trs: Vec<f64> = c[1..].iter().enumerate().map(|(i, x)| (x.h - x.l).max((x.h - c[i].c).abs()).max((x.l - c[i].c).abs())).collect();
    ema(&trs, period).last().copied()
}

/// Indicator summary for a candle series (oldest first).
pub fn analyze(candles: &[Candle]) -> Value {
    let closes: Vec<f64> = candles.iter().map(|c| c.c).collect();
    let last = *closes.last().unwrap_or(&0.0);
    let e20 = *ema(&closes, 20).last().unwrap_or(&0.0);
    let e50 = *ema(&closes, 50).last().unwrap_or(&0.0);
    let e200 = (closes.len() >= 200).then(|| *ema(&closes, 200).last().unwrap());
    let e12 = ema(&closes, 12);
    let e26 = ema(&closes, 26);
    let macd: Vec<f64> = e12.iter().zip(&e26).map(|(a, b)| a - b).collect();
    let signal = ema(&macd, 9);
    let r = rsi(&closes, 14);
    let window = &closes[closes.len().saturating_sub(20)..];
    let mean = window.iter().sum::<f64>() / window.len() as f64;
    let sd = (window.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / window.len() as f64).sqrt();
    let a = atr(candles, 14);
    let recent = &candles[candles.len().saturating_sub(50)..];
    let high = recent.iter().map(|c| c.h).fold(f64::MIN, f64::max);
    let low = recent.iter().map(|c| c.l).fold(f64::MAX, f64::min);
    let pct = |v: f64| round((v - last) / last * 100.0, 2);
    let trend = if last > e20 && e20 > e50 { "uptrend" } else if last < e20 && e20 < e50 { "downtrend" } else { "range" };
    let (ml, sl) = (macd.last().copied(), signal.last().copied());
    json!({
        "last": last, "candles": candles.len(), "trend": trend,
        "rsi14": r.map(|x| round(x, 1)), "ema20": e20, "ema50": e50, "ema200": e200,
        "macd": { "line": ml, "signal": sl, "histogram": ml.unwrap_or(0.0) - sl.unwrap_or(0.0) },
        "bollinger20": { "upper": mean + 2.0 * sd, "middle": mean, "lower": mean - 2.0 * sd, "width_pct": round(4.0 * sd / mean * 100.0, 2) },
        "atr14": a, "atr14_pct": a.map(|a| round(a / last * 100.0, 2)),
        "range_50": { "high": high, "low": low, "high_dist_pct": pct(high), "low_dist_pct": pct(low) },
    })
}

pub struct TechnicalAnalysis(PluginMeta);

pub fn technical_analysis() -> Arc<dyn Plugin> {
    Arc::new(TechnicalAnalysis(meta(
        "technical-analysis",
        "Technical Analysis",
        "data",
        "RSI, EMAs, MACD, Bollinger Bands, ATR and trend for any BSC pool (PancakeSwap and more) on GeckoTerminal.",
        "Computes standard indicators from candles so the agent reasons from numbers rather than eyeballing prices. Works on any DEX pool GeckoTerminal tracks on BNB Chain, in chat and on autopilot.",
        &["read-market-data"],
    )))
}

#[async_trait]
impl Plugin for TechnicalAnalysis {
    fn meta(&self) -> &PluginMeta {
        &self.0
    }
    async fn knowledge(&self, _ctx: &Ctx) -> Option<String> {
        Some("When judging price action, call technical_indicators rather than estimating indicators yourself. Treat indicators as context, not signals on their own.".into())
    }
    fn read_tools(&self, _ctx: &Ctx) -> ToolSet {
        vec![Tool::new(
            "technical_indicators",
            "Indicator summary from ~200 candles: trend (EMA20/50 stack), RSI14, EMA20/50/200, MACD(12,26,9), Bollinger(20,2), ATR14 and the 50-candle range.",
            schema(&[
                ("source", json!({ "type": "string", "enum": ["geckoterminal"], "default": "geckoterminal", "description": "Candle source" }), false),
                ("symbol", json!({ "type": "string", "description": "'network:poolAddress', e.g. 'bsc:0x…' (a PancakeSwap or other BSC pool)" }), true),
                ("interval", json!({ "type": "string", "enum": ["15m", "1h", "4h", "1d"] }), true),
            ]),
            |v| async move {
                let symbol = v["symbol"].as_str().ok_or("invalid input: symbol")?.to_string();
                let interval = v["interval"].as_str().ok_or("invalid input: interval")?.to_string();
                let (network, pool) = match symbol.split_once(':') {
                    Some((a, b)) => (a.to_string(), b.to_string()),
                    None => ("bsc".into(), symbol.clone()),
                };
                if network.is_empty() || pool.is_empty() {
                    return Ok("Pass symbol as 'network:poolAddress', e.g. 'bsc:0x…'.".into());
                }
                let (unit, agg) = gecko_timeframe(&interval).ok_or("invalid input: interval")?;
                let candles = geckoterminal::ohlcv(&network, &pool, unit, agg, 200).await.map_err(fail)?;
                if candles.len() < 30 {
                    return Ok(Value::String(format!("Only {} candles available; not enough history for indicators.", candles.len())));
                }
                let mut out = json!({ "source": v["source"].as_str().unwrap_or("geckoterminal"), "symbol": symbol, "interval": interval });
                for (k, val) in analyze(&candles).as_object().unwrap() {
                    out[k] = val.clone();
                }
                json_text(out)
            },
        )]
    }
}

// ---- DeFi yields ----

pub struct DefiYields(PluginMeta);

pub fn defi_yields() -> Arc<dyn Plugin> {
    Arc::new(DefiYields(meta(
        "defi-yields",
        "DeFi Yields",
        "data",
        "Where yield really is across 15,000 DeFi pools: organic APY vs reward emissions, 30-day means, outliers, IL risk.",
        "Search DefiLlama's yields dataset by chain, asset, project and size. Each pool shows organic yield (fees and interest) apart from reward emissions, its 30-day mean, the 7-day trend, impermanent-loss risk and whether it's a statistical outlier. Read-only.",
        &["read-market-data", "third-party"],
    )))
}

static POOLS: LazyLock<std::sync::Mutex<Option<(std::time::Instant, Value)>>> = LazyLock::new(Default::default);

fn opt_round(v: &Value, d: i32) -> Value {
    if v.is_null() { Value::Null } else { json!(round(n(v), d)) }
}

fn yield_row(p: &Value) -> Value {
    json!({
        "project": p["project"], "chain": p["chain"], "symbol": p["symbol"], "tvl_usd": n(&p["tvlUsd"]).round(),
        "apy_pct": round(n(&p["apy"]), 2), "organic_apy_pct": round(n(&p["apyBase"]), 2), "reward_apy_pct": round(n(&p["apyReward"]), 2),
        "apy_30d_mean_pct": opt_round(&p["apyMean30d"], 2), "apy_change_7d_pct": opt_round(&p["apyPct7D"], 2),
        "stablecoin": p["stablecoin"], "il_risk": p["ilRisk"], "exposure": p["exposure"], "outlier": p["outlier"],
        "outlook": p["predictions"]["predictedClass"], "pool_id": p["pool"], "url": format!("https://defillama.com/yields/pool/{}", p["pool"].as_str().unwrap_or("")),
    })
}

#[async_trait]
impl Plugin for DefiYields {
    fn meta(&self) -> &PluginMeta {
        &self.0
    }
    async fn knowledge(&self, _ctx: &Ctx) -> Option<String> {
        Some("## Judging a yield\nPrefer organic APY (fees, interest) over reward APY (token emissions, which fall as the token does). Distrust outliers and pools under $1M TVL. Compare today's APY with the 30-day mean. Lending a stablecoin has no impermanent loss; LP positions do.".into())
    }
    fn read_tools(&self, _ctx: &Ctx) -> ToolSet {
        vec![Tool::new(
            "find_yields",
            "Find yield pools: filter by chain (e.g. BSC, Ethereum, Base, Arbitrum), asset symbol (USDT, USDC, BNB…), project (venus-core-pool, pancakeswap-amm-v3…), stablecoins only, and minimum TVL; sorted by organic or total APY.",
            schema(&[
                ("chain", json!({ "type": "string", "maxLength": 30 }), false),
                ("symbol", json!({ "type": "string", "maxLength": 30 }), false),
                ("project", json!({ "type": "string", "maxLength": 60 }), false),
                ("stablecoin_only", json!({ "type": "boolean" }), false),
                ("min_tvl_usd", json!({ "type": "number", "minimum": 0 }), false),
                ("sort", json!({ "type": "string", "enum": ["organic", "total", "tvl"] }), false),
                ("include_outliers", json!({ "type": "boolean" }), false),
                ("limit", json!({ "type": "integer", "minimum": 1, "maximum": 30 }), false),
            ]),
            |v| async move {
                let all = cached_json(&POOLS, Duration::from_secs(900), "https://yields.llama.fi/pools").await.map_err(fail)?;
                let chain = v["chain"].as_str().map(str::to_lowercase);
                let sym = v["symbol"].as_str().map(str::to_uppercase);
                let project = v["project"].as_str().map(str::to_lowercase);
                let stable_only = v["stablecoin_only"].as_bool().unwrap_or(false);
                let min_tvl = v["min_tvl_usd"].as_f64().unwrap_or(1_000_000.0);
                let sort = v["sort"].as_str().unwrap_or("organic").to_string();
                let outliers = v["include_outliers"].as_bool().unwrap_or(false);
                let limit = v["limit"].as_u64().unwrap_or(12) as usize;
                let mut list: Vec<&Value> = all["data"]
                    .as_array()
                    .map(|a| a.iter().collect::<Vec<_>>())
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|p: &&Value| {
                        chain.as_ref().is_none_or(|c| p["chain"].as_str().is_some_and(|x| x.to_lowercase() == *c))
                            && sym.as_ref().is_none_or(|s| p["symbol"].as_str().is_some_and(|x| x.to_uppercase().split(['-', '_', '/', ' ']).any(|part| part == s)))
                            && project.as_ref().is_none_or(|pr| p["project"].as_str().is_some_and(|x| x.to_lowercase().contains(pr.as_str())))
                            && (!stable_only || p["stablecoin"] == true)
                            && n(&p["tvlUsd"]) >= min_tvl
                            && (outliers || p["outlier"] != true)
                    })
                    .collect();
                let key = |p: &Value| match sort.as_str() {
                    "tvl" => n(&p["tvlUsd"]),
                    "total" => n(&p["apy"]),
                    _ => n(&p["apyBase"]),
                };
                list.sort_by(|a, b| key(b).partial_cmp(&key(a)).unwrap_or(std::cmp::Ordering::Equal));
                json_text(json!({ "matched": list.len(), "pools": list.iter().take(limit).map(|p| yield_row(p)).collect::<Vec<_>>() }))
            },
        )]
    }
}

// ---- Derivatives ----

pub struct Derivatives(PluginMeta);

pub fn derivatives() -> Arc<dyn Plugin> {
    Arc::new(Derivatives(meta(
        "derivatives-data",
        "Derivatives Data",
        "data",
        "Funding rates, open interest and long/short ratios from Binance and Aster futures: the crowding behind a move.",
        "Reads the public futures data of Binance and Aster: funding now and its 3-day average (per 8h and annualised), basis, open interest and its 24h change, and the long/short account ratio. A funding scan finds the most crowded longs and shorts across the market. Read-only; for trading perps install Perps on Aster.",
        &["read-market-data", "third-party"],
    )))
}

fn venue_base(v: &str) -> &'static str {
    if v == "aster" { "https://fapi.asterdex.com" } else { "https://fapi.binance.com" }
}

fn funding(rate: f64) -> Value {
    json!({ "per_8h_pct": round(rate * 100.0, 4), "annualised_pct": round(rate * 3.0 * 365.0 * 100.0, 1) })
}

async fn snapshot(venue: &str, symbol: &str) -> Value {
    let base = venue_base(venue);
    let p = match get(&format!("{base}/fapi/v1/premiumIndex?symbol={symbol}")).await {
        Ok(p) => p,
        Err(e) => return json!({ "venue": venue, "error": e.message }),
    };
    let (mark, index) = (n(&p["markPrice"]), n(&p["indexPrice"]));
    let next = chrono::DateTime::from_timestamp_millis(n(&p["nextFundingTime"]) as i64).map(crate::public::iso);
    let mut out = json!({
        "venue": venue, "symbol": symbol, "mark_price": mark,
        "basis_pct": if index != 0.0 { json!(round((mark - index) / index * 100.0, 4)) } else { Value::Null },
        "funding_now": funding(n(&p["lastFundingRate"])), "next_funding": next,
    });
    if let Ok(fr) = get(&format!("{base}/fapi/v1/fundingRate?symbol={symbol}&limit=9")).await {
        if let Some(a) = fr.as_array().filter(|a| !a.is_empty()) {
            out["funding_avg_last_3d"] = funding(a.iter().map(|f| n(&f["fundingRate"])).sum::<f64>() / a.len() as f64);
        }
    }
    if let Ok(oi) = get(&format!("{base}/fapi/v1/openInterest?symbol={symbol}")).await {
        out["open_interest_contracts"] = json!(n(&oi["openInterest"]));
    }
    if venue == "binance" {
        if let Ok(h) = get(&format!("{base}/futures/data/openInterestHist?symbol={symbol}&period=1h&limit=25")).await {
            if let Some(a) = h.as_array().filter(|a| a.len() > 1) {
                let (first, last) = (n(&a[0]["sumOpenInterestValue"]), n(&a[a.len() - 1]["sumOpenInterestValue"]));
                out["open_interest_usd"] = json!(last.round());
                out["open_interest_change_24h_pct"] = if first != 0.0 { json!(round((last - first) / first * 100.0, 2)) } else { Value::Null };
            }
        }
        if let Ok(ls) = get(&format!("{base}/futures/data/globalLongShortAccountRatio?symbol={symbol}&period=1h&limit=25")).await {
            if let Some(a) = ls.as_array().filter(|a| !a.is_empty()) {
                let now = &a[a.len() - 1];
                out["long_short_accounts"] = json!({ "ratio": round(n(&now["longShortRatio"]), 3), "long_pct": round(n(&now["longAccount"]) * 100.0, 1), "ratio_24h_ago": round(n(&a[0]["longShortRatio"]), 3) });
            }
        }
    }
    out
}

static PREMIUMS: LazyLock<std::sync::Mutex<Option<(std::time::Instant, Value)>>> = LazyLock::new(Default::default);

#[async_trait]
impl Plugin for Derivatives {
    fn meta(&self) -> &PluginMeta {
        &self.0
    }
    async fn knowledge(&self, _ctx: &Ctx) -> Option<String> {
        Some("## Reading derivatives\nHigh positive funding with rising open interest means crowded longs (squeeze risk down); deeply negative funding with rising OI means crowded shorts. Price up with OI down is shorts closing, not new demand. Treat these as evidence, not signals on their own.".into())
    }
    fn read_tools(&self, _ctx: &Ctx) -> ToolSet {
        vec![
            Tool::new(
                "get_derivatives",
                "Futures positioning for one perpetual (e.g. BTCUSDT, BNBUSDT): funding, basis, open interest and trend, long/short ratio, on Binance, Aster or both.",
                schema(&[("symbol", json!({ "type": "string", "pattern": "^[A-Z0-9]{2,20}USDT?$" }), true), ("venue", json!({ "type": "string", "enum": ["binance", "aster", "both"] }), false)]),
                |v| async move {
                    let symbol = v["symbol"].as_str().ok_or("invalid input: symbol")?.to_string();
                    let ok = symbol.len() >= 5 && symbol.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()) && (symbol.ends_with("USDT") || symbol.ends_with("USD"));
                    if !ok {
                        return Err("invalid input: symbol".into());
                    }
                    let venues = match v["venue"].as_str().unwrap_or("both") {
                        "both" => vec!["binance", "aster"],
                        "aster" => vec!["aster"],
                        _ => vec!["binance"],
                    };
                    let mut out = vec![];
                    for venue in venues {
                        out.push(snapshot(venue, &symbol).await);
                    }
                    json_text(Value::Array(out))
                },
            ),
            Tool::new(
                "scan_funding",
                "The most extreme funding rates across all Binance USDT perpetuals right now: where longs or shorts are paying the most to stay in.",
                schema(&[("top", json!({ "type": "integer", "minimum": 3, "maximum": 25 }), false)]),
                |v| async move {
                    let top = v["top"].as_u64().unwrap_or(10) as usize;
                    let all = cached_json(&PREMIUMS, Duration::from_secs(60), "https://fapi.binance.com/fapi/v1/premiumIndex").await.map_err(fail)?;
                    let mut by: Vec<&Value> = all.as_array().map(|a| a.iter().filter(|p| p["symbol"].as_str().is_some_and(|s| s.ends_with("USDT"))).collect()).unwrap_or_default();
                    let markets = by.len();
                    by.sort_by(|a, b| n(&b["lastFundingRate"]).partial_cmp(&n(&a["lastFundingRate"])).unwrap_or(std::cmp::Ordering::Equal));
                    let row = |p: &Value| {
                        let mut r = funding(n(&p["lastFundingRate"]));
                        r["symbol"] = p["symbol"].clone();
                        r
                    };
                    let longs: Vec<Value> = by.iter().take(top).map(|p| row(p)).collect();
                    let shorts: Vec<Value> = by.iter().rev().take(top).map(|p| row(p)).collect();
                    json_text(json!({ "longs_paying_most": longs, "shorts_paying_most": shorts, "markets": markets }))
                },
            ),
        ]
    }
}

// ---- Polymarket ----

pub struct Polymarket(PluginMeta);

pub fn polymarket() -> Arc<dyn Plugin> {
    Arc::new(Polymarket(meta(
        "polymarket",
        "Polymarket",
        "data",
        "Live odds, liquidity and price history from Polymarket, the deepest prediction market. Read-only.",
        "Search Polymarket's markets and read their implied probabilities, 24h moves, spreads, liquidity and price history. Useful as crowd-priced evidence about elections, rates, crypto prices and events, and to compare with Opinion on BNB Chain. Read-only: the agent doesn't trade on Polymarket.",
        &["read-market-data", "third-party"],
    )))
}

const GAMMA: &str = "https://gamma-api.polymarket.com";

fn parse_list(v: &Value) -> Vec<String> {
    v.as_str().and_then(|s| serde_json::from_str::<Vec<Value>>(s).ok()).map(|a| a.iter().map(|x| x.as_str().map(String::from).unwrap_or_else(|| x.to_string())).collect()).unwrap_or_default()
}

fn market_view(m: &Value) -> Value {
    let outcomes = parse_list(&m["outcomes"]);
    let prices: Vec<f64> = parse_list(&m["outcomePrices"]).iter().map(|p| p.parse().unwrap_or(0.0)).collect();
    json!({
        "question": m["question"], "slug": m["slug"],
        "odds": outcomes.iter().enumerate().map(|(i, o)| json!({ "outcome": o, "probability_pct": round(prices.get(i).copied().unwrap_or(0.0) * 100.0, 1) })).collect::<Vec<_>>(),
        "change_24h_pts": if m["oneDayPriceChange"].is_null() { Value::Null } else { json!(round(n(&m["oneDayPriceChange"]) * 100.0, 1)) },
        "spread_pts": if m["bestBid"].is_null() || m["bestAsk"].is_null() { Value::Null } else { json!(round((n(&m["bestAsk"]) - n(&m["bestBid"])) * 100.0, 1)) },
        "liquidity_usd": n(&m["liquidity"]).round(), "volume_24h_usd": n(&m["volume24hr"]).round(),
        "ends": m["endDate"], "url": format!("https://polymarket.com/market/{}", m["slug"].as_str().unwrap_or("")),
    })
}

static TOP_MARKETS: LazyLock<std::sync::Mutex<Option<(std::time::Instant, Value)>>> = LazyLock::new(Default::default);

#[async_trait]
impl Plugin for Polymarket {
    fn meta(&self) -> &PluginMeta {
        &self.0
    }
    async fn knowledge(&self, _ctx: &Ctx) -> Option<String> {
        Some("## Reading prediction markets\nA price of 0.62 means the market puts it at 62%. Thin liquidity or a wide spread makes that number weak evidence. Note how odds moved, not just where they are.".into())
    }
    fn read_tools(&self, _ctx: &Ctx) -> ToolSet {
        vec![
            Tool::new(
                "polymarket_search",
                "Search Polymarket for markets about a topic (e.g. 'bitcoin 150k', 'fed rate', 'election'). Without a query, the most traded markets right now.",
                schema(&[("query", json!({ "type": "string", "maxLength": 120 }), false), ("limit", json!({ "type": "integer", "minimum": 1, "maximum": 20 }), false)]),
                |v| async move {
                    let limit = v["limit"].as_u64().unwrap_or(8) as usize;
                    let Some(q) = v["query"].as_str().filter(|q| !q.is_empty()) else {
                        let top = cached_json(&TOP_MARKETS, Duration::from_secs(180), &format!("{GAMMA}/markets?active=true&closed=false&limit=60&order=volume24hr&ascending=false")).await.map_err(fail)?;
                        return json_text(json!({ "markets": top.as_array().map(|a| a.iter().take(limit).map(market_view).collect::<Vec<_>>()).unwrap_or_default() }));
                    };
                    let enc: String = url::form_urlencoded::byte_serialize(q.as_bytes()).collect();
                    let r = get(&format!("{GAMMA}/public-search?q={enc}&limit_per_type=10")).await.map_err(fail)?;
                    let mut markets: Vec<Value> = r["events"].as_array().cloned().unwrap_or_default().iter().flat_map(|e| e["markets"].as_array().cloned().unwrap_or_default()).filter(|m| m["active"] != false && m["closed"] != true).collect();
                    markets.sort_by(|a, b| n(&b["volume24hr"]).partial_cmp(&n(&a["volume24hr"])).unwrap_or(std::cmp::Ordering::Equal));
                    json_text(json!({ "markets": markets.iter().take(limit).map(market_view).collect::<Vec<_>>() }))
                },
            ),
            Tool::new(
                "polymarket_history",
                "How a market's odds moved: hourly probability history of its first outcome (usually Yes) over the last day, week or month.",
                schema(&[("slug", json!({ "type": "string", "maxLength": 200 }), true), ("interval", json!({ "type": "string", "enum": ["1d", "1w", "1m"] }), false)]),
                |v| async move {
                    let slug = v["slug"].as_str().ok_or("invalid input: slug")?.to_string();
                    let interval = v["interval"].as_str().unwrap_or("1w").to_string();
                    let enc: String = url::form_urlencoded::byte_serialize(slug.as_bytes()).collect();
                    let ms = get(&format!("{GAMMA}/markets?slug={enc}")).await.map_err(fail)?;
                    let Some(m) = ms.as_array().and_then(|a| a.first()).cloned() else { return json_text(json!({ "error": "no market with that slug" })) };
                    let Some(token) = parse_list(&m["clobTokenIds"]).into_iter().next() else { return json_text(json!({ "error": "this market has no order book" })) };
                    let fidelity = if interval == "1d" { 15 } else { 60 };
                    let h = get(&format!("https://clob.polymarket.com/prices-history?market={token}&interval={interval}&fidelity={fidelity}")).await.map_err(fail)?;
                    let pts = h["history"].as_array().cloned().unwrap_or_default();
                    let step = pts.len().div_ceil(48).max(1);
                    let history: Vec<Value> = pts
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| i % step == 0 || *i == pts.len() - 1)
                        .map(|(_, p)| json!({ "time": chrono::DateTime::from_timestamp(n(&p["t"]) as i64, 0).map(crate::public::iso), "probability_pct": round(n(&p["p"]) * 100.0, 1) }))
                        .collect();
                    json_text(json!({ "market": market_view(&m), "outcome": parse_list(&m["outcomes"]).into_iter().next().unwrap_or_else(|| "Yes".into()), "history": history }))
                },
            ),
        ]
    }
}
