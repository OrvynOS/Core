//! Orvyn RNG on BNB Chain (contracts/src/rng/OrvynRNG.sol): agents' on-chain registration and the
//! relayer that commits and fulfils requested rounds.
//!
//! - A hosted agent registers its node from its own wallet, staking the contract's minimum, with
//!   itself as payee. The wallet policy allows exactly that.
//! - The relayer (ORVYN_RNG_RELAYER_KEY, or the operator key) commits closed rounds that contracts
//!   asked for before drand's round exists, then fulfils once drand publishes. It also pays out
//!   agents' earnings to their wallets.

use alloy::primitives::{Address, Bytes, FixedBytes, U256};
use alloy::sol;
use alloy::sol_types::{SolCall, SolEvent};
use serde_json::{json, Value};

use super::{aad, bls, drand, drand_round_for, round_close, round_of, DST_POSSESSION};
use crate::db::{db, Agent, RngNode};
use crate::env::env;
use crate::error::{Error, Result};
use crate::evm::{self, rpc_error};
use crate::secrets::{decrypt_secret, SecretKind};

sol! {
    #[sol(rpc)]
    interface OrvynRNG {
        function register(bytes publicKey, address payee, bytes proof) payable returns (uint256 index);
        function exit(uint256 index);
        function withdrawStake(uint256 index);
        function withdraw();
        function withdrawFor(address account);
        function commit(uint64 round, bytes32 value);
        function commitmentFor(uint64 round, uint32[] indices, bytes aggregateSignature) pure returns (bytes32);
        function fulfill(uint64 round, uint32[] indices, bytes publicKeys, bytes aggregateSignature, bytes drandSignature) returns (bytes32);
        function pendingRequests(uint64 round) view returns (uint256);
        function commitment(uint64 round) view returns (bytes32);
        function randomness(uint64 round) view returns (bytes32);
        function earnings(address account) view returns (uint256);
        function minStake() view returns (uint256);
        event NodeRegistered(uint256 indexed index, address indexed payee, bytes publicKey);
    }
}

pub fn rng_contract() -> Option<Address> {
    env().orvyn_rng_contract.as_deref().and_then(|a| a.parse().ok())
}

pub fn relayer_key() -> Result<String> {
    let e = env();
    e.orvyn_rng_relayer_key.clone().or_else(|| e.orvyn_operator_private_key.clone()).ok_or_else(|| Error::unavailable("ORVYN_RNG_RELAYER_KEY (or ORVYN_OPERATOR_PRIVATE_KEY) is not set"))
}

fn contract(addr: Address) -> OrvynRNG::OrvynRNGInstance<alloy::providers::DynProvider> {
    OrvynRNG::new(addr, evm::bsc())
}

fn unhex(s: &str) -> Bytes {
    hex::decode(s.trim_start_matches("0x")).unwrap_or_default().into()
}

/// The registration proof the contract checks: "orvyn-rng:v1:register:" ‖ payee ‖ publicKey.
pub fn registration_proof(secret_hex: &str, payee: &str, public_key: &str) -> Result<String> {
    let mut msg = b"orvyn-rng:v1:register:".to_vec();
    msg.extend(evm::parse_address(payee)?.as_slice());
    msg.extend(unhex(&bls::g2_eip(public_key)?).as_ref());
    bls::g1_eip(&bls::sign(secret_hex, &msg, DST_POSSESSION)?)
}

/// Register a hosted agent's node on chain from its own wallet, staking the minimum, once it can
/// afford it (stake plus its gas reserve). Returns {state, index?, needBnb?}.
pub async fn register_on_chain(agent: &Agent) -> Result<Value> {
    let Some(addr) = rng_contract() else { return Ok(json!({ "state": "off" })) };
    let node: Option<RngNode> = sqlx::query_as("select * from rng_nodes where agent_id = $1").bind(&agent.id).fetch_optional(db()).await?;
    let Some(node) = node.filter(|n| n.active && n.secret_encrypted.is_some()) else { return Ok(json!({ "state": "not-a-node" })) };
    if let Some(i) = node.chain_index {
        return Ok(json!({ "state": "registered", "index": i }));
    }
    let c = contract(addr);
    let stake = c.minStake().call().await.map_err(rpc_error)?;
    let have = evm::bnb_balance(&agent.evm_address).await?;
    let need = stake + evm::to_wei(agent.gas_reserve_bnb) + evm::to_wei(0.002);
    if have < need {
        let short = evm::to_bnb(need - have) + 0.0001;
        return Ok(json!({ "state": "needs-bnb", "needBnb": format!("{short:.4}") }));
    }
    let payee = evm::parse_address(&agent.evm_address)?.to_string();
    let secret = decrypt_secret(node.secret_encrypted.as_deref().unwrap(), &aad(&agent.id), SecretKind::AiKey)?;
    let proof = registration_proof(&secret, &payee, &node.public_key)?;
    let data = OrvynRNG::registerCall { publicKey: unhex(&bls::g2_eip(&node.public_key)?), payee: evm::parse_address(&payee)?, proof: unhex(&proof) }.abi_encode();
    let tx = crate::wallets::agent_send(&agent.id, &agent.evm_wallet, addr, data.into(), stake).await?;
    let receipt = crate::wallets::receipt_of(tx).await?.ok_or_else(|| Error::internal("no receipt"))?;
    for log in receipt.inner.logs() {
        if log.address() != addr {
            continue;
        }
        if let Ok(e) = OrvynRNG::NodeRegistered::decode_log_data(log.data()) {
            let index: i32 = e.index.to::<u64>() as i32;
            sqlx::query("update rng_nodes set chain_index = $2 where agent_id = $1").bind(&agent.id).bind(index).execute(db()).await?;
            return Ok(json!({ "state": "registered", "index": index }));
        }
    }
    Err(Error::internal(format!("registration {tx} didn't emit NodeRegistered")))
}

