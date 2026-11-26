//! Wallets: Privy authorization signatures match Privy's SDK byte for byte (vectors/privy.json);
//! the local signer matches viem (vectors/local-signer.json); wallet metadata keeps its JSON shape.
mod support;

use alloy::primitives::{Address, Bytes, U256};
use alloy::signers::local::PrivateKeySigner;
use orvyn_core::wallet_policy::guard::RawSigner;
use orvyn_core::wallet_policy::{TxRequest, TypedData};
use orvyn_core::wallets::{authorization_payload, authorization_signature, canonical_json, LocalSigner, WalletMetadata};
use serde_json::{json, Value};

#[test]
fn privy_authorization_signatures_match_the_sdk() {
    let v: Value = serde_json::from_str(include_str!("vectors/privy.json")).unwrap();
    let key = v["key"].as_str().unwrap();
    for c in v["cases"].as_array().unwrap() {
        let i = &c["input"];
        let payload = authorization_payload(i["method"].as_str().unwrap(), i["url"].as_str().unwrap(), &i["body"], i["headers"].as_object().unwrap());
        assert_eq!(payload, c["canonical"].as_str().unwrap());
        // RFC 6979: deterministic, so the same bytes as Privy's SDK.
        assert_eq!(authorization_signature(key, &payload).unwrap(), c["signature"].as_str().unwrap());
    }
    assert_eq!(canonical_json(&json!({ "b": [1, { "d": "x", "c": true }], "a": null })), r#"{"a":null,"b":[1,{"c":true,"d":"x"}]}"#);
    assert!(authorization_signature("wallet-auth:not-a-key", "x").is_err());
}

#[tokio::test]
async fn the_local_signer_matches_viem() {
    let v: Value = serde_json::from_str(include_str!("vectors/local-signer.json")).unwrap();
    let signer = LocalSigner(v["key"].as_str().unwrap().parse::<PrivateKeySigner>().unwrap());
    assert_eq!(signer.address(), v["address"].as_str().unwrap());
    assert_eq!(signer.sign_message(v["message"]["text"].as_str().unwrap().as_bytes()).await.unwrap(), v["message"]["signature"].as_str().unwrap());
    let typed: TypedData = serde_json::from_value(v["typed"]["data"].clone()).unwrap();
    assert_eq!(signer.sign_typed_data(&typed).await.unwrap(), v["typed"]["signature"].as_str().unwrap());
    let t = &v["tx"]["input"];
    let n = |k: &str| t[k].as_str().unwrap().parse::<u128>().unwrap();
    let tx = TxRequest {
        chain_id: 56,
        nonce: Some(t["nonce"].as_u64().unwrap()),
        to: Some(t["to"].as_str().unwrap().parse::<Address>().unwrap()),
        value: U256::from(n("value")),
        data: t["data"].as_str().unwrap().parse::<Bytes>().unwrap(),
        gas: Some(n("gas") as u64),
        max_fee_per_gas: Some(n("maxFeePerGas")),
        max_priority_fee_per_gas: Some(n("maxPriorityFeePerGas")),
        ..Default::default()
    };
    assert_eq!(signer.sign_transaction(&tx).await.unwrap(), v["tx"]["signed"].as_str().unwrap());
}

#[test]
fn wallet_metadata_keeps_its_shape() {
    let privy = json!({ "custody": "privy", "walletId": "wal_1", "accountAddress": "0xAbC0000000000000000000000000000000000001" });
    let m = WalletMetadata::from_value(&privy).unwrap();
    assert_eq!(m, WalletMetadata::Privy { wallet_id: "wal_1".into(), account_address: "0xAbC0000000000000000000000000000000000001".into() });
    assert_eq!(serde_json::to_value(&m).unwrap(), privy);
    let local = json!({ "custody": "local", "accountAddress": "0x01", "key": "aster" });
    assert_eq!(serde_json::to_value(WalletMetadata::from_value(&local).unwrap()).unwrap(), local);
    assert!(WalletMetadata::from_value(&json!({})).is_err());
}
