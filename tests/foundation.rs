//! Settings, secrets (byte-compatible with the TypeScript core) and migrations (drizzle-compatible).
mod support;

use orvyn_core::db::migrate::{migrate, migrations};
use orvyn_core::env::Env;
use orvyn_core::secrets::{decrypt_with, encrypt_with, secret_hint, SecretKind};
use serde_json::Value;
use std::collections::HashMap;

fn kind(k: &str) -> SecretKind {
    if k == "ai-key" { SecretKind::AiKey } else { SecretKind::ShieldedKey }
}

#[test]
fn secrets_sealed_by_typescript_open_in_rust() {
    let v: Value = serde_json::from_str(include_str!("vectors/secrets.json")).unwrap();
    let secret = v["secret"].as_str().unwrap();
    for c in v["sealed"].as_array().unwrap() {
        let (plain, aad, k, payload) = (c["plaintext"].as_str().unwrap(), c["aad"].as_str().unwrap(), kind(c["kind"].as_str().unwrap()), c["payload"].as_str().unwrap());
        assert_eq!(decrypt_with(secret, payload, aad, k).unwrap(), plain);
        // Bound to its owner and its kind.
        assert!(decrypt_with(secret, payload, "someone-else", k).is_err());
        let other = if k == SecretKind::AiKey { SecretKind::ShieldedKey } else { SecretKind::AiKey };
        assert!(decrypt_with(secret, payload, aad, other).is_err());
    }
    for h in v["hints"].as_array().unwrap() {
        assert_eq!(secret_hint(h["secret"].as_str().unwrap()), h["hint"].as_str().unwrap());
    }
}

#[test]
fn secrets_round_trip_and_refuse_tampering() {
    let s = "another-secret-0123456789abcdef0123456789";
    let sealed = encrypt_with(s, "sk-test-key", "agent_9", SecretKind::AiKey).unwrap();
    assert!(sealed.starts_with("v1:"));
    assert_eq!(decrypt_with(s, &sealed, "agent_9", SecretKind::AiKey).unwrap(), "sk-test-key");
    let mut parts: Vec<String> = sealed.split(':').map(String::from).collect();
    parts[3] = parts[3].chars().rev().collect();
    assert!(decrypt_with(s, &parts.join(":"), "agent_9", SecretKind::AiKey).is_err());
    assert!(decrypt_with(s, "v2:a:b:c", "agent_9", SecretKind::AiKey).is_err());
    assert!(decrypt_with("wrong-secret-0123456789abcdef0123456789", &sealed, "agent_9", SecretKind::AiKey).is_err());
}

#[test]
fn settings_parse_like_the_typescript_core() {
    let vars = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<HashMap<_, _>>();
    let base = [("DATABASE_URL", "postgres://u@localhost/db"), ("AI_KEY_ENCRYPTION_SECRET", support::AI_SECRET), ("SESSION_SECRET", support::SESSION_SECRET)];
    let e = Env::parse(&vars(&base)).unwrap();
    assert_eq!(e.database_pool_size, 5);
    assert_eq!(e.orvyn_api_url, "http://localhost:3100");
    assert_eq!(e.fourmeme_token_manager, "0x5c952063c7fc8610FFDB798152D69F0B9550762b");
    assert_eq!(e.billing_renew_days, 3.0);
    assert!(!e.erc8004_enabled);

    // Empty values count as unset; bad ones are all reported.
    let mut bad = base.to_vec();
    bad.extend([("ORVYN_STUDIO_KEY", "short"), ("ORVYN_REGISTRY_CONTRACT", "0x123"), ("BILLING_RENEW_DAYS", "90"), ("HOSTING_MODE", "cloud"), ("PRIVY_APP_ID", "")]);
    let err = Env::parse(&vars(&bad)).unwrap_err();
    for k in ["ORVYN_STUDIO_KEY", "ORVYN_REGISTRY_CONTRACT", "BILLING_RENEW_DAYS", "HOSTING_MODE"] {
        assert!(err.contains(k), "{err}");
    }
    assert!(Env::parse(&vars(&[("DATABASE_URL", "postgres://u@localhost/db")])).unwrap_err().contains("SESSION_SECRET"));
}

#[tokio::test]
async fn migrations_apply_once_with_drizzles_bookkeeping() {
    let pool = support::fresh_db().await;
    // fresh_db migrated it: everything is recorded, with drizzle's hashes and times.
    let rows: Vec<(String, i64)> = sqlx::query_as("select hash, created_at from drizzle.__drizzle_migrations order by id").fetch_all(&pool).await.unwrap();
    assert_eq!(rows.len(), migrations().len());
    for (row, m) in rows.iter().zip(migrations()) {
        assert_eq!(row, &(m.hash(), m.when));
    }
    // Running again applies nothing.
    assert!(migrate(&pool).await.unwrap().is_empty());
    // The tables are there.
    let n: i64 = sqlx::query_scalar("select count(*) from information_schema.tables where table_schema = 'public'").fetch_one(&pool).await.unwrap();
    assert!(n >= 50, "{n} tables");
}

#[tokio::test]
async fn rows_load_into_their_types() {
    let pool = support::fresh_db().await;
    sqlx::query(
        "insert into agents (id, name, symbol, owner_account_id, ai_provider, ai_model, ai_key_encrypted, ai_key_hint, evm_address, evm_wallet)
         values ('a1', 'Nova', 'NOVA', '0xabc', 'anthropic', 'claude', 'v1:x:y:z', 'sk-…1234', '0xdef', '{}')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let a: orvyn_core::db::Agent = sqlx::query_as("select * from agents where id = 'a1'").fetch_one(&pool).await.unwrap();
    assert_eq!((a.name.as_str(), a.tier.as_str(), a.status.as_str(), a.launchpad.as_str()), ("Nova", "basic", "awaiting_funds", "fourmeme"));
    let json = serde_json::to_value(&a).unwrap();
    assert_eq!(json["ownerAccountId"], "0xabc");
    assert_eq!(json["autopilotIntervalMin"], 60);
}
