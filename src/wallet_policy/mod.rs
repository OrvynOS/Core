//! The wallet policy: what an agent's wallet may sign, checked before anything reaches Privy.
//!
//! - guard.rs: the gate in front of every agent wallet (held agents, rate limit, rules, daily caps,
//!   owner grants used once, the wallet_log).
//! - rules.rs: transactions, messages and typed data on BNB Chain, delegating to x402, trade, aster,
//!   orvynpay, venus and rng.
//! - spend.rs: daily caps; grants.rs: owner-signed withdrawals; config.rs: addresses and caps.

pub mod aster;
pub mod config;
pub mod grants;
pub mod guard;
pub mod orvynpay;
pub mod privy_policy;
pub mod rng;
pub mod rules;
pub mod spend;
pub mod trade;
pub mod venus;
pub mod x402;

use std::collections::BTreeMap;

use alloy::primitives::{Address, Bytes, U256};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Kind};

/// A signing request the rules refuse (the API answers 403).
pub fn refuse(message: impl Into<String>) -> Error {
    Error::new(Kind::Forbidden, message)
}

/// What a signature spends, per capped asset ("bnb", "usd", "trade-bnb", "x402-usd", …), in wei
/// or micro-USD.
pub type Spends = BTreeMap<String, U256>;

pub fn add_spend(spends: &mut Spends, asset: &str, amount: U256) {
    if amount > U256::ZERO {
        *spends.entry(asset.to_string()).or_default() += amount;
    }
}

/// The agent as the rules see it.
#[derive(Debug, Clone)]
pub struct EvmAgent {
    pub agent_id: String,
    /// Its main wallet (lowercase 0x).
    pub address: String,
    /// Its owner's wallet (lowercase 0x); the zero address before the agent row exists.
    pub owner: String,
    /// Its Aster API wallet, once created.
    pub aster_signer: Option<String>,
}

/// What a check found: a one-line summary for the log, what it spends, and the owner grant it
/// uses (burned when signed).
#[derive(Debug, Clone, Default)]
pub struct Checked {
    pub summary: String,
    pub spends: Spends,
    pub grant_nonce: Option<String>,
}

impl Checked {
    pub fn new(summary: impl Into<String>, spends: Spends) -> Self {
        Self { summary: summary.into(), spends, grant_nonce: None }
    }
}

/// A transaction to sign, as the agent's code builds it.
#[derive(Debug, Clone, Default)]
pub struct TxRequest {
    pub chain_id: u64,
    pub to: Option<Address>,
    pub value: U256,
    pub data: Bytes,
    pub nonce: Option<u64>,
    pub gas: Option<u64>,
    pub max_fee_per_gas: Option<u128>,
    pub max_priority_fee_per_gas: Option<u128>,
    pub gas_price: Option<u128>,
    /// EIP-7702 authorizations attached (always refused).
    pub has_authorization_list: bool,
}

/// EIP-712 typed data, in the JSON shape wallets and Privy take.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TypedData {
    #[serde(default)]
    pub domain: Value,
    #[serde(default)]
    pub types: BTreeMap<String, Vec<TypedField>>,
    #[serde(default)]
    pub primary_type: String,
    #[serde(default)]
    pub message: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TypedField {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
}

impl TypedData {
    pub fn domain_str(&self, k: &str) -> Option<&str> {
        self.domain.get(k).and_then(Value::as_str)
    }
    /// The domain's chain id, whether given as a number or a string.
    pub fn domain_chain_id(&self) -> Option<u64> {
        match self.domain.get("chainId")? {
            Value::Number(n) => n.as_u64(),
            Value::String(s) => s.parse().ok(),
            _ => None,
        }
    }
    /// Whether the types (other than EIP712Domain) are exactly `want`, field for field.
    pub fn has_types(&self, want: &[(&str, &[(&str, &str)])]) -> bool {
        let names: Vec<&String> = self.types.keys().filter(|k| *k != "EIP712Domain").collect();
        names.len() == want.len()
            && want.iter().all(|(name, fields)| {
                self.types.get(*name).is_some_and(|got| got.len() == fields.len() && fields.iter().zip(got).all(|((n, t), g)| g.name == *n && g.kind == *t))
            })
    }
}

pub(crate) fn addr(a: &Address) -> String {
    format!("{a:#x}")
}

/// 18-decimal stablecoin amount → micro-USD (the policy's USD unit).
pub(crate) fn usd_micros(amount: U256) -> U256 {
    amount / U256::from(10u64).pow(U256::from(12u8))
}

static NOW_OVERRIDE_MS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// Pin the policy's clock (tests). 0 unpins it.
pub fn set_now_for_tests(ms: i64) {
    NOW_OVERRIDE_MS.store(ms, std::sync::atomic::Ordering::SeqCst);
}

/// The policy's clock, in milliseconds.
pub fn now_ms() -> i64 {
    match NOW_OVERRIDE_MS.load(std::sync::atomic::Ordering::SeqCst) {
        0 => chrono::Utc::now().timestamp_millis(),
        pinned => pinned,
    }
}

pub(crate) fn now_secs() -> u64 {
    (now_ms() / 1000) as u64
}

/// A number in typed-data JSON (a number or a decimal/hex string) as U256.
pub(crate) fn json_u256(v: Option<&Value>, what: &str) -> crate::Result<U256> {
    let bad = || refuse(format!("malformed number in the {what}"));
    match v {
        Some(Value::Number(n)) => n.as_u64().map(U256::from).ok_or_else(bad),
        Some(Value::String(s)) => {
            if let Some(hex) = s.strip_prefix("0x") {
                U256::from_str_radix(hex, 16).map_err(|_| bad())
            } else {
                U256::from_str_radix(s, 10).map_err(|_| bad())
            }
        }
        _ => Err(bad()),
    }
}

pub(crate) fn json_lower(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.to_lowercase(),
        Some(other) if !other.is_null() => other.to_string().to_lowercase(),
        _ => String::new(),
    }
}
