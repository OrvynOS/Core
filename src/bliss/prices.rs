//! One place to price what an agent talks about, for grading its calls: a token by its contract
//! address (DexScreener, deepest pool on that chain) or a major coin by CoinGecko id. Cached briefly.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq)]
pub enum Subject {
    Token { chain: String, address: String },
    Coin { id: String },
}

#[derive(Debug, Clone)]
pub struct Price {
    pub price_usd: f64,
    pub label: String,
    pub liquidity_usd: Option<f64>,
}

static CACHE: LazyLock<Mutex<HashMap<String, (Instant, Price)>>> = LazyLock::new(Default::default);
const TTL: Duration = Duration::from_secs(30);

async fn get(url: &str) -> Result<Value> {
    let res = crate::http::client().get(url).header("accept", "application/json").timeout(Duration::from_secs(10)).send().await.map_err(|e| Error::internal(e.to_string()))?;
    if !res.status().is_success() {
        let host = url::Url::parse(url).ok().and_then(|u| u.host_str().map(String::from)).unwrap_or_default();
        return Err(Error::internal(format!("{host} returned {}", res.status().as_u16())));
    }
    res.json().await.map_err(|e| Error::internal(e.to_string()))
}

async fn load(s: &Subject) -> Result<Price> {
    match s {
        Subject::Coin { id } => {
            let r = get(&format!("https://api.coingecko.com/api/v3/simple/price?ids={id}&vs_currencies=usd")).await?;
            let p = r[id]["usd"].as_f64().filter(|p| *p > 0.0).ok_or_else(|| Error::internal(format!("no price for {id}")))?;
            Ok(Price { price_usd: p, label: id.clone(), liquidity_usd: None })
        }
        Subject::Token { chain, address } => {
            // DEXSCREENER_BASE_URL: a stand-in market (tests).
            let base = std::env::var("DEXSCREENER_BASE_URL").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| "https://api.dexscreener.com".into());
            let r = get(&format!("{}/latest/dex/tokens/{address}", base.trim_end_matches('/'))).await?;
            let mut pairs: Vec<Value> = r["pairs"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|p| p["chainId"] == chain.as_str() && p["baseToken"]["address"].as_str().is_some_and(|a| a.eq_ignore_ascii_case(address)) && p["priceUsd"].as_str().and_then(|v| v.parse::<f64>().ok()).is_some_and(|v| v > 0.0))
                .collect();
            if pairs.is_empty() {
                return Err(Error::internal(format!("no {chain} pool prices {address}")));
            }
            pairs.sort_by(|a, b| b["liquidity"]["usd"].as_f64().unwrap_or(0.0).partial_cmp(&a["liquidity"]["usd"].as_f64().unwrap_or(0.0)).unwrap_or(std::cmp::Ordering::Equal));
            let best = &pairs[0];
            Ok(Price { price_usd: best["priceUsd"].as_str().unwrap().parse().unwrap(), label: format!("${}", best["baseToken"]["symbol"].as_str().unwrap_or("")), liquidity_usd: best["liquidity"]["usd"].as_f64() })
        }
    }
}

pub fn subject_ref(s: &Subject) -> String {
    match s {
        Subject::Coin { id } => id.clone(),
        Subject::Token { chain, address } => format!("{chain}:{}", address.to_lowercase()),
    }
}

pub fn parse_ref(kind: &str, r: &str) -> Subject {
    if kind == "coin" {
        return Subject::Coin { id: r.into() };
    }
    let (chain, address) = r.split_once(':').unwrap_or((r, ""));
    Subject::Token { chain: chain.into(), address: address.into() }
}

pub async fn price_of(s: &Subject) -> Result<Price> {
    let key = match s {
        Subject::Coin { id } => format!("coin:{id}"),
        Subject::Token { chain, address } => format!("token:{chain}:{}", address.to_lowercase()),
    };
    if let Some((at, p)) = CACHE.lock().unwrap().get(&key) {
        if at.elapsed() < TTL {
            return Ok(p.clone());
        }
    }
    let p = load(s).await?;
    CACHE.lock().unwrap().insert(key, (Instant::now(), p.clone()));
    Ok(p)
}

/// Forget cached prices.
pub fn reset_prices() {
    CACHE.lock().unwrap().clear();
}
