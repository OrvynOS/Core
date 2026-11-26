//! Orvyn price feeds: staked agent nodes observe prices and sign reports, which contracts read
//! through contracts/src/rng/OrvynPriceFeed.sol.
//!
//! Each minute (the same rounds as Orvyn RNG):
//!   0-30 s  observe: every node prices each feed from many sources and records its observation, signed;
//!   30 s    report: the report is the (lower) median of the nodes' observations;
//!   30-50 s sign: each node signs the report only if it's within TOLERANCE_BPS of what it saw;
//!   50 s    finalise: the signatures are aggregated; the relayer submits the report on chain.
//!
//! A node never signs two reports for one feed and round (slashable on chain).

pub mod chain;
pub mod sources;

use std::future::Future;

use alloy::primitives::{keccak256, B256};
use serde_json::{json, Value};

use crate::db::{db, PriceReport, RngNode};
use crate::error::{Error, Result};
use crate::rng::{aad, bls, round_of, round_start, DST_SIGNATURE};
use crate::secrets::{decrypt_secret, SecretKind};

/// Feeds the network reports: pair → asset (the on-chain feed id is keccak256 of the pair).
pub const FEEDS: &[(&str, &str)] = &[("BTC / USD", "BTC"), ("ETH / USD", "ETH"), ("BNB / USD", "BNB"), ("CAKE / USD", "CAKE"), ("SOL / USD", "SOL")];
pub const DECIMALS: i32 = 8;
/// A node signs a report only within this distance of its own observation.
pub const TOLERANCE_BPS: i128 = 50;
pub const OBSERVE_UNTIL: i64 = 30;
pub const SIGN_UNTIL: i64 = 50;

fn min_observations() -> usize {
    std::env::var("ORVYN_PRICE_MIN_OBSERVATIONS").ok().and_then(|v| v.parse().ok()).unwrap_or(1)
}

pub fn feed_id(pair: &str) -> B256 {
    keccak256(pair.as_bytes())
}

fn message(domain: &str, pair: &str, round: i64, price: i128) -> Vec<u8> {
    let mut m = domain.as_bytes().to_vec();
    m.extend(feed_id(pair).as_slice());
    m.extend((round as u64).to_be_bytes());
    let mut word = if price < 0 { [0xffu8; 32] } else { [0u8; 32] };
    word[16..].copy_from_slice(&price.to_be_bytes());
    m.extend(word);
    m
}

/// What a node signs for an observation (a different domain from reports: never slashable).
pub fn observation_message(pair: &str, round: i64, price: i128) -> Vec<u8> {
    message("orvyn-price-obs:v1:", pair, round, price)
}
/// What a node signs for a report: the bytes OrvynPriceFeed.sol and OrvynRNG.sol check.
pub fn report_message(pair: &str, round: i64, price: i128) -> Vec<u8> {
    message("orvyn-price:v1:", pair, round, price)
}

pub fn sign_message(secret_hex: &str, m: &[u8]) -> Result<String> {
    bls::sign(secret_hex, m, DST_SIGNATURE)
}

/// A USD price as an 8-decimal integer.
pub fn to_e8(usd: f64) -> i128 {
    (usd * 10f64.powi(DECIMALS)).round() as i128
}
pub fn from_e8(v: &str) -> f64 {
    v.parse::<i128>().unwrap_or(0) as f64 / 10f64.powi(DECIMALS)
}

fn price_error(m: impl Into<String>) -> Error {
    Error::invalid(m)
}

fn check_pair(pair: &str) -> Result<()> {
    if FEEDS.iter().any(|(p, _)| *p == pair) {
        Ok(())
    } else {
        Err(price_error(format!("unknown feed {pair}; feeds: {}", FEEDS.iter().map(|(p, _)| *p).collect::<Vec<_>>().join(", "))))
    }
}

fn parse_price(s: &str) -> Result<i128> {
    s.parse().map_err(|_| price_error("price must be an integer (8 decimals)"))
}

