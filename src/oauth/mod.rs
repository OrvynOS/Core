//! Orvyn OAuth: "Sign in with Orvyn" (OAuth 2.1 + OpenID Connect) for Orvyn's own apps and outside
//! developers. A person signs in as themselves or as one of their agents.
//!
//! - Authorization code with PKCE (S256, required for every client).
//! - Device authorization (RFC 8628), for Vyn on desktop and other input-limited apps.
//! - Refresh tokens rotate on every use; replaying a used one revokes the whole family. Codes are
//!   single use and live 60 seconds. Secrets and tokens are stored hashed.
//! - Access and ID tokens are ES256 JWTs (jwt.rs) verifiable with the published JWKS.

pub mod jwt;

use std::collections::BTreeMap;

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine as _;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

pub use jwt::{jwks, verify_jwt};

use crate::db::{db, Agent, OauthClient, OauthCode, OauthDeviceCode, OauthGrant, OauthRefreshToken};

/// An OAuth error: its protocol code (`invalid_grant`…), a message and the HTTP status.
#[derive(Debug, Clone, PartialEq)]
pub struct OAuthError {
    pub code: &'static str,
    pub message: String,
    pub status: u16,
}

impl std::fmt::Display for OAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl From<crate::Error> for OAuthError {
    fn from(e: crate::Error) -> Self {
        let status = e.status();
        OAuthError { code: "server_error", message: e.message, status }
    }
}

impl From<sqlx::Error> for OAuthError {
    fn from(e: sqlx::Error) -> Self {
        crate::Error::from(e).into()
    }
}

pub type OResult<T> = std::result::Result<T, OAuthError>;

fn err(code: &'static str, message: impl Into<String>, status: u16) -> OAuthError {
    OAuthError { code, message: message.into(), status }
}

pub const SCOPES: &[(&str, &str)] = &[
    ("openid", "Confirm who you are"),
    ("profile", "Name, handle and picture"),
    ("email", "Email address"),
    ("wallet", "Wallet address on BNB Chain"),
    ("agent", "Agent identity: Binance Agent ID, ERC-8004 id and Orvyn profile"),
    ("offline_access", "Stay signed in"),
    ("vyn", "Use Vyn, your Orvyn assistant: your conversations and settings"),
    ("agents", "Connect your self-hosted agents to Orvyn (their page, identity and activity)"),
    ("social", "Use Orvyn Social as you: read feeds, post, comment, repost and follow"),
    ("inference", "Use Orvyn AI with your Orvyn credits (coding agents on this computer)"),
];
/// Scopes only Orvyn's own apps may hold.
const FIRST_PARTY_SCOPES: &[&str] = &["vyn", "agents", "inference"];

fn known_scope(s: &str) -> bool {
    SCOPES.iter().any(|(k, _)| *k == s)
}

const ACCESS_TTL: i64 = 15 * 60;
const ID_TTL: i64 = 60 * 60;
const CODE_TTL_MS: i64 = 60_000;
const REFRESH_TTL_MS: i64 = 30 * 86_400_000;
const DEVICE_TTL_MS: i64 = 10 * 60_000;
const DEVICE_INTERVAL: i64 = 5;

fn sha256(s: &str) -> String {
    B64URL.encode(Sha256::digest(s.as_bytes()))
}

fn token(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    rand::fill(&mut b[..]);
    B64URL.encode(b)
}

fn now_s() -> i64 {
    chrono::Utc::now().timestamp()
}

fn env_url(names: &[&str], default: &str) -> String {
    names.iter().find_map(|n| std::env::var(n).ok().filter(|v| !v.is_empty())).unwrap_or_else(|| default.into()).trim_end_matches('/').to_string()
}

pub fn issuer() -> String {
    env_url(&["ORVYN_OAUTH_ISSUER", "NEXT_PUBLIC_API_URL"], "https://api.orvyn.cash")
}

fn app_url() -> String {
    env_url(&["ORVYN_APP_URL", "NEXT_PUBLIC_APP_URL"], "https://app.orvyn.cash")
}

/// OpenID Connect discovery document.
pub fn discovery() -> Value {
    let iss = issuer();
    json!({
        "issuer": iss,
        "authorization_endpoint": format!("{}/oauth/authorize", app_url()),
        "token_endpoint": format!("{iss}/oauth/token"),
        "userinfo_endpoint": format!("{iss}/oauth/userinfo"),
        "jwks_uri": format!("{iss}/oauth/jwks"),
        "revocation_endpoint": format!("{iss}/oauth/revoke"),
        "device_authorization_endpoint": format!("{iss}/oauth/device_authorization"),
        "scopes_supported": SCOPES.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token", "urn:ietf:params:oauth:grant-type:device_code"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none", "client_secret_basic", "client_secret_post"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["ES256"],
        "claims_supported": ["sub", "orvyn_subject_type", "name", "preferred_username", "picture", "email", "email_verified", "wallet_address", "agent_id_name", "erc8004_agent_id", "agent_environment", "profile"],
    })
}

// ---------- Clients ----------

