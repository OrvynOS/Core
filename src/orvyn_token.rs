//! $ORVYN, the Orvyn platform's token: paying with it costs 2.5% less, for Orvyn credits (AI) and
//! for agent hosting. Off until the token launches and ORVYN_TOKEN_CONTRACT is set.
//!
//! - Credits: a deposit in $ORVYN is valued at the market (its deepest pool) and credited as if 2.5%
//!   more had been paid, so credits cost 2.5% less.
//! - Hosting: the registry charges a fixed amount of each token per month, set by its owner. The
//!   $ORVYN price is the USD price minus 2.5%, at the market rate (`orvyn-admin orvyn-hosting-price`
//!   sets it; run it again as the price moves).

use alloy::primitives::U256;

use crate::error::{Error, Result};

/// 2.5% off when paying with $ORVYN.
pub const DISCOUNT: f64 = 0.025;
pub const DISCOUNT_BPS: u32 = 250;
pub const SYMBOL: &str = "ORVYN";
pub const DECIMALS: u32 = 18;

/// The token's contract on BNB Chain (lowercase 0x), once it's live.
pub fn contract() -> Option<String> {
    std::env::var("ORVYN_TOKEN_CONTRACT").ok().map(|v| v.trim().to_lowercase()).filter(|v| crate::evm::is_address(v))
}

pub fn live() -> bool {
    contract().is_some()
}

/// $ORVYN's price in USD (its deepest pool on BNB Chain).
pub async fn usd_price() -> Result<f64> {
    let token = contract().ok_or_else(|| Error::unavailable("$ORVYN isn't live yet."))?;
    let pair = crate::data::dexscreener::best_pair("bsc", &token).await.ok().flatten();
    let price = pair.and_then(|p| p["priceUsd"].as_str().and_then(|s| s.parse::<f64>().ok())).unwrap_or(0.0);
    if price > 0.0 && price.is_finite() {
        Ok(price)
    } else {
        Err(Error::unavailable("Can't price $ORVYN right now. Try again in a minute."))
    }
}

/// Credits for a deposit worth `usd` at the market: 2.5% off means `usd / 0.975` of credit.
pub fn credits_for(usd: f64) -> f64 {
    usd / (1.0 - DISCOUNT)
}

/// What a USD price costs in $ORVYN base units, 2.5% off, at `token_usd`.
pub fn discounted_units(usd: f64, token_usd: f64) -> U256 {
    let tokens = usd * (1.0 - DISCOUNT) / token_usd;
    // To 6 decimals, then scaled to the token's 18.
    U256::from((tokens * 1e6).round() as u128) * U256::from(10u64).pow(U256::from(DECIMALS - 6))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_and_a_half_percent_off() {
        // $97.50 of $ORVYN buys $100 of credits.
        assert!((credits_for(97.5) - 100.0).abs() < 1e-9);
        // $20 of hosting at $0.50 a token: 39 tokens (not 40).
        assert_eq!(discounted_units(20.0, 0.5), U256::from(39u64) * U256::from(10u64).pow(U256::from(18u64)));
    }
}