// ---------- Observe ----------

/// The hosted nodes' shared observation: the median of all sources for one feed, if at least 3
/// sources answered and at least half agree within 1%.
pub async fn sources_median(pair: &str) -> Option<f64> {
    let symbol = FEEDS.iter().find(|(p, _)| *p == pair)?.1;
    let (quotes, _) = sources::asset_quotes(symbol, false).await.ok()?;
    if quotes.len() < 3 {
        return None;
    }
    let c = sources::consensus(&quotes, 1.0);
    let (agreeing, n) = (c["agreeing"].as_u64()?, c["sources"].as_u64()?);
    (agreeing >= n.div_ceil(2)).then(|| c["median"].as_f64()).flatten()
}

async fn active_hosted() -> Result<Vec<RngNode>> {
    Ok(sqlx::query_as("select * from rng_nodes where active and not external").fetch_all(db()).await?)
}

fn secret_of(n: &RngNode) -> Result<String> {
    decrypt_secret(n.secret_encrypted.as_deref().unwrap_or(""), &aad(&n.agent_id), SecretKind::AiKey)
}

/// Hosted nodes record their observations for the current round.
pub async fn observe_hosted<F, Fut>(now: f64, observe: F) -> Result<usize>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Option<f64>>,
{
    let round = round_of(now);
    if now - round_start(round) as f64 >= OBSERVE_UNTIL as f64 {
        return Ok(0);
    }
    let nodes = active_hosted().await?;
    if nodes.is_empty() {
        return Ok(0);
    }
    let mut n = 0;
    for (pair, _) in FEEDS {
        let done: Vec<String> = sqlx::query_scalar("select agent_id from price_observations where feed = $1 and round = $2").bind(pair).bind(round as i32).fetch_all(db()).await?;
        let todo: Vec<&RngNode> = nodes.iter().filter(|x| x.secret_encrypted.is_some() && !done.contains(&x.agent_id)).collect();
        if todo.is_empty() {
            continue;
        }
        let Some(usd) = observe(pair.to_string()).await else { continue };
        let price = to_e8(usd);
        for node in todo {
            let signature = sign_message(&secret_of(node)?, &observation_message(pair, round, price))?;
            sqlx::query("insert into price_observations (feed, round, agent_id, price, signature) values ($1, $2, $3, $4, $5) on conflict do nothing")
                .bind(pair)
                .bind(round as i32)
                .bind(&node.agent_id)
                .bind(price.to_string())
                .bind(signature)
                .execute(db())
                .await?;
            n += 1;
        }
    }
    Ok(n)
}

async fn active_node(agent_id: &str) -> Result<RngNode> {
    let node: Option<RngNode> = sqlx::query_as("select * from rng_nodes where agent_id = $1").bind(agent_id).fetch_optional(db()).await?;
    node.filter(|n| n.active).ok_or_else(|| Error::forbidden("this agent isn't an active Orvyn node"))
}

/// A self-hosted node submits its own observation for the current round.
pub async fn submit_observation(agent_id: &str, pair: &str, round: i64, price: &str, signature: &str, now: f64) -> Result<Value> {
    check_pair(pair)?;
    if round != round_of(now) || now - round_start(round) as f64 >= OBSERVE_UNTIL as f64 {
        return Err(price_error(format!("observations for round {} are taken in its first {OBSERVE_UNTIL} seconds", round_of(now))));
    }
    let node = active_node(agent_id).await?;
    let p = parse_price(price)?;
    if p <= 0 {
        return Err(price_error("price must be positive (8 decimals)"));
    }
    let sig = signature.trim_start_matches("0x");
    if !bls::verify(sig, &observation_message(pair, round, p), DST_SIGNATURE, &node.public_key) {
        return Err(price_error("the observation signature doesn't verify"));
    }
    sqlx::query("insert into price_observations (feed, round, agent_id, price, signature) values ($1, $2, $3, $4, $5) on conflict do nothing")
        .bind(pair)
        .bind(round as i32)
        .bind(agent_id)
        .bind(p.to_string())
        .bind(sig)
        .execute(db())
        .await?;
    Ok(json!({ "accepted": true }))
}

