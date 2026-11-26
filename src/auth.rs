//! Sign in with an EVM wallet (Sign-In with Ethereum, EIP-4361) on BNB Chain, sessions, Orvyn
//! Mobile's sign-in codes, and holder gating for agent chat.
//!
//! The client builds a SIWE message for one of Orvyn's domains (statement SIGN_IN_STATEMENT, chain
//! 56, a fresh nonce, issued now) and the wallet signs it. We check the message's fields, verify the
//! signature (EOAs, and smart wallets via ERC-1271/6492), refuse stale or reused nonces, and issue
//! an HMAC-signed session token for the address (lowercase). Tokens are the TypeScript core's:
//! `base64url(JSON {a, e}).base64url(HMAC-SHA256(SESSION_SECRET, body))`.

use std::sync::LazyLock;

use alloy::primitives::{Address, U256};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::db::db;
use crate::env::env;
use crate::error::{Error, Result};
use crate::evm;

pub const SIGN_IN_STATEMENT: &str = "Sign in to Orvyn";
pub const SIGN_IN_CHAIN_ID: u64 = 56;
const NONCE_MAX_AGE_MS: i64 = 5 * 60_000;
const SESSION_TTL_MS: i64 = 7 * 24 * 3_600_000;
const MOBILE_CODE_TTL_MS: i64 = 2 * 60_000;

pub fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

// ---- SIWE messages (EIP-4361), parsed as viem parses them ----

static PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:(?<scheme>[a-zA-Z][a-zA-Z0-9+\-.]*)://)?(?<domain>[a-zA-Z0-9+\-.]*(?::[0-9]{1,5})?) (?:wants you to sign in with your Ethereum account:\n)(?<address>0x[a-fA-F0-9]{40})\n\n(?:(?<statement>.*)\n\n)?").unwrap()
});
static SUFFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:URI: (?<uri>.+))\n(?:Version: (?<version>.+))\n(?:Chain ID: (?<chainId>\d+))\n(?:Nonce: (?<nonce>[a-zA-Z0-9]+))\n(?:Issued At: (?<issuedAt>.+))(?:\nExpiration Time: (?<expirationTime>.+))?(?:\nNot Before: (?<notBefore>.+))?(?:\nRequest ID: (?<requestId>.*))?(?:\nResources:(?<resources>(?:\n- .+)*))?").unwrap()
});
static SIWE_TIME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})$").unwrap());

/// A SIWE time: RFC 3339 only. `Err` for a value that's present but not a valid time.
fn siwe_time(v: Option<&str>) -> std::result::Result<Option<DateTime<Utc>>, ()> {
    match v {
        None => Ok(None),
        Some(v) if SIWE_TIME.is_match(v) => DateTime::parse_from_rfc3339(v).map(|t| Some(t.with_timezone(&Utc))).map_err(|_| ()),
        Some(_) => Err(()),
    }
}

#[derive(Debug, Clone, Default)]
pub struct SiweMessage {
    pub scheme: Option<String>,
    pub domain: Option<String>,
    pub address: Option<String>,
    pub statement: Option<String>,
    pub uri: Option<String>,
    pub version: Option<String>,
    pub chain_id: Option<u64>,
    pub nonce: Option<String>,
    pub issued_at: Option<std::result::Result<DateTime<Utc>, ()>>,
    pub expiration_time: Option<std::result::Result<DateTime<Utc>, ()>>,
    pub not_before: Option<std::result::Result<DateTime<Utc>, ()>>,
    pub request_id: Option<String>,
    pub resources: Option<Vec<String>>,
}

