//! Binance Agentic Wallet: an agent works an on-chain wallet its owner holds in the Binance App
//! (Binance's MPC "Agentic Wallet": BSC, Ethereum, Base, Solana and more), through Binance's own
//! CLI, `baw` (npm @binance/agentic-wallet).
//!
//! - Signing in: Orvyn runs `baw auth signin` for the agent and shows the owner Binance's sign-in
//!   link and pairing code; the owner checks the code and confirms in the Binance App, while Orvyn
//!   waits on `baw auth verify`. The Binance App holds the real limits (daily limit, which tokens,
//!   what happens to risky transactions); Orvyn can't change them. Signing out (`baw auth signout`)
//!   revokes the agent's access; so does signing it out in the Binance App.
//! - One session per agent: `baw` keeps its session in `$BINANCE_BAW_DIR/session.json`, encrypted
//!   with a key from `BINANCE_INSTANCE_ID`. Orvyn gives each agent its own random instance id and
//!   keeps the session file in the database (encrypted again, bound to the agent), writing it to a
//!   private temporary directory for each command and reading it back after. `baw` must be
//!   installed without its OS-keychain module (`--ignore-scripts`, see the Dockerfiles): with a
//!   keychain it would keep one session for the whole machine.
//! - Writes (swaps, limit orders, sends) are agent actions, so autopilot only, and each passes the
//!   owner's Orvyn limits first (per order and per day in USD, which chains, sends on or off). Sends
//!   only reach addresses in the owner's Binance address book; Binance enforces that.
//!
//! Env: BAW_BIN (default `baw`; a path ending in .js runs with node).

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::db::{db, log_agent, Agent, AgentLogKind};
use crate::error::{Error, Result};
use crate::secrets::{decrypt_secret, encrypt_secret, SecretKind};

pub const PLUGIN_ID: &str = "binance-wallet";
/// The `baw` version Orvyn is built and tested against.
pub const BAW_VERSION: &str = "1.12.0";

const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// `baw auth verify` waits up to 5 minutes for the owner's confirmation.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(330);

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct WalletSession {
    pub agent_id: String,
    pub account_id: String,
    pub instance_encrypted: String,
    pub session_encrypted: Option<String>,
    pub status: String,
    pub qr_code_id: Option<String>,
    pub pairing_code: Option<String>,
    pub signin_url: Option<String>,
    pub signin_expires_at: Option<DateTime<Utc>>,
    pub addresses: Value,
    pub settings: Option<Value>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// What `baw` answered: its `data`, or its error.
#[derive(Debug, Clone)]
pub enum Answer {
    Ok(Value),
    Failed { code: i64, name: String, message: String },
}

impl Answer {
    /// Signed out on Binance's side (session expired, revoked in the App, never signed in).
    pub fn signed_out(&self) -> bool {
        matches!(self, Answer::Failed { code, .. } if (10003000..=10003003).contains(code))
    }
    pub fn into_result(self) -> std::result::Result<Value, String> {
        match self {
            Answer::Ok(v) => Ok(v),
            Answer::Failed { name, message, .. } => Err(if name.is_empty() { message } else { format!("{name}: {message}") }),
        }
    }
}

fn instance_aad(agent_id: &str) -> String {
    format!("binance-wallet-instance:{agent_id}")
}
fn session_aad(agent_id: &str) -> String {
    format!("binance-wallet-session:{agent_id}")
}

pub async fn session(agent_id: &str) -> Result<Option<WalletSession>> {
    Ok(sqlx::query_as("select * from binance_wallet_sessions where agent_id = $1").bind(agent_id).fetch_optional(db()).await?)
}

/// One `baw` command at a time per agent (each reads and writes the agent's session).
static LOCKS: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = LazyLock::new(Default::default);
fn lock_of(agent_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    LOCKS.lock().unwrap().entry(agent_id.to_string()).or_default().clone()
}

/// The private directory a command runs in; removed however the command ends.
struct TempDir(std::path::PathBuf);
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn command() -> tokio::process::Command {
    let bin = std::env::var("BAW_BIN").ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| "baw".into());
    if bin.ends_with(".js") || bin.ends_with(".mjs") {
        let mut c = tokio::process::Command::new("node");
        c.arg(bin);
        c
    } else {
        tokio::process::Command::new(bin)
    }
}

