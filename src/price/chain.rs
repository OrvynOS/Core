//! The price relayer: submits finalised reports to OrvynPriceFeed.sol when the contract would take
//! them (the price moved by the feed's threshold, or its heartbeat is due) and enough staked nodes
//! signed them. Submitting is permissionless; Orvyn's relayer does it so feeds stay fresh.

use alloy::primitives::{Address, Bytes, I256, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use serde_json::{json, Value};

use super::{feed_id, FEEDS};
use crate::db::{db, PriceReport};
use crate::error::Result;
use crate::evm::{self, rpc_error};
use crate::rng::bls;

sol! {
    #[sol(rpc)]
    interface OrvynPriceFeed {
        struct Feed { string description; uint16 quorum; uint16 deviationBps; uint32 heartbeat; address referenceFeed; uint16 maxReferenceDeviationBps; uint256 rewardPerReport; uint16 signersBps; uint256 balance; int256 price; uint64 round; uint64 updatedAt; uint16 signers; bool exists; }
        function submit(bytes32 feedId, uint64 round, int256 price, uint32[] indices, bytes publicKeys, bytes aggregateSignature);
        function isUseful(bytes32 feedId, int256 price) view returns (bool);
        function latest(bytes32 feedId) view returns (int256 price, uint64 round, uint64 updatedAt, uint16 signers);
        function feed(bytes32 feedId) view returns (Feed);
    }
}

pub fn price_feed_contract() -> Option<Address> {
    std::env::var("ORVYN_PRICE_FEED_CONTRACT").ok().and_then(|a| a.parse().ok())
}

/// Submit each feed's latest finalised report if the contract would take it.
pub async fn relay_reports() -> Result<Value> {
    let Some(addr) = price_feed_contract() else { return Ok(json!({ "submitted": 0 })) };
    let c = OrvynPriceFeed::new(addr, evm::bsc());
    let mut submitted = 0;
    for (pair, _) in FEEDS {
        let id = feed_id(pair);
        let r: Option<PriceReport> = sqlx::query_as("select * from price_reports where feed = $1 and aggregate_signature is not null and submit_tx is null order by round desc limit 1").bind(pair).fetch_optional(db()).await?;
        let Some(r) = r else { continue };
        let participants = r.participants.as_ref().and_then(|p| p.as_array()).cloned().unwrap_or_default();
        if participants.is_empty() {
            continue;
        }
        let on_chain = c.feed(id).call().await.map_err(rpc_error)?;
        if !on_chain.exists || (r.round as u64) <= on_chain.round || participants.len() < on_chain.quorum as usize {
            continue;
        }
        let price: I256 = r.price.parse().unwrap_or_default();
        if !c.isUseful(id, price).call().await.map_err(rpc_error)? {
            continue;
        }
        // Signers in chain-index order, as the contract checks them.
        let mut nodes = vec![];
        for p in &participants {
            let index: Option<Option<i32>> = sqlx::query_scalar("select chain_index from rng_nodes where agent_id = $1").bind(p["agentId"].as_str().unwrap_or("")).fetch_optional(db()).await?;
            match index.flatten() {
                Some(i) => nodes.push((i as u32, p["publicKey"].as_str().unwrap_or("").to_string())),
                None => break,
            }
        }
        if nodes.len() != participants.len() {
            continue;
        }
        nodes.sort_by_key(|n| n.0);
        let mut pks = String::new();
        for (_, pk) in &nodes {
            pks.push_str(&bls::g2_eip(pk)?);
        }
        let data = OrvynPriceFeed::submitCall {
            feedId: id,
            round: r.round as u64,
            price,
            indices: nodes.iter().map(|n| n.0).collect(),
            publicKeys: Bytes::from(hex::decode(pks).unwrap_or_default()),
            aggregateSignature: Bytes::from(hex::decode(bls::g1_eip(r.aggregate_signature.as_deref().unwrap_or(""))?).unwrap_or_default()),
        }
        .abi_encode();
        let hash = crate::wallets::send_from_key(&crate::rng::chain::relayer_key()?, addr, data.into(), U256::ZERO).await?;
        sqlx::query("update price_reports set submit_tx = $3 where feed = $1 and round = $2").bind(pair).bind(r.round).bind(format!("{hash:#x}")).execute(db()).await?;
        submitted += 1;
    }
    Ok(json!({ "submitted": submitted }))
}
