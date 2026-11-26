//! Sign-in: sessions, mobile codes and SIWE messages are the TypeScript core's (vectors/auth.json);
//! nonces burn once; Privy sign-ins are recorded.
mod support;

use alloy::primitives::U256;
use chrono::{DateTime, Utc};
use orvyn_core::accounts::{person_from_linked, record_privy_person, record_wallet_sign_in, user_view, PrivyPerson};
use orvyn_core::auth::*;
use orvyn_core::evm::sig::{message_hash, recover};
use serde_json::{json, Value};

fn vectors() -> Value {
    support::test_env(&[]);
    serde_json::from_str(include_str!("vectors/auth.json")).unwrap()
}

#[test]
fn sessions_are_the_typescript_cores() {
    let v = vectors();
    let (token, now) = (v["session"].as_str().unwrap(), v["now"].as_i64().unwrap());
    assert_eq!(read_session_at(Some(token), now).as_deref(), Some("0xabcdef0000000000000000000000000000000001"));
    // The same token, made here from the same expiry.
    let body: Value = {
        use base64::Engine;
        let b = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(token.split('.').next().unwrap()).unwrap();
        serde_json::from_slice(&b).unwrap()
    };
    let issued = body["e"].as_i64().unwrap() - 7 * 24 * 3_600_000;
    assert_eq!(create_session_at("0xAbCdEf0000000000000000000000000000000001", issued), token);
    // Expired, tampered, or malformed: no session.
    assert_eq!(read_session_at(Some(token), now + 8 * 24 * 3_600_000), None);
    let tampered = format!("{}x", token);
    assert_eq!(read_session_at(Some(&tampered), now), None);
    assert_eq!(read_session_at(Some("nodot"), now), None);
    assert_eq!(read_session_at(None, now), None);
    // A fresh one reads back.
    assert_eq!(read_session(Some(&create_session("0xDEF"))).as_deref(), Some("0xdef"));
}

#[test]
fn mobile_codes_are_the_typescript_cores() {
    let v = vectors();
    let (code, verifier, now) = (v["mobile"]["code"].as_str().unwrap(), v["mobile"]["verifier"].as_str().unwrap(), v["now"].as_i64().unwrap());
    let (account, nonce) = check_mobile_code_at(code, verifier, now).unwrap();
    assert_eq!(account, "0xabcdef0000000000000000000000000000000001");
    assert_eq!(nonce.len(), 32);
    assert!(check_mobile_code_at(code, "wrong-verifier", now).unwrap_err().message.contains("verifier"));
    assert!(check_mobile_code_at(code, verifier, now + 3 * 60_000).unwrap_err().message.contains("expired"));
    assert!(check_mobile_code_at(&code.replacen('.', "x.", 1), verifier, now).is_err());
    // Codes made here check the same way; challenges must be PKCE S256 shaped.
    let challenge = {
        use base64::Engine;
        use sha2::Digest;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(b"my-verifier"))
    };
    let mine = create_mobile_code("0x1", &challenge).unwrap();
    assert!(check_mobile_code_at(&mine, "my-verifier", now_ms()).is_ok());
    assert!(create_mobile_code("0x1", "short").is_err());
}