/// https, or http on localhost, or a custom scheme for native apps (vyn://…). No fragments.
pub fn check_redirect_uri(uri: &str) -> OResult<()> {
    let u = url::Url::parse(uri).map_err(|_| err("invalid_redirect_uri", format!("not a URL: {uri}"), 400))?;
    if u.fragment().is_some() {
        return Err(err("invalid_redirect_uri", "redirect URIs can't have a fragment", 400));
    }
    let host = u.host_str().unwrap_or("");
    let local = host == "localhost" || host.ends_with(".localhost") || host == "127.0.0.1" || host == "[::1]";
    let scheme = u.scheme();
    if scheme == "https" || (scheme == "http" && local) {
        return Ok(());
    }
    let custom = regex::Regex::new(r"^[a-z][a-z0-9+.-]*\.?[a-z0-9+.-]*$").unwrap().is_match(scheme) && !["http", "javascript", "data", "file", "vbscript"].contains(&scheme);
    if custom {
        return Ok(());
    }
    Err(err("invalid_redirect_uri", format!("redirect URIs must be https (or http://localhost, or an app scheme): {uri}"), 400))
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientInput {
    pub name: String,
    /// "public" | "confidential".
    #[serde(rename = "type")]
    pub kind: String,
    pub redirect_uris: Vec<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
    pub homepage: Option<String>,
    pub logo_url: Option<String>,
}

pub fn client_view(c: &OauthClient) -> Value {
    json!({
        "id": c.id,
        "name": c.name,
        "type": c.kind,
        "redirectUris": c.redirect_uris,
        "scopes": c.scopes,
        "homepage": c.homepage,
        "logoUrl": c.logo_url,
        "firstParty": c.first_party,
        "status": c.status,
        "createdAt": c.created_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    })
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
}

pub async fn register_client(owner_account_id: &str, input: &ClientInput, first_party: bool, id: Option<&str>) -> OResult<(Value, Option<String>)> {
    let name = input.name.trim();
    if name.is_empty() || name.chars().count() > 60 {
        return Err(err("invalid_client_metadata", "the app name must be 1-60 characters", 400));
    }
    if input.redirect_uris.is_empty() || input.redirect_uris.len() > 10 {
        return Err(err("invalid_client_metadata", "give 1-10 redirect URIs", 400));
    }
    for u in &input.redirect_uris {
        check_redirect_uri(u)?;
    }
    let scopes: Vec<String> = if input.scopes.is_empty() { vec!["openid".into(), "profile".into(), "agent".into()] } else { input.scopes.clone() };
    if scopes.iter().any(|s| !known_scope(s)) {
        return Err(err("invalid_client_metadata", format!("scopes must be among {}", SCOPES.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(", ")), 400));
    }
    if !first_party && scopes.iter().any(|s| FIRST_PARTY_SCOPES.contains(&s.as_str())) {
        return Err(err("invalid_client_metadata", "that scope is reserved for Orvyn's own apps", 400));
    }
    if input.kind != "public" && input.kind != "confidential" {
        return Err(err("invalid_client_metadata", "type must be public or confidential", 400));
    }
    let id = id.map(String::from).unwrap_or_else(|| format!("orv_{}", token(12)));
    let secret = (input.kind == "confidential").then(|| format!("orvs_{}", token(32)));
    let client: OauthClient = sqlx::query_as(
        "insert into oauth_clients (id, name, owner_account_id, type, secret_hash, redirect_uris, scopes, homepage, logo_url, first_party) values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) returning *",
    )
    .bind(&id)
    .bind(name)
    .bind(owner_account_id.to_lowercase())
    .bind(&input.kind)
    .bind(secret.as_deref().map(sha256))
    .bind(json!(input.redirect_uris))
    .bind(json!(scopes))
    .bind(&input.homepage)
    .bind(&input.logo_url)
    .bind(first_party)
    .fetch_one(db())
    .await?;
    Ok((client_view(&client), secret))
}

/// The person's apps, newest first, each with how many people and agents use it and when it was
/// last used (a token issued or refreshed).
pub async fn clients_of(account_id: &str) -> OResult<Vec<Value>> {
    let rows: Vec<OauthClient> = sqlx::query_as("select * from oauth_clients where owner_account_id = $1 and status <> 'deleted' order by created_at desc").bind(account_id.to_lowercase()).fetch_all(db()).await?;
    let mut out = Vec::with_capacity(rows.len());
    for c in &rows {
        let (users, last): (i64, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as(
            "select (select count(*) from oauth_grants where client_id = $1 and revoked_at is null), (select max(created_at) from oauth_refresh_tokens where client_id = $1)",
        )
        .bind(&c.id)
        .fetch_one(db())
        .await?;
        let mut v = client_view(c);
        v["users"] = json!(users);
        v["lastUsedAt"] = json!(last.map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)));
        out.push(v);
    }
    Ok(out)
}

/// Changes to an app: any of name, redirect URIs, scopes, homepage and logo (null clears those two).
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientUpdate {
    pub name: Option<String>,
    pub redirect_uris: Option<Vec<String>>,
    pub scopes: Option<Vec<String>>,
    #[serde(default, deserialize_with = "some_or_null")]
    pub homepage: Option<Option<String>>,
    #[serde(default, deserialize_with = "some_or_null")]
    pub logo_url: Option<Option<String>>,
}

fn some_or_null<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Ok(Some(<Option<String> as serde::Deserialize>::deserialize(d)?))
}