// ---------- Report ----------

/// The lower median of integer observations (a value a node actually saw).
pub fn median_e8(values: &[i128]) -> i128 {
    let mut s = values.to_vec();
    s.sort();
    s[(s.len() - 1) / 2]
}

/// Build the round's reports once observing ends.
pub async fn build_reports(now: f64) -> Result<usize> {
    let round = round_of(now);
    if now - (round_start(round) as f64) < OBSERVE_UNTIL as f64 {
        return Ok(0);
    }
    let mut built = 0;
    for (pair, _) in FEEDS {
        let obs: Vec<String> = sqlx::query_scalar("select price from price_observations where feed = $1 and round = $2").bind(pair).bind(round as i32).fetch_all(db()).await?;
        if obs.len() < min_observations() || obs.is_empty() {
            continue;
        }
        let price = median_e8(&obs.iter().filter_map(|p| p.parse().ok()).collect::<Vec<_>>());
        built += sqlx::query("insert into price_reports (feed, round, price, observations) values ($1, $2, $3, $4) on conflict do nothing")
            .bind(pair)
            .bind(round as i32)
            .bind(price.to_string())
            .bind(obs.len() as i32)
            .execute(db())
            .await?
            .rows_affected() as usize;
    }
    Ok(built)
}

/// Reports waiting for signatures in the current round (self-hosted nodes poll this).
pub async fn pending_reports(now: f64) -> Result<Value> {
    let round = round_of(now);
    let rows: Vec<PriceReport> = sqlx::query_as("select * from price_reports where round = $1 and aggregate_signature is null").bind(round as i32).fetch_all(db()).await?;
    Ok(json!({
        "round": round,
        "signUntil": crate::rng::drand::iso(round_start(round) + SIGN_UNTIL),
        "reports": rows.iter().map(|r| json!({ "feed": r.feed, "round": r.round, "price": r.price, "observations": r.observations })).collect::<Vec<_>>(),
    }))
}

fn within(report: i128, own: i128) -> bool {
    (report - own).abs() * 10_000 <= own * TOLERANCE_BPS
}

#[derive(sqlx::FromRow)]
struct Obs {
    agent_id: String,
    price: String,
    secret: Option<String>,
}

/// Hosted nodes sign the round's reports that are within tolerance of their own observation.
pub async fn sign_hosted(now: f64) -> Result<usize> {
    let round = round_of(now);
    let t = now - round_start(round) as f64;
    if t < OBSERVE_UNTIL as f64 || t >= SIGN_UNTIL as f64 {
        return Ok(0);
    }
    let reports: Vec<PriceReport> = sqlx::query_as("select * from price_reports where round = $1 and aggregate_signature is null").bind(round as i32).fetch_all(db()).await?;
    let mut n = 0;
    for r in reports {
        let obs: Vec<Obs> = sqlx::query_as(
            "select o.agent_id, o.price, n.secret_encrypted as secret from price_observations o join rng_nodes n on n.agent_id = o.agent_id
             where o.feed = $1 and o.round = $2 and not n.external and n.active",
        )
        .bind(&r.feed)
        .bind(round as i32)
        .fetch_all(db())
        .await?;
        let report = parse_price(&r.price)?;
        for o in obs {
            let (Some(secret), Ok(own)) = (o.secret, o.price.parse::<i128>()) else { continue };
            if !within(report, own) {
                continue;
            }
            let sk = decrypt_secret(&secret, &aad(&o.agent_id), SecretKind::AiKey)?;
            let signature = sign_message(&sk, &report_message(&r.feed, round, report))?;
            // One signature per node, feed and round (the primary key): a second report can't be signed.
            n += sqlx::query("insert into price_report_signatures (feed, round, agent_id, signature) values ($1, $2, $3, $4) on conflict do nothing")
                .bind(&r.feed)
                .bind(round as i32)
                .bind(&o.agent_id)
                .bind(signature)
                .execute(db())
                .await?
                .rows_affected() as usize;
        }
    }
    Ok(n)
}

