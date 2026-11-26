//! Orvyn OAuth against the TypeScript core (vectors/oauth.json): the same key gives the same JWKS
//! and key id, TypeScript-signed tokens verify here, redirect URI rules and discovery match; and
//! the flows on a throwaway database: code + PKCE, refresh rotation and reuse detection, device
//! authorization, userinfo, revocation.
mod support;

use std::collections::BTreeMap;
use std::sync::OnceLock;

use base64::Engine as _;
use orvyn_core::oauth::{self, jwt};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

fn v() -> Value {
    support::test_env(&[]);
    let v: Value = serde_json::from_str(include_str!("vectors/oauth.json")).unwrap();
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        unsafe {
            std::env::set_var("ORVYN_OAUTH_SIGNING_KEY", v["keyBase64"].as_str().unwrap());
            std::env::set_var("ORVYN_OAUTH_ISSUER", "https://api.test");
            std::env::set_var("ORVYN_APP_URL", "https://app.test");
        }
        jwt::reset_signing_key();
    });
    v
}

fn canon(v: &Value) -> String {
    orvyn_core::environment::canonical_json(v)
}

#[test]
fn keys_tokens_uris_and_discovery_match() {
    let v = v();
    assert_eq!(canon(&oauth::jwks().unwrap()), canon(&v["jwks"]));
    let access = oauth::verify_jwt(v["access"].as_str().unwrap(), Some("at+jwt")).expect("a TypeScript-signed access token verifies");
    assert_eq!(access["sub"], "user:0xabc");
    assert_eq!(access["gid"], 7);
    assert!(oauth::verify_jwt(v["access"].as_str().unwrap(), Some("JWT")).is_none(), "wrong type");
    assert!(oauth::verify_jwt(v["expired"].as_str().unwrap(), Some("at+jwt")).is_none(), "expired");
    assert_eq!(oauth::verify_jwt(v["id"].as_str().unwrap(), None).unwrap()["name"], "Nova");
    let mut tampered = v["access"].as_str().unwrap().to_string();
    tampered.replace_range(tampered.len() - 4.., "AAAA");
    assert!(oauth::verify_jwt(&tampered, None).is_none());
    // Ours verify too, with the same header shape.
    let mut claims = serde_json::Map::new();
    claims.insert("sub".into(), json!("user:0xdef"));
    claims.insert("exp".into(), json!(4102444800i64));
    let mine = jwt::sign_jwt(&claims, "at+jwt").unwrap();
    assert_eq!(mine.split('.').next(), v["access"].as_str().unwrap().split('.').next(), "same header bytes");
    assert_eq!(oauth::verify_jwt(&mine, Some("at+jwt")).unwrap()["sub"], "user:0xdef");
    for u in v["uris"].as_array().unwrap() {
        let got = oauth::check_redirect_uri(u[0].as_str().unwrap()).err().map(|e| Value::from(e.code)).unwrap_or(Value::Null);
        assert_eq!(got, u[1], "{}", u[0]);
    }
    assert_eq!(canon(&oauth::discovery()), canon(&v["discovery"]));
}