/// Update one of the person's apps (not Orvyn's own). Narrowing scopes doesn't touch existing
/// sign-ins; they keep what they were granted until they sign in again.
pub async fn update_client(account_id: &str, client_id: &str, u: &ClientUpdate) -> OResult<Value> {
    let c: OauthClient = sqlx::query_as("select * from oauth_clients where id = $1 and owner_account_id = $2 and status <> 'deleted'")
        .bind(client_id)
        .bind(account_id.to_lowercase())
        .fetch_optional(db())
        .await?
        .ok_or_else(|| err("invalid_client", "no such app of yours", 404))?;
    if c.first_party {
        return Err(err("invalid_client", "Orvyn's own apps can't be changed here", 403));
    }
    let name = match &u.name {
        Some(n) if n.trim().is_empty() || n.trim().chars().count() > 60 => return Err(err("invalid_client_metadata", "the app name must be 1-60 characters", 400)),
        Some(n) => n.trim().to_string(),
        None => c.name.clone(),
    };
    let redirects = match &u.redirect_uris {
        Some(r) => {
            if r.is_empty() || r.len() > 10 {
                return Err(err("invalid_client_metadata", "give 1-10 redirect URIs", 400));
            }
            for x in r {
                check_redirect_uri(x)?;
            }
            json!(r)
        }
        None => c.redirect_uris.clone(),
    };
    let scopes = match &u.scopes {
        Some(s) => {
            if s.is_empty() || s.iter().any(|x| !known_scope(x)) {
                return Err(err("invalid_client_metadata", format!("scopes must be among {}", SCOPES.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(", ")), 400));
            }
            if s.iter().any(|x| FIRST_PARTY_SCOPES.contains(&x.as_str())) {
                return Err(err("invalid_client_metadata", "that scope is reserved for Orvyn's own apps", 400));
            }
            json!(s)
        }
        None => c.scopes.clone(),
    };
    let url = |v: &Option<Option<String>>, current: &Option<String>| -> OResult<Option<String>> {
        match v {
            None => Ok(current.clone()),
            Some(None) => Ok(None),
            Some(Some(s)) if s.trim().is_empty() => Ok(None),
            Some(Some(s)) if s.len() <= 500 && (s.starts_with("https://") || s.starts_with("http://localhost")) => Ok(Some(s.trim().to_string())),
            Some(Some(_)) => Err(err("invalid_client_metadata", "homepage and logo must be https URLs", 400)),
        }
    };
    let homepage = url(&u.homepage, &c.homepage)?;
    let logo = url(&u.logo_url, &c.logo_url)?;
    let updated: OauthClient = sqlx::query_as("update oauth_clients set name = $2, redirect_uris = $3, scopes = $4, homepage = $5, logo_url = $6 where id = $1 returning *")
        .bind(client_id)
        .bind(name)
        .bind(redirects)
        .bind(scopes)
        .bind(homepage)
        .bind(logo)
        .fetch_one(db())
        .await?;
    Ok(client_view(&updated))
}

/// Delete one of the person's apps: every sign-in to it ends (grants and refresh tokens revoked,
/// pending codes dropped) and its client id stops working.
pub async fn delete_client(account_id: &str, client_id: &str) -> OResult<()> {
    let first_party: Option<bool> = sqlx::query_scalar("select first_party from oauth_clients where id = $1 and owner_account_id = $2 and status <> 'deleted'")
        .bind(client_id)
        .bind(account_id.to_lowercase())
        .fetch_optional(db())
        .await?;
    match first_party {
        None => return Err(err("invalid_client", "no such app of yours", 404)),
        Some(true) => return Err(err("invalid_client", "Orvyn's own apps can't be deleted here", 403)),
        Some(false) => {}
    }
    let mut tx = db().begin().await?;
    sqlx::query("update oauth_refresh_tokens set revoked_at = now() where client_id = $1 and revoked_at is null").bind(client_id).execute(&mut *tx).await?;
    sqlx::query("update oauth_grants set revoked_at = now() where client_id = $1 and revoked_at is null").bind(client_id).execute(&mut *tx).await?;
    sqlx::query("delete from oauth_codes where client_id = $1").bind(client_id).execute(&mut *tx).await?;
    sqlx::query("delete from oauth_device_codes where client_id = $1").bind(client_id).execute(&mut *tx).await?;
    sqlx::query("update oauth_clients set status = 'deleted', secret_hash = null where id = $1").bind(client_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn rotate_secret(account_id: &str, client_id: &str) -> OResult<String> {
    let secret = format!("orvs_{}", token(32));
    let n = sqlx::query("update oauth_clients set secret_hash = $3 where id = $1 and owner_account_id = $2 and type = 'confidential'")
        .bind(client_id)
        .bind(account_id.to_lowercase())
        .bind(sha256(&secret))
        .execute(db())
        .await?
        .rows_affected();
    if n == 0 {
        return Err(err("invalid_client", "no such confidential app of yours", 404));
    }
    Ok(secret)
}

/// Orvyn's own apps, created on first use in every deployment.
fn first_party(id: &str) -> Option<ClientInput> {
    let c = |name: &str, uri: &str, scopes: &[&str]| ClientInput {
        name: name.into(),
        kind: "public".into(),
        redirect_uris: uri.split(' ').map(String::from).collect(),
        scopes: scopes.iter().map(|s| s.to_string()).collect(),
        homepage: Some("https://orvyn.cash".into()),
        logo_url: None,
    };
    match id {
        "orv_vyn" => Some(c("Vyn", "vyn://oauth/callback", &["openid", "profile", "email", "wallet", "agent", "offline_access", "vyn"])),
        // Orvyn Social on the web (and its local development server).
        "orv_social" => Some(c("Orvyn Social", "https://social.orvyn.cash/callback http://social.localhost:3000/callback", &["openid", "profile", "agent", "offline_access", "social"])),
        "orv_agent_runtime" => Some(c("Orvyn agent (self-hosted)", "http://localhost/orvyn-agent", &["openid", "agents"])),
        // Vyness, the command line that runs coding agents on Orvyn AI (device sign-in).
        "orv_vyness" => Some(c("Vyness", "http://localhost/vyness", &["openid", "profile", "inference"])),
        _ => None,
    }
}

async fn active_client(client_id: Option<&str>) -> OResult<OauthClient> {
    let id = client_id.filter(|c| !c.is_empty()).ok_or_else(|| err("invalid_client", "client_id is required", 401))?;
    if let Some(fp) = first_party(id) {
        let _ = register_client("orvyn", &fp, true, Some(id)).await;
    }
    let c: Option<OauthClient> = sqlx::query_as("select * from oauth_clients where id = $1").bind(id).fetch_optional(db()).await?;
    c.filter(|c| c.status == "active").ok_or_else(|| err("invalid_client", "unknown or suspended client", 401))
}

fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |x, (p, q)| x | (p ^ q)) == 0
}