pub fn parse_siwe(message: &str) -> SiweMessage {
    let mut m = SiweMessage::default();
    let s = |v: Option<regex::Match>| v.map(|x| x.as_str().to_string());
    let time = |v: Option<regex::Match>| v.map(|x| siwe_time(Some(x.as_str())).map(|t| t.unwrap()));
    if let Some(c) = PREFIX.captures(message) {
        m.scheme = s(c.name("scheme")).filter(|v| !v.is_empty());
        m.domain = s(c.name("domain"));
        m.address = s(c.name("address"));
        m.statement = s(c.name("statement")).filter(|v| !v.is_empty());
    }
    if let Some(c) = SUFFIX.captures(message) {
        m.uri = s(c.name("uri"));
        m.version = s(c.name("version"));
        m.chain_id = c.name("chainId").and_then(|v| v.as_str().parse().ok());
        m.nonce = s(c.name("nonce"));
        m.issued_at = time(c.name("issuedAt"));
        m.expiration_time = time(c.name("expirationTime"));
        m.not_before = time(c.name("notBefore"));
        m.request_id = s(c.name("requestId")).filter(|v| !v.is_empty());
        m.resources = c.name("resources").map(|r| r.as_str().split("\n- ").skip(1).map(String::from).collect());
    }
    m
}

/// viem's validateSiweMessage with no expected address, domain, nonce or scheme: the lifetime
/// and the address's form.
pub fn validate_siwe(m: &SiweMessage, now: DateTime<Utc>) -> bool {
    match m.expiration_time {
        Some(Err(())) => return false,
        Some(Ok(t)) if now >= t => return false,
        _ => {}
    }
    match m.not_before {
        Some(Err(())) => return false,
        Some(Ok(t)) if now < t => return false,
        _ => {}
    }
    m.address.as_deref().is_some_and(evm::is_address)
}

/// Domains a SIWE message may be for: the app, the API and the root (ORVYN_ROOT_DOMAIN).
fn allowed_domain(domain: &str) -> bool {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let root = var("ORVYN_ROOT_DOMAIN").or_else(|| var("NEXT_PUBLIC_ROOT_DOMAIN")).unwrap_or_else(|| "orvyn.cash".into()).to_lowercase();
    let extra: Vec<String> = var("ORVYN_SIWE_DOMAINS").unwrap_or_default().split(',').map(|d| d.trim().to_lowercase()).filter(|d| !d.is_empty()).collect();
    let d = domain.to_lowercase();
    [root.clone(), format!("app.{root}"), format!("api.{root}"), "app.localhost:3000".into(), "localhost:3000".into(), "localhost:3002".into()]
        .iter()
        .chain(extra.iter())
        .any(|a| *a == d)
}

#[derive(Debug, Clone, Deserialize)]
pub struct SignInPayload {
    /// The full EIP-4361 message the wallet signed.
    pub message: String,
    /// 0x signature.
    pub signature: String,
}

/// The message's fields, checked for a fresh SIWE message with this statement (no signature check).
pub fn check_siwe_fields(message: &str, statement: &str, now: DateTime<Utc>) -> Result<SiweMessage> {
    let bad = |m: &str| Error::unauthorized(m.to_string());
    let f = parse_siwe(message);
    let (Some(address), Some(_), Some(issued), Some(domain)) = (&f.address, &f.nonce, &f.issued_at, &f.domain) else {
        return Err(bad("malformed sign-in message"));
    };
    let _ = address;
    if f.statement.as_deref() != Some(statement) {
        return Err(bad("unexpected message statement"));
    }
    if f.chain_id != Some(SIGN_IN_CHAIN_ID) {
        return Err(bad("sign in on BNB Chain (chain 56)"));
    }
    if !allowed_domain(domain) {
        return Err(bad(&format!("message is for {domain}, not Orvyn")));
    }
    // Stricter than the TypeScript core, which let an unparseable time through this check.
    let Ok(issued) = issued else { return Err(bad("malformed sign-in message")) };
    if (now.timestamp_millis() - issued.timestamp_millis()).abs() > NONCE_MAX_AGE_MS {
        return Err(bad("signature expired, try again"));
    }
    if !validate_siwe(&f, now) {
        return Err(bad("message expired or not yet valid"));
    }
    Ok(f)
}

