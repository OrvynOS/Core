//! drand quicknet (League of Entropy; a round every 3 seconds), verified against the network's
//! pinned key before use, so a mirror can't hand us a number of its choosing.

use std::time::Duration;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

pub const CHAIN: &str = "52db9ba70e0cc0f6eaf7803dd07447a1f5477735fd3f661792ba94600c84e971";
pub const PUBLIC_KEY: &str = "83cf0f2896adee7eb8b5f01fcad3912212c437e0073e911fb90022d3e760183c8c4b450b6a0a6c3ac6a5776a2d1064510d1fec758c921cc22b0e17e63aaf4bcb5ed66304de9cf809bd274ca73bab4af5a6e9c76a4bc09e76eae8991ef5ece45a";
pub const GENESIS: i64 = 1692803367;
pub const PERIOD: i64 = 3;
const MIRRORS: &[&str] = &["https://api.drand.sh", "https://drand.cloudflare.com", "https://api2.drand.sh"];
const DST: &[u8] = b"BLS_SIG_BLS12381G1_XMD:SHA-256_SSWU_RO_NUL_";

pub fn round_at(unix_seconds: f64) -> i64 {
    ((unix_seconds - GENESIS as f64) / PERIOD as f64).floor() as i64 + 1
}

pub fn time_of_round(round: i64) -> i64 {
    GENESIS + (round - 1) * PERIOD
}

/// True when `signature` is the network's signature for `round`.
pub fn verify_round(round: i64, signature: &str) -> bool {
    let msg = Sha256::digest((round as u64).to_be_bytes());
    super::bls::verify(signature, &msg, DST, PUBLIC_KEY)
}

pub fn randomness_of(signature: &str) -> String {
    hex::encode(Sha256::digest(hex::decode(signature).unwrap_or_default()))
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Beacon {
    pub round: i64,
    pub signature: String,
    pub randomness: String,
    pub at: String,
}

/// A verified beacon round (the latest, or a specific past one).
pub async fn beacon(round: Option<i64>) -> Result<Beacon> {
    let mut last = "no mirror answered".to_string();
    for base in MIRRORS {
        let host = base.trim_start_matches("https://");
        let url = format!("{base}/{CHAIN}/public/{}", round.map(|r| r.to_string()).unwrap_or_else(|| "latest".into()));
        let res = match crate::http::client().get(url).timeout(Duration::from_secs(8)).send().await {
            Ok(r) => r,
            Err(e) => {
                last = e.to_string();
                continue;
            }
        };
        if !res.status().is_success() {
            last = format!("{host} returned {}", res.status().as_u16());
            continue;
        }
        let Ok(r) = res.json::<Value>().await else {
            last = format!("{host} sent bad JSON");
            continue;
        };
        let (Some(got), Some(sig)) = (r["round"].as_i64(), r["signature"].as_str()) else { continue };
        if round.is_some_and(|want| want != got) {
            continue;
        }
        if !verify_round(got, sig) {
            last = format!("{host} sent a round that failed verification");
            continue;
        }
        // The randomness is sha256(signature): computed here, never taken from the mirror.
        return Ok(Beacon { round: got, signature: sig.into(), randomness: randomness_of(sig), at: iso(time_of_round(got)) });
    }
    Err(Error::unavailable(last))
}

pub fn iso(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0).unwrap_or_default().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