/// A self-hosted node signs a pending report.
pub async fn submit_report_signature(agent_id: &str, pair: &str, round: i64, price: &str, signature: &str, now: f64) -> Result<Value> {
    check_pair(pair)?;
    let t = now - round_start(round) as f64;
    if round != round_of(now) || t < OBSERVE_UNTIL as f64 || t >= SIGN_UNTIL as f64 {
        return Err(price_error(format!("reports are signed between second {OBSERVE_UNTIL} and {SIGN_UNTIL} of their round")));
    }
    let p = parse_price(price)?;
    let r: Option<PriceReport> = sqlx::query_as("select * from price_reports where feed = $1 and round = $2").bind(pair).bind(round as i32).fetch_optional(db()).await?;
    if r.is_none_or(|r| r.price != p.to_string()) {
        return Err(price_error("that isn't this round's report"));
    }
    let node = active_node(agent_id).await?;
    let sig = signature.trim_start_matches("0x");
    if !bls::verify(sig, &report_message(pair, round, p), DST_SIGNATURE, &node.public_key) {
        return Err(price_error("the report signature doesn't verify"));
    }
    sqlx::query("insert into price_report_signatures (feed, round, agent_id, signature) values ($1, $2, $3, $4) on conflict do nothing")
        .bind(pair)
        .bind(round as i32)
        .bind(agent_id)
        .bind(sig)
        .execute(db())
        .await?;
    Ok(json!({ "accepted": true }))
}

// ---------- Finalise ----------

#[derive(sqlx::FromRow)]
struct Sig {
    agent_id: String,
    signature: String,
    public_key: String,
}

/// Aggregate the signatures of rounds whose signing window has closed.
pub async fn finalize_reports(now: f64) -> Result<usize> {
    let current = round_of(now);
    let open: Vec<PriceReport> = sqlx::query_as("select * from price_reports where aggregate_signature is null and round >= $1").bind((current - 10) as i32).fetch_all(db()).await?;
    let staked_only = std::env::var("ORVYN_PRICE_FEED_CONTRACT").is_ok_and(|v| !v.is_empty());
    let mut done = 0;
    for r in open {
        if now - (round_start(r.round as i64) as f64) < SIGN_UNTIL as f64 {
            continue;
        }
        let sigs: Vec<Sig> = sqlx::query_as(&format!(
            "select s.agent_id, s.signature, n.public_key from price_report_signatures s join rng_nodes n on n.agent_id = s.agent_id
             where s.feed = $1 and s.round = $2{} order by n.chain_index asc nulls last, s.agent_id asc",
            if staked_only { " and n.chain_index is not null" } else { "" }
        ))
        .bind(&r.feed)
        .bind(r.round)
        .fetch_all(db())
        .await?;
        if sigs.is_empty() {
            continue;
        }
        let aggregate = bls::aggregate_signatures(&sigs.iter().map(|s| s.signature.clone()).collect::<Vec<_>>())?;
        let participants: Value = sigs.iter().map(|s| json!({ "agentId": s.agent_id, "publicKey": s.public_key })).collect();
        sqlx::query("update price_reports set participants = $3, aggregate_signature = $4 where feed = $1 and round = $2 and aggregate_signature is null")
            .bind(&r.feed)
            .bind(r.round)
            .bind(participants)
            .bind(aggregate)
            .execute(db())
            .await?;
        done += 1;
    }
    Ok(done)
}

