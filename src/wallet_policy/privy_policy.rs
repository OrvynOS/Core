//! Privy wallet policy for agent wallets: a coarser copy of the guard's rules that Privy enforces
//! inside its enclave, so a bug that skipped the guard still couldn't sign just anything. Privy
//! refuses whatever no ALLOW rule matches.
//!
//! - Transactions on BNB Chain only (chain id 56), and only:
//!   - calls to Orvyn's registry, four.meme's TokenManager2, the ERC-8004 registry and the trading
//!     contracts (PancakeSwap, Uniswap, Flap), Aster's vault, OrvynPay, the shielded pool, Venus
//!     and Orvyn RNG;
//!   - `approve` (any token) to Permit2, the registry or a trading contract;
//!   - `transfer` (any token) and plain BNB sends: owner-signed withdrawals. Privy can't check the
//!     owner's signature, so only the guard enforces that; these rules stop a token's funds from
//!     being approved or routed anywhere else.
//! - Typed data: Permit2 on BNB Chain (x402), and Aster account actions (chain 56, no verifying contract).
//! - Messages: up to 4 KB (OrvynID proofs, four.meme sign-in).
//! - Never: key export, raw hash signing, EIP-7702.
//!
//! Created once (`create_policies`, by `orvyn-admin privy-policy`), owned by Orvyn's authorization key
//! so only it can change it, and attached to each new agent wallet (PRIVY_AGENT_POLICY_ID).

use alloy::primitives::Address;
use serde_json::{json, Value};

use super::aster::{ACCOUNT_CHAIN_ID, SIGNING_CHAIN_ID, VAULT};
use super::config::cfg;
use super::trade::trade_contracts;
use super::venus::venus_contracts;
use super::x402::PERMIT2;
use crate::env::env;
use crate::error::Result;

const ZERO: &str = "0x0000000000000000000000000000000000000000";

/// Both spellings of each address, lowercase then checksummed, without repeats (in order).
fn both<'a>(addresses: impl IntoIterator<Item = Option<&'a str>>) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for a in addresses.into_iter().flatten() {
        let lower = a.to_lowercase();
        let checksum = lower.parse::<Address>().map(|x| x.to_checksum(None)).unwrap_or_else(|_| a.to_string());
        for s in [lower, checksum] {
            if !out.contains(&s) {
                out.push(s);
            }
        }
    }
    out
}

/// A one-function ABI, as viem's parseAbi gives it.
fn abi(name: &str, a: &str, b: &str) -> Value {
    json!([{
        "type": "function",
        "name": name,
        "stateMutability": "nonpayable",
        "inputs": [{ "type": "address", "name": a }, { "type": "uint256", "name": b }],
        "outputs": [{ "type": "bool" }],
    }])
}

fn cond(source: &str, field: &str, operator: &str, value: Value) -> Value {
    json!({ "field_source": source, "field": field, "operator": operator, "value": value })
}

fn on_bsc() -> Value {
    cond("ethereum_transaction", "chain_id", "eq", json!("56"))
}

fn rule(name: &str, method: &str, conditions: Vec<Value>) -> Value {
    json!({ "name": name, "method": method, "action": "ALLOW", "conditions": conditions })
}

