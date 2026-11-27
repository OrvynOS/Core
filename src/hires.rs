//! Agent marketplace: anyone can hire an agent for a one-off task (a "hire"), paid in USDT, USDC or
//! BNB into the escrow contract on BNB Chain (contracts/src/OrvynJobs.sol, ORVYN_JOBS_CONTRACT).
//!
//! 1. The client asks (`create_hire`) and pays with `fund` (`payment_tx`). The contract checks
//!    nothing about the agent, so `sync_hire` checks the funded terms against the hire and refunds
//!    anything that doesn't match.
//! 2. The agent works on it in its own session (the "Hire me" plugin), with read-only tools, and
//!    delivers a result.
//! 3. Orvyn's operator (`sync_agent_hires`) puts the delivery on chain, which starts the client's
//!    review window, refunds declined or late jobs, and releases payment once the window passes.
//!
//! The chain is the source of truth for money; this module mirrors it.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

use alloy::primitives::{Address, FixedBytes, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use regex::Regex;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::billing::{Call, Calls};
use crate::db::{db, log_agent, Agent, AgentLogKind, Hire, HireMessage, MailMessage};
use crate::env::env;
use crate::error::{Error, Result};
use crate::evm::{self, rpc_error, BSC};
use crate::push::{self, Notice};

pub const HIRE_PLUGIN_ID: &str = "hire";
/// "native" is BNB (address(0) in the escrow).
pub const NATIVE: &str = "native";
const DECIMALS: u32 = 18;
const HOUR_MS: i64 = 3_600_000;

pub const BRIEF_CHARS: usize = 8000;
pub const MESSAGE_CHARS: usize = 4000;
pub const RESULT_CHARS: usize = 60_000;
pub const REVIEW_CHARS: usize = 1000;
/// A request must be paid within this long, or it expires.
pub const PAYMENT_WINDOW_MS: i64 = HOUR_MS;
/// Unpaid requests a client may have open at once.
pub const UNPAID_PER_CLIENT: i64 = 3;
/// Agent sessions per hire before it's declined and refunded.
pub const AGENT_ATTEMPTS: i32 = 3;

/// Hires that hold money in escrow (or are about to).
const OPEN: &[&str] = &["funded", "delivered", "disputed"];
/// Hires the orchestrator keeps in sync with the chain.
const TRACKED: &[&str] = &["awaiting_payment", "funded", "delivered", "disputed", "invalid"];

pub fn hire_token(key: &str) -> Option<&'static str> {
    match key {
        "usdt" => Some(BSC.usdt),
        "usdc" => Some(BSC.usdc),
        "bnb" => Some(NATIVE),
        _ => None,
    }
}

fn hire_error(m: impl Into<String>) -> Error {
    Error::invalid(m)
}

pub fn marketplace_enabled() -> bool {
    env().orvyn_jobs_contract.is_some()
}

pub fn jobs_contract() -> Result<Address> {
    env().orvyn_jobs_contract.as_deref().and_then(|c| c.parse().ok()).ok_or_else(|| Error::unavailable("The marketplace isn't enabled on this Orvyn deployment yet."))
}

fn app_url(path: &str) -> String {
    let base = std::env::var("ORVYN_APP_URL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "https://app.orvyn.cash".into());
    format!("{}{path}", base.trim_end_matches('/'))
}

/// Amount in whole units (USDT, USDC or BNB, all 18 decimals), to 6 decimals.
pub fn to_usd(base_units: &str) -> f64 {
    let v: U256 = base_units.parse().unwrap_or_default();
    (v / U256::from(10u64).pow(U256::from(12))).to::<u128>() as f64 / 1e6
}

fn usd(base_units: &str) -> String {
    crate::auth::js_number(to_usd(base_units))
}

pub fn token_symbol(token: &str) -> &'static str {
    if token == NATIVE {
        "BNB"
    } else if token.eq_ignore_ascii_case(BSC.usdt) {
        "USDT"
    } else {
        "USDC"
    }
}

fn on_chain_token(token: &str) -> Result<Address> {
    if token == NATIVE {
        Ok(Address::ZERO)
    } else {
        evm::parse_address(token)
    }
}

// ---------------------------------------------------------------------------------------------
// The escrow contract
// ---------------------------------------------------------------------------------------------

sol! {
    #[sol(rpc)]
    interface OrvynJobs {
        struct Job { address client; address payee; address token; uint16 feeBps; uint8 status; uint64 reviewPeriod; uint64 fundedAt; uint64 deadline; uint64 deliveredAt; uint256 amount; uint256 paidToPayee; uint256 refunded; uint256 fee; bytes32 resultHash; string agentId; }
        function getJob(string jobId) view returns (Job job, uint64 reviewEnds);
        function tokens() view returns (address[]);
        function feeBps() view returns (uint16);
        function reviewPeriod() view returns (uint64);
        function minAmount(address token) view returns (uint256);
        function fund(string jobId, string agentId, address payee, address token, uint256 amount, uint64 deadline) payable;
        function markDelivered(string jobId, bytes32 resultHash);
        function refund(string jobId);
        function release(string jobId);
        function approve(string jobId);
        function dispute(string jobId, string reason);
        function reclaim(string jobId);
    }
}

const STATUS: &[Option<&str>] = &[None, Some("Funded"), Some("Delivered"), Some("Disputed"), Some("Released"), Some("Refunded"), Some("Resolved")];

#[derive(Debug, Clone, Serialize)]
pub struct ChainJob {
    pub job_id: String,
    pub agent_id: String,
    pub client: String,
    pub payee: String,
    pub token: String,
    pub amount: String,
    pub fee_bps: u16,
    pub review_ms: i64,
    pub status: &'static str,
    pub funded_at_ms: i64,
    pub deadline_ms: i64,
    pub delivered_at_ms: Option<i64>,
    pub result_hash: Option<String>,
    pub paid_to_payee: String,
    pub refunded: String,
    pub fee: String,
    pub review_ends_ms: Option<i64>,
}

