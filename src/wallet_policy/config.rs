//! What the wallet policy needs to know about the outside world, from the environment, with BNB
//! Chain mainnet defaults. Read once, on first use (tests can reload).

use std::sync::{Arc, LazyLock, RwLock};

use alloy::primitives::U256;

pub struct PolicyConfig {
    pub chain_id: u64,
    pub registry: Option<String>,
    pub fourmeme_manager: String,
    pub erc8004_registry: String,
    pub erc8004_chain: String,
    pub pancake_v2_router: Option<String>,
    pub pancake_smart_router: Option<String>,
    pub uniswap_router: Option<String>,
    pub flap_portal: Option<String>,
    pub daily_cap_trade_bnb: U256,
    pub daily_cap_trade_usd: U256,
    pub stablecoins: Vec<String>,
    pub grant_max_age_ms: i64,
    pub max_launch_fee: U256,
    pub daily_cap_bnb: U256,
    pub daily_cap_usd: U256,
    pub x402_tokens: Vec<String>,
    pub x402_max_per_payment: U256,
    pub x402_max_approval: U256,
    pub x402_max_deadline_sec: u64,
    pub daily_cap_x402_usd: U256,
    pub aster_symbols: Vec<String>,
    pub aster_max_leverage: f64,
    pub aster_max_order_usd: f64,
    pub daily_cap_aster_usd: U256,
    pub daily_cap_aster_order_usd: U256,
    pub orvyn_pay: Option<String>,
    /// The jobs escrow (OrvynJobs): agents hiring other agents fund, approve, dispute and reclaim there.
    pub orvyn_jobs: Option<String>,
    pub orvyn_rng: Option<String>,
    pub max_rng_stake: U256,
    pub shielded_pool: Option<String>,
    pub daily_cap_pay_bnb: U256,
    pub daily_cap_pay_usd: U256,
    pub daily_cap_lend_bnb: U256,
    pub daily_cap_lend_usd: U256,
    pub rate_per_min: usize,
}

const WEI: u32 = 18;
const USD: u32 = 6;

/// "1.5" with `digits` decimals → base units (extra decimals are dropped, as the TypeScript core does).
pub fn decimal(value: Option<String>, fallback: &str, digits: u32) -> U256 {
    let v = value.filter(|v| !v.is_empty()).unwrap_or_else(|| fallback.to_string());
    let (whole, frac) = v.split_once('.').unwrap_or((&v, ""));
    let unit = U256::from(10u64).pow(U256::from(digits));
    let mut frac: String = frac.chars().take(digits as usize).collect();
    while frac.len() < digits as usize {
        frac.push('0');
    }
    let whole = U256::from_str_radix(whole, 10).unwrap_or_default();
    let frac = if frac.is_empty() { U256::ZERO } else { U256::from_str_radix(&frac, 10).unwrap_or_default() };
    whole * unit + frac
}

fn var(k: &str) -> Option<String> {
    std::env::var(k).ok()
}
fn lower(v: Option<String>) -> Option<String> {
    v.filter(|v| !v.is_empty()).map(|v| v.to_lowercase())
}
/// A trading venue's address, or None when set to "off".
fn venue(v: Option<String>, fallback: &str) -> Option<String> {
    match v.as_deref() {
        Some("off") => None,
        Some(x) if !x.is_empty() => Some(x.to_lowercase()),
        _ => Some(fallback.to_lowercase()),
    }
}
fn list(v: Option<String>, fallback: &str, upper: bool) -> Vec<String> {
    v.filter(|v| !v.is_empty())
        .unwrap_or_else(|| fallback.to_string())
        .split(',')
        .map(|t| if upper { t.trim().to_uppercase() } else { t.trim().to_lowercase() })
        .filter(|t| !t.is_empty())
        .collect()
}
fn num(v: Option<String>, fallback: f64) -> f64 {
    v.filter(|v| !v.is_empty()).and_then(|v| v.parse().ok()).unwrap_or(fallback)
}