/// Authenticate a client at the token endpoint: confidential ones by secret; public ones by id.
pub async fn authenticate_client(client_id: Option<&str>, client_secret: Option<&str>) -> OResult<OauthClient> {
    let c = active_client(client_id).await?;
    if c.kind == "confidential" {
        let ok = matches!((client_secret, &c.secret_hash), (Some(s), Some(h)) if same(&sha256(s), h));
        if !ok {
            return Err(err("invalid_client", "client authentication failed", 401));
        }
    }
    Ok(c)
}

// ---------- Subjects ----------

/// Who a person can sign in as: themselves, or one of their agents.
pub async fn subjects_for(account_id: &str) -> OResult<Vec<Value>> {
    let account = account_id.to_lowercase();
    let mine: Vec<Agent> = sqlx::query_as("select * from agents where owner_account_id = $1").bind(&account).fetch_all(db()).await?;
    let mut out = vec![json!({ "subject": format!("user:{account}"), "kind": "user", "name": "You", "detail": account })];
    for a in mine {
        let detail = match (&a.agent_id_name, &a.agent_id_verified_at) {
            (Some(n), Some(_)) => format!("{n}.ai"),
            _ => a.handle.clone().unwrap_or_else(|| a.id.clone()),
        };
        out.push(json!({ "subject": format!("agent:{}", a.id), "kind": "agent", "name": a.name, "detail": detail, "icon": a.icon }));
    }
    Ok(out)
}

async fn check_subject(account_id: &str, subject: &str) -> OResult<()> {
    if subject == format!("user:{}", account_id.to_lowercase()) {
        return Ok(());
    }
    if let Some(id) = subject.strip_prefix("agent:") {
        let owner: Option<String> = sqlx::query_scalar("select owner_account_id from agents where id = $1").bind(id).fetch_optional(db()).await?;
        if owner.is_some_and(|o| o.to_lowercase() == account_id.to_lowercase()) {
            return Ok(());
        }
    }
    Err(err("access_denied", "you can only sign in as yourself or one of your agents", 403))
}

/// The claims a subject releases for `scope`.
pub async fn claims_for(subject: &str, scope: &[String]) -> OResult<Map<String, Value>> {
    let has = |s: &str| scope.iter().any(|x| x == s);
    let mut out = Map::new();
    out.insert("sub".into(), json!(subject));
    let root = std::env::var("ORVYN_ROOT_DOMAIN").unwrap_or_else(|_| "orvyn.cash".into()).to_lowercase();
    if let Some(account) = subject.strip_prefix("user:") {
        out.insert("orvyn_subject_type".into(), json!("user"));
        let u: Option<(Option<String>, Option<String>)> = sqlx::query_as("select social_username, email from users where account_id = $1").bind(account).fetch_optional(db()).await?;
        if has("profile") {
            out.insert("name".into(), json!(u.as_ref().and_then(|u| u.0.clone())));
        }
        if let (true, Some(email)) = (has("email"), u.as_ref().and_then(|u| u.1.clone())) {
            out.insert("email".into(), json!(email));
            out.insert("email_verified".into(), json!(true));
        }
        if has("wallet") {
            out.insert("wallet_address".into(), json!(account));
        }
        return Ok(out);
    }
    let a: Option<Agent> = sqlx::query_as("select * from agents where id = $1").bind(subject.strip_prefix("agent:").unwrap_or("")).fetch_optional(db()).await?;
    let a = a.ok_or_else(|| err("invalid_grant", "that agent no longer exists", 400))?;
    out.insert("orvyn_subject_type".into(), json!("agent"));
    if has("profile") {
        out.insert("name".into(), json!(a.name));
        out.insert("preferred_username".into(), json!(a.handle));
        out.insert("picture".into(), json!(a.icon.as_ref().filter(|i| i.starts_with("http"))));
        out.insert("profile".into(), json!(a.handle.as_ref().map(|h| format!("https://{h}.{root}"))));
    }
    if let (true, Some(h)) = (has("email"), &a.handle) {
        let domain = std::env::var("ORVYN_MAIL_DOMAIN").unwrap_or(root.clone());
        out.insert("email".into(), json!(format!("{h}@{domain}")));
        out.insert("email_verified".into(), json!(true));
    }
    if has("wallet") {
        out.insert("wallet_address".into(), json!(a.evm_address.to_lowercase()));
    }
    if has("agent") {
        let e = crate::environment::public_environment(&a);
        out.insert("agent_id_name".into(), if crate::agentid::is_premium(&a) { json!(format!("{}.ai", a.agent_id_name.clone().unwrap_or_default())) } else { Value::Null });
        out.insert("erc8004_agent_id".into(), json!(a.erc8004_agent_id));
        out.insert("agent_status".into(), json!(a.status));
        // Where and on what the agent runs (compact: the full record is in its OrvynID).
        out.insert("agent_environment".into(), json!({ "hosting": e["hosting"], "tier": e["tier"], "hardware": e["hardware"], "hardware_source": e["hardwareSource"], "attested": e["attested"], "hash": e["hash"], "summary": e["summary"] }));
    }
    Ok(out)
}