fn lower(a: Address) -> String {
    format!("{a:#x}")
}

pub async fn chain_job(id: &str) -> Result<Option<ChainJob>> {
    let r = OrvynJobs::new(jobs_contract()?, evm::bsc()).getJob(id.to_string()).call().await.map_err(rpc_error)?;
    let (job, review_ends) = (r.job, r.reviewEnds);
    let Some(status) = STATUS.get(job.status as usize).copied().flatten() else { return Ok(None) };
    let ms = |s: u64| s as i64 * 1000;
    Ok(Some(ChainJob {
        job_id: id.into(),
        agent_id: job.agentId,
        client: lower(job.client),
        payee: lower(job.payee),
        token: if job.token == Address::ZERO { NATIVE.into() } else { lower(job.token) },
        amount: job.amount.to_string(),
        fee_bps: job.feeBps,
        review_ms: ms(job.reviewPeriod),
        status,
        funded_at_ms: ms(job.fundedAt),
        deadline_ms: ms(job.deadline),
        delivered_at_ms: (job.deliveredAt > 0).then(|| ms(job.deliveredAt)),
        result_hash: (job.deliveredAt > 0).then(|| job.resultHash.to_string()),
        paid_to_payee: job.paidToPayee.to_string(),
        refunded: job.refunded.to_string(),
        fee: job.fee.to_string(),
        review_ends_ms: (review_ends > 0).then(|| ms(review_ends)),
    }))
}

#[derive(Debug, Clone, Serialize)]
pub struct JobsConfig {
    pub tokens: Vec<String>,
    pub fee_bps: u16,
    pub review_ms: i64,
    /// Minimum job size per accepted token (base units).
    pub min_amount: HashMap<String, String>,
}

static CONFIG: LazyLock<Mutex<Option<(Instant, JobsConfig)>>> = LazyLock::new(Default::default);

pub async fn jobs_config() -> Result<JobsConfig> {
    if let Some((at, c)) = CONFIG.lock().unwrap().clone() {
        if at.elapsed().as_secs() < 60 {
            return Ok(c);
        }
    }
    let c = OrvynJobs::new(jobs_contract()?, evm::bsc());
    let tokens = c.tokens().call().await.map_err(rpc_error)?;
    let fee = c.feeBps().call().await.map_err(rpc_error)?;
    let review = c.reviewPeriod().call().await.map_err(rpc_error)?;
    let mut min_amount = HashMap::new();
    let mut keys = vec![];
    for t in tokens {
        let key = if t == Address::ZERO { NATIVE.to_string() } else { lower(t) };
        min_amount.insert(key.clone(), c.minAmount(t).call().await.map_err(rpc_error)?.to_string());
        keys.push(key);
    }
    let config = JobsConfig { tokens: keys, fee_bps: fee, review_ms: review as i64 * 1000, min_amount };
    *CONFIG.lock().unwrap() = Some((Instant::now(), config.clone()));
    Ok(config)
}

fn operator_key() -> Result<String> {
    env().orvyn_jobs_operator_private_key.clone().ok_or_else(|| Error::unavailable("ORVYN_JOBS_OPERATOR_PRIVATE_KEY is not set"))
}

/// BNB price in USD (deepest WBNB pool), to price BNB jobs from a USD offer.
pub(crate) async fn bnb_usd() -> Result<f64> {
    let pair = crate::data::dexscreener::best_pair("bsc", BSC.wrapped).await.ok().flatten();
    let price = pair.and_then(|p| p["priceUsd"].as_str().and_then(|s| s.parse::<f64>().ok())).unwrap_or(0.0);
    if price > 0.0 {
        Ok(price)
    } else {
        Err(hire_error("Couldn't price BNB right now; pay in USDT or USDC."))
    }
}

// ---------------------------------------------------------------------------------------------
// Offers (the "Hire me" plugin's settings)
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Offer {
    /// Job category (tiers.rs JOB_CATEGORIES); decides the hosting tier it needs.
    pub category: String,
    pub title: String,
    pub description: String,
    pub price_usd: f64,
    pub delivery_hours: f64,
    pub max_open_jobs: f64,
    #[serde(skip)]
    pub instructions: String,
}

pub fn offer_of(config: &Value) -> Offer {
    let s = |k: &str, d: &str| match &config[k] {
        Value::Null => d.to_string(),
        Value::String(s) => s.clone(),
        v => v.to_string(),
    };
    let n = |k: &str, d: f64| match &config[k] {
        Value::Null => d,
        Value::String(s) => s.trim().parse().unwrap_or(if s.trim().is_empty() { 0.0 } else { f64::NAN }),
        v => v.as_f64().unwrap_or(f64::NAN),
    };
    Offer {
        // Offers made before categories existed were written reports.
        category: s("category", "research"),
        title: s("offerTitle", ""),
        description: s("offerDescription", ""),
        price_usd: n("priceUsd", 0.0),
        delivery_hours: n("deliveryHours", 24.0),
        max_open_jobs: n("maxOpenJobs", 3.0),
        instructions: s("instructions", ""),
    }
}

pub async fn agent_offer(agent_id: &str) -> Result<Option<Offer>> {
    let config: Option<Value> = sqlx::query_scalar("select config from agent_plugins where agent_id = $1 and plugin_id = $2 and enabled = true").bind(agent_id).bind(HIRE_PLUGIN_ID).fetch_optional(db()).await?;
    Ok(config.as_ref().map(offer_of))
}

/// Open (paid, not yet settled) jobs per agent.
pub async fn open_hire_counts(agent_ids: &[String]) -> Result<HashMap<String, i64>> {
    if agent_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<(String, i64)> = sqlx::query_as("select agent_id, count(*) from hires where agent_id = any($1) and status = any($2) group by agent_id").bind(agent_ids).bind(OPEN).fetch_all(db()).await?;
    Ok(rows.into_iter().collect())
}

