//! BNB Smart Chain: the chain table, RPC clients, token reads, units, and signature checks.
//! Orvyn runs on BNB Chain only; the table keeps its shape (keyed by chain key) so integrations
//! take a chain the same way, but "bsc" is the only entry.
//!
//! Sending from an agent's wallet lives in `crate::wallets` (it goes through the wallet policy).

pub mod sig;

use std::sync::OnceLock;

use alloy::primitives::{Address, U256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::sol;

use crate::error::{Error, Result};

pub struct EvmChain {
    pub key: &'static str,
    pub label: &'static str,
    pub chain_id: u64,
    /// The public RPC when none is configured (viem's default for the chain).
    pub default_rpc: &'static str,
    pub kyber: &'static str,
    pub dexscreener: &'static str,
    pub geckoterminal: &'static str,
    /// GoPlus chain id.
    pub goplus: &'static str,
    pub native_symbol: &'static str,
    /// Wrapped native token.
    pub wrapped: &'static str,
    pub usdc: &'static str,
    pub usdt: &'static str,
}

/// The address integrations use for the native coin.
pub const NATIVE: &str = "0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE";

pub const BSC: EvmChain = EvmChain {
    key: "bsc",
    label: "BNB Chain",
    chain_id: 56,
    default_rpc: "https://56.rpc.thirdweb.com",
    kyber: "bsc",
    dexscreener: "bsc",
    geckoterminal: "bsc",
    goplus: "56",
    native_symbol: "BNB",
    wrapped: "0xbb4cdb9cbd36b01bd1cbaebf2de08d9173bc095c",
    // Binance-Peg, both 18 decimals.
    usdc: "0x8ac76a51cc950d9822d68b83fe1ad97b32cd580d",
    usdt: "0x55d398326f99059ff775485246999027b3197955",
};

pub const EVM_CHAINS: &[&EvmChain] = &[&BSC];

pub fn evm_chain(key: &str) -> Result<&'static EvmChain> {
    EVM_CHAINS
        .iter()
        .copied()
        .find(|c| c.key == key)
        .ok_or_else(|| Error::invalid(format!("Unsupported chain: {key}. Orvyn runs on BNB Chain (\"bsc\").")))
}

pub fn is_address(value: &str) -> bool {
    crate::env::is_address(value)
}

pub fn parse_address(value: &str) -> Result<Address> {
    if !is_address(value) {
        return Err(Error::invalid(format!("not an address: {value}")));
    }
    value.parse().map_err(|_| Error::invalid(format!("not an address: {value}")))
}

/// BNB, WBNB and the stablecoins: tokens that don't need a security screen.
pub fn is_base_asset(chain: &EvmChain, token: &str) -> bool {
    [NATIVE, chain.wrapped, chain.usdc, chain.usdt].iter().any(|t| t.eq_ignore_ascii_case(token))
}

/// The RPC for a chain: EVM_RPC_<KEY>, then (BSC) BSC_RPC_URL, then the public default.
pub fn rpc_url(chain_key: &str) -> String {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    var(&format!("EVM_RPC_{}", chain_key.to_uppercase()))
        .or_else(|| if chain_key == "bsc" { crate::env::env().bsc_rpc_url.clone() } else { None })
        .unwrap_or_else(|| evm_chain(chain_key).map(|c| c.default_rpc.to_string()).unwrap_or_default())
}

/// A read client for the chain (cached).
pub fn provider(chain_key: &str) -> Result<DynProvider> {
    static BSC_PROVIDER: OnceLock<DynProvider> = OnceLock::new();
    evm_chain(chain_key)?;
    let make = || {
        let url = rpc_url(chain_key).parse().expect("RPC URL");
        ProviderBuilder::new().connect_http(url).erased()
    };
    Ok(BSC_PROVIDER.get_or_init(make).clone())
}

/// The BNB Chain read client.
pub fn bsc() -> DynProvider {
    provider("bsc").expect("bsc")
}

sol! {
    #[sol(rpc)]
    interface IERC20 {
        function balanceOf(address owner) external view returns (uint256);
        function decimals() external view returns (uint8);
        function allowance(address owner, address spender) external view returns (uint256);
        function approve(address spender, uint256 amount) external returns (bool);
        function transfer(address to, uint256 amount) external returns (bool);
        function symbol() external view returns (string);
        event Transfer(address indexed from, address indexed to, uint256 value);
    }
}

pub fn rpc_error(e: impl std::fmt::Display) -> Error {
    Error::internal(format!("BNB Chain RPC: {e}"))
}

pub async fn bnb_balance(address: &str) -> Result<U256> {
    bsc().get_balance(parse_address(address)?).await.map_err(rpc_error)
}

pub async fn erc20_balance(token: &str, owner: &str) -> Result<U256> {
    IERC20::new(parse_address(token)?, bsc()).balanceOf(parse_address(owner)?).call().await.map_err(rpc_error)
}

pub async fn token_balance(chain_key: &str, token: &str, owner: &str) -> Result<U256> {
    let p = provider(chain_key)?;
    if token.eq_ignore_ascii_case(NATIVE) {
        return p.get_balance(parse_address(owner)?).await.map_err(rpc_error);
    }
    IERC20::new(parse_address(token)?, p).balanceOf(parse_address(owner)?).call().await.map_err(rpc_error)
}

pub async fn token_decimals(chain_key: &str, token: &str) -> Result<u8> {
    if token.eq_ignore_ascii_case(NATIVE) {
        return Ok(18);
    }
    IERC20::new(parse_address(token)?, provider(chain_key)?).decimals().call().await.map_err(rpc_error)
}

/// `amount` in whole units → base units (6 decimal places kept, as the TypeScript core does).
pub fn to_units(amount: f64, decimals: u8) -> U256 {
    let micros = (amount * 1e6).floor().max(0.0) as u128;
    U256::from(micros) * U256::from(10).pow(U256::from(decimals)) / U256::from(1_000_000u64)
}

/// Base units → whole units (as a float).
pub fn from_units(raw: U256, decimals: u8) -> f64 {
    raw.to_string().parse::<f64>().unwrap_or(f64::MAX) / 10f64.powi(decimals as i32)
}

/// BNB → wei (18 decimal places, as viem's parseEther(bnb.toFixed(18))).
pub fn to_wei(bnb: f64) -> U256 {
    alloy::primitives::utils::parse_ether(&format!("{bnb:.18}")).unwrap_or_default()
}

/// wei → BNB.
pub fn to_bnb(wei: U256) -> f64 {
    alloy::primitives::utils::format_ether(wei).parse().unwrap_or(0.0)
}

/// Upper bound on the gas a plain native transfer costs right now (with 2x headroom).
pub async fn native_transfer_cost(chain_key: &str) -> Result<U256> {
    let price = provider(chain_key)?.get_gas_price().await.map_err(rpc_error)?;
    Ok(U256::from(21_000u64) * U256::from(price) * U256::from(2u8))
}
