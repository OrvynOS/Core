//! Privy: people's sign-in (email, social, embedded wallets) and agents' server wallets.
//!
//! Agent wallets are owned by Orvyn's authorization key (PRIVY_AUTHORIZATION_KEY, a P-256 key
//! registered in the Privy dashboard as a key quorum, PRIVY_AUTHORIZATION_KEY_ID): signing needs
//! both the app secret and that key, so the app secret alone can't move agents' funds.
//!
//! This module is the REST client (api.privy.io) and identity-token verification; wallets.rs
//! uses it for agents' wallets.

use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use serde_json::Value;

use crate::env::env;
use crate::error::{Error, Result};
use crate::http::client;

pub fn api_url() -> String {
    std::env::var("PRIVY_API_BASE_URL").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "https://api.privy.io".into())
}

/// The app's id and secret, or an error saying they're not set.
pub fn credentials() -> Result<(&'static str, &'static str)> {
    let e = env();
    match (e.privy_app_id.as_deref(), e.privy_app_secret.as_deref()) {
        (Some(id), Some(secret)) => Ok((id, secret)),
        _ => Err(Error::unavailable("PRIVY_APP_ID and PRIVY_APP_SECRET are not set")),
    }
}

/// A request to Privy's API with the app's basic auth and id.
pub fn request(method: reqwest::Method, path: &str) -> Result<reqwest::RequestBuilder> {
    let (id, secret) = credentials()?;
    Ok(client()
        .request(method, format!("{}{}", api_url(), path))
        .header("authorization", format!("Basic {}", B64.encode(format!("{id}:{secret}"))))
        .header("privy-app-id", id))
}

/// Send a Privy API request and read its JSON (errors carry Privy's message).
pub async fn send(req: reqwest::RequestBuilder) -> Result<Value> {
    let res = req.send().await.map_err(|e| Error::internal(format!("Privy: {e}")))?;
    let status = res.status();
    let body: Value = res.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let msg = body.get("error").and_then(Value::as_str).or_else(|| body.get("message").and_then(Value::as_str)).unwrap_or("request failed");
        return Err(Error::internal(format!("Privy {status}: {msg}")));
    }
    Ok(body)
}

// ---- Identity tokens ----

/// A verified identity token: the user and their linked accounts (the token's own JSON).
#[derive(Debug, Clone)]
pub struct IdentityUser {
    pub id: String,
    pub linked_accounts: Vec<Value>,
}

#[derive(Deserialize)]
struct Claims {
    sub: String,
    linked_accounts: Option<String>,
}

#[derive(Deserialize)]
struct Jwks {
    keys: Vec<jsonwebtoken::jwk::Jwk>,
}

static JWKS: LazyLock<Mutex<Option<(Instant, Vec<jsonwebtoken::jwk::Jwk>)>>> = LazyLock::new(Default::default);

async fn app_keys(app_id: &str, kid: Option<&str>) -> Result<Vec<DecodingKey>> {
    if let Some(pem) = env().privy_verification_key.as_deref() {
        let key = DecodingKey::from_ec_pem(pem.as_bytes()).map_err(|_| Error::internal("PRIVY_VERIFICATION_KEY isn't an EC public key"))?;
        return Ok(vec![key]);
    }
    let cached = JWKS.lock().unwrap().clone().filter(|(at, _)| at.elapsed() < Duration::from_secs(3600));
    let keys = match cached {
        // A key we haven't seen: refetch (Privy rotated), else use the cache.
        Some((_, keys)) if kid.is_none_or(|k| keys.iter().any(|j| j.common.key_id.as_deref() == Some(k))) => keys,
        _ => {
            let res = client()
                .get(format!("{}/v1/apps/{app_id}/jwks.json", api_url()))
                .header("privy-app-id", app_id)
                .send()
                .await
                .map_err(|e| Error::internal(format!("Privy keys: {e}")))?;
            let jwks: Jwks = res.json().await.map_err(|e| Error::internal(format!("Privy keys: {e}")))?;
            *JWKS.lock().unwrap() = Some((Instant::now(), jwks.keys.clone()));
            jwks.keys
        }
    };
    Ok(keys
        .iter()
        .filter(|j| kid.is_none_or(|k| j.common.key_id.as_deref() == Some(k)))
        .filter_map(|j| DecodingKey::from_jwk(j).ok())
        .collect())
}

/// Verify a Privy identity token (ES256, issuer privy.io, audience the app) and read its user.
pub async fn verify_identity_token(token: &str) -> Result<IdentityUser> {
    let (app_id, _) = credentials()?;
    let header = jsonwebtoken::decode_header(token).map_err(|_| Error::unauthorized("Authentication token is invalid"))?;
    if header.alg != Algorithm::ES256 || header.typ.as_deref().is_some_and(|t| t != "JWT") {
        return Err(Error::unauthorized("Authentication token is invalid"));
    }
    let mut validation = Validation::new(Algorithm::ES256);
    validation.set_issuer(&["privy.io"]);
    validation.set_audience(&[app_id]);
    let mut last = Error::unauthorized("Failed to verify authentication token");
    for key in app_keys(app_id, header.kid.as_deref()).await? {
        match jsonwebtoken::decode::<Claims>(token, &key, &validation) {
            Ok(data) => {
                let linked = data.claims.linked_accounts.ok_or_else(|| Error::unauthorized("Unable to parse identity token"))?;
                let linked_accounts: Vec<Value> = serde_json::from_str(&linked).map_err(|_| Error::unauthorized("Unable to parse identity token"))?;
                return Ok(IdentityUser { id: data.claims.sub, linked_accounts });
            }
            Err(e) if matches!(e.kind(), jsonwebtoken::errors::ErrorKind::ExpiredSignature) => {
                return Err(Error::unauthorized("Authentication token expired"));
            }
            Err(_) => last = Error::unauthorized("Authentication token is invalid"),
        }
    }
    Err(last)
}