/// Verify a fresh SIWE signature whose statement is `statement` and burn its nonce so it can't be
/// replayed. Returns the signer (lowercase). Used for sign-in and for actions that need the wallet's
/// say-so on top of a session, like withdrawals (with their own statement).
pub async fn verify_wallet_message(input: &SignInPayload, statement: &str) -> Result<String> {
    let f = check_siwe_fields(&input.message, statement, Utc::now())?;
    let address: Address = f.address.as_deref().unwrap().parse().map_err(|_| Error::unauthorized("malformed sign-in message"))?;
    let signature = hex::decode(input.signature.trim_start_matches("0x")).map_err(|_| Error::unauthorized("signature doesn't match the address"))?;
    if !evm::sig::verify_hash(address, evm::sig::message_hash(&input.message), &signature).await? {
        return Err(Error::unauthorized("signature doesn't match the address"));
    }
    let account = format!("{address:#x}");
    // The primary key refuses a replayed nonce.
    burn_nonce(&format!("siwe:{account}:{}", f.nonce.unwrap())).await?;
    Ok(account)
}

/// Record a one-time value as used; a second use is refused.
pub async fn burn_nonce(nonce: &str) -> Result<()> {
    let inserted = sqlx::query("insert into used_nonces (nonce) values ($1) on conflict do nothing").bind(nonce).execute(db()).await?;
    if inserted.rows_affected() == 0 {
        return Err(Error::unauthorized("this signature was already used, try again"));
    }
    Ok(())
}

pub struct SignedIn {
    pub address: String,
    pub token: String,
}

pub async fn verify_sign_in(input: &SignInPayload) -> Result<SignedIn> {
    let address = verify_wallet_message(input, SIGN_IN_STATEMENT).await?;
    Ok(SignedIn { token: create_session(&address), address })
}

/// A random SIWE nonce (alphanumeric, as EIP-4361 requires).
pub fn new_nonce() -> String {
    hex::encode(rand::random::<[u8; 16]>())
}

// ---- Sessions ----

fn sign_with(secret: &str, value: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac");
    mac.update(value.as_bytes());
    B64URL.encode(mac.finalize().into_bytes())
}

fn sign(value: &str) -> String {
    sign_with(&env().session_secret, value)
}

fn macs_equal(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(Serialize, Deserialize)]
struct SessionBody {
    a: String,
    e: i64,
}

pub fn create_session(account_id: &str) -> String {
    create_session_at(account_id, now_ms())
}

pub fn create_session_at(account_id: &str, now: i64) -> String {
    let body = B64URL.encode(serde_json::to_vec(&SessionBody { a: account_id.to_lowercase(), e: now + SESSION_TTL_MS }).unwrap());
    let mac = sign(&body);
    format!("{body}.{mac}")
}

/// The account (lowercase EVM address) for a valid session token, or None.
pub fn read_session(token: Option<&str>) -> Option<String> {
    read_session_at(token, now_ms())
}

pub fn read_session_at(token: Option<&str>, now: i64) -> Option<String> {
    let (body, mac) = token?.split_once('.')?;
    if body.is_empty() || mac.is_empty() || mac.contains('.') || !macs_equal(&sign(body), mac) {
        return None;
    }
    let s: SessionBody = serde_json::from_slice(&B64URL.decode(body).ok()?).ok()?;
    (s.e > now).then_some(s.a)
}

// ---- Orvyn Mobile sign-in ----
// The app opens app.orvyn.cash/mobile/connect in a browser tab, where the person signs in with
// their wallet as usual. That page gets a one-time code for the signed-in account and hands it back
// to the app (orvyn://auth). The app swaps it for a session token. PKCE (RFC 7636, S256) binds the
// code to the app that started the flow.

#[derive(Serialize, Deserialize)]
struct MobileBody {
    a: String,
    c: String,
    e: i64,
    n: String,
}