/// The JSON envelope `baw --json` prints (`{"success": true, "data": …}` or `{"success": false,
/// "error": {code, name, message}}`), from its output.
pub fn parse_envelope(stdout: &str) -> Option<Answer> {
    let v: Value = serde_json::from_str(stdout.trim()).ok().or_else(|| stdout.lines().rev().find_map(|l| serde_json::from_str::<Value>(l.trim()).ok().filter(|v| v.get("success").is_some())))?;
    if v["success"] == true {
        return Some(Answer::Ok(v.get("data").cloned().unwrap_or(Value::Null)));
    }
    let e = &v["error"];
    Some(Answer::Failed {
        code: e["code"].as_i64().or_else(|| e["code"].as_str().and_then(|c| c.parse().ok())).unwrap_or(0),
        name: e["name"].as_str().unwrap_or("").to_string(),
        message: e["message"].as_str().unwrap_or("Binance Agentic Wallet failed").to_string(),
    })
}

/// Run `baw <args> --json` in the agent's own session.
pub async fn run(agent_id: &str, args: &[&str], timeout: Duration) -> Result<Answer> {
    let lock = lock_of(agent_id);
    let _guard = lock.lock().await;
    let row = session(agent_id).await?.ok_or_else(|| Error::precondition("connect the Binance Agentic Wallet first"))?;
    let instance = decrypt_secret(&row.instance_encrypted, &instance_aad(agent_id), SecretKind::AiKey)?;
    let before = match &row.session_encrypted {
        Some(s) => Some(decrypt_secret(s, &session_aad(agent_id), SecretKind::AiKey)?),
        None => None,
    };

    let dir = TempDir(std::env::temp_dir().join(format!("orvyn-baw-{}", hex::encode(rand::random::<[u8; 12]>()))));
    create_private_dir(&dir.0)?;
    let file = dir.0.join("session.json");
    if let Some(s) = &before {
        write_private(&file, s)?;
    }

    let mut cmd = command();
    cmd.args(args)
        .arg("--json")
        .env("BINANCE_BAW_DIR", &dir.0)
        .env("BINANCE_INSTANCE_ID", &instance)
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = match tokio::time::timeout(timeout, cmd.output()).await {
        Err(_) => return Err(Error::unavailable("Binance Agentic Wallet didn't answer in time")),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => return Err(Error::unavailable("the Binance Agentic Wallet CLI (baw) isn't installed on this server")),
        Ok(Err(e)) => return Err(Error::internal(format!("baw: {e}"))),
        Ok(Ok(o)) => o,
    };

    // Keep whatever the session became (signing in writes it; signing out removes it).
    let after = std::fs::read_to_string(&file).ok();
    if after != before {
        let enc = after.as_deref().map(|s| encrypt_secret(s, &session_aad(agent_id), SecretKind::AiKey)).transpose()?;
        sqlx::query("update binance_wallet_sessions set session_encrypted = $2, updated_at = now() where agent_id = $1").bind(agent_id).bind(enc).execute(db()).await?;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_envelope(&stdout).ok_or_else(|| {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Error::unavailable(format!("Binance Agentic Wallet gave no answer{}", if stderr.trim().is_empty() { String::new() } else { format!(": {}", crate::http::clip(stderr.trim(), 200)) }))
    })
}

fn create_private_dir(path: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(path).map_err(|e| Error::internal(e.to_string()))
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir(path).map_err(|e| Error::internal(e.to_string()))
    }
}

fn write_private(path: &std::path::Path, contents: &str) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path).map_err(|e| Error::internal(e.to_string()))?;
        f.write_all(contents.as_bytes()).map_err(|e| Error::internal(e.to_string()))
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents).map_err(|e| Error::internal(e.to_string()))
    }
}

/// The agent signed out (expired, revoked in the App, or the owner disconnected).
async fn mark_signed_out(agent_id: &str, why: &str) -> Result<()> {
    sqlx::query("update binance_wallet_sessions set status = 'disconnected', error = $2, qr_code_id = null, pairing_code = null, signin_url = null, signin_expires_at = null, updated_at = now() where agent_id = $1").bind(agent_id).bind(why).execute(db()).await?;
    crate::bliss::merge_state(agent_id, PLUGIN_ID, json!({ "connected": false })).await
}