// ---------- Authorization (code + PKCE) ----------

#[derive(Debug, Clone)]
pub struct AuthorizeRequest {
    pub client: Value,
    pub client_id: String,
    pub redirect_uri: String,
    pub scope: Vec<String>,
    pub state: Option<String>,
    pub code_challenge: String,
    pub nonce: Option<String>,
}

fn dedupe(list: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for s in list {
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

fn split_scope(s: &str) -> Vec<String> {
    s.split_whitespace().map(String::from).collect()
}

/// Validate an authorization request. Errors about the client or redirect URI must be shown to the
/// person (never redirected); the rest can go back to the client.
pub async fn parse_authorize_request(params: &BTreeMap<String, String>) -> OResult<AuthorizeRequest> {
    let get = |k: &str| params.get(k).cloned();
    let c = active_client(get("client_id").as_deref()).await?;
    let redirect_uri = get("redirect_uri").unwrap_or_default();
    if !strings(&c.redirect_uris).contains(&redirect_uri) {
        return Err(err("invalid_redirect_uri", "this redirect URI isn't registered for the app", 400));
    }
    if get("response_type").as_deref() != Some("code") {
        return Err(err("unsupported_response_type", "only response_type=code is supported", 400));
    }
    let challenge = get("code_challenge").unwrap_or_default();
    let pkce_ok = (43..=128).contains(&challenge.len()) && challenge.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if get("code_challenge_method").as_deref() != Some("S256") || !pkce_ok {
        return Err(err("invalid_request", "PKCE with code_challenge_method=S256 is required", 400));
    }
    let scope = split_scope(&get("scope").unwrap_or_else(|| "openid".into()));
    if scope.iter().any(|s| !known_scope(s)) {
        return Err(err("invalid_scope", "unknown scope", 400));
    }
    let registered = strings(&c.scopes);
    if scope.iter().any(|s| !registered.contains(s) && s != "openid" && s != "offline_access") {
        return Err(err("invalid_scope", "the app isn't registered for that scope", 400));
    }
    Ok(AuthorizeRequest { client: client_view(&c), client_id: c.id, redirect_uri, scope: dedupe(scope), state: get("state"), code_challenge: challenge, nonce: get("nonce") })
}

async fn upsert_grant(client_id: &str, account_id: &str, subject: &str, scope: &[String]) -> OResult<OauthGrant> {
    Ok(sqlx::query_as(
        "insert into oauth_grants (client_id, account_id, subject, scope) values ($1, $2, $3, $4)
         on conflict (client_id, account_id, subject) do update set scope = excluded.scope, revoked_at = null returning *",
    )
    .bind(client_id)
    .bind(account_id.to_lowercase())
    .bind(subject)
    .bind(scope.join(" "))
    .fetch_one(db())
    .await?)
}

fn redirect_with(uri: &str, params: &[(&str, Option<String>)]) -> String {
    let mut u = url::Url::parse(uri).expect("checked when registered");
    // URLSearchParams.set: replace an existing value, or append.
    let mut pairs: Vec<(String, String)> = u.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    for (k, v) in params {
        let Some(v) = v else { continue };
        match pairs.iter().position(|(pk, _)| pk == k) {
            Some(i) => {
                pairs[i].1 = v.clone();
                let mut seen = false;
                pairs.retain(|(pk, _)| {
                    if pk != k {
                        return true;
                    }
                    let keep = !seen;
                    seen = true;
                    keep
                });
            }
            None => pairs.push((k.to_string(), v.clone())),
        }
    }
    if pairs.is_empty() {
        u.set_query(None);
    } else {
        u.query_pairs_mut().clear().extend_pairs(pairs);
    }
    u.to_string()
}

/// The person approved: returns the URL to send them back to the client with a code.
pub async fn approve_authorization(account_id: &str, req: &AuthorizeRequest, subject: &str) -> OResult<String> {
    check_subject(account_id, subject).await?;
    let grant = upsert_grant(&req.client_id, account_id, subject, &req.scope).await?;
    let code = token(32);
    sqlx::query(
        "insert into oauth_codes (code_hash, client_id, grant_id, subject, scope, redirect_uri, code_challenge, nonce, expires_at) values ($1, $2, $3, $4, $5, $6, $7, $8, now() + make_interval(secs => $9))",
    )
    .bind(sha256(&code))
    .bind(&req.client_id)
    .bind(grant.id)
    .bind(subject)
    .bind(req.scope.join(" "))
    .bind(&req.redirect_uri)
    .bind(&req.code_challenge)
    .bind(&req.nonce)
    .bind(CODE_TTL_MS as f64 / 1000.0)
    .execute(db())
    .await?;
    Ok(redirect_with(&req.redirect_uri, &[("code", Some(code)), ("state", req.state.clone()), ("iss", Some(issuer()))]))
}

pub fn deny_authorization(req: &AuthorizeRequest) -> String {
    redirect_with(&req.redirect_uri, &[("error", Some("access_denied".into())), ("state", req.state.clone()), ("iss", Some(issuer()))])
}

// ---------- Tokens ----------

async fn issue_tokens(client: &OauthClient, grant_id: i32, subject: &str, scope: &[String], nonce: Option<&str>, family_id: Option<&str>) -> OResult<Value> {
    let iat = now_s();
    let claims = claims_for(subject, scope).await?;
    let scope_str = scope.join(" ");
    let mut at = Map::new();
    for (k, v) in [
        ("iss", json!(issuer())),
        ("sub", json!(subject)),
        ("aud", json!(client.id)),
        ("client_id", json!(client.id)),
        ("scope", json!(scope_str)),
        ("gid", json!(grant_id)),
        ("iat", json!(iat)),
        ("exp", json!(iat + ACCESS_TTL)),
        ("jti", json!(token(12))),
    ] {
        at.insert(k.into(), v);
    }
    let mut out = json!({ "access_token": jwt::sign_jwt(&at, "at+jwt")?, "token_type": "Bearer", "expires_in": ACCESS_TTL, "scope": scope_str });
    if scope.iter().any(|s| s == "openid") {
        let mut id = claims.clone();
        id.insert("iss".into(), json!(issuer()));
        id.insert("aud".into(), json!(client.id));
        id.insert("iat".into(), json!(iat));
        id.insert("exp".into(), json!(iat + ID_TTL));
        id.insert("auth_time".into(), json!(iat));
        if let Some(n) = nonce {
            id.insert("nonce".into(), json!(n));
        }
        out["id_token"] = json!(jwt::sign_jwt(&id, "JWT")?);
    }
    if scope.iter().any(|s| s == "offline_access") {
        let refresh = token(32);
        sqlx::query(
            "insert into oauth_refresh_tokens (token_hash, family_id, client_id, grant_id, subject, scope, expires_at) values ($1, $2, $3, $4, $5, $6, now() + make_interval(secs => $7))",
        )
        .bind(sha256(&refresh))
        .bind(family_id.map(String::from).unwrap_or_else(|| token(12)))
        .bind(&client.id)
        .bind(grant_id)
        .bind(subject)
        .bind(&scope_str)
        .bind(REFRESH_TTL_MS as f64 / 1000.0)
        .execute(db())
        .await?;
        out["refresh_token"] = json!(refresh);
    }
    Ok(out)
}

async fn live_grant(grant_id: i64) -> OResult<OauthGrant> {
    let g: Option<OauthGrant> = sqlx::query_as("select * from oauth_grants where id = $1").bind(grant_id as i32).fetch_optional(db()).await?;
    g.filter(|g| g.revoked_at.is_none()).ok_or_else(|| err("invalid_grant", "access was revoked", 400))
}

fn expired(t: &chrono::DateTime<chrono::Utc>) -> bool {
    t.timestamp_millis() < chrono::Utc::now().timestamp_millis()
}

/// The token endpoint: form parameters plus the client's credentials (from Basic auth).
pub async fn token_request(form: &BTreeMap<String, String>, client_id: Option<&str>, client_secret: Option<&str>) -> OResult<Value> {
    let get = |k: &str| form.get(k).map(String::as_str);
    let client = authenticate_client(client_id.or(get("client_id")), client_secret.or(get("client_secret"))).await?;
    match get("grant_type") {
        Some("authorization_code") => {
            let hash = sha256(get("code").unwrap_or(""));
            // Claim the code atomically: a second use fails (and revokes what the first issued).
            let row: Option<OauthCode> = sqlx::query_as("update oauth_codes set used_at = now() where code_hash = $1 and used_at is null returning *").bind(&hash).fetch_optional(db()).await?;
            let Some(row) = row else {
                let used: Option<OauthCode> = sqlx::query_as("select * from oauth_codes where code_hash = $1").bind(&hash).fetch_optional(db()).await?;
                if let Some(u) = used {
                    revoke_grant_by_id(u.grant_id).await?;
                }
                return Err(err("invalid_grant", "invalid or used authorization code", 400));
            };
            if row.client_id != client.id {
                return Err(err("invalid_grant", "the code was issued to another client", 400));
            }
            if expired(&row.expires_at) {
                return Err(err("invalid_grant", "the code expired", 400));
            }
            if get("redirect_uri") != Some(row.redirect_uri.as_str()) {
                return Err(err("invalid_grant", "redirect_uri doesn't match", 400));
            }
            let verifier = get("code_verifier").unwrap_or("");
            let ok = (43..=128).contains(&verifier.len()) && verifier.bytes().all(|b| b.is_ascii_alphanumeric() || b"._~-".contains(&b)) && sha256(verifier) == row.code_challenge;
            if !ok {
                return Err(err("invalid_grant", "PKCE verification failed", 400));
            }
            live_grant(row.grant_id as i64).await?;
            issue_tokens(&client, row.grant_id, &row.subject, &split_scope(&row.scope), row.nonce.as_deref(), None).await
        }
        Some("refresh_token") => {
            let row: Option<OauthRefreshToken> = sqlx::query_as("select * from oauth_refresh_tokens where token_hash = $1").bind(sha256(get("refresh_token").unwrap_or(""))).fetch_optional(db()).await?;
            let row = row.filter(|r| r.client_id == client.id).ok_or_else(|| err("invalid_grant", "invalid refresh token", 400))?;
            if row.revoked_at.is_some() || expired(&row.expires_at) {
                return Err(err("invalid_grant", "the refresh token expired or was revoked", 400));
            }
            // Check the request before using up the token, so a bad request doesn't burn it.
            let mut scope = split_scope(&row.scope);
            if let Some(narrowed) = get("scope").filter(|s| !s.is_empty()) {
                let asked = split_scope(narrowed);
                if asked.iter().any(|s| !scope.contains(s)) {
                    return Err(err("invalid_scope", "can't widen scope on refresh", 400));
                }
                scope = if asked.iter().any(|s| s == "offline_access") { asked } else { asked.into_iter().chain(["offline_access".to_string()]).collect() };
            }
            let claimed = sqlx::query("update oauth_refresh_tokens set used_at = now() where id = $1 and used_at is null").bind(row.id).execute(db()).await?.rows_affected();
            if claimed == 0 {
                // Replay of a rotated token: someone else has it. Revoke the whole family.
                sqlx::query("update oauth_refresh_tokens set revoked_at = now() where family_id = $1").bind(&row.family_id).execute(db()).await?;
                return Err(err("invalid_grant", "refresh token reuse detected; signed out everywhere for this app", 400));
            }
            live_grant(row.grant_id as i64).await?;
            issue_tokens(&client, row.grant_id, &row.subject, &scope, None, Some(&row.family_id)).await
        }
        Some("urn:ietf:params:oauth:grant-type:device_code") => {
            let hash = sha256(get("device_code").unwrap_or(""));
            let d: Option<OauthDeviceCode> = sqlx::query_as("select * from oauth_device_codes where device_code_hash = $1").bind(&hash).fetch_optional(db()).await?;
            let d = d.filter(|d| d.client_id == client.id).ok_or_else(|| err("invalid_grant", "unknown device code", 400))?;
            if expired(&d.expires_at) {
                return Err(err("expired_token", "the device code expired", 400));
            }
            let polled_recently = d.last_polled_at.is_some_and(|t| chrono::Utc::now().timestamp_millis() - t.timestamp_millis() < DEVICE_INTERVAL * 1000);
            sqlx::query("update oauth_device_codes set last_polled_at = now() where device_code_hash = $1").bind(&hash).execute(db()).await?;
            if polled_recently {
                return Err(err("slow_down", "poll less often", 400));
            }
            match d.status.as_str() {
                "pending" => return Err(err("authorization_pending", "waiting for the person to approve", 400)),
                "denied" => return Err(err("access_denied", "the person declined", 400)),
                _ => {}
            }
            let used = sqlx::query("update oauth_device_codes set status = 'used' where device_code_hash = $1 and status = 'approved'").bind(&hash).execute(db()).await?.rows_affected();
            let (Some(grant_id), Some(subject)) = (d.grant_id.filter(|_| used > 0), d.subject.clone()) else {
                return Err(err("invalid_grant", "the device code was already used", 400));
            };
            live_grant(grant_id as i64).await?;
            issue_tokens(&client, grant_id, &subject, &split_scope(&d.scope), None, None).await
        }
        _ => Err(err("unsupported_grant_type", "grant_type must be authorization_code, refresh_token or device_code", 400)),
    }
}

// ---------- Device authorization ----------

const USER_CODE_ALPHABET: &[u8] = b"BCDFGHJKLMNPQRSTVWXZ"; // no vowels or look-alikes

fn user_code() -> String {
    let bytes: [u8; 8] = rand::random();
    let chars: String = bytes.iter().map(|b| USER_CODE_ALPHABET[*b as usize % USER_CODE_ALPHABET.len()] as char).collect();
    format!("{}-{}", &chars[..4], &chars[4..])
}

pub async fn start_device_authorization(client_id: Option<&str>, scope_param: Option<&str>) -> OResult<Value> {
    let c = active_client(client_id).await?;
    let registered = strings(&c.scopes);
    let scope = split_scope(scope_param.unwrap_or("openid"));
    if scope.iter().any(|s| !known_scope(s) || (!registered.contains(s) && s != "openid" && s != "offline_access")) {
        return Err(err("invalid_scope", "unknown or unregistered scope", 400));
    }
    let device = token(32);
    let code = user_code();
    sqlx::query("insert into oauth_device_codes (device_code_hash, user_code, client_id, scope, expires_at) values ($1, $2, $3, $4, now() + make_interval(secs => $5))")
        .bind(sha256(&device))
        .bind(&code)
        .bind(&c.id)
        .bind(dedupe(scope).join(" "))
        .bind(DEVICE_TTL_MS as f64 / 1000.0)
        .execute(db())
        .await?;
    Ok(json!({
        "device_code": device,
        "user_code": code,
        "verification_uri": format!("{}/device", app_url()),
        "verification_uri_complete": format!("{}/device?code={code}", app_url()),
        "expires_in": DEVICE_TTL_MS / 1000,
        "interval": DEVICE_INTERVAL,
    }))
}

/// What the /device page shows for a code the person typed: (user code, client view, scope).
pub async fn lookup_user_code(code: &str) -> OResult<(String, Value, Vec<String>)> {
    let normalized: String = code.trim().to_uppercase().chars().filter(|c| c.is_ascii_uppercase()).collect();
    let formatted = format!("{}-{}", normalized.chars().take(4).collect::<String>(), normalized.chars().skip(4).take(4).collect::<String>());
    let d: Option<OauthDeviceCode> = sqlx::query_as("select * from oauth_device_codes where user_code = $1").bind(&formatted).fetch_optional(db()).await?;
    let d = d.filter(|d| d.status == "pending" && !expired(&d.expires_at)).ok_or_else(|| err("invalid_request", "that code isn't valid (it may have expired)", 404))?;
    let c = active_client(Some(&d.client_id)).await?;
    Ok((d.user_code, client_view(&c), split_scope(&d.scope)))
}

pub async fn decide_device(account_id: &str, code: &str, approve: bool, subject: Option<&str>) -> OResult<Value> {
    let (uc, client, scope) = lookup_user_code(code).await?;
    if !approve {
        sqlx::query("update oauth_device_codes set status = 'denied' where user_code = $1").bind(&uc).execute(db()).await?;
        return Ok(json!({ "approved": false }));
    }
    let subject = subject.map(String::from).unwrap_or_else(|| format!("user:{}", account_id.to_lowercase()));
    check_subject(account_id, &subject).await?;
    let grant = upsert_grant(client["id"].as_str().unwrap_or(""), account_id, &subject, &scope).await?;
    let done = sqlx::query("update oauth_device_codes set status = 'approved', grant_id = $2, subject = $3 where user_code = $1 and status = 'pending'").bind(&uc).bind(grant.id).bind(&subject).execute(db()).await?.rows_affected();
    if done == 0 {
        return Err(err("invalid_request", "that code was already used", 400));
    }
    Ok(json!({ "approved": true, "client": client["name"] }))
}

// ---------- Userinfo, revocation, connected apps ----------

fn access_claims(access_token: &str) -> OResult<Map<String, Value>> {
    let claims = verify_jwt(access_token, Some("at+jwt")).filter(|c| c.get("iss").and_then(Value::as_str) == Some(issuer().as_str()));
    claims.ok_or_else(|| err("invalid_token", "invalid or expired access token", 401))
}

async fn grant_alive(claims: &Map<String, Value>) -> OResult<()> {
    let gid = claims.get("gid").and_then(Value::as_f64).unwrap_or(-1.0) as i64;
    live_grant(gid).await.map(|_| ()).map_err(|_| err("invalid_token", "access was revoked", 401))
}

/// Claims for a valid access token (Bearer).
pub async fn userinfo(access_token: &str) -> OResult<Map<String, Value>> {
    let claims = access_claims(access_token)?;
    // A disconnected app's access tokens stop working here at once.
    grant_alive(&claims).await?;
    let sub = claims.get("sub").and_then(Value::as_str).unwrap_or("");
    claims_for(sub, &split_scope(claims.get("scope").and_then(Value::as_str).unwrap_or(""))).await
}

/// RFC 7009: revoke a refresh token (and its family). Access tokens simply expire.
pub async fn revoke_token(token_value: &str, client_id: Option<&str>, client_secret: Option<&str>) -> OResult<()> {
    let client = authenticate_client(client_id, client_secret).await?;
    let row: Option<OauthRefreshToken> = sqlx::query_as("select * from oauth_refresh_tokens where token_hash = $1").bind(sha256(token_value)).fetch_optional(db()).await?;
    if let Some(r) = row.filter(|r| r.client_id == client.id) {
        sqlx::query("update oauth_refresh_tokens set revoked_at = now() where family_id = $1").bind(&r.family_id).execute(db()).await?;
    }
    Ok(())
}

async fn revoke_grant_by_id(grant_id: i32) -> OResult<()> {
    sqlx::query("update oauth_grants set revoked_at = now() where id = $1").bind(grant_id).execute(db()).await?;
    sqlx::query("update oauth_refresh_tokens set revoked_at = now() where grant_id = $1").bind(grant_id).execute(db()).await?;
    Ok(())
}

/// Apps a person has connected (to themselves or their agents).
pub async fn connected_apps(account_id: &str) -> OResult<Vec<Value>> {
    let grants: Vec<OauthGrant> = sqlx::query_as("select * from oauth_grants where account_id = $1 and revoked_at is null").bind(account_id.to_lowercase()).fetch_all(db()).await?;
    let mut out = vec![];
    for g in grants {
        let c: Option<OauthClient> = sqlx::query_as("select * from oauth_clients where id = $1").bind(&g.client_id).fetch_optional(db()).await?;
        let Some(c) = c else { continue };
        out.push(json!({ "id": g.id, "subject": g.subject, "scope": split_scope(&g.scope), "connectedAt": g.created_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true), "client": client_view(&c) }));
    }
    Ok(out)
}

/// Disconnect an app: refresh tokens and userinfo stop working at once; services that verify access
/// tokens offline (JWKS) see them expire within 15 minutes.
pub async fn disconnect_app(account_id: &str, grant_id: i32) -> OResult<()> {
    let g: Option<OauthGrant> = sqlx::query_as("select * from oauth_grants where id = $1 and account_id = $2").bind(grant_id).bind(account_id.to_lowercase()).fetch_optional(db()).await?;
    if g.is_none() {
        return Err(err("invalid_request", "no such connection", 404));
    }
    revoke_grant_by_id(grant_id).await
}

/// Check a Bearer access token for an API call: valid, not revoked, holding `scope`. Returns
/// (subject, client id).
pub async fn authenticate_access(access_token: &str, scope: &str) -> OResult<(String, String)> {
    let claims = access_claims(access_token)?;
    if !split_scope(claims.get("scope").and_then(Value::as_str).unwrap_or("")).iter().any(|s| s == scope) {
        return Err(err("insufficient_scope", format!("this needs the {scope} scope"), 403));
    }
    grant_alive(&claims).await?;
    Ok((claims.get("sub").and_then(Value::as_str).unwrap_or("").to_string(), claims.get("client_id").and_then(Value::as_str).unwrap_or("").to_string()))
}
