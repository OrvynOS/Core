//! Billing: environment hashes (attested on chain) and Binance Pay signatures match the TypeScript
//! core (vectors/billing.json); hosting and the billing pass work in shared mode.
mod support;

use alloy::primitives::U256;
use orvyn_core::binancepay::{cents_to_str, sign_request, verify_signature};
use orvyn_core::environment::{describe_environment, environment_hash, environment_of};
use orvyn_core::invoices::money;
use orvyn_core::tiers::{category_blocked, Tier};
use serde_json::Value;

fn vectors() -> Value {
    support::test_env(&[]);
    serde_json::from_str(include_str!("vectors/billing.json")).unwrap()
}

#[test]
fn environment_hashes_match() {
    let v = vectors();
    for e in v["environments"].as_array().unwrap() {
        let a = &e["agent"];
        let hw = a.get("hardware").filter(|h| !h.is_null());
        let record = environment_of(a["id"].as_str().unwrap(), a["hostingStatus"].as_str().unwrap(), Tier::parse(a["tier"].as_str().unwrap()), hw);
        assert_eq!(orvyn_core::environment::canonical_json(&record), orvyn_core::environment::canonical_json(&e["record"]), "{}", a["id"]);
        assert_eq!(environment_hash(&record), e["hash"].as_str().unwrap(), "{}", a["id"]);
        assert_eq!(describe_environment(&record), e["summary"].as_str().unwrap());
    }
}

#[test]
fn binance_pay_signatures() {
    let v = vectors();
    let s = &v["signRequest"];
    assert_eq!(sign_request(s["body"].as_str().unwrap(), s["secret"].as_str().unwrap(), s["timestamp"].as_str().unwrap(), s["nonce"].as_str().unwrap()), s["signature"].as_str().unwrap());
    let w = &v["webhook"];
    let (body, ts, n, sig, key) = (w["body"].as_str().unwrap(), w["timestamp"].as_str().unwrap(), w["nonce"].as_str().unwrap(), w["signature"].as_str().unwrap(), w["publicKey"].as_str().unwrap());
    assert!(verify_signature(body, ts, n, sig, key));
    assert!(!verify_signature(&body.replace("20.00", "2000.00"), ts, n, sig, key));
    assert!(!verify_signature(body, "1795000000001", n, sig, key));
    for c in v["cents"].as_array().unwrap() {
        assert_eq!(cents_to_str(U256::from_str_radix(c["cents"].as_str().unwrap(), 10).unwrap()), c["text"].as_str().unwrap());
    }
    assert_eq!(money(U256::from(20_005_000_000_000_000_000u128), 18), "20.00");
    assert_eq!(money(U256::from(1_999_999_999_999_999_999u128), 18), "1.99");
}

#[test]
fn tiers_gate_categories() {
    assert_eq!(category_blocked(Tier::Basic, "video-edit").unwrap(), "Video editing needs Pro hosting (this agent is on Basic).");
    assert!(category_blocked(Tier::Studio, "worlds").is_none());
    assert_eq!(category_blocked(Tier::Pro, "nope").unwrap(), "Unknown job category.");
}

#[tokio::test]
async fn hosting_and_billing_in_shared_mode() {
    vectors();
    let db = support::shared_db().await;
    sqlx::query("insert into agents (id, name, symbol, owner_account_id, ai_provider, ai_model, ai_key_encrypted, ai_key_hint, evm_address, evm_wallet, status) values ('h1', 'H', 'H', '0x1', 'x', 'x', 'x', 'x', '0x2', '{}', 'live')")
        .execute(db)
        .await
        .unwrap();
    let agent: orvyn_core::db::Agent = sqlx::query_as("select * from agents where id = 'h1'").fetch_one(db).await.unwrap();
    // No registry configured: the billing pass just provisions the host.
    let agent = orvyn_core::subscription::run_billing(agent).await.unwrap();
    assert_eq!(agent.hosting_status, "running");
    let agent = orvyn_core::hosting::suspend_host(agent, "testing").await.unwrap();
    assert_eq!(agent.hosting_status, "suspended");
    let agent = orvyn_core::hosting::resume_host(agent).await.unwrap();
    assert_eq!(agent.hosting_status, "running");
    let logs: Vec<String> = sqlx::query_scalar("select message from agent_logs where agent_id = 'h1' order by id").fetch_all(db).await.unwrap();
    assert_eq!(logs, vec!["Agent suspended: testing", "Agent resumed: subscription is active"]);
}