/// Run a command for an agent's tool: signed-out answers mark the wallet disconnected.
pub async fn call(agent_id: &str, args: &[&str]) -> std::result::Result<Value, String> {
    let connected: Option<String> = sqlx::query_scalar("select status from binance_wallet_sessions where agent_id = $1").bind(agent_id).fetch_optional(db()).await.map_err(|e| e.to_string())?;
    if connected.as_deref() != Some("connected") {
        return Err("the Binance Agentic Wallet isn't connected; the owner signs it in from the plugin's settings".into());
    }
    let answer = run(agent_id, args, CALL_TIMEOUT).await.map_err(|e| e.message)?;
    if answer.signed_out() {
        let _ = mark_signed_out(agent_id, "Signed out of the Binance Agentic Wallet (expired or revoked in the Binance App). Sign in again.").await;
        let _ = log_agent(agent_id, AgentLogKind::Plugin, "Binance Agentic Wallet signed out (expired or revoked)").await;
    }
    answer.into_result()
}

// ── Signing in ──────────────────────────────────────────────────────────────────────────────

/// The verify waits running, one per agent (a new sign-in replaces the old one).
static VERIFYING: LazyLock<Mutex<HashMap<String, tokio::task::AbortHandle>>> = LazyLock::new(Default::default);

fn stop_verifying(agent_id: &str) {
    if let Some(h) = VERIFYING.lock().unwrap().remove(agent_id) {
        h.abort();
    }
}

async fn check_owner(account_id: &str, agent: &Agent) -> Result<()> {
    if agent.owner_account_id.to_lowercase() != account_id.to_lowercase() {
        return Err(Error::forbidden("only the agent's owner can connect its Binance Agentic Wallet"));
    }
    let installed: Option<String> = sqlx::query_scalar("select plugin_id from agent_plugins where agent_id = $1 and plugin_id = $2").bind(&agent.id).bind(PLUGIN_ID).fetch_optional(db()).await?;
    if installed.is_none() {
        return Err(Error::invalid("install the Binance Agentic Wallet plugin on this agent first"));
    }
    Ok(())
}

fn ms_time(v: &Value) -> Option<DateTime<Utc>> {
    let ms = v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))?;
    DateTime::from_timestamp_millis(ms)
}

/// Start signing the agent in. Returns the view (with Binance's link and pairing code while it waits
/// for the owner).
pub async fn start_connect(account_id: &str, agent: &Agent) -> Result<Value> {
    check_owner(account_id, agent).await?;
    let instance = encrypt_secret(&hex::encode(rand::random::<[u8; 32]>()), &instance_aad(&agent.id), SecretKind::AiKey)?;
    // Keep the agent's instance (and session) if it has one: its wallet stays the same.
    sqlx::query("insert into binance_wallet_sessions (agent_id, account_id, instance_encrypted) values ($1, $2, $3) on conflict (agent_id) do update set account_id = excluded.account_id, error = null, updated_at = now()")
        .bind(&agent.id)
        .bind(account_id.to_lowercase())
        .bind(instance)
        .execute(db())
        .await?;
    stop_verifying(&agent.id);

    let data = run(&agent.id, &["auth", "signin"], CALL_TIMEOUT).await?.into_result().map_err(Error::unavailable)?;
    if data["status"] == "ALREADY_CONNECTED" {
        refresh(&agent.id).await?;
        return view(&agent.id).await;
    }
    let (Some(qr), Some(url)) = (data["qrCodeId"].as_str(), data["urlForWeb"].as_str()) else {
        return Err(Error::unavailable("Binance didn't send a sign-in link"));
    };
    let pairing = data["pairingCode"].as_str().map(String::from);
    let expires = ms_time(&data["expireAt"]).unwrap_or_else(|| Utc::now() + chrono::Duration::minutes(5));
    sqlx::query("update binance_wallet_sessions set status = 'pending', qr_code_id = $2, pairing_code = $3, signin_url = $4, signin_expires_at = $5, error = null, updated_at = now() where agent_id = $1")
        .bind(&agent.id)
        .bind(qr)
        .bind(&pairing)
        .bind(url)
        .bind(expires)
        .execute(db())
        .await?;

    let (agent_id, qr) = (agent.id.clone(), qr.to_string());
    let task = tokio::spawn(async move {
        let outcome = finish_connect(&agent_id, &qr).await;
        if let Err(e) = outcome {
            let _ = sqlx::query("update binance_wallet_sessions set status = 'disconnected', error = $3, qr_code_id = null, pairing_code = null, signin_url = null, signin_expires_at = null, updated_at = now() where agent_id = $1 and qr_code_id = $2")
                .bind(&agent_id)
                .bind(&qr)
                .bind(&e.message)
                .execute(db())
                .await;
        }
        VERIFYING.lock().unwrap().remove(&agent_id);
    });
    VERIFYING.lock().unwrap().insert(agent.id.clone(), task.abort_handle());
    view(&agent.id).await
}

