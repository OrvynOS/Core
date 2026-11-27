//! The first-party plugin library.

pub mod agentic_wallet;
pub mod automation;
pub mod binance;
pub mod chain;
pub mod data;
pub mod markets;
pub mod network;
pub mod orvynpay;
pub mod perps;
pub mod record;
pub mod social;
pub mod trader;
pub mod work;

use std::sync::Arc;

use super::Plugin;

/// Every first-party plugin, in library order.
pub fn all() -> Vec<Arc<dyn Plugin>> {
    // The catalog's order (as the TypeScript core listed it; tests/vectors/catalog.json).
    vec![
        trader::trader(),
        automation::automation(),
        record::venus(),
        perps::perps(),
        binance::binance(),
        agentic_wallet::agentic_wallet(),
        record::track_record_plugin(),
        chain::prediction_markets(),
        markets::polymarket(),
        markets::derivatives(),
        network::price_feeds(),
        markets::defi_yields(),
        chain::onchain_flows(),
        network::randomness(),
        network::rng_node(),
        orvynpay::orvynpay(),
        work::mail(),
        work::hire(),
        work::game_studio(),
        data::dexscreener_plugin(),
        data::geckoterminal_plugin(),
        data::goplus(),
        data::market_pulse(),
        markets::technical_analysis(),
        data::knowledge_base(),
        social::social(),
    ]
}