fn pkce() -> (String, String) {
    let verifier = format!("{}{}", "a".repeat(20), "B".repeat(30));
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn form(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn query(url: &str) -> BTreeMap<String, String> {
    url::Url::parse(url).unwrap().query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect()
}

#[tokio::test]
async fn code_refresh_device_and_revocation() {
    v();
    let db = support::shared_db().await;
    let owner = "0x1111111111111111111111111111111111111111";
    sqlx::query(
        "insert into agents (id, handle, name, symbol, owner_account_id, ai_provider, ai_model, ai_key_encrypted, ai_key_hint, evm_address, evm_wallet, status)
         values ('a1', 'nova', 'Nova', 'NOVA', $1, 'x', 'x', 'x', 'x', '0x2222222222222222222222222222222222222222', '{}', 'live')",
    )
    .bind(owner)
    .execute(db)
    .await
    .unwrap();

    // A confidential app, registered by its developer.
    let input: oauth::ClientInput = serde_json::from_value(json!({ "name": "Data Co", "type": "confidential", "redirectUris": ["https://data.example/cb?x=1"], "scopes": ["openid", "profile", "agent", "wallet"] })).unwrap();
    let (client, secret) = oauth::register_client("0xdev", &input, false, None).await.unwrap();
    let (id, secret) = (client["id"].as_str().unwrap().to_string(), secret.unwrap());
    let reserved: oauth::ClientInput = serde_json::from_value(json!({ "name": "Sneaky", "type": "public", "redirectUris": ["https://s.example/cb"], "scopes": ["vyn"] })).unwrap();
    assert_eq!(oauth::register_client("0xdev", &reserved, false, None).await.unwrap_err().code, "invalid_client_metadata");

    // Authorize as the agent, with PKCE.
    let (verifier, challenge) = pkce();
    let params = form(&[("client_id", &id), ("redirect_uri", "https://data.example/cb?x=1"), ("response_type", "code"), ("code_challenge", &challenge), ("code_challenge_method", "S256"), ("scope", "openid profile agent wallet offline_access"), ("state", "s1"), ("nonce", "n1")]);
    let req = oauth::parse_authorize_request(&params).await.unwrap();
    let mut bad = params.clone();
    bad.insert("scope".into(), "openid vyn".into());
    assert_eq!(oauth::parse_authorize_request(&bad).await.unwrap_err().code, "invalid_scope");
    assert_eq!(oauth::approve_authorization("0xstranger", &req, "agent:a1").await.unwrap_err().code, "access_denied");
    let back = oauth::approve_authorization(owner, &req, "agent:a1").await.unwrap();
    let q = query(&back);
    assert_eq!((q["x"].as_str(), q["state"].as_str(), q["iss"].as_str()), ("1", "s1", "https://api.test"));
    let code = q["code"].clone();

    // Exchange it: wrong secret, wrong verifier, then right.
    let exchange = form(&[("grant_type", "authorization_code"), ("code", &code), ("redirect_uri", "https://data.example/cb?x=1"), ("code_verifier", &verifier)]);
    assert_eq!(oauth::token_request(&exchange, Some(&id), Some("wrong")).await.unwrap_err().code, "invalid_client");
    let tokens = oauth::token_request(&exchange, Some(&id), Some(&secret)).await.unwrap();
    let id_token = oauth::verify_jwt(tokens["id_token"].as_str().unwrap(), None).unwrap();
    assert_eq!((id_token["sub"].as_str(), id_token["nonce"].as_str(), id_token["preferred_username"].as_str()), (Some("agent:a1"), Some("n1"), Some("nova")));
    assert_eq!(id_token["wallet_address"], "0x2222222222222222222222222222222222222222");
    assert_eq!(id_token["agent_environment"]["tier"], "basic");
    let info = oauth::userinfo(tokens["access_token"].as_str().unwrap()).await.unwrap();
    assert_eq!(info["orvyn_subject_type"], "agent");
    // The code is single use: replaying it revokes what it issued.
    assert_eq!(oauth::token_request(&exchange, Some(&id), Some(&secret)).await.unwrap_err().code, "invalid_grant");
    assert_eq!(oauth::userinfo(tokens["access_token"].as_str().unwrap()).await.unwrap_err().code, "invalid_token");

    // A fresh authorization; refresh rotates, and replaying a used refresh token revokes the family.
    let back = oauth::approve_authorization(owner, &req, "agent:a1").await.unwrap();
    let exchange = form(&[("grant_type", "authorization_code"), ("code", &query(&back)["code"]), ("redirect_uri", "https://data.example/cb?x=1"), ("code_verifier", &verifier)]);
    let tokens = oauth::token_request(&exchange, Some(&id), Some(&secret)).await.unwrap();
    let r1 = tokens["refresh_token"].as_str().unwrap().to_string();
    let widen = form(&[("grant_type", "refresh_token"), ("refresh_token", &r1), ("scope", "openid vyn")]);
    assert_eq!(oauth::token_request(&widen, Some(&id), Some(&secret)).await.unwrap_err().code, "invalid_scope");
    let refreshed = oauth::token_request(&form(&[("grant_type", "refresh_token"), ("refresh_token", &r1), ("scope", "openid wallet")]), Some(&id), Some(&secret)).await.unwrap();
    assert_eq!(refreshed["scope"], "openid wallet offline_access");
    let r2 = refreshed["refresh_token"].as_str().unwrap().to_string();
    assert_eq!(oauth::token_request(&form(&[("grant_type", "refresh_token"), ("refresh_token", &r1)]), Some(&id), Some(&secret)).await.unwrap_err().code, "invalid_grant");
    assert_eq!(oauth::token_request(&form(&[("grant_type", "refresh_token"), ("refresh_token", &r2)]), Some(&id), Some(&secret)).await.unwrap_err().code, "invalid_grant", "the family is revoked");

    // Device authorization for the self-hosted agent client (first party, created on first use).
    let start = oauth::start_device_authorization(Some("orv_agent_runtime"), Some("openid agents")).await.unwrap();
    let device = form(&[("grant_type", "urn:ietf:params:oauth:grant-type:device_code"), ("device_code", start["device_code"].as_str().unwrap())]);
    assert_eq!(oauth::token_request(&device, Some("orv_agent_runtime"), None).await.unwrap_err().code, "authorization_pending");
    assert_eq!(oauth::token_request(&device, Some("orv_agent_runtime"), None).await.unwrap_err().code, "slow_down");
    let typed = start["user_code"].as_str().unwrap().to_lowercase().replace('-', " ");
    let (_, client, scope) = oauth::lookup_user_code(&typed).await.unwrap();
    assert_eq!((client["name"].as_str(), scope), (Some("Orvyn agent (self-hosted)"), vec!["openid".to_string(), "agents".to_string()]));
    oauth::decide_device(owner, &typed, true, None).await.unwrap();
    sqlx::query("update oauth_device_codes set last_polled_at = now() - interval '1 minute'").execute(db).await.unwrap();
    let t = oauth::token_request(&device, Some("orv_agent_runtime"), None).await.unwrap();
    let (subject, cid) = oauth::authenticate_access(t["access_token"].as_str().unwrap(), "agents").await.unwrap();
    assert_eq!((subject.as_str(), cid.as_str()), (format!("user:{owner}").as_str(), "orv_agent_runtime"));
    assert_eq!(oauth::authenticate_access(t["access_token"].as_str().unwrap(), "vyn").await.unwrap_err().code, "insufficient_scope");

    // Connected apps, and disconnecting one.
    let apps = oauth::connected_apps(owner).await.unwrap();
    assert_eq!(apps.len(), 2);
    for a in &apps {
        oauth::disconnect_app(owner, a["id"].as_i64().unwrap() as i32).await.unwrap();
    }
    assert_eq!(oauth::authenticate_access(t["access_token"].as_str().unwrap(), "agents").await.unwrap_err().code, "invalid_token");
    assert!(oauth::connected_apps(owner).await.unwrap().is_empty());
    let subjects = oauth::subjects_for(owner).await.unwrap();
    assert_eq!(subjects.len(), 2);
    assert_eq!(subjects[1]["detail"], "nova");
}