pub fn agent_policy_rules() -> Vec<Value> {
    let c = cfg();
    let trade = trade_contracts();
    let venus = venus_contracts();
    let erc8004 = (c.erc8004_chain == "bsc").then_some(c.erc8004_registry.as_str());
    let contracts = both(
        [c.registry.as_deref(), Some(c.fourmeme_manager.as_str()), erc8004, Some(VAULT), c.orvyn_pay.as_deref(), c.shielded_pool.as_deref()]
            .into_iter()
            .chain(trade.iter().map(|t| Some(t.as_str())))
            .chain(venus.iter().map(|v| Some(*v)))
            .chain([c.orvyn_rng.as_deref()]),
    );
    let spenders = both(
        [Some(PERMIT2), c.registry.as_deref(), Some(VAULT), c.orvyn_pay.as_deref(), c.shielded_pool.as_deref()]
            .into_iter()
            .chain(trade.iter().map(|t| Some(t.as_str())))
            .chain(venus.iter().map(|v| Some(*v))),
    );
    let mut approve = cond("ethereum_calldata", "approve.spender", "in", json!(spenders));
    approve["abi"] = abi("approve", "spender", "amount");
    // Any `transfer` call (Privy only allows eq / in on an address field, so match the amount):
    // the recipient is the owner's withdrawal address, which the guard checks.
    let mut transfer = cond("ethereum_calldata", "transfer.amount", "gte", json!("0"));
    transfer["abi"] = abi("transfer", "to", "amount");
    let no_value = || cond("ethereum_transaction", "value", "eq", json!("0"));
    vec![
        rule("Orvyn contracts and trading venues", "eth_signTransaction", vec![on_bsc(), cond("ethereum_transaction", "to", "in", json!(contracts))]),
        // Privy rule names are under 50 characters.
        rule("Approvals: Permit2, registry, trading venues", "eth_signTransaction", vec![on_bsc(), no_value(), approve]),
        rule("Token withdrawals (owner-signed, Orvyn-checked)", "eth_signTransaction", vec![on_bsc(), no_value(), transfer]),
        rule("BNB withdrawals (owner-signed, checked by Orvyn)", "eth_signTransaction", vec![on_bsc(), cond("ethereum_transaction", "data_byte_length", "eq", json!("0"))]),
        rule(
            "x402 payments (Permit2 on BNB Chain)",
            "eth_signTypedData_v4",
            vec![cond("ethereum_typed_data_domain", "chainId", "eq", json!("56")), cond("ethereum_typed_data_domain", "verifyingContract", "in", json!(both([Some(PERMIT2)])))],
        ),
        rule(
            "Aster account actions (API wallet, withdraw)",
            "eth_signTypedData_v4",
            vec![cond("ethereum_typed_data_domain", "chainId", "eq", json!(ACCOUNT_CHAIN_ID.to_string())), cond("ethereum_typed_data_domain", "verifyingContract", "eq", json!(ZERO))],
        ),
        rule("Short messages (OrvynID, four.meme sign-in)", "personal_sign", vec![cond("message", "byte_length", "lte", json!("4096"))]),
    ]
}

/// Privy wallet policy for each agent's Aster API wallet: Aster API request signatures only
/// (EIP-712 on Aster's signing chain id 1666, no verifying contract). No transactions, messages
/// or other typed data, so it can never move funds on chain.
pub fn aster_signer_policy_rules() -> Vec<Value> {
    vec![rule(
        "Aster API requests",
        "eth_signTypedData_v4",
        vec![cond("ethereum_typed_data_domain", "chainId", "eq", json!(SIGNING_CHAIN_ID.to_string())), cond("ethereum_typed_data_domain", "verifyingContract", "eq", json!(ZERO))],
    )]
}

/// Create both policies in the Privy app, owned by PRIVY_AUTHORIZATION_KEY_ID when it's set.
/// Returns (agent policy, Aster policy) as Privy answered.
pub async fn create_policies() -> Result<(Value, Value)> {
    let owner = env().privy_authorization_key_id.clone();
    let create = |name: &str, rules: Vec<Value>| {
        let mut body = json!({ "version": "1.0", "name": name, "chain_type": "ethereum", "rules": rules });
        if let Some(o) = &owner {
            body["owner_id"] = json!(o);
        }
        body
    };
    let agent = crate::privy::send(crate::privy::request(reqwest::Method::POST, "/v1/policies")?.json(&create("Orvyn agent wallets", agent_policy_rules()))).await?;
    let aster = crate::privy::send(crate::privy::request(reqwest::Method::POST, "/v1/policies")?.json(&create("Orvyn Aster API wallets", aster_signer_policy_rules()))).await?;
    Ok((agent, aster))
}
