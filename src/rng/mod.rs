//! Orvyn RNG: a verifiable randomness network run by agents.
//!
//! Each participating agent holds a BLS12-381 key used only for this (it can't move funds). Every
//! minute is a round. During the first 50 seconds each agent signs the round number; BLS signatures
//! are unique, so an agent has exactly one valid contribution per round. At the close the
//! signatures are aggregated (and, with a contract, committed on chain within the 20-second commit
//! window), and the round's output is
//!
//!   output = sha256("orvyn-rng:v1" ‖ round (8 bytes) ‖ compressed aggregate (48) ‖ drandRandomness(D))
//!
//! where D is the first drand round after the commit window. contracts/src/rng/OrvynRNG.sol
//! verifies exactly this on BNB Chain. Anyone can verify a round from its record.

pub mod bls;
pub mod chain;
pub mod drand;

use std::future::Future;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::db::{db, RngNode, RngRound};
use crate::error::{Error, Result};
use crate::secrets::{decrypt_secret, encrypt_secret, SecretKind};

pub const VERSION: &str = "orvyn-rng:v1";
pub const PERIOD_SECONDS: i64 = 60;
/// Contributions are accepted for this many seconds of each round.
pub const CLOSE_AFTER_SECONDS: i64 = 50;
/// After the close, this long to fix the participants (on chain) before drand's round exists.
pub const COMMIT_WINDOW_SECONDS: i64 = 20;
pub const DST_SIGNATURE: &[u8] = b"BLS_SIG_BLS12381G1_XMD:SHA-256_SSWU_RO_POP_";
pub const DST_POSSESSION: &[u8] = b"BLS_POP_BLS12381G1_XMD:SHA-256_SSWU_RO_POP_";

pub fn aad(agent_id: &str) -> String {
    format!("rng:{agent_id}")
}

pub fn now_s() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64 / 1000.0
}

pub fn round_of(unix_seconds: f64) -> i64 {
    (unix_seconds / PERIOD_SECONDS as f64).floor() as i64
}
pub fn round_start(round: i64) -> i64 {
    round * PERIOD_SECONDS
}
pub fn round_close(round: i64) -> i64 {
    round_start(round) + CLOSE_AFTER_SECONDS
}
/// The drand round mixed into `round`: the first one after the commit window.
pub fn drand_round_for(round: i64) -> i64 {
    drand::round_at((round_close(round) + COMMIT_WINDOW_SECONDS) as f64) + 1
}

pub fn round_message(round: i64) -> Vec<u8> {
    format!("{VERSION}:{round}").into_bytes()
}

fn possession_message(public_key: &str) -> Result<Vec<u8>> {
    let mut m = format!("{VERSION}:pop:").into_bytes();
    m.extend(hex::decode(bls::g2_eip(public_key)?).unwrap());
    Ok(m)
}

pub fn sign_round(secret_hex: &str, round: i64) -> Result<String> {
    bls::sign(secret_hex, &round_message(round), DST_SIGNATURE)
}

/// Proof that the holder of `public_key` controls its secret (required to join).
pub fn prove_key(secret_hex: &str, public_key: &str) -> Result<String> {
    bls::sign(secret_hex, &possession_message(public_key)?, DST_POSSESSION)
}

fn rng_error(m: impl Into<String>) -> Error {
    Error::invalid(m)
}

// ---------- Membership ----------

async fn node(agent_id: &str) -> Result<Option<RngNode>> {
    Ok(sqlx::query_as("select * from rng_nodes where agent_id = $1").bind(agent_id).fetch_optional(db()).await?)
}

/// A hosted agent joins: Orvyn makes and seals its key. Idempotent; re-activates a node that left.
pub async fn join_hosted(agent_id: &str) -> Result<Value> {
    if let Some(n) = node(agent_id).await? {
        if !n.active {
            sqlx::query("update rng_nodes set active = true where agent_id = $1").bind(agent_id).execute(db()).await?;
        }
        return Ok(json!({ "agentId": agent_id, "publicKey": n.public_key }));
    }
    let (secret, public_key) = bls::new_key();
    sqlx::query("insert into rng_nodes (agent_id, public_key, secret_encrypted) values ($1, $2, $3) on conflict do nothing")
        .bind(agent_id)
        .bind(&public_key)
        .bind(encrypt_secret(&secret, &aad(agent_id), SecretKind::AiKey)?)
        .execute(db())
        .await?;
    Ok(json!({ "agentId": agent_id, "publicKey": public_key }))
}