pub async fn open_hire_count(agent_id: &str) -> Result<i64> {
    Ok(sqlx::query_scalar("select count(*) from hires where agent_id = $1 and status = any($2)").bind(agent_id).bind(OPEN).fetch_one(db()).await?)
}

/// Why an agent can't take a job right now, or None if it can.
pub fn unavailable_reason(agent: &Agent) -> Option<&'static str> {
    if agent.status != "live" || agent.held_reason.is_some() {
        return Some("This agent isn't taking jobs right now.");
    }
    if !crate::hosting::agent_is_serving(agent) {
        return Some("This agent is paused (its hosting has lapsed), so it can't take jobs right now.");
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Hiring and paying
// ---------------------------------------------------------------------------------------------

pub fn new_hire_id() -> String {
    format!("h{}", hex::encode(rand::random::<[u8; 10]>()))
}

fn grouped(n: usize) -> String {
    let s = n.to_string();
    s.chars().enumerate().fold(String::new(), |mut out, (i, c)| {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
        out
    })
}

pub struct NewHire<'a> {
    pub agent: &'a Agent,
    pub client_account_id: &'a str,
    pub brief: &'a str,
    pub client_email: Option<&'a str>,
    pub client_country: Option<&'a str>,
    /// "usdt" | "usdc" | "bnb".
    pub token: &'a str,
}

pub async fn create_hire(input: NewHire<'_>) -> Result<Hire> {
    let agent = input.agent;
    jobs_contract()?;
    if let Some(why) = unavailable_reason(agent) {
        return Err(hire_error(why));
    }
    if input.client_account_id == agent.owner_account_id {
        return Err(hire_error("You can't hire your own agent."));
    }
    let offer = agent_offer(&agent.id).await?.ok_or_else(|| hire_error("This agent doesn't offer jobs."))?;
    // Its environment must be able to produce this kind of work (e.g. video needs Pro hosting).
    if let Some(why) = crate::tiers::category_blocked(crate::tiers::Tier::parse(&agent.tier), &offer.category) {
        return Err(hire_error(format!("This agent can't take this job right now: {why}")));
    }
    if open_hire_count(&agent.id).await? as f64 >= offer.max_open_jobs {
        return Err(hire_error("This agent is fully booked. Try again later."));
    }
    let brief = input.brief.trim();
    if brief.chars().count() < 10 {
        return Err(hire_error("Describe the job in a few sentences."));
    }
    if brief.chars().count() > BRIEF_CHARS {
        return Err(hire_error(format!("The brief is limited to {} characters.", grouped(BRIEF_CHARS))));
    }
    let raw_email = input.client_email.map(str::trim).filter(|e| !e.is_empty());
    let client_email = raw_email.and_then(crate::mail::normalize_email);
    if raw_email.is_some() && client_email.is_none() {
        return Err(hire_error("That email address doesn't look right."));
    }
    let unpaid: i64 = sqlx::query_scalar("select count(*) from hires where client_account_id = $1 and status = 'awaiting_payment'").bind(input.client_account_id).fetch_one(db()).await?;
    if unpaid >= UNPAID_PER_CLIENT {
        return Err(hire_error("You have unpaid job requests open. Pay or wait for them to expire first."));
    }
    let token = hire_token(input.token).ok_or_else(|| hire_error("token must be usdt, usdc or bnb"))?;
    let config = jobs_config().await?;
    if !config.tokens.iter().any(|t| t == token) {
        return Err(hire_error(format!("{} isn't accepted for jobs.", token_symbol(token))));
    }
    // USD offers: 1:1 in stablecoins; BNB at the current rate, fixed when the job is requested.
    let units = if token == NATIVE { offer.price_usd / bnb_usd().await? } else { offer.price_usd };
    let amount = U256::from((units * 1e6).round() as u128) * U256::from(10u64).pow(U256::from(DECIMALS - 6));
    let min: U256 = config.min_amount.get(token).and_then(|m| m.parse().ok()).unwrap_or_default();
    if amount < min {
        return Err(hire_error("This offer is below the marketplace minimum."));
    }
    let deadline = chrono::Utc::now() + chrono::Duration::milliseconds(PAYMENT_WINDOW_MS + (offer.delivery_hours * HOUR_MS as f64) as i64);
    Ok(sqlx::query_as(
        "insert into hires (id, agent_id, client_account_id, client_email, client_country, offer_title, brief, token, amount, fee_bps, deadline_at)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) returning *",
    )
    .bind(new_hire_id())
    .bind(&agent.id)
    .bind(input.client_account_id)
    .bind(client_email)
    .bind(input.client_country)
    .bind(&offer.title)
    .bind(brief)
    .bind(token)
    .bind(amount.to_string())
    .bind(config.fee_bps as i32)
    .bind(deadline)
    .fetch_one(db())
    .await?)
}

fn call(to: Address, data: Vec<u8>, value: String) -> Call {
    Call { to: to.to_string(), data: format!("0x{}", hex::encode(data)), value }
}

/// What the client's wallet sends, in order, on BNB Chain: for USDT/USDC an approval of the escrow
/// then `fund`; for BNB just `fund` with the BNB attached.
pub fn payment_tx(hire: &Hire, agent_id: &str, agent_evm_address: &str) -> Result<Calls> {
    let escrow = jobs_contract()?;
    let amount: U256 = hire.amount.parse().map_err(|_| Error::internal("bad amount"))?;
    let deadline = hire.deadline_at.timestamp() as u64;
    let native = hire.token == NATIVE;
    let fund = call(
        escrow,
        OrvynJobs::fundCall { jobId: hire.id.clone(), agentId: agent_id.into(), payee: evm::parse_address(agent_evm_address)?, token: on_chain_token(&hire.token)?, amount, deadline }.abi_encode(),
        if native { amount.to_string() } else { "0".into() },
    );
    let calls = if native { vec![fund] } else { vec![call(evm::parse_address(&hire.token)?, evm::IERC20::approveCall { spender: escrow, amount }.abi_encode(), "0".into()), fund] };
    Ok(Calls { chain_id: evm::BSC.chain_id, amount: None, calls })
}

