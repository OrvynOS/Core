//! One asset's price from many independent sources: the Chainlink and Pyth oracles, Binance, OKX
//! and Coinbase, CoinGecko, and PancakeSwap v3 / Uniswap v3 time-weighted prices read from the
//! pools on BNB Chain; any BNB Chain token by address (DEX TWAPs and DexScreener).

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use alloy::primitives::{aliases::U24, Address};
use alloy::sol;
use serde::Serialize;
use serde_json::{json, Value};

use crate::bliss::round;
use crate::error::{Error, Result};
use crate::evm::{self, rpc_error};

pub const WBNB: &str = "0xbb4CdB9CBd36B01bD1cBaEBF2De08d9173bc095c";
pub const USDT: &str = "0x55d398326f99059fF775485246999027B3197955";

pub struct Asset {
    pub chainlink: Option<&'static str>,
    pub pyth: &'static str,
    pub binance: Option<&'static str>,
    pub okx: Option<&'static str>,
    pub coinbase: Option<&'static str>,
    pub coingecko: &'static str,
    pub bsc_token: Option<&'static str>,
}

pub const ASSET_SYMBOLS: &[&str] = &["BTC", "ETH", "BNB", "CAKE", "SOL", "USDT", "USDC"];

pub fn asset(symbol: &str) -> Option<Asset> {
    let a = |chainlink, pyth, binance, okx, coinbase, coingecko, bsc_token| Asset { chainlink: Some(chainlink), pyth, binance, okx, coinbase, coingecko, bsc_token };
    Some(match symbol {
        "BTC" => a("0x264990fbd0A4796A3E3d8E37C4d5F87a3aCa5Ebf", "Crypto.BTC/USD", Some("BTCUSDT"), Some("BTC-USDT"), Some("BTC-USD"), "bitcoin", Some("0x7130d2A12B9BCbFAe4f2634d864A1Ee1Ce3Ead9c")),
        "ETH" => a("0x9ef1B8c0E4F7dc8bF5719Ea496883DC6401d5b2e", "Crypto.ETH/USD", Some("ETHUSDT"), Some("ETH-USDT"), Some("ETH-USD"), "ethereum", Some("0x2170Ed0880ac9A755fd29B2688956BD959F933F8")),
        "BNB" => a("0x0567F2323251f0Aab15c8dFb1967E4e8A7D42aeE", "Crypto.BNB/USD", Some("BNBUSDT"), Some("BNB-USDT"), None, "binancecoin", Some(WBNB)),
        "CAKE" => a("0xB6064eD41d4f67e353768aA239cA86f4F73665a1", "Crypto.CAKE/USD", Some("CAKEUSDT"), Some("CAKE-USDT"), None, "pancakeswap-token", Some("0x0E09FaBB73Bd3Ade0a17ECC321fD13a19e81cE82")),
        "SOL" => a("0x0E8a53DD9c13589df6382F13dA6B3Ec8F919B323", "Crypto.SOL/USD", Some("SOLUSDT"), Some("SOL-USDT"), Some("SOL-USD"), "solana", None),
        "USDT" => a("0xB97Ad0E74fa7d920791E90258A6E2085088b4320", "Crypto.USDT/USD", None, None, Some("USDT-USD"), "tether", None),
        "USDC" => a("0x51597f405303C4377E36123cBc172b13269EA163", "Crypto.USDC/USD", Some("USDCUSDT"), Some("USDC-USDT"), None, "usd-coin", None),
        _ => return None,
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct Quote {
    pub source: String,
    pub kind: &'static str,
    pub price: f64,
    #[serde(rename = "ageSeconds", skip_serializing_if = "Option::is_none")]
    pub age_seconds: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

fn q(source: &str, kind: &'static str, price: f64, age: Option<i64>, note: Option<String>) -> Quote {
    Quote { source: source.into(), kind, price, age_seconds: age, note }
}

fn now_s() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64 / 1000.0
}

async fn get_json(url: &str) -> Result<Value> {
    let res = crate::http::client().get(url).header("accept", "application/json").timeout(Duration::from_secs(8)).send().await.map_err(|e| Error::unavailable(e.to_string()))?;
    if !res.status().is_success() {
        let host = url.split('/').nth(2).unwrap_or(url);
        return Err(Error::unavailable(format!("{host} {}", res.status().as_u16())));
    }
    res.json().await.map_err(|e| Error::unavailable(e.to_string()))
}

fn num(v: &Value) -> Option<f64> {
    v.as_str().and_then(|s| s.parse().ok()).or_else(|| v.as_f64())
}

sol! {
    #[sol(rpc)]
    interface Aggregator {
        function decimals() view returns (uint8);
        function latestRoundData() view returns (uint80, int256, uint256, uint256, uint80);
    }
    #[sol(rpc)]
    interface V3Factory {
        function getPool(address, address, uint24) view returns (address);
    }
    #[sol(rpc)]
    interface V3Pool {
        function token0() view returns (address);
        function liquidity() view returns (uint128);
        function observe(uint32[] secondsAgos) view returns (int56[] tickCumulatives, uint160[] secondsPerLiquidityCumulativeX128s);
        function slot0() view returns (uint160 sqrtPriceX96, int24 tick, uint16, uint16, uint16, uint32, bool);
    }
}

pub async fn chainlink(feed: &str) -> Result<Quote> {
    let c = Aggregator::new(evm::parse_address(feed)?, evm::bsc());
    let dec = c.decimals().call().await.map_err(rpc_error)?;
    let r = c.latestRoundData().call().await.map_err(rpc_error)?;
    let answer: f64 = r._1.to_string().parse().unwrap_or(0.0);
    let updated: f64 = r._3.to_string().parse().unwrap_or(0.0);
    Ok(q("chainlink", "oracle", answer / 10f64.powi(dec as i32), Some((now_s() - updated).round() as i64), None))
}

/// Pyth feed ids, looked up one symbol at a time and kept.
static PYTH_IDS: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(Default::default);

/// Pyth's public Hermes refuses price reads without authorisation, so Pyth is read from a Hermes
/// endpoint you have access to (PYTH_HERMES_URL). Without it, Pyth is reported as unavailable.
async fn pyth(symbol: &str) -> Result<Quote> {
    let hermes = std::env::var("PYTH_HERMES_URL").ok().filter(|s| !s.is_empty()).ok_or_else(|| Error::unavailable("needs PYTH_HERMES_URL (Pyth's public Hermes requires authorisation)"))?;
    let hermes = hermes.trim_end_matches('/');
    let cached = PYTH_IDS.lock().unwrap().get(symbol).cloned();
    let id = match cached {
        Some(id) => id,
        None => {
            let base = symbol.trim_start_matches("Crypto.").split('/').next().unwrap_or("");
            let feeds = get_json(&format!("https://hermes.pyth.network/v2/price_feeds?query={}&asset_type=crypto", urlencode(base))).await?;
            let id = feeds.as_array().and_then(|a| a.iter().find(|x| x["attributes"]["symbol"] == symbol)).and_then(|x| x["id"].as_str()).map(String::from).ok_or_else(|| Error::not_found(format!("no Pyth feed for {symbol}")))?;
            PYTH_IDS.lock().unwrap().insert(symbol.into(), id.clone());
            id
        }
    };
    let r = get_json(&format!("{hermes}/v2/updates/price/latest?ids[]={id}&parsed=true")).await?;
    let p = &r["parsed"][0]["price"];
    let expo = p["expo"].as_i64().unwrap_or(0) as i32;
    let price = num(&p["price"]).unwrap_or(0.0) * 10f64.powi(expo);
    let conf = num(&p["conf"]).unwrap_or(0.0) * 10f64.powi(expo);
    let age = (now_s() - p["publish_time"].as_f64().unwrap_or(0.0)).round() as i64;
    Ok(q("pyth", "oracle", price, Some(age), Some(format!("±{}% confidence", crate::auth::js_number(round(conf * 100.0 / price, 3))))))
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

async fn binance(symbol: &str) -> Result<Quote> {
    let r = get_json(&format!("https://api.binance.com/api/v3/ticker/price?symbol={symbol}")).await?;
    Ok(q("binance", "exchange", num(&r["price"]).unwrap_or(0.0), None, Some("in USDT".into())))
}

async fn okx(inst: &str) -> Result<Quote> {
    let r = get_json(&format!("https://www.okx.com/api/v5/market/ticker?instId={inst}")).await?;
    let d = &r["data"][0];
    let age = ((chrono::Utc::now().timestamp_millis() as f64 - num(&d["ts"]).unwrap_or(0.0)) / 1000.0).round() as i64;
    Ok(q("okx", "exchange", num(&d["last"]).unwrap_or(0.0), Some(age), Some("in USDT".into())))
}

async fn coinbase(product: &str) -> Result<Quote> {
    let r = get_json(&format!("https://api.exchange.coinbase.com/products/{product}/ticker")).await?;
    let age = r["time"].as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map(|t| ((chrono::Utc::now().timestamp_millis() - t.timestamp_millis()) as f64 / 1000.0).round() as i64);
    Ok(q("coinbase", "exchange", num(&r["price"]).unwrap_or(0.0), age, None))
}

async fn coingecko(id: &str) -> Result<Quote> {
    let r = get_json(&format!("https://api.coingecko.com/api/v3/simple/price?ids={id}&vs_currencies=usd&include_last_updated_at=true")).await?;
    let v = &r[id];
    let usd = v["usd"].as_f64().filter(|p| *p != 0.0).ok_or_else(|| Error::not_found(format!("no CoinGecko price for {id}")))?;
    Ok(q("coingecko", "aggregator", usd, v["last_updated_at"].as_f64().map(|t| (now_s() - t).round() as i64), None))
}

fn dex(name: &str) -> (&'static str, [u32; 4]) {
    if name == "pancakeswap-v3" {
        ("0x0BFbCF9fa4f9C56B0F40a671Ad40E0805A091865", [100, 500, 2500, 10000])
    } else {
        ("0xdB1d10011AD0Ff90774D0C6Bb92e5C5c8b4461F7", [100, 500, 3000, 10000])
    }
}

/// A DEX's time-weighted price of `token` in `quote` (both BNB Chain), from its most liquid fee
/// tier: 30 minutes if the pool keeps that much history, else 5 minutes, else (flagged) spot.
pub async fn dex_twap(name: &str, token: &str, quote: &str) -> Result<Quote> {
    let p = evm::bsc();
    let (factory, fees) = dex(name);
    let f = V3Factory::new(evm::parse_address(factory)?, p.clone());
    let (t, qt) = (evm::parse_address(token)?, evm::parse_address(quote)?);
    let mut best: Option<(Address, u128)> = None;
    for fee in fees {
        let Ok(pool) = f.getPool(t, qt, U24::from(fee)).call().await else { continue };
        if pool == Address::ZERO {
            continue;
        }
        let liq = V3Pool::new(pool, p.clone()).liquidity().call().await.unwrap_or(0);
        if best.is_none_or(|(_, l)| liq > l) {
            best = Some((pool, liq));
        }
    }
    let (pool, _) = best.ok_or_else(|| Error::not_found(format!("no {name} pool for this pair")))?;
    let pc = V3Pool::new(pool, p.clone());
    let token0 = pc.token0().call().await.map_err(rpc_error)?;
    let dec_t = evm::token_decimals("bsc", token).await? as i32;
    let dec_q = evm::token_decimals("bsc", quote).await? as i32;
    let mut tick: Option<f64> = None;
    let mut window = 0;
    for secs in [1800u32, 300] {
        if let Ok(r) = pc.observe(vec![secs, 0]).call().await {
            let (a, b): (i64, i64) = (r.tickCumulatives[0].as_i64(), r.tickCumulatives[1].as_i64());
            tick = Some((b - a) as f64 / secs as f64);
            window = secs;
            break;
        }
    }
    let tick = match tick {
        Some(t) => t,
        None => pc.slot0().call().await.map_err(rpc_error)?.tick.as_i32() as f64,
    };
    // price of token0 in token1 = 1.0001^tick, adjusted for decimals.
    let t0_is_token = token0 == t;
    let raw = 1.0001f64.powf(tick) * 10f64.powi(if t0_is_token { dec_t - dec_q } else { dec_q - dec_t });
    let price = if t0_is_token { raw } else { 1.0 / raw };
    let note = if window > 0 { format!("{}-minute TWAP", window / 60) } else { "spot (no TWAP history: manipulable)".into() };
    Ok(q(name, "dex", price, None, Some(note)))
}

/// Orvyn's own network: the latest report its staked nodes signed (if it's recent).
async fn orvyn_network(symbol: &str) -> Result<Quote> {
    let pair = super::FEEDS.iter().find(|(_, s)| *s == symbol).map(|(p, _)| *p).ok_or_else(|| Error::not_found("not an Orvyn network feed"))?;
    let reports = super::latest_reports().await?;
    let r = reports.iter().find(|x| x["feed"] == pair).filter(|r| r["verification"]["valid"] == true).ok_or_else(|| Error::not_found("no verified Orvyn report yet"))?;
    let at = r["at"].as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map(|t| t.timestamp()).unwrap_or(0);
    let age = (now_s() - at as f64).round() as i64;
    if age > 300 {
        return Err(Error::unavailable(format!("the latest Orvyn report is {age}s old")));
    }
    Ok(q("orvyn-network", "oracle", r["price"].as_f64().unwrap_or(0.0), Some(age), Some(format!("{} staked nodes signed", r["signers"]))))
}

async fn dexscreener(token: &str) -> Result<Quote> {
    let r = get_json(&format!("https://api.dexscreener.com/latest/dex/tokens/{token}")).await?;
    let best = r["pairs"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p["chainId"] == "bsc" && p["baseToken"]["address"].as_str().is_some_and(|a| a.eq_ignore_ascii_case(token)) && num(&p["priceUsd"]).unwrap_or(0.0) > 0.0)
        .max_by(|a, b| a["liquidity"]["usd"].as_f64().unwrap_or(0.0).total_cmp(&b["liquidity"]["usd"].as_f64().unwrap_or(0.0)))
        .ok_or_else(|| Error::not_found("no DexScreener pool"))?;
    Ok(q("dexscreener", "aggregator", num(&best["priceUsd"]).unwrap_or(0.0), None, Some(format!("deepest pool ${} liquidity", best["liquidity"]["usd"].as_f64().unwrap_or(0.0).round()))))
}

// ---------- Consensus ----------

/// The median, each quote's deviation from it, outliers and the spread of the rest.
pub fn consensus(quotes: &[Quote], outlier_pct: f64) -> Value {
    let mut sorted: Vec<f64> = quotes.iter().map(|q| q.price).collect();
    sorted.sort_by(f64::total_cmp);
    if sorted.is_empty() {
        return json!({ "median": null, "sources": 0, "agreeing": 0, "spread_pct": 0, "quotes": [] });
    }
    let mid = sorted.len() / 2;
    let median = if sorted.len() % 2 == 1 { sorted[mid] } else { (sorted[mid - 1] + sorted[mid]) / 2.0 };
    let rows: Vec<Value> = quotes
        .iter()
        .map(|q| {
            let dev = (q.price - median) / median * 100.0;
            let mut v = serde_json::to_value(q).unwrap();
            v["price"] = json!(round(q.price, if q.price < 1.0 { 8 } else { 4 }));
            v["deviation_pct"] = json!(round(dev, 3));
            v["outlier"] = json!(dev.abs() > outlier_pct);
            v
        })
        .collect();
    let inliers: Vec<f64> = rows.iter().filter(|r| r["outlier"] == false).filter_map(|r| r["price"].as_f64()).collect();
    let spread = if inliers.is_empty() { 0.0 } else { (inliers.iter().cloned().fold(f64::MIN, f64::max) - inliers.iter().cloned().fold(f64::MAX, f64::min)) / median * 100.0 };
    json!({ "median": round(median, if median < 1.0 { 8 } else { 4 }), "sources": rows.len(), "agreeing": inliers.len(), "spread_pct": round(spread, 3), "quotes": rows })
}

type Task = (&'static str, std::pin::Pin<Box<dyn std::future::Future<Output = Result<Quote>> + Send>>);

async fn gather(tasks: Vec<Task>) -> (Vec<Quote>, Vec<Value>) {
    let names: Vec<&str> = tasks.iter().map(|(n, _)| *n).collect();
    let results = futures_util::future::join_all(tasks.into_iter().map(|(_, f)| f)).await;
    let (mut quotes, mut failed) = (vec![], vec![]);
    for (name, r) in names.into_iter().zip(results) {
        match r {
            Ok(q) if q.price.is_finite() && q.price > 0.0 => quotes.push(q),
            Ok(_) => failed.push(json!({ "source": name, "error": "no price" })),
            Err(e) => failed.push(json!({ "source": name, "error": crate::http::clip(&e.message, 120) })),
        }
    }
    (quotes, failed)
}

/// Every source's price for a listed asset, in USD (exchange pairs in USDT are noted as such).
pub async fn asset_quotes(symbol: &str, include_orvyn: bool) -> Result<(Vec<Quote>, Vec<Value>)> {
    let a = asset(symbol).ok_or_else(|| Error::invalid(format!("unknown asset {symbol}; listed: {}", ASSET_SYMBOLS.join(", "))))?;
    let mut tasks: Vec<Task> = vec![("pyth", Box::pin(pyth(a.pyth))), ("coingecko", Box::pin(coingecko(a.coingecko)))];
    // The network's own report when asked for (not when the network itself observes: circular).
    if include_orvyn {
        let s = symbol.to_string();
        tasks.push(("orvyn-network", Box::pin(async move { orvyn_network(&s).await })));
    }
    if let Some(f) = a.chainlink {
        tasks.push(("chainlink", Box::pin(chainlink(f))));
    }
    if let Some(s) = a.binance {
        tasks.push(("binance", Box::pin(binance(s))));
    }
    if let Some(s) = a.okx {
        tasks.push(("okx", Box::pin(okx(s))));
    }
    if let Some(s) = a.coinbase {
        tasks.push(("coinbase", Box::pin(coinbase(s))));
    }
    if let Some(t) = a.bsc_token.filter(|t| *t != USDT) {
        tasks.push(("pancakeswap-v3", Box::pin(dex_twap("pancakeswap-v3", t, USDT))));
        tasks.push(("uniswap-v3", Box::pin(dex_twap("uniswap-v3", t, USDT))));
    }
    Ok(gather(tasks).await)
}

async fn via_bnb(name: &'static str, token: String) -> Result<Quote> {
    let in_bnb = dex_twap(name, &token, WBNB).await?;
    let bnb = chainlink(asset("BNB").unwrap().chainlink.unwrap()).await?;
    Ok(Quote { price: in_bnb.price * bnb.price, note: Some(format!("{} via WBNB × Chainlink BNB/USD", in_bnb.note.unwrap_or_default())), ..in_bnb })
}

async fn either(name: &'static str, token: String) -> Result<Quote> {
    match dex_twap(name, &token, USDT).await {
        Ok(q) => Ok(q),
        Err(_) => via_bnb(name, token).await,
    }
}

/// Any BNB Chain token: DEX TWAPs against USDT (or via WBNB × BNB's USD price) and DexScreener.
pub async fn token_quotes(token: &str) -> (Vec<Quote>, Vec<Value>) {
    let t = token.to_string();
    gather(vec![
        ("pancakeswap-v3", Box::pin(either("pancakeswap-v3", t.clone()))),
        ("uniswap-v3", Box::pin(either("uniswap-v3", t.clone()))),
        ("dexscreener", Box::pin(async move { dexscreener(&t).await })),
    ])
    .await
}