/// Wait for the owner's confirmation in the Binance App, then read the wallet.
async fn finish_connect(agent_id: &str, qr: &str) -> Result<()> {
    let answer = run(agent_id, &["auth", "verify", "--qrCodeId", qr], VERIFY_TIMEOUT).await?;
    let data = answer.into_result().map_err(|e| Error::invalid(format!("Not signed in: {e}")))?;
    if data["status"] != "SUCCESS" {
        return Err(Error::invalid("Not signed in: Binance didn't confirm the sign-in"));
    }
    refresh(agent_id).await?;
    log_agent(agent_id, AgentLogKind::Plugin, "Owner signed the agent in to their Binance Agentic Wallet").await?;
    Ok(())
}

/// Read the wallet's status, addresses and the owner's limits in the Binance App.
pub async fn refresh(agent_id: &str) -> Result<()> {
    let status = run(agent_id, &["wallet", "status"], CALL_TIMEOUT).await?;
    if status.signed_out() {
        return mark_signed_out(agent_id, "Not signed in").await;
    }
    let status = status.into_result().map_err(Error::unavailable)?;
    let state = match status["status"].as_str() {
        Some("CONNECTED") => "connected",
        Some("CREATING") => "creating",
        _ => return mark_signed_out(agent_id, "Not signed in").await,
    };
    let addresses = if state == "connected" { run(agent_id, &["wallet", "address"], CALL_TIMEOUT).await?.into_result().unwrap_or(Value::Null) } else { Value::Null };
    let settings = if state == "connected" { run(agent_id, &["wallet", "settings"], CALL_TIMEOUT).await?.into_result().ok() } else { None };
    let addresses = address_list(&addresses);
    sqlx::query("update binance_wallet_sessions set status = $2, addresses = $3, settings = coalesce($4, settings), qr_code_id = null, pairing_code = null, signin_url = null, signin_expires_at = null, error = null, updated_at = now() where agent_id = $1")
        .bind(agent_id)
        .bind(state)
        .bind(&addresses)
        .bind(&settings)
        .execute(db())
        .await?;
    crate::bliss::merge_state(agent_id, PLUGIN_ID, json!({ "connected": state == "connected", "addresses": addresses })).await
}

/// `wallet address` as a list of {chain, address}, whatever shape it comes in.
pub fn address_list(v: &Value) -> Value {
    let items: Vec<Value> = match v {
        Value::Array(a) => a.clone(),
        Value::Object(o) => match o.get("addresses").or_else(|| o.get("list")) {
            Some(Value::Array(a)) => a.clone(),
            _ => o.iter().filter_map(|(k, v)| v.as_str().map(|a| json!({ "chain": k, "address": a }))).collect(),
        },
        _ => vec![],
    };
    Value::Array(
        items
            .into_iter()
            .filter_map(|i| {
                let address = i["address"].as_str().or_else(|| i["walletAddress"].as_str())?.to_string();
                let chain = i["chain"].as_str().or_else(|| i["simpleName"].as_str()).or_else(|| i["chainName"].as_str()).or_else(|| i["name"].as_str()).map(String::from).or_else(|| i["binanceChainId"].as_str().map(String::from)).unwrap_or_default();
                Some(json!({ "chain": chain, "address": address }))
            })
            .collect(),
    )
}

