//! What an agent holds on BNB Chain, priced in USD: every token in its wallet (the same list
//! withdrawals use). Prices: stablecoins at $1, BNB and WBNB from the deepest WBNB pool,
//! everything else from DexScreener; unknown prices stay null rather than guessed. Balances are on
//! chain anyway, so the portfolio is public. Cached for a minute per agent.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::db::Agent;
use crate::error::Result;
use crate::evm::BSC;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Holding {
    pub key: String,
    pub network: String,
    pub symbol: String,
    /// Decimal string.
    pub balance: String,
    pub price_usd: Option<f64>,
    pub value_usd: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Portfolio {
    pub updated_at: String,
    /// Sum of everything with a known price.
    pub total_usd: f64,
    pub holdings: Vec<Holding>,
    /// Assets whose balance couldn't be read just now.
    pub unavailable: Vec<String>,
}

async fn dex(address: &str) -> Option<f64> {
    let pair = crate::data::dexscreener::best_pair("bsc", address).await.ok().flatten()?;
    pair["priceUsd"].as_str().and_then(|p| p.parse::<f64>().ok()).filter(|p| p.is_finite() && *p > 0.0)
}

/// USD price of one unit of an asset (by its key, "bsc:<token|native>"), or None.
async fn price_of(key: &str) -> Option<f64> {
    let token = key.split(':').nth(1)?.to_lowercase();
    if token == BSC.usdt || token == BSC.usdc {
        return Some(1.0);
    }
    if token == "native" || token == BSC.wrapped {
        return dex(BSC.wrapped).await;
    }
    dex(&token).await
}

static CACHE: LazyLock<Mutex<HashMap<String, (Instant, Portfolio)>>> = LazyLock::new(Default::default);

pub async fn agent_portfolio(agent: &Agent) -> Result<Portfolio> {
    if let Some((at, p)) = CACHE.lock().unwrap().get(&agent.id).cloned() {
        if at.elapsed() < Duration::from_secs(60) {
            return Ok(p);
        }
    }
    let p = build(agent).await?;
    CACHE.lock().unwrap().insert(agent.id.clone(), (Instant::now(), p.clone()));
    Ok(p)
}

async fn build(agent: &Agent) -> Result<Portfolio> {
    let (resolved, unavailable) = crate::withdraw::agent_assets(agent).await?;
    let prices = futures_util::future::join_all(resolved.iter().map(|t| async move { tokio::time::timeout(Duration::from_secs(8), price_of(&t.key)).await.ok().flatten() })).await;
    let mut holdings: Vec<Holding> = resolved
        .iter()
        .zip(prices)
        .map(|(t, price)| {
            let balance = crate::withdraw::format_units(t.balance, t.decimals);
            let value = price.map(|p| balance.parse::<f64>().unwrap_or(0.0) * p);
            Holding { key: t.key.clone(), network: BSC.label.into(), symbol: t.symbol.clone(), balance, price_usd: price, value_usd: value }
        })
        .collect();
    holdings.sort_by(|x, y| y.value_usd.unwrap_or(-1.0).total_cmp(&x.value_usd.unwrap_or(-1.0)));
    let total_usd = holdings.iter().map(|h| h.value_usd.unwrap_or(0.0)).sum();
    Ok(Portfolio { updated_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true), total_usd, holdings, unavailable })
}
