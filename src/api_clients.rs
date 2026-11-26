//! Who is calling Orvyn's developer API with a key. Orvyn's own runtime uses ORVYN_INTERNAL_API_KEY.
//! Outside apps are API clients: ORVYN_API_KEYS lists them as "<client id>:<key>"; a key raises
//! their rate limits. The id always comes from the key, never from a request.

use sha2::{Digest, Sha256};

use crate::env::env;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiCaller {
    Internal { key: String },
    Client { key: String, client_id: String },
}

impl ApiCaller {
    fn key(&self) -> &str {
        match self {
            ApiCaller::Internal { key } | ApiCaller::Client { key, .. } => key,
        }
    }
}

pub fn valid_client_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Client id for a bare key (no "<id>:" prefix): derived from the key, stable.
fn derived_id(key: &str) -> String {
    format!("k{}", &hex::encode(Sha256::digest(key.as_bytes()))[..12])
}

pub fn api_callers() -> Vec<ApiCaller> {
    let e = env();
    let mut callers = vec![];
    if let Some(internal) = e.orvyn_internal_api_key.as_deref().map(str::trim).filter(|k| k.len() >= 16) {
        callers.push(ApiCaller::Internal { key: internal.into() });
    }
    for entry in e.orvyn_api_keys.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (client_id, key) = match entry.find(':').filter(|i| *i > 0) {
            Some(at) => (entry[..at].to_string(), entry[at + 1..].trim().to_string()),
            None => (derived_id(entry), entry.to_string()),
        };
        if key.len() >= 16 && valid_client_id(&client_id) {
            callers.push(ApiCaller::Client { key, client_id });
        }
    }
    callers
}

fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The caller for a presented key, or None if it isn't a valid key.
pub fn api_caller_for_key(given: Option<&str>) -> Option<ApiCaller> {
    let given = given.filter(|g| !g.is_empty())?;
    api_callers().into_iter().find(|c| same(c.key(), given))
}

/// ISO 3166-1 alpha-2 country of a request (from the platform's header), or None when unknown.
/// ORVYN_COUNTRY_HEADERS (comma-separated, first present wins) says which headers to trust.
pub fn country_of(header: impl Fn(&str) -> Option<String>) -> Option<String> {
    let configured: Vec<String> = std::env::var("ORVYN_COUNTRY_HEADERS").unwrap_or_default().split(',').map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect();
    let names: Vec<String> = if configured.is_empty() { ["x-vercel-ip-country", "cf-ipcountry", "cloudfront-viewer-country"].iter().map(|s| s.to_string()).collect() } else { configured };
    for name in names {
        let Some(v) = header(&name).map(|v| v.trim().to_uppercase()) else { continue };
        // XX / T1 (Tor) and the like mean unknown.
        if v.len() == 2 && v.bytes().all(|b| b.is_ascii_uppercase()) && v != "XX" && v != "T1" {
            return Some(v);
        }
    }
    None
}