pub struct Participants {
    pub indices: Vec<u32>,
    pub public_keys: Bytes,
    pub aggregate: String,
    pub aggregate_eip: Bytes,
}

#[derive(sqlx::FromRow)]
struct Row {
    signature: String,
    public_key: String,
    chain_index: Option<i32>,
}

/// The staked participants of `round`, in index order, as the contract wants them.
pub async fn chain_participants(round: i64) -> Result<Option<Participants>> {
    let rows: Vec<Row> = sqlx::query_as(
        "select p.signature, n.public_key, n.chain_index from rng_partials p join rng_nodes n on n.agent_id = p.agent_id
         where p.round = $1 and n.chain_index is not null and p.created_at < to_timestamp($2) order by n.chain_index asc",
    )
    .bind(round as i32)
    .bind(round_close(round) as f64)
    .fetch_all(db())
    .await?;
    if rows.is_empty() {
        return Ok(None);
    }
    let aggregate = bls::aggregate_signatures(&rows.iter().map(|r| r.signature.clone()).collect::<Vec<_>>())?;
    let mut pks = String::new();
    for r in &rows {
        pks.push_str(&bls::g2_eip(&r.public_key)?);
    }
    Ok(Some(Participants {
        indices: rows.iter().map(|r| r.chain_index.unwrap() as u32).collect(),
        public_keys: unhex(&pks),
        aggregate_eip: unhex(&bls::g1_eip(&aggregate)?),
        aggregate,
    }))
}

/// One relayer pass: commit closed rounds that contracts asked for, inside the commit window;
/// fulfil committed rounds once drand is out.
pub async fn relay_rounds(now: f64) -> Result<Value> {
    let Some(addr) = rng_contract() else { return Ok(json!({ "committed": 0, "fulfilled": 0 })) };
    let key = relayer_key()?;
    let c = contract(addr);
    let (mut committed, mut fulfilled) = (0, 0);
    let current = round_of(now);
    for round in current - 3..=current {
        if now < round_close(round) as f64 {
            continue;
        }
        let d = drand_round_for(round);
        let drand_at = drand::time_of_round(d) as f64;
        let r = round as u64;
        let (pending, commitment, out) = (c.pendingRequests(r).call().await.map_err(rpc_error)?, c.commitment(r).call().await.map_err(rpc_error)?, c.randomness(r).call().await.map_err(rpc_error)?);
        if pending == U256::ZERO && commitment == FixedBytes::ZERO {
            continue; // nobody asked for this round
        }
        let Some(set) = chain_participants(round).await? else { continue };
        if commitment == FixedBytes::ZERO && now < drand_at - 1.0 {
            let value = c.commitmentFor(r, set.indices.clone(), set.aggregate_eip.clone()).call().await.map_err(rpc_error)?;
            let hash = crate::wallets::send_from_key(&key, addr, OrvynRNG::commitCall { round: r, value }.abi_encode().into(), U256::ZERO).await?;
            sqlx::query("update rng_rounds set commit_tx = $2 where round = $1").bind(round as i32).bind(format!("{hash:#x}")).execute(db()).await?;
            committed += 1;
            continue;
        }
        if commitment != FixedBytes::ZERO && out == FixedBytes::ZERO && now >= drand_at {
            let b = drand::beacon(Some(d)).await?;
            let data = OrvynRNG::fulfillCall { round: r, indices: set.indices.clone(), publicKeys: set.public_keys.clone(), aggregateSignature: set.aggregate_eip.clone(), drandSignature: unhex(&bls::g1_eip(&b.signature)?) }.abi_encode();
            let gas = 3_000_000 + 60_000 * set.indices.len() as u64 + 200_000 * pending.saturating_to::<u64>();
            let hash = crate::wallets::send_from_key_with_gas(&key, addr, data.into(), U256::ZERO, Some(gas)).await?;
            sqlx::query("update rng_rounds set fulfill_tx = $2 where round = $1").bind(round as i32).bind(format!("{hash:#x}")).execute(db()).await?;
            fulfilled += 1;
        }
    }
    Ok(json!({ "committed": committed, "fulfilled": fulfilled }))
}

/// Pay out a hosted agent's on-chain earnings to its wallet when they're worth the gas.
pub async fn pay_out(agent: &Agent) -> Result<Option<String>> {
    let Some(addr) = rng_contract() else { return Ok(None) };
    let account = evm::parse_address(&agent.evm_address)?;
    let owed = contract(addr).earnings(account).call().await.map_err(rpc_error)?;
    if owed < evm::to_wei(0.001) {
        return Ok(None);
    }
    let hash = crate::wallets::send_from_key(&relayer_key()?, addr, OrvynRNG::withdrawForCall { account }.abi_encode().into(), U256::ZERO).await?;
    Ok(Some(format!("{hash:#x}")))
}

/// Nodes that are active off chain but not yet staked on chain (for status pages).
pub async fn unstaked_nodes() -> Result<Vec<String>> {
    Ok(sqlx::query_scalar("select agent_id from rng_nodes where active and chain_index is null").fetch_all(db()).await?)
}