pub async fn view(agent_id: &str) -> Result<Value> {
    let Some(mut s) = session(agent_id).await? else { return Ok(json!({ "status": "disconnected", "connected": false })) };
    // A sign-in nobody is waiting on any more (its code expired, or the server restarted meanwhile).
    if s.status == "pending" && s.signin_expires_at.is_none_or(|t| t + chrono::Duration::seconds(30) < Utc::now()) {
        s.status = "disconnected".into();
        s.error = Some("The sign-in code expired. Start again.".into());
    }
    let iso = |t: DateTime<Utc>| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let pending = (s.status == "pending").then(|| json!({ "url": s.signin_url, "pairingCode": s.pairing_code, "expiresAt": s.signin_expires_at.map(iso) }));
    let settings = s.settings.as_ref().map(|v| json!({ "dailyLimit": v["dailyLimit"], "tradeAllTokens": v["tradeAllTokens"], "abnormalTxnHandling": v["abnormalTxnHandling"], "sessionExpireTime": v["sessionExpireTime"] }));
    Ok(json!({
        "status": s.status,
        "connected": s.status == "connected",
        "pending": pending,
        "addresses": s.addresses,
        "limits": settings,
        "error": s.error,
        "updatedAt": iso(s.updated_at),
    }))
}

/// Sign the agent out (Binance forgets the session) and forget it here.
pub async fn disconnect(account_id: &str, agent: &Agent) -> Result<bool> {
    if agent.owner_account_id.to_lowercase() != account_id.to_lowercase() {
        return Err(Error::forbidden("only the agent's owner can disconnect its Binance Agentic Wallet"));
    }
    stop_verifying(&agent.id);
    let Some(s) = session(&agent.id).await? else { return Ok(false) };
    if s.session_encrypted.is_some() {
        // Best effort: the owner can also sign the agent out in the Binance App.
        let _ = run(&agent.id, &["auth", "signout"], CALL_TIMEOUT).await;
    }
    sqlx::query("delete from binance_wallet_sessions where agent_id = $1").bind(&agent.id).execute(db()).await?;
    crate::bliss::merge_state(&agent.id, PLUGIN_ID, json!({ "connected": false, "addresses": [] })).await?;
    log_agent(&agent.id, AgentLogKind::Plugin, "Owner disconnected the Binance Agentic Wallet").await?;
    Ok(true)
}

// ── The owner's limits ──────────────────────────────────────────────────────────────────────

pub const NATIVE: &str = "0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

/// Stablecoins Orvyn values at $1, by chain (lowercased EVM addresses; Solana's as is).
const STABLECOINS: &[(&str, &str)] = &[
    ("56", "0x55d398326f99059ff775485246999027b3197955"),  // USDT
    ("56", "0x8ac76a51cc950f9822d68b83fe1ad97b32cd580d"),  // USDC
    ("1", "0xdac17f958d2ee523a2206206994597c13d831ec7"),   // USDT
    ("1", "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),   // USDC
    ("8453", "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"), // USDC
    ("42161", "0xaf88d065e77c8cc2239327c5edb3a432268e5831"), // USDC
    ("42161", "0xfd086bc7cd5c481dcc9c85ebe478a1c0b69fcbb9"), // USDT
    ("137", "0x3c499c542cef5e3811e1192ce70d8cc03d5c3359"),  // USDC
    ("137", "0xc2132d05d31c914a87c6611c10748aeb04b58e8f"),  // USDT
    ("CT_501", "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"), // USDC
    ("CT_501", "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB"), // USDT
];

fn native_symbol(chain: &str) -> Option<&'static str> {
    Some(match chain {
        "56" => "BNB",
        "1" | "8453" | "42161" => "ETH",
        "137" => "POL",
        "CT_501" => "SOL",
        _ => return None,
    })
}

fn is_native(chain: &str, token: &str) -> bool {
    token.eq_ignore_ascii_case(NATIVE) || (chain == "CT_501" && token == "So11111111111111111111111111111111111111111")
}

fn is_stable(chain: &str, token: &str) -> bool {
    STABLECOINS.iter().any(|(c, t)| *c == chain && t.eq_ignore_ascii_case(token))
}

const STABLE_SYMBOLS: &[&str] = &["USDT", "USDC", "FDUSD", "USD1", "DAI", "TUSD"];

