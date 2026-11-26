//! Orvyn OAuth's signing key and JWTs (ES256, P-256).
//!
//! ORVYN_OAUTH_SIGNING_KEY is a PKCS#8 PEM (or its base64) EC P-256 private key:
//!   openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 | base64 -w0
//! Without it, development gets a throwaway key per process (tokens die on restart); production
//! refuses to start OAuth. The public half is served as a JWKS so anyone can verify Orvyn tokens.
//! Tokens signed by the TypeScript core with the same key verify here, and the other way round.

use std::sync::RwLock;

use base64::engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD as B64URL};
use base64::Engine as _;
use p256::ecdsa::signature::{Signer, Verifier};
use p256::ecdsa::{Signature, SigningKey, VerifyingKey};
use p256::pkcs8::DecodePrivateKey;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

pub struct Key {
    pub signing: SigningKey,
    pub verifying: VerifyingKey,
    pub kid: String,
    x: String,
    y: String,
}

static KEY: RwLock<Option<std::sync::Arc<Key>>> = RwLock::new(None);

fn load() -> Result<Key> {
    let raw = std::env::var("ORVYN_OAUTH_SIGNING_KEY").unwrap_or_default().trim().to_string();
    let signing = if !raw.is_empty() {
        let pem = if raw.contains("BEGIN") { raw } else { String::from_utf8(B64.decode(raw.as_bytes()).map_err(|_| Error::unavailable("ORVYN_OAUTH_SIGNING_KEY isn't PEM or base64 PEM"))?).map_err(|_| Error::unavailable("bad ORVYN_OAUTH_SIGNING_KEY"))? };
        SigningKey::from_pkcs8_pem(&pem).map_err(|_| Error::unavailable("ORVYN_OAUTH_SIGNING_KEY must be an EC P-256 key"))?
    } else {
        if std::env::var("NODE_ENV").as_deref() == Ok("production") || std::env::var("ORVYN_ENV").as_deref() == Ok("production") {
            return Err(Error::unavailable("ORVYN_OAUTH_SIGNING_KEY is required in production"));
        }
        SigningKey::random(&mut p256::elliptic_curve::rand_core::OsRng)
    };
    let verifying = *signing.verifying_key();
    let point = verifying.to_encoded_point(false);
    let (x, y) = (B64URL.encode(point.x().unwrap()), B64URL.encode(point.y().unwrap()));
    // RFC 7638 thumbprint as the key id.
    let thumb = format!("{{\"crv\":\"P-256\",\"kty\":\"EC\",\"x\":\"{x}\",\"y\":\"{y}\"}}");
    let kid = B64URL.encode(Sha256::digest(thumb.as_bytes()));
    Ok(Key { signing, verifying, kid, x, y })
}

pub fn signing_key() -> Result<std::sync::Arc<Key>> {
    if let Some(k) = KEY.read().unwrap().clone() {
        return Ok(k);
    }
    let k = std::sync::Arc::new(load()?);
    *KEY.write().unwrap() = Some(k.clone());
    Ok(k)
}

/// Tests only: load the key again (after changing ORVYN_OAUTH_SIGNING_KEY).
pub fn reset_signing_key() {
    *KEY.write().unwrap() = None;
}

pub fn jwks() -> Result<Value> {
    let k = signing_key()?;
    Ok(json!({ "keys": [{ "kty": "EC", "crv": "P-256", "x": k.x, "y": k.y, "kid": k.kid, "use": "sig", "alg": "ES256" }] }))
}

/// Sign claims as a compact JWT. The header is written as the TypeScript core writes it.
pub fn sign_jwt(claims: &Map<String, Value>, typ: &str) -> Result<String> {
    let k = signing_key()?;
    let header = format!("{{\"alg\":\"ES256\",\"typ\":{},\"kid\":{}}}", serde_json::to_string(typ)?, serde_json::to_string(&k.kid)?);
    let input = format!("{}.{}", B64URL.encode(header), B64URL.encode(serde_json::to_string(claims)?));
    let sig: Signature = k.signing.sign(input.as_bytes());
    Ok(format!("{input}.{}", B64URL.encode(sig.to_bytes())))
}

/// Verify an Orvyn-signed JWT; returns its claims, or None if invalid or expired.
pub fn verify_jwt(token: &str, expected_type: Option<&str>) -> Option<Map<String, Value>> {
    let k = signing_key().ok()?;
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let header: Value = serde_json::from_slice(&B64URL.decode(parts[0].trim_end_matches('=')).ok()?).ok()?;
    if header["alg"] != "ES256" || header["kid"] != k.kid.as_str() {
        return None;
    }
    if expected_type.is_some_and(|t| header["typ"] != t) {
        return None;
    }
    let sig = Signature::from_slice(&B64URL.decode(parts[2].trim_end_matches('=')).ok()?).ok()?;
    k.verifying.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig).ok()?;
    let claims: Map<String, Value> = serde_json::from_slice(&B64URL.decode(parts[1].trim_end_matches('=')).ok()?).ok()?;
    if claims.get("exp").and_then(Value::as_f64).is_some_and(|exp| exp * 1000.0 < chrono::Utc::now().timestamp_millis() as f64) {
        return None;
    }
    Some(claims)
}