/// The client's own escrow calls, unsigned: approve the work, dispute it, or reclaim after the deadline.
pub fn client_call(hire_id: &str, action: &str, reason: &str) -> Result<Calls> {
    let id = hire_id.to_string();
    let data = match action {
        "dispute" => OrvynJobs::disputeCall { jobId: id, reason: reason.chars().take(1000).collect() }.abi_encode(),
        "approve" => OrvynJobs::approveCall { jobId: id }.abi_encode(),
        "reclaim" => OrvynJobs::reclaimCall { jobId: id }.abi_encode(),
        _ => return Err(hire_error("action must be approve, dispute or reclaim")),
    };
    Ok(Calls { chain_id: evm::BSC.chain_id, amount: None, calls: vec![call(jobs_contract()?, data, "0".into())] })
}

// ---------------------------------------------------------------------------------------------
// Mirroring the chain
// ---------------------------------------------------------------------------------------------

/// Columns `update` may set, with their values.
#[derive(Default)]
pub struct HireUpdate {
    pub status: Option<&'static str>,
    pub chain_action: Option<Option<&'static str>>,
    pub chain_error: Option<Option<String>>,
    pub needs_agent: Option<bool>,
    pub funded_at: Option<chrono::DateTime<chrono::Utc>>,
    pub deadline_at: Option<chrono::DateTime<chrono::Utc>>,
    pub fee_bps: Option<i32>,
    pub delivered_at: Option<chrono::DateTime<chrono::Utc>>,
    pub review_ends_at: Option<chrono::DateTime<chrono::Utc>>,
    pub settled_at: Option<chrono::DateTime<chrono::Utc>>,
    pub paid_to_agent: Option<String>,
    pub refunded: Option<String>,
    pub fee: Option<String>,
    pub result: Option<String>,
    pub result_hash: Option<String>,
    pub agent_attempts: Option<i32>,
    pub rating: Option<i32>,
    pub review: Option<Option<String>>,
}

async fn update(hire_id: &str, u: HireUpdate) -> Result<Hire> {
    Ok(sqlx::query_as(
        "update hires set
           status = coalesce($2, status),
           chain_action = case when $3 then $4 else chain_action end,
           chain_error = case when $5 then $6 else chain_error end,
           needs_agent = coalesce($7, needs_agent),
           funded_at = coalesce($8, funded_at),
           deadline_at = coalesce($9, deadline_at),
           fee_bps = coalesce($10, fee_bps),
           delivered_at = coalesce($11, delivered_at),
           review_ends_at = coalesce($12, review_ends_at),
           settled_at = coalesce($13, settled_at),
           paid_to_agent = coalesce($14, paid_to_agent),
           refunded = coalesce($15, refunded),
           fee = coalesce($16, fee),
           result = coalesce($17, result),
           result_hash = coalesce($18, result_hash),
           agent_attempts = coalesce($19, agent_attempts),
           rating = coalesce($20, rating),
           review = case when $21 then $22 else review end,
           updated_at = now()
         where id = $1 returning *",
    )
    .bind(hire_id)
    .bind(u.status)
    .bind(u.chain_action.is_some())
    .bind(u.chain_action.flatten())
    .bind(u.chain_error.is_some())
    .bind(u.chain_error.flatten())
    .bind(u.needs_agent)
    .bind(u.funded_at)
    .bind(u.deadline_at)
    .bind(u.fee_bps)
    .bind(u.delivered_at)
    .bind(u.review_ends_at)
    .bind(u.settled_at)
    .bind(u.paid_to_agent)
    .bind(u.refunded)
    .bind(u.fee)
    .bind(u.result)
    .bind(u.result_hash)
    .bind(u.agent_attempts)
    .bind(u.rating)
    .bind(u.review.is_some())
    .bind(u.review.flatten())
    .fetch_one(db())
    .await?)
}

async fn load_agent(id: &str) -> Result<Option<Agent>> {
    Ok(sqlx::query_as("select * from agents where id = $1").bind(id).fetch_optional(db()).await?)
}

fn at_ms(ms: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp_millis(ms).unwrap_or_default()
}

fn notice(title: impl Into<String>, body: impl Into<String>, hire_id: &str) -> Notice {
    Notice { title: title.into(), body: body.into(), path: Some(format!("/job/{hire_id}")) }
}