/// A symbol's USD price (stablecoins: 1).
async fn usd_price(symbol: &str) -> Option<f64> {
    let s = symbol.trim().to_uppercase();
    if STABLE_SYMBOLS.contains(&s.as_str()) {
        return Some(1.0);
    }
    crate::binance::market_price(&format!("{s}USDT")).await
}

/// The owner's Orvyn limits (the plugin's settings).
#[derive(Debug, Clone)]
pub struct Limits {
    pub max_order_usd: f64,
    pub max_daily_usd: f64,
    pub chains: Vec<String>,
    pub allow_limit_orders: bool,
    pub allow_sends: bool,
}

impl Limits {
    pub fn from_settings(c: &crate::bliss::Settings) -> Limits {
        Limits {
            max_order_usd: c.get("maxOrderUsd").and_then(Value::as_f64).unwrap_or(50.0),
            max_daily_usd: c.get("maxDailyUsd").and_then(Value::as_f64).unwrap_or(200.0),
            chains: c.get("chains").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(chain_id).filter(|s| !s.is_empty()).collect()).unwrap_or_else(|| vec!["56".into()]),
            allow_limit_orders: c.get("allowLimitOrders").and_then(Value::as_bool).unwrap_or(true),
            allow_sends: c.get("allowSends").and_then(Value::as_bool).unwrap_or(false),
        }
    }
}

/// A chain as `baw` names it (binanceChainId), from an id or a common name.
pub fn chain_id(v: &str) -> String {
    match v.trim().to_lowercase().as_str() {
        "bsc" | "bnb" | "bnb chain" | "bnb smart chain" | "56" => "56".into(),
        "eth" | "ethereum" | "1" => "1".into(),
        "base" | "8453" => "8453".into(),
        "sol" | "solana" | "ct_501" => "CT_501".into(),
        "arbitrum" | "arb" | "42161" => "42161".into(),
        "polygon" | "matic" | "pol" | "137" => "137".into(),
        other => other.to_uppercase(),
    }
}

/// What an agent's write is, for the limits.
#[derive(Debug, Clone, PartialEq)]
pub enum Write {
    Swap { chain: String, from: String, to: String, qty: String },
    Limit { chain: String, from: String, to: String, qty: String },
    Send { chain: String, token: String, amount: String },
    Cancel,
}

/// The size of a swap or limit order in USD, from Binance's own quote.
async fn quote_usd(agent_id: &str, chain: &str, from: &str, to: &str, qty: &str) -> std::result::Result<f64, String> {
    if is_stable(chain, from) {
        return qty.parse::<f64>().map_err(|_| "fromTokenQty isn't a number".to_string());
    }
    let q = call(agent_id, &["market-order", "quote", "--fromTokenQty", qty, "--fromToken", from, "--toToken", to, "--binanceChainId", chain]).await?;
    let num = |v: &Value| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()));
    let (fs, fa) = (q["fromCoinSymbol"].as_str().unwrap_or(""), num(&q["fromCoinAmount"]));
    let (ts, ta) = (q["toCoinSymbol"].as_str().unwrap_or(""), num(&q["toCoinAmount"]));
    if let (Some(a), true) = (ta, STABLE_SYMBOLS.contains(&ts.to_uppercase().as_str()) || is_stable(chain, to)) {
        return Ok(a);
    }
    if let (Some(a), Some(p)) = (fa, usd_price(fs).await) {
        return Ok(a * p);
    }
    if let (Some(a), Some(p)) = (ta, usd_price(ts).await) {
        return Ok(a * p);
    }
    Err("Orvyn couldn't value this order in USD, so it's refused".into())
}