pub fn load() -> PolicyConfig {
    PolicyConfig {
        chain_id: 56,
        registry: lower(var("ORVYN_REGISTRY_CONTRACT")),
        fourmeme_manager: lower(var("FOURMEME_TOKEN_MANAGER")).unwrap_or_else(|| "0x5c952063c7fc8610ffdb798152d69f0b9550762b".into()),
        erc8004_registry: lower(var("ERC8004_REGISTRY")).unwrap_or_else(|| "0x8004a169fb4a3325136eb29fa0ceb6d2e539a432".into()),
        erc8004_chain: var("ERC8004_CHAIN").filter(|v| !v.is_empty()).unwrap_or_else(|| "bsc".into()),
        pancake_v2_router: venue(var("WALLET_PANCAKE_V2_ROUTER"), "0x10ED43C718714eb63d5aA57B78B54704E256024E"),
        pancake_smart_router: venue(var("WALLET_PANCAKE_SMART_ROUTER"), "0x13f4EA83D0bd40E75C8222255bc855a974568Dd4"),
        uniswap_router: venue(var("WALLET_UNISWAP_ROUTER"), "0xB971eF87ede563556b2ED4b1C0b0019111Dd85d2"),
        flap_portal: venue(var("WALLET_FLAP_PORTAL").or_else(|| var("FLAP_PORTAL")), "0xe2cE6ab80874Fa9Fa2aAE65D277Dd6B8e65C9De0"),
        daily_cap_trade_bnb: decimal(var("WALLET_DAILY_CAP_TRADE_BNB"), "5", WEI),
        daily_cap_trade_usd: decimal(var("WALLET_DAILY_CAP_TRADE_USD"), "5000", USD),
        stablecoins: vec!["0x55d398326f99059ff775485246999027b3197955".into(), "0x8ac76a51cc950d9822d68b83fe1ad97b32cd580d".into()],
        grant_max_age_ms: 10 * 60_000,
        max_launch_fee: decimal(var("WALLET_MAX_LAUNCH_FEE_BNB"), "0.05", WEI),
        daily_cap_bnb: decimal(var("WALLET_DAILY_CAP_BNB"), "1", WEI),
        daily_cap_usd: decimal(var("WALLET_DAILY_CAP_USD"), "1000", USD),
        x402_tokens: list(var("WALLET_X402_TOKENS"), "0x55d398326f99059fF775485246999027B3197955,0x8AC76a51cc950d9822D68b83fE1Ad97B32Cd580d", false),
        x402_max_per_payment: decimal(var("WALLET_X402_MAX_PER_PAYMENT_USD"), "10", WEI),
        x402_max_approval: decimal(var("WALLET_X402_MAX_APPROVAL_USD"), "100", WEI),
        x402_max_deadline_sec: num(var("WALLET_X402_MAX_DEADLINE_SEC"), 3600.0) as u64,
        daily_cap_x402_usd: decimal(var("WALLET_DAILY_CAP_X402_USD"), "50", USD),
        aster_symbols: list(var("WALLET_ASTER_SYMBOLS"), "BTCUSDT,ETHUSDT,BNBUSDT,SOLUSDT,ASTERUSDT", true),
        aster_max_leverage: num(var("WALLET_ASTER_MAX_LEVERAGE"), 10.0),
        aster_max_order_usd: num(var("WALLET_ASTER_MAX_ORDER_USD"), 1000.0),
        daily_cap_aster_usd: decimal(var("WALLET_DAILY_CAP_ASTER_USD"), "1000", USD),
        daily_cap_aster_order_usd: decimal(var("WALLET_DAILY_CAP_ASTER_ORDER_USD"), "20000", USD),
        orvyn_pay: lower(var("ORVYN_PAY_CONTRACT")),
        orvyn_jobs: lower(var("ORVYN_JOBS_CONTRACT")),
        orvyn_rng: lower(var("ORVYN_RNG_CONTRACT")),
        max_rng_stake: decimal(var("WALLET_MAX_RNG_STAKE_BNB"), "0.5", WEI),
        shielded_pool: lower(var("ORVYN_SHIELDED_POOL")),
        daily_cap_pay_bnb: decimal(var("WALLET_DAILY_CAP_PAY_BNB"), "0.5", WEI),
        daily_cap_pay_usd: decimal(var("WALLET_DAILY_CAP_PAY_USD"), "500", USD),
        daily_cap_lend_bnb: decimal(var("WALLET_DAILY_CAP_LEND_BNB"), "1", WEI),
        daily_cap_lend_usd: decimal(var("WALLET_DAILY_CAP_LEND_USD"), "1000", USD),
        rate_per_min: num(var("WALLET_RATE_PER_MIN"), 60.0) as usize,
    }
}

static CONFIG: LazyLock<RwLock<Arc<PolicyConfig>>> = LazyLock::new(|| RwLock::new(Arc::new(load())));

pub fn cfg() -> Arc<PolicyConfig> {
    CONFIG.read().unwrap().clone()
}

/// Re-read the environment (tests).
pub fn reload() {
    *CONFIG.write().unwrap() = Arc::new(load());
}

/// The daily cap for a spend asset, or None if that asset isn't capped.
pub fn daily_cap(asset: &str) -> Option<U256> {
    let c = cfg();
    Some(match asset {
        "bnb" => c.daily_cap_bnb,
        "usd" => c.daily_cap_usd,
        "x402-usd" => c.daily_cap_x402_usd,
        "trade-bnb" => c.daily_cap_trade_bnb,
        "trade-usd" => c.daily_cap_trade_usd,
        "aster-usd" => c.daily_cap_aster_usd,
        "pay-bnb" => c.daily_cap_pay_bnb,
        "pay-usd" => c.daily_cap_pay_usd,
        "aster-order-usd" => c.daily_cap_aster_order_usd,
        "lend-bnb" => c.daily_cap_lend_bnb,
        "lend-usd" => c.daily_cap_lend_usd,
        _ => return None,
    })
}