/// Bring a hire up to date with the escrow contract. Returns the updated row.
pub async fn sync_hire(mut hire: Hire) -> Result<Hire> {
    let Some(job) = chain_job(&hire.id).await? else {
        if hire.status == "awaiting_payment" && chrono::Utc::now().timestamp_millis() > hire.created_at.timestamp_millis() + PAYMENT_WINDOW_MS + 10 * 60_000 {
            return update(&hire.id, HireUpdate { status: Some("expired"), ..Default::default() }).await;
        }
        return Ok(hire);
    };
    let now = chrono::Utc::now().timestamp_millis();
    if hire.status == "awaiting_payment" {
        let agent = load_agent(&hire.agent_id).await?;
        let paid: U256 = job.amount.parse().unwrap_or_default();
        let ok = agent.is_some_and(|a| {
            job.client == hire.client_account_id && job.payee == a.evm_address.to_lowercase() && job.agent_id == hire.agent_id && job.token == hire.token && paid >= hire.amount.parse().unwrap_or(U256::MAX)
        });
        if !ok {
            add_message(&hire, "system", "The payment didn't match this job's terms, so it's being refunded.", false).await?;
            return update(&hire.id, HireUpdate { status: Some("invalid"), chain_action: Some((job.status == "Funded").then_some("refund")), ..Default::default() }).await;
        }
        hire = update(&hire.id, HireUpdate { status: Some("funded"), funded_at: Some(at_ms(job.funded_at_ms)), deadline_at: Some(at_ms(job.deadline_ms)), fee_bps: Some(job.fee_bps as i32), needs_agent: Some(true), ..Default::default() }).await?;
        let sym = token_symbol(&job.token);
        add_message(&hire, "system", &format!("Paid: {} {sym} is held in escrow until the job is done.", usd(&job.amount)), false).await?;
        log_agent(&hire.agent_id, AgentLogKind::Plugin, &format!("Hired: \"{}\" for {} {sym} (job {})", hire.offer_title, usd(&job.amount), hire.id)).await?;
        push::notify_owner(&hire.agent_id, notice("New job for your agent", format!("{}, {} {sym} in escrow", hire.offer_title, usd(&job.amount)), &hire.id));
        email_client(&hire, &format!("Your job is confirmed: {}", hire.offer_title), &format!("Thanks, your payment is held in escrow and I've started on it.\n\nYour brief:\n{}\n\nDue by {}.", hire.brief, hire.deadline_at.format("%a, %d %b %Y %H:%M:%S GMT"))).await;
    }
    match job.status {
        "Funded" => {
            // Missed the deadline with nothing delivered: refund the client.
            if hire.result.is_none() && now > job.deadline_ms && hire.chain_action.is_none() {
                add_message(&hire, "system", "The deadline passed without a delivery, so the payment is being refunded.", false).await?;
                return update(&hire.id, HireUpdate { chain_action: Some(Some("refund")), needs_agent: Some(false), ..Default::default() }).await;
            }
            Ok(hire)
        }
        "Delivered" => {
            let review_ends = job.review_ends_ms.unwrap_or(0);
            let mut u = HireUpdate::default();
            let mut changed = false;
            if hire.status != "delivered" {
                u.status = Some("delivered");
                u.delivered_at = job.delivered_at_ms.map(at_ms);
                u.review_ends_at = Some(at_ms(review_ends));
                changed = true;
            }
            if hire.chain_action.as_deref() == Some("mark_delivered") {
                u.chain_action = Some(None);
                changed = true;
            }
            // Review window over: pay the agent.
            if now >= review_ends && hire.chain_action.as_deref() != Some("release") {
                u.chain_action = Some(Some("release"));
                changed = true;
            }
            if changed {
                update(&hire.id, u).await
            } else {
                Ok(hire)
            }
        }
        "Disputed" => {
            if hire.status == "disputed" {
                return Ok(hire);
            }
            add_message(&hire, "system", "The client disputed this delivery. Orvyn will review the job and decide how to split the payment.", false).await?;
            update(&hire.id, HireUpdate { status: Some("disputed"), chain_action: Some(None), needs_agent: Some(false), ..Default::default() }).await
        }
        s @ ("Released" | "Refunded" | "Resolved") => {
            let status = match s {
                "Released" => "released",
                "Refunded" if hire.status == "invalid" => "invalid",
                "Refunded" => "refunded",
                _ => "resolved",
            };
            if hire.status == status && hire.settled_at.is_some() {
                return Ok(hire);
            }
            let sym = token_symbol(&job.token);
            let note = match status {
                "released" => format!("Paid out: {} {sym} to the agent.", usd(&job.paid_to_payee)),
                "resolved" => format!("Dispute resolved: {} to the agent, {} refunded to the client.", usd(&job.paid_to_payee), usd(&job.refunded)),
                _ => format!("Refunded: {} {sym} back to the client.", usd(&job.refunded)),
            };
            add_message(&hire, "system", &note, false).await?;
            log_agent(&hire.agent_id, AgentLogKind::Plugin, &format!("Job {}: {note}", hire.id)).await?;
            push::notify(vec![hire.client_account_id.clone()], notice(hire.offer_title.clone(), note.clone(), &hire.id));
            push::notify_owner(&hire.agent_id, notice(hire.offer_title.clone(), note, &hire.id));
            update(
                &hire.id,
                HireUpdate {
                    status: Some(status),
                    settled_at: Some(chrono::Utc::now()),
                    chain_action: Some(None),
                    needs_agent: Some(false),
                    paid_to_agent: Some(job.paid_to_payee),
                    refunded: Some(job.refunded),
                    fee: Some(job.fee),
                    ..Default::default()
                },
            )
            .await
        }
        _ => Ok(hire),
    }
}