#[test]
fn siwe_messages_parse_and_check_like_viem() {
    let v = vectors();
    let now: DateTime<Utc> = v["siweNow"].as_str().unwrap().parse().unwrap();
    let signer = v["signer"].as_str().unwrap();
    for case in v["siwe"].as_array().unwrap() {
        let (name, message, parsed) = (case["name"].as_str().unwrap(), case["message"].as_str().unwrap(), &case["parsed"]);
        let m = parse_siwe(message);
        assert_eq!(m.domain.as_deref(), parsed["domain"].as_str(), "{name}");
        assert_eq!(m.address.as_deref(), parsed["address"].as_str(), "{name}");
        assert_eq!(m.statement.as_deref(), parsed["statement"].as_str(), "{name}");
        assert_eq!(m.nonce.as_deref(), parsed["nonce"].as_str(), "{name}");
        assert_eq!(m.uri.as_deref(), parsed["uri"].as_str(), "{name}");
        assert_eq!(m.chain_id, parsed["chainId"].as_u64(), "{name}");
        assert_eq!(m.request_id.as_deref(), parsed["requestId"].as_str(), "{name}");
        let resources: Option<Vec<String>> = parsed.get("resources").map(|r| serde_json::from_value(r.clone()).unwrap());
        assert_eq!(m.resources, resources, "{name}");

        let checked = check_siwe_fields(message, SIGN_IN_STATEMENT, now);
        let expect = match name {
            "ok" | "with resources" => None,
            "wrong statement" => Some("unexpected message statement"),
            "wrong chain" => Some("chain 56"),
            "wrong domain" => Some("not Orvyn"),
            "expired" | "not yet" => Some("expired or not yet valid"),
            other => panic!("{other}"),
        };
        match (expect, checked) {
            (None, Ok(_)) => {}
            (Some(want), Err(e)) => assert!(e.message.contains(want), "{name}: {}", e.message),
            (want, got) => panic!("{name}: wanted {want:?}, got {:?}", got.map(|_| ())),
        }
        // viem's signature recovers to the signer; a changed message doesn't.
        let sig = hex::decode(case["signature"].as_str().unwrap().trim_start_matches("0x")).unwrap();
        assert_eq!(recover(message_hash(message), &sig).map(|a| format!("{a:#x}")).as_deref(), Some(signer), "{name}");
        assert_ne!(recover(message_hash(&message.replace("abc123", "abc124")), &sig).map(|a| format!("{a:#x}")).as_deref(), Some(signer));
    }
    // Stale: 6 minutes after it was issued.
    let late: DateTime<Utc> = "2026-11-23T12:06:00Z".parse().unwrap();
    let ok = v["siwe"][0]["message"].as_str().unwrap();
    assert!(check_siwe_fields(ok, SIGN_IN_STATEMENT, late).unwrap_err().message.contains("expired, try again"));
    // An unparseable Issued At is refused.
    let garbage = ok.replace("Issued At: 2026-11-23T12:00:00.000Z", "Issued At: yesterday");
    assert!(check_siwe_fields(&garbage, SIGN_IN_STATEMENT, now).is_err());
}

#[test]
fn holder_gate() {
    let tokens = |n: u64| U256::from(n) * U256::from(10u64).pow(U256::from(18u8));
    let s = holder_from_raw(tokens(5), 10.0);
    assert!(!s.allowed);
    assert_eq!(s.reason.as_deref(), Some("hold at least 10 tokens to chat (you hold 5)"));
    assert!(holder_from_raw(tokens(10), 10.0).allowed);
    assert_eq!(holder_from_raw(U256::from(1_500_000_000_000_000_000u64), 1.0).balance, 1.5);
}

#[test]
fn privy_linked_accounts() {
    let accounts = vec![
        json!({ "type": "email", "address": "nova@example.com" }),
        json!({ "type": "wallet", "chain_type": "ethereum", "wallet_client_type": "metamask", "address": "0x1111111111111111111111111111111111111111" }),
        json!({ "type": "wallet", "chain_type": "ethereum", "wallet_client_type": "privy", "address": "0xAAAA000000000000000000000000000000000002" }),
        json!({ "type": "twitter_oauth", "username": "nova" }),
    ];
    let p = person_from_linked("did:privy:1", &accounts).unwrap();
    assert_eq!(p.account_id, "0xaaaa000000000000000000000000000000000002");
    assert_eq!((p.email.as_deref(), p.social_provider.as_deref(), p.social_username.as_deref()), (Some("nova@example.com"), Some("twitter"), Some("nova")));
    assert!(person_from_linked("did:privy:2", &accounts[..2]).unwrap_err().message.contains("still being created"));
}

#[tokio::test]
async fn nonces_burn_once_and_people_are_recorded() {
    vectors();
    support::shared_db().await;
    burn_nonce("siwe:0xabc:n1").await.unwrap();
    assert!(burn_nonce("siwe:0xabc:n1").await.is_err());

    let person = PrivyPerson { privy_user_id: "did:privy:9".into(), account_id: "0x9999000000000000000000000000000000000009".into(), email: Some("a@b.c".into()), social_provider: None, social_username: None };
    let first = record_privy_person(&person).await.unwrap();
    assert!(first.first_sign_in);
    assert_eq!(read_session(Some(&first.token)).as_deref(), Some(person.account_id.as_str()));
    assert!(!record_privy_person(&person).await.unwrap().first_sign_in);
    // The same Privy user can't become another account.
    let moved = PrivyPerson { account_id: "0x8888000000000000000000000000000000000008".into(), ..person.clone() };
    assert!(record_privy_person(&moved).await.unwrap_err().message.contains("different Orvyn wallet"));

    record_wallet_sign_in("0xBEEF000000000000000000000000000000000001").await.unwrap();
    let u = orvyn_core::accounts::get_user("0xbeef000000000000000000000000000000000001").await.unwrap().unwrap();
    assert_eq!(u.sign_in_method, "wallet");
    let view = serde_json::to_value(user_view(Some(&first.user), &person.account_id)).unwrap();
    assert_eq!(view["signInMethod"], "privy");
    assert_eq!(view["email"], "a@b.c");
    assert_eq!(view["wallet"]["bsc"], person.account_id.as_str());
}