/// Whether the agent may make this write under its owner's limits. Ok(Some(usd)) counts toward the
/// day; Err(why) is refused.
pub async fn check_write(agent_id: &str, limits: &Limits, w: &Write) -> std::result::Result<Option<f64>, String> {
    let chain = match w {
        Write::Swap { chain, .. } | Write::Limit { chain, .. } | Write::Send { chain, .. } => chain.clone(),
        Write::Cancel => return Ok(None),
    };
    if !limits.chains.contains(&chain) {
        return Err(format!("the owner hasn't allowed chain {chain} (allowed: {})", if limits.chains.is_empty() { "none".into() } else { limits.chains.join(", ") }));
    }
    let usd = match w {
        Write::Swap { from, to, qty, .. } => quote_usd(agent_id, &chain, from, to, qty).await?,
        Write::Limit { from, to, qty, .. } => {
            if !limits.allow_limit_orders {
                return Err("the owner hasn't allowed limit orders".into());
            }
            quote_usd(agent_id, &chain, from, to, qty).await?
        }
        Write::Send { token, amount, .. } => {
            if !limits.allow_sends {
                return Err("the owner hasn't allowed sending from the wallet".into());
            }
            let amount: f64 = amount.parse().map_err(|_| "amount isn't a number".to_string())?;
            if is_stable(&chain, token) {
                amount
            } else if is_native(&chain, token) {
                let price = usd_price(native_symbol(&chain).ok_or("Orvyn can't price this chain's coin")?).await.ok_or("Orvyn couldn't get a price, so the send is refused")?;
                amount * price
            } else {
                return Err("agents may only send the chain's own coin, USDT or USDC".into());
            }
        }
        Write::Cancel => unreachable!(),
    };
    if !(usd.is_finite() && usd >= 0.0) {
        return Err("Orvyn couldn't value this in USD, so it's refused".into());
    }
    if usd > limits.max_order_usd {
        return Err(format!("this is about ${usd:.2}, over the owner's limit of ${} per order", limits.max_order_usd));
    }
    let today: f64 = sqlx::query_scalar("select coalesce(sum(notional_usd), 0)::float8 from binance_wallet_orders where agent_id = $1 and created_at > now() - interval '1 day'")
        .bind(agent_id)
        .fetch_one(db())
        .await
        .map_err(|e| e.to_string())?;
    if today + usd > limits.max_daily_usd {
        return Err(format!("this (${usd:.2}) would pass the owner's daily limit of ${} (${today:.2} used in the last 24h)", limits.max_daily_usd));
    }
    Ok(Some(usd))
}

pub async fn record_write(agent_id: &str, kind: &str, usd: f64) -> Result<()> {
    sqlx::query("insert into binance_wallet_orders (agent_id, kind, notional_usd) values ($1, $2, $3)").bind(agent_id).bind(kind).bind(usd).execute(db()).await?;
    Ok(())
}

/// After a swap is submitted, wait (up to about a minute) for Binance to finish it.
pub async fn await_swap(agent_id: &str, order_id: &str) -> Value {
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_secs(3)).await;
        if let Ok(v) = call(agent_id, &["market-order", "list", "--orderId", order_id]).await {
            if let Some(o) = v["list"].as_array().and_then(|l| l.first()) {
                if matches!(o["status"].as_str(), Some("FINISHED") | Some("FAILED")) {
                    return o.clone();
                }
            }
        }
    }
    json!({ "orderId": order_id, "status": "PENDING", "note": "submitted; still pending, check with agentic_wallet_orders" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelopes() {
        match parse_envelope(r#"{"success": true, "data": {"status": "CONNECTED"}}"#) {
            Some(Answer::Ok(v)) => assert_eq!(v["status"], "CONNECTED"),
            other => panic!("{other:?}"),
        }
        let failed = parse_envelope("progress…\n{\"success\": false, \"error\": {\"code\": 10003002, \"name\": \"SESSION_EXPIRED\", \"message\": \"Session expired\"}}\n").unwrap();
        assert!(failed.signed_out());
        assert_eq!(failed.into_result().unwrap_err(), "SESSION_EXPIRED: Session expired");
        assert!(parse_envelope("not json").is_none());
    }

    #[test]
    fn chains_and_tokens() {
        assert_eq!(chain_id("BSC"), "56");
        assert_eq!(chain_id("solana"), "CT_501");
        assert!(is_native("56", "0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE"));
        assert!(is_stable("56", "0x55d398326f99059fF775485246999027B3197955"));
        assert!(!is_stable("1", "0x55d398326f99059fF775485246999027B3197955"));
    }

    #[test]
    fn addresses_in_any_shape() {
        let a = address_list(&json!([{ "binanceChainId": "56", "simpleName": "BSC", "address": "0xabc" }]));
        assert_eq!(a, json!([{ "chain": "BSC", "address": "0xabc" }]));
        let b = address_list(&json!({ "EVM": "0xabc", "Solana": "So1" }));
        assert_eq!(b.as_array().unwrap().len(), 2);
    }
}