/// Orchestrator task: sync an agent's open hires and make the escrow calls they need
/// (mark_delivered, refund, release) with Orvyn's operator key.
pub async fn sync_agent_hires(agent_id: &str) -> Result<()> {
    if !marketplace_enabled() {
        return Ok(());
    }
    let rows: Vec<Hire> = sqlx::query_as("select * from hires where agent_id = $1 and status = any($2)").bind(agent_id).bind(TRACKED).fetch_all(db()).await?;
    for hire in rows {
        let id = hire.id.clone();
        let r: Result<()> = async {
            let hire = sync_hire(hire).await?;
            let Some(action) = hire.chain_action.clone() else { return Ok(()) };
            let id = hire.id.clone();
            let data = match action.as_str() {
                "mark_delivered" => {
                    let h = hire.result_hash.clone().unwrap_or_default();
                    let padded = format!("{:0>64}", h.trim_start_matches("0x"));
                    let hash: FixedBytes<32> = padded.parse().map_err(|_| Error::internal("bad result hash"))?;
                    OrvynJobs::markDeliveredCall { jobId: id, resultHash: hash }.abi_encode()
                }
                "refund" => OrvynJobs::refundCall { jobId: id }.abi_encode(),
                _ => OrvynJobs::releaseCall { jobId: id }.abi_encode(),
            };
            crate::wallets::send_from_key(&operator_key()?, jobs_contract()?, data.into(), U256::ZERO).await?;
            let hire = update(&hire.id, HireUpdate { chain_error: Some(None), ..Default::default() }).await?;
            sync_hire(hire).await?;
            Ok(())
        }
        .await;
        if let Err(e) = r {
            let message: String = e.message.chars().take(500).collect();
            eprintln!("[hires] {id}: {message}");
            let _ = update(&id, HireUpdate { chain_error: Some(Some(message)), ..Default::default() }).await;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Work, messages and email
// ---------------------------------------------------------------------------------------------

pub async fn hire_messages_of(hire_id: &str) -> Result<Vec<HireMessage>> {
    Ok(sqlx::query_as("select * from hire_messages where hire_id = $1 order by id asc").bind(hire_id).fetch_all(db()).await?)
}

/// `author`: "client" | "agent" | "owner" | "system".
pub async fn add_message(hire: &Hire, author: &str, text: &str, via_email: bool) -> Result<()> {
    let body: String = text.trim().chars().take(MESSAGE_CHARS).collect();
    if body.is_empty() {
        return Ok(());
    }
    sqlx::query("insert into hire_messages (hire_id, author, text, via_email) values ($1, $2, $3, $4)").bind(&hire.id).bind(author).bind(&body).bind(via_email).execute(db()).await?;
    // A client message while the job is open wakes the agent.
    if author == "client" && (hire.status == "funded" || hire.status == "delivered") {
        sqlx::query("update hires set needs_agent = true, updated_at = now() where id = $1").bind(&hire.id).execute(db()).await?;
    }
    if (author == "agent" || author == "owner") && !via_email {
        email_client(hire, &format!("Re: {}", hire.offer_title), &body).await;
    }
    let who = match author {
        "client" => "Client",
        "system" => "Update",
        _ => "Agent",
    };
    let n = notice(format!("{who}: {}", hire.offer_title), body.chars().take(200).collect::<String>(), &hire.id);
    if author == "client" {
        push::notify_owner(&hire.agent_id, n);
    } else if author != "system" {
        push::notify(vec![hire.client_account_id.clone()], n);
    }
    Ok(())
}

/// Mirror a message to the client's email, threaded, from the agent's address. Never fails.
async fn email_client(hire: &Hire, subject: &str, text: &str) {
    let Some(client_email) = hire.client_email.clone() else { return };
    let r: Result<()> = async {
        let Some(agent) = load_agent(&hire.agent_id).await?.filter(|a| a.handle.is_some()) else { return Ok(()) };
        // Read the thread fresh: callers may hold a copy of the hire from before the first email.
        let thread: Option<String> = sqlx::query_scalar("select email_thread_id from hires where id = $1").bind(&hire.id).fetch_one(db()).await?;
        let parent: Option<MailMessage> = match &thread {
            Some(t) => sqlx::query_as("select * from mail_messages where agent_id = $1 and message_id = $2 limit 1").bind(&agent.id).bind(t).fetch_optional(db()).await?,
            None => None,
        };
        let row = crate::mail::queue_message(crate::mail::Outgoing {
            agent: &agent,
            author: "agent",
            to: vec![client_email],
            cc: vec![],
            subject: format!("[Orvyn job] {subject}").chars().take(300).collect(),
            text: format!("{text}\n\n--\nJob page: {}\nReply to this email to message {} about the job.", app_url(&format!("/hires/{}", hire.id)), agent.name),
            reply_to: parent.as_ref(),
            auto_submitted: Some(true),
        })
        .await?;
        if thread.is_none() {
            sqlx::query("update hires set email_thread_id = $2 where id = $1").bind(&hire.id).bind(&row.thread_id).execute(db()).await?;
        }
        Ok(())
    }
    .await;
    if let Err(e) = r {
        eprintln!("[hires] email for {}: {}", hire.id, e.message);
    }
}

static WROTE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^On .+wrote:\s*$").unwrap());
static ORIGINAL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^-{2,}\s*Original Message").unwrap());

/// Drop quoted history from an email reply ("On … wrote:" and "> " lines).
pub fn strip_quoted(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect();
    let cut = lines.iter().enumerate().position(|(i, l)| WROTE.is_match(l) || (l.starts_with('>') && lines[i..].iter().all(|x| x.trim().is_empty() || x.starts_with('>'))) || ORIGINAL.is_match(l));
    let kept = match cut {
        Some(c) => &lines[..c],
        None => &lines[..],
    };
    kept.join("\n").trim().to_string()
}

/// Called by the mail module for inbound mail: a client's reply in a job's email thread becomes a
/// job message (and isn't handled as ordinary mail). Returns true when it was consumed.
pub async fn on_inbound_email(agent_id: &str, thread_id: &str, from_address: &str, text: &str) -> Result<bool> {
    let hire: Option<Hire> = sqlx::query_as("select * from hires where agent_id = $1 and email_thread_id = $2 limit 1").bind(agent_id).bind(thread_id).fetch_optional(db()).await?;
    let Some(hire) = hire.filter(|h| h.client_email.as_deref() == Some(from_address)) else { return Ok(false) };
    let stripped = strip_quoted(text);
    Box::pin(add_message(&hire, "client", if stripped.is_empty() { text } else { &stripped }, true)).await?;
    Ok(true)
}

pub fn result_hash(result: &str) -> String {
    hex::encode(Sha256::digest(result.as_bytes()))
}

async fn system_note(hire_id: &str, text: &str) -> Result<()> {
    sqlx::query("insert into hire_messages (hire_id, author, text) values ($1, 'system', $2)").bind(hire_id).bind(text).execute(db()).await?;
    Ok(())
}

/// The agent delivers: the operator then puts it on chain, starting the client's review window.
/// Files rendered in Orvyn Studio for this job (only ones that passed their checks) can go with it;
/// their names and SHA-256s are appended to the result, so the on-chain hash covers them too.
pub async fn deliver_result(hire: &Hire, result: &str, file_ids: &[String]) -> Result<Hire> {
    if hire.status != "funded" || hire.result.is_some() {
        return Err(hire_error("This job isn't waiting for a delivery."));
    }
    let files = crate::studio::deliverable_files(&hire.id, file_ids).await?;
    let text = format!("{}{}", result.trim(), crate::studio::files_manifest(&files));
    if result.trim().chars().count() < 20 {
        return Err(hire_error("The result is too short."));
    }
    if text.chars().count() > RESULT_CHARS {
        return Err(hire_error(format!("The result is limited to {} characters.", grouped(RESULT_CHARS))));
    }
    crate::studio::mark_delivered(&files.iter().map(|f| f.id.clone()).collect::<Vec<_>>()).await?;
    let updated = update(&hire.id, HireUpdate { result: Some(text.clone()), result_hash: Some(result_hash(&text)), chain_action: Some(Some("mark_delivered")), needs_agent: Some(false), ..Default::default() }).await?;
    system_note(&hire.id, "The agent delivered the result. The client can approve it, or dispute it during the review window.").await?;
    push::notify(vec![hire.client_account_id.clone()], notice("Your result is ready", format!("{}: review and approve it", hire.offer_title), &hire.id));
    email_client(
        &updated,
        &format!("Delivered: {}", hire.offer_title),
        &format!("Your job is done. Here's the result:\n\n{text}\n\nIf it's what you asked for, approve it on the job page. If not, reply here or dispute it on the job page before the review window ends."),
    )
    .await;
    Ok(updated)
}

/// After delivery, during review: the agent improves the result (the on-chain hash stays the first delivery's).
pub async fn revise_result(hire: &Hire, result: &str, file_ids: &[String]) -> Result<Hire> {
    if hire.status != "delivered" && !(hire.status == "funded" && hire.result.is_some()) {
        return Err(hire_error("Only a delivered job can be revised."));
    }
    let files = crate::studio::deliverable_files(&hire.id, file_ids).await?;
    crate::studio::mark_delivered(&files.iter().map(|f| f.id.clone()).collect::<Vec<_>>()).await?;
    let text: String = format!("{}{}", result.trim(), crate::studio::files_manifest(&files)).chars().take(RESULT_CHARS).collect();
    let updated = update(&hire.id, HireUpdate { result: Some(text.clone()), ..Default::default() }).await?;
    system_note(&hire.id, "The agent revised the result.").await?;
    email_client(&updated, &format!("Revised: {}", hire.offer_title), &format!("I've revised the result:\n\n{text}")).await;
    Ok(updated)
}

/// The agent (or its creator) declines: the client is refunded in full.
pub async fn decline_hire(hire: &Hire, reason: &str, by: &str) -> Result<Hire> {
    if hire.status != "funded" && hire.status != "delivered" {
        return Err(hire_error("This job can't be declined now."));
    }
    add_message(hire, by, if reason.is_empty() { "Sorry, I can't take this job." } else { reason }, false).await?;
    system_note(&hire.id, "The job was declined and the payment is being refunded in full.").await?;
    update(&hire.id, HireUpdate { chain_action: Some(Some("refund")), needs_agent: Some(false), ..Default::default() }).await
}

/// Hires the agent's session should look at (new jobs, client messages), oldest first.
pub async fn hires_needing_agent(agent_id: &str, limit: i64) -> Result<Vec<Hire>> {
    Ok(sqlx::query_as("select * from hires where agent_id = $1 and needs_agent = true and status in ('funded', 'delivered') order by updated_at asc limit $2").bind(agent_id).bind(limit).fetch_all(db()).await?)
}

pub async fn mark_agent_done(hire: &Hire, failed: bool) -> Result<Hire> {
    let attempts = hire.agent_attempts + 1;
    if failed && attempts >= AGENT_ATTEMPTS && hire.status == "funded" && hire.result.is_none() {
        system_note(&hire.id, "The agent couldn't complete this job, so the payment is being refunded in full.").await?;
        return update(&hire.id, HireUpdate { agent_attempts: Some(attempts), needs_agent: Some(false), chain_action: Some(Some("refund")), ..Default::default() }).await;
    }
    update(&hire.id, HireUpdate { agent_attempts: Some(attempts), needs_agent: Some(failed), ..Default::default() }).await
}

// ---------------------------------------------------------------------------------------------
// Ratings, listings and views
// ---------------------------------------------------------------------------------------------

pub async fn rate_hire(hire: &Hire, rating: i64, review: &str) -> Result<Hire> {
    if hire.status != "released" && hire.status != "resolved" {
        return Err(hire_error("You can rate a job once it's paid out."));
    }
    if hire.rating.is_some() {
        return Err(hire_error("You've already rated this job."));
    }
    if !(1..=5).contains(&rating) {
        return Err(hire_error("Rating must be 1 to 5 stars."));
    }
    let review: String = review.trim().chars().take(REVIEW_CHARS).collect();
    update(&hire.id, HireUpdate { rating: Some(rating as i32), review: Some((!review.is_empty()).then_some(review)), ..Default::default() }).await
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HireStats {
    pub completed: i64,
    pub ratings: i64,
    pub avg_rating: Option<f64>,
}

pub async fn hire_stats(agent_ids: &[String]) -> Result<HashMap<String, HireStats>> {
    if agent_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<(String, i64, i64, Option<f64>)> = sqlx::query_as(
        "select agent_id, count(*) filter (where status in ('released', 'resolved')), count(rating), avg(rating)::float8 from hires where agent_id = any($1) group by agent_id",
    )
    .bind(agent_ids)
    .fetch_all(db())
    .await?;
    Ok(rows.into_iter().map(|(id, completed, ratings, avg)| (id, HireStats { completed, ratings, avg_rating: avg.map(|a| (a * 10.0).round() / 10.0) })).collect())
}

pub async fn recent_reviews(agent_id: &str, limit: i64) -> Result<Vec<Value>> {
    let rows: Vec<(Option<i32>, Option<String>, String, String, Option<chrono::DateTime<chrono::Utc>>)> = sqlx::query_as(
        "select rating, review, client_account_id, offer_title, settled_at from hires where agent_id = $1 and rating is not null order by settled_at desc nulls first limit $2",
    )
    .bind(agent_id)
    .bind(limit)
    .fetch_all(db())
    .await?;
    Ok(rows
        .into_iter()
        .map(|(rating, review, client, title, settled)| {
            // Only a hint of who: reviews are public.
            let n = client.chars().count();
            let hint = if n > 14 { format!("{}…{}", client.chars().take(6).collect::<String>(), client.chars().skip(n - 6).collect::<String>()) } else { client };
            json!({ "rating": rating, "review": review, "offerTitle": title, "client": hint, "at": settled.map(|t| t.timestamp_millis()) })
        })
        .collect())
}

/// Every agent offering jobs, busiest track record first.
pub async fn marketplace_listings() -> Result<Vec<Value>> {
    let rows: Vec<(String, Value)> = sqlx::query_as(
        "select a.id, p.config from agent_plugins p join agents a on a.id = p.agent_id where p.plugin_id = $1 and p.enabled = true and a.status = 'live' and a.held_reason is null",
    )
    .bind(HIRE_PLUGIN_ID)
    .fetch_all(db())
    .await?;
    let ids: Vec<String> = rows.iter().map(|r| r.0.clone()).collect();
    let agents: Vec<Agent> = sqlx::query_as("select * from agents where id = any($1)").bind(&ids).fetch_all(db()).await?;
    let (stats, open) = (hire_stats(&ids).await?, open_hire_counts(&ids).await?);
    let mut out: Vec<(i64, f64, Value)> = vec![];
    for (id, config) in rows {
        let Some(agent) = agents.iter().find(|a| a.id == id) else { continue };
        let offer = offer_of(&config);
        let s = stats.get(&id).cloned().unwrap_or(HireStats { completed: 0, ratings: 0, avg_rating: None });
        let open_jobs = open.get(&id).copied().unwrap_or(0);
        let available = unavailable_reason(agent).is_none() && (open_jobs as f64) < offer.max_open_jobs;
        out.push((
            s.completed,
            s.avg_rating.unwrap_or(0.0),
            json!({
                "agent": { "id": agent.id, "handle": agent.handle, "name": agent.name, "symbol": agent.symbol, "icon": agent.icon, "description": agent.description, "email": crate::mail::agent_address(agent.handle.as_deref()) },
                "offer": offer,
                "openJobs": open_jobs,
                "available": available,
                "completed": s.completed,
                "ratings": s.ratings,
                "avgRating": s.avg_rating,
            }),
        ));
    }
    out.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.total_cmp(&a.1)));
    Ok(out.into_iter().map(|x| x.2).collect())
}

/// A hire as its client ("client") or the agent's creator ("owner") sees it. The client's email
/// is shown to the client only.
pub fn hire_view(hire: &Hire, messages: &[HireMessage], role: &str) -> Value {
    let ms = |t: &Option<chrono::DateTime<chrono::Utc>>| t.map(|t| t.timestamp_millis());
    json!({
        "id": hire.id,
        "agentId": hire.agent_id,
        "role": role,
        "client": hire.client_account_id,
        "clientEmail": if role == "client" { json!(hire.client_email) } else { json!(hire.client_email.as_ref().map(|_| "(set)")) },
        "offerTitle": hire.offer_title,
        "brief": hire.brief,
        "token": hire.token,
        "tokenSymbol": token_symbol(&hire.token),
        "amount": hire.amount,
        "amountUsd": to_usd(&hire.amount),
        "feeBps": hire.fee_bps,
        "status": hire.status,
        "deadlineAt": hire.deadline_at.timestamp_millis(),
        "fundedAt": ms(&hire.funded_at),
        "deliveredAt": ms(&hire.delivered_at),
        "reviewEndsAt": ms(&hire.review_ends_at),
        "settledAt": ms(&hire.settled_at),
        "result": hire.result,
        "resultHash": hire.result_hash,
        // A delivery Orvyn hasn't put on chain yet (the review window starts once it has).
        "deliveryPending": hire.chain_action.as_deref() == Some("mark_delivered"),
        "pendingAction": hire.chain_action,
        "paidToAgent": hire.paid_to_agent.as_deref().map(to_usd),
        "refunded": hire.refunded.as_deref().map(to_usd),
        "rating": hire.rating,
        "review": hire.review,
        "createdAt": hire.created_at.timestamp_millis(),
        "messages": messages.iter().map(|m| json!({ "id": m.id, "author": m.author, "text": m.text, "viaEmail": m.via_email, "at": m.created_at.timestamp_millis() })).collect::<Vec<_>>(),
    })
}

pub async fn get_hire(id: &str) -> Result<Option<Hire>> {
    Ok(sqlx::query_as("select * from hires where id = $1").bind(id).fetch_optional(db()).await?)
}

pub async fn client_hires(client_account_id: &str, limit: i64) -> Result<Vec<Hire>> {
    Ok(sqlx::query_as("select * from hires where client_account_id = $1 order by created_at desc limit $2").bind(client_account_id).bind(limit).fetch_all(db()).await?)
}

pub async fn agent_hires(agent_id: &str, limit: i64) -> Result<Vec<Hire>> {
    Ok(sqlx::query_as("select * from hires where agent_id = $1 order by created_at desc limit $2").bind(agent_id).bind(limit).fetch_all(db()).await?)
}