/// A self-hosted agent joins with its own key and a proof of possession.
pub async fn join_external(agent_id: &str, public_key: &str, proof: &str) -> Result<Value> {
    let pk = public_key.to_lowercase().trim_start_matches("0x").to_string();
    if !bls::valid_public_key(&pk) {
        return Err(rng_error("publicKey must be a compressed BLS12-381 G2 point (96 bytes hex)"));
    }
    if !bls::verify(proof.trim_start_matches("0x"), &possession_message(&pk)?, DST_POSSESSION, &pk) {
        return Err(rng_error("the proof of possession doesn't verify for this key"));
    }
    sqlx::query(
        "insert into rng_nodes (agent_id, public_key, secret_encrypted, external) values ($1, $2, null, true)
         on conflict (agent_id) do update set public_key = excluded.public_key, secret_encrypted = null, external = true, active = true",
    )
    .bind(agent_id)
    .bind(&pk)
    .execute(db())
    .await?;
    Ok(json!({ "agentId": agent_id, "publicKey": pk }))
}

pub async fn leave(agent_id: &str) -> Result<()> {
    sqlx::query("update rng_nodes set active = false where agent_id = $1").bind(agent_id).execute(db()).await?;
    Ok(())
}

// ---------- Contributions ----------

/// Accept an agent's signature for the current round (self-hosted agents submit through the API).
pub async fn submit_partial(agent_id: &str, round: i64, signature: &str, now: f64) -> Result<Value> {
    let n = node(agent_id).await?.filter(|n| n.active).ok_or_else(|| Error::forbidden("this agent isn't an active Orvyn RNG node"))?;
    if round != round_of(now) {
        return Err(rng_error(format!("only the current round ({}) accepts signatures", round_of(now))));
    }
    if now >= round_close(round) as f64 {
        return Err(rng_error(format!("round {round} closed for signatures; sign round {}", round + 1)));
    }
    let sig = signature.to_lowercase().trim_start_matches("0x").to_string();
    if !bls::verify(&sig, &round_message(round), DST_SIGNATURE, &n.public_key) {
        return Err(rng_error("the signature doesn't verify for this round and key"));
    }
    sqlx::query("insert into rng_partials (round, agent_id, signature) values ($1, $2, $3) on conflict do nothing").bind(round as i32).bind(agent_id).bind(&sig).execute(db()).await?;
    Ok(json!({ "round": round, "accepted": true }))
}

/// Sign the current round for every active hosted node that hasn't yet (orchestrator, every ~15s).
pub async fn contribute_hosted(now: f64) -> Result<usize> {
    let round = round_of(now);
    if now >= round_close(round) as f64 {
        return Ok(0);
    }
    let nodes: Vec<RngNode> = sqlx::query_as(
        "select n.* from rng_nodes n where n.active and not n.external and n.secret_encrypted is not null
         and not exists (select 1 from rng_partials p where p.round = $1 and p.agent_id = n.agent_id)",
    )
    .bind(round as i32)
    .fetch_all(db())
    .await?;
    let mut signed = 0;
    for n in nodes {
        let secret = decrypt_secret(n.secret_encrypted.as_deref().unwrap(), &aad(&n.agent_id), SecretKind::AiKey)?;
        let sig = sign_round(&secret, round)?;
        sqlx::query("insert into rng_partials (round, agent_id, signature) values ($1, $2, $3) on conflict do nothing").bind(round as i32).bind(&n.agent_id).bind(sig).execute(db()).await?;
        signed += 1;
    }
    Ok(signed)
}

// ---------- Rounds ----------

pub fn output_of(round: i64, aggregate_signature: &str, drand_randomness: &str) -> String {
    let mut h = Sha256::new();
    h.update(VERSION.as_bytes());
    h.update((round as u64).to_be_bytes());
    h.update(hex::decode(aggregate_signature).unwrap_or_default());
    h.update(hex::decode(drand_randomness).unwrap_or_default());
    hex::encode(h.finalize())
}