fn is_pkce_challenge(c: &str) -> bool {
    c.len() == 43 && c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

pub fn create_mobile_code(account_id: &str, code_challenge: &str) -> Result<String> {
    if !is_pkce_challenge(code_challenge) {
        return Err(Error::invalid("invalid code challenge"));
    }
    let body = MobileBody { a: account_id.to_string(), c: code_challenge.to_string(), e: now_ms() + MOBILE_CODE_TTL_MS, n: new_nonce() };
    let body = B64URL.encode(serde_json::to_vec(&body).unwrap());
    let mac = sign(&format!("mobile:{body}"));
    Ok(format!("{body}.{mac}"))
}

/// Check a mobile code and its PKCE verifier at `now`: the account and the code's one-time nonce.
pub fn check_mobile_code_at(code: &str, code_verifier: &str, now: i64) -> Result<(String, String)> {
    let invalid = || Error::unauthorized("invalid code");
    let (body, mac) = code.split_once('.').ok_or_else(invalid)?;
    if !macs_equal(&sign(&format!("mobile:{body}")), mac) {
        return Err(invalid());
    }
    let m: MobileBody = serde_json::from_slice(&B64URL.decode(body).map_err(|_| invalid())?).map_err(|_| invalid())?;
    if m.e < now {
        return Err(Error::unauthorized("code expired, sign in again"));
    }
    if B64URL.encode(Sha256::digest(code_verifier.as_bytes())) != m.c {
        return Err(Error::unauthorized("invalid code verifier"));
    }
    Ok((m.a, m.n))
}

/// Redeem a one-time mobile code with its PKCE verifier: a session token for the account.
pub async fn redeem_mobile_code(code: &str, code_verifier: &str) -> Result<SignedIn> {
    let (account, nonce) = check_mobile_code_at(code, code_verifier, now_ms())?;
    burn_nonce(&format!("mobile:{nonce}")).await?;
    Ok(SignedIn { token: create_session(&account), address: account })
}

// ---- Holder gate ----

#[derive(Debug, Clone, Serialize)]
pub struct HolderStatus {
    pub allowed: bool,
    pub balance: f64,
    pub reason: Option<String>,
}

/// Whether `account_id` holds enough of the agent's token (BEP-20 on BSC, 18 decimals) to use
/// holder chat.
pub async fn holder_status(token_address: Option<&str>, launchpad: &str, min_holding_tokens: f64, account_id: &str) -> Result<HolderStatus> {
    let Some(token) = token_address else {
        // Agents without a token have no holder gate: anyone signed in may chat (rate limits apply).
        if launchpad == "none" {
            return Ok(HolderStatus { allowed: true, balance: 0.0, reason: None });
        }
        return Ok(HolderStatus { allowed: false, balance: 0.0, reason: Some("token not launched yet".into()) });
    };
    let raw = evm::erc20_balance(token, account_id).await?;
    Ok(holder_from_raw(raw, min_holding_tokens))
}

/// Balance in whole tokens (6 decimal places), and the decision.
pub fn holder_from_raw(raw: U256, min_holding_tokens: f64) -> HolderStatus {
    let micros: U256 = raw / U256::from(10u64).pow(U256::from(12u8));
    let balance = micros.to_string().parse::<f64>().unwrap_or(f64::MAX) / 1e6;
    let allowed = balance >= min_holding_tokens;
    HolderStatus {
        allowed,
        balance,
        reason: (!allowed).then(|| format!("hold at least {} tokens to chat (you hold {})", js_number(min_holding_tokens), js_number(balance))),
    }
}

/// A number as JavaScript prints it (integers without ".0").
pub fn js_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e21 { format!("{}", n as i64) } else { format!("{n}") }
}

#[cfg(test)]
mod tests {
    #[test]
    fn hmac_matches_node() {
        // node -e 'console.log(require("crypto").createHmac("sha256","k".repeat(32)).update("body").digest("base64url"))'
        assert_eq!(super::sign_with(&"k".repeat(32), "body"), "_4gOnvXvlsW2uBPJK6TEHfpesfPK3uj9zFxaMyBhvtc");
    }
}