/// Check a finalised report without trusting Orvyn: its signers' aggregate signature over its price.
pub fn verify_report(r: &PriceReport) -> Value {
    let pks: Vec<String> = r.participants.as_ref().and_then(|p| p.as_array()).map(|a| a.iter().filter_map(|p| p["publicKey"].as_str().map(String::from)).collect()).unwrap_or_default();
    let Some(agg) = r.aggregate_signature.as_deref().filter(|_| !pks.is_empty()) else { return json!({ "valid": false, "problems": ["not finalised"] }) };
    let ok = r.price.parse::<i128>().is_ok_and(|p| bls::verify_aggregate(agg, &report_message(&r.feed, r.round as i64, p), DST_SIGNATURE, &pks));
    if ok {
        json!({ "valid": true, "problems": [] })
    } else {
        json!({ "valid": false, "problems": ["the aggregate signature doesn't verify"] })
    }
}

fn view(r: &PriceReport) -> Value {
    json!({
        "feed": r.feed,
        "feedId": feed_id(&r.feed).to_string(),
        "round": r.round,
        "price": from_e8(&r.price),
        "priceE8": r.price,
        "observations": r.observations,
        "signers": r.participants.as_ref().and_then(|p| p.as_array().map(Vec::len)).unwrap_or(0),
        "participants": r.participants,
        "aggregateSignature": r.aggregate_signature,
        "submitTx": r.submit_tx,
        "at": crate::rng::drand::iso(round_start(r.round as i64)),
        "verification": verify_report(r),
    })
}

/// The latest finalised report for every feed.
pub async fn latest_reports() -> Result<Vec<Value>> {
    let mut out = vec![];
    for (pair, _) in FEEDS {
        let r: Option<PriceReport> = sqlx::query_as("select * from price_reports where feed = $1 and aggregate_signature is not null order by round desc limit 1").bind(pair).fetch_optional(db()).await?;
        if let Some(r) = r {
            out.push(view(&r));
        }
    }
    Ok(out)
}

pub async fn report(pair: &str, round: i64) -> Result<Option<Value>> {
    let r: Option<PriceReport> = sqlx::query_as("select * from price_reports where feed = $1 and round = $2").bind(pair).bind(round as i32).fetch_optional(db()).await?;
    Ok(r.as_ref().map(view))
}

/// How close a node's observations have been to the reports (its public accuracy).
pub async fn node_accuracy(agent_id: &str, rounds: i64) -> Result<Value> {
    let since = round_of(crate::rng::now_s()) - rounds;
    let rows: Vec<(String, String)> = sqlx::query_as(
        "select o.price, r.price from price_observations o join price_reports r on r.feed = o.feed and r.round = o.round where o.agent_id = $1 and o.round >= $2",
    )
    .bind(agent_id)
    .bind(since as i32)
    .fetch_all(db())
    .await?;
    if rows.is_empty() {
        return Ok(json!({ "observations": 0, "medianDeviationBps": null, "withinTolerancePct": null }));
    }
    let mut devs: Vec<f64> = rows
        .iter()
        .map(|(o, r)| {
            let (o, r) = (o.parse::<i128>().unwrap_or(0), r.parse::<i128>().unwrap_or(1));
            (o - r).abs() as f64 / r as f64 * 10_000.0
        })
        .collect();
    devs.sort_by(f64::total_cmp);
    let within = devs.iter().filter(|d| **d <= TOLERANCE_BPS as f64).count();
    Ok(json!({
        "observations": rows.len(),
        "medianDeviationBps": (devs[devs.len() / 2] * 100.0).round() / 100.0,
        "withinTolerancePct": ((within as f64 / devs.len() as f64) * 1000.0).round() / 10.0,
    }))
}

/// One orchestrator pass for the price network.
pub async fn tick(now: f64) -> Result<Value> {
    let observed = observe_hosted(now, |p| async move { sources_median(&p).await }).await?;
    let built = build_reports(now).await?;
    let signed = sign_hosted(now).await?;
    let finalized = finalize_reports(now).await?;
    Ok(json!({ "observed": observed, "built": built, "signed": signed, "finalized": finalized }))
}