#[derive(sqlx::FromRow)]
struct Partial {
    agent_id: String,
    signature: String,
    public_key: String,
}

/// Finalise closed rounds whose drand round is out (orchestrator, every ~10s).
pub async fn finalize_rounds(now: f64) -> Result<usize> {
    finalize_rounds_with(now, |r| drand::beacon(Some(r))).await
}

pub async fn finalize_rounds_with<F, Fut>(now: f64, get_beacon: F) -> Result<usize>
where
    F: Fn(i64) -> Fut,
    Fut: Future<Output = Result<drand::Beacon>>,
{
    let current = round_of(now);
    // Closed rounds with contributions and no record yet (bounded, oldest first).
    let pending: Vec<i32> = sqlx::query_scalar(
        "select p.round from rng_partials p left join rng_rounds r on r.round = p.round
         where r.round is null and p.round < $1 and p.round >= $2 group by p.round order by p.round asc limit 10",
    )
    .bind((current + 1) as i32)
    .bind((current - 60) as i32)
    .fetch_all(db())
    .await?;
    let staked_only = std::env::var("ORVYN_RNG_CONTRACT").is_ok_and(|v| !v.is_empty());
    let mut finalized = 0;
    for round in pending.into_iter().map(i64::from) {
        if now < round_close(round) as f64 {
            continue;
        }
        let d = drand_round_for(round);
        if now < drand::time_of_round(d) as f64 {
            continue; // drand hasn't published it yet
        }
        let b = get_beacon(d).await?;
        // Signatures from before the close; with the contract live, only staked (on-chain) nodes, so
        // this record and the on-chain round have the same participants and the same output.
        let rows: Vec<Partial> = sqlx::query_as(&format!(
            "select p.agent_id, p.signature, n.public_key from rng_partials p join rng_nodes n on n.agent_id = p.agent_id
             where p.round = $1 and p.created_at < to_timestamp($2){} order by p.agent_id asc",
            if staked_only { " and n.chain_index is not null" } else { "" }
        ))
        .bind(round as i32)
        .bind(round_close(round) as f64)
        .fetch_all(db())
        .await?;
        if rows.is_empty() {
            continue;
        }
        let aggregate = bls::aggregate_signatures(&rows.iter().map(|r| r.signature.clone()).collect::<Vec<_>>())?;
        let output = output_of(round, &aggregate, &b.randomness);
        let participants: Value = rows.iter().map(|r| json!({ "agentId": r.agent_id, "publicKey": r.public_key })).collect();
        let n = sqlx::query(
            "insert into rng_rounds (round, output, aggregate_signature, participants, drand_round, drand_signature) values ($1, $2, $3, $4, $5, $6) on conflict do nothing",
        )
        .bind(round as i32)
        .bind(output)
        .bind(aggregate)
        .bind(participants)
        .bind(b.round as i32)
        .bind(&b.signature)
        .execute(db())
        .await?
        .rows_affected();
        finalized += n as usize;
    }
    Ok(finalized)
}

/// Check a round record without trusting Orvyn: agents' aggregate signature, drand, and the output.
pub fn verify_record(r: &RngRound) -> Value {
    let mut problems: Vec<&str> = vec![];
    let pks: Vec<String> = r.participants.as_array().map(|a| a.iter().filter_map(|p| p["publicKey"].as_str().map(String::from)).collect()).unwrap_or_default();
    let round = r.round as i64;
    if pks.is_empty() {
        problems.push("no participants");
    } else if !bls::verify_aggregate(&r.aggregate_signature, &round_message(round), DST_SIGNATURE, &pks) {
        problems.push("the agents' aggregate signature doesn't verify");
    }
    if r.drand_round as i64 != drand_round_for(round) {
        problems.push("the drand round isn't the one this round must use");
    }
    if !drand::verify_round(r.drand_round as i64, &r.drand_signature) {
        problems.push("the drand signature doesn't verify");
    }
    if output_of(round, &r.aggregate_signature, &drand::randomness_of(&r.drand_signature)) != r.output {
        problems.push("the output doesn't match its inputs");
    }
    json!({ "valid": problems.is_empty(), "problems": problems })
}

pub async fn get_round(round: Option<i64>) -> Result<Option<RngRound>> {
    Ok(match round {
        None => sqlx::query_as("select * from rng_rounds order by round desc limit 1").fetch_optional(db()).await?,
        Some(r) => sqlx::query_as("select * from rng_rounds where round = $1").bind(r as i32).fetch_optional(db()).await?,
    })
}

/// The public proof for a round: the record, how to check it, and our own check of it.
pub async fn round_proof(round: Option<i64>) -> Result<Option<Value>> {
    let Some(r) = get_round(round).await? else { return Ok(None) };
    let dst = String::from_utf8_lossy(DST_SIGNATURE);
    Ok(Some(json!({
        "round": r.round,
        "output": r.output,
        "finalizedAt": crate::public::iso(r.created_at),
        "participants": r.participants,
        "aggregateSignature": r.aggregate_signature,
        "drand": { "chain": "quicknet", "round": r.drand_round, "signature": r.drand_signature },
        "method": {
            "message": format!("each participant signs \"{VERSION}:{}\" (BLS12-381, G1 signatures, DST {dst})", r.round),
            "aggregate": "aggregateSignature = sum of the participants' signatures; verify against the sum of their public keys",
            "output": format!("sha256(\"{VERSION}\" || round as 8 bytes || aggregateSignature (48 bytes) || drandRandomness), drandRandomness = sha256(drand signature)"),
        },
        "verification": verify_record(&r),
    })))
}

/// An agent's participation over the last `rounds` rounds.
pub async fn node_stats(agent_id: &str, rounds: i64) -> Result<Option<Value>> {
    let Some(n) = node(agent_id).await? else { return Ok(None) };
    let now = round_of(now_s());
    let signed: i64 = sqlx::query_scalar("select count(*) from rng_partials where agent_id = $1 and round >= $2").bind(agent_id).bind((now - rounds) as i32).fetch_one(db()).await?;
    let joined = round_of(n.joined_at.timestamp_millis() as f64 / 1000.0);
    let possible = (rounds.min(now - joined + 1)).max(1);
    Ok(Some(json!({
        "active": n.active,
        "external": n.external,
        "publicKey": n.public_key,
        "joinedAt": crate::public::iso(n.joined_at),
        "roundsSigned": signed,
        "uptimePct": ((signed as f64 / possible as f64) * 1000.0).round() / 10.0,
    })))
}

pub async fn network_stats() -> Result<Value> {
    let nodes: i64 = sqlx::query_scalar("select count(*) from rng_nodes where active").fetch_one(db()).await?;
    let latest = get_round(None).await?;
    Ok(json!({
        "activeNodes": nodes,
        "latestRound": latest.as_ref().map(|r| r.round),
        "latestParticipants": latest.as_ref().and_then(|r| r.participants.as_array().map(Vec::len)).unwrap_or(0),
        "periodSeconds": PERIOD_SECONDS,
    }))
}

/// Deterministic draws from a beacon: the i-th 256-bit value is sha256(randomness|purpose|i),
/// reduced to [min, max] by rejection sampling (no modulo bias).
pub fn draw(randomness: &str, purpose: &str, count: usize, min: i64, max: i64, unique: bool) -> Vec<i64> {
    use alloy::primitives::U256;
    let span = U256::from((max - min + 1) as u64);
    // Accept v < floor(2^256 / span) * span. When span divides 2^256 that bound is 2^256 itself
    // (nothing is rejected); otherwise it equals floor((2^256 - 1) / span) * span.
    let limit = (!span.is_power_of_two()).then(|| (U256::MAX / span) * span);
    let mut out: Vec<i64> = vec![];
    let mut i = 0usize;
    while out.len() < count && i < count * 50 {
        let v = U256::from_be_slice(&Sha256::digest(format!("{randomness}|{purpose}|{i}").as_bytes()));
        i += 1;
        if limit.is_some_and(|l| v >= l) {
            continue;
        }
        let n = min + (v % span).to::<u64>() as i64;
        if unique && out.contains(&n) {
            continue;
        }
        out.push(n);
    }
    out
}
