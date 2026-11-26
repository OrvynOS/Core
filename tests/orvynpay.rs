//! OrvynPay against the TypeScript core (vectors/orvynpay.json): Poseidon, notes, nullifiers,
//! trees, encrypted notes both ways, ExtData hashes, public amounts, refs, payment calls, and a
//! fully specified private transaction's circuit input.
mod support;

use alloy::primitives::{Address, I256, U256};
use orvyn_core::orvynpay::*;
use serde_json::Value;

fn v() -> Value {
    support::test_env(&[("ORVYN_PAY_CONTRACT", "0x4444444444444444444444444444444444444444")]);
    serde_json::from_str(include_str!("vectors/orvynpay.json")).unwrap()
}

fn u(v: &Value) -> U256 {
    v.as_str().unwrap().parse().unwrap()
}

#[test]
fn hashes_notes_and_trees_match() {
    let v = v();
    assert_eq!(public_key_of(U256::from(1)), u(&v["poseidon"]["one"]));
    assert_eq!(hash2(U256::from(1), U256::from(2)), u(&v["poseidon"]["two"]));
    let (sk, pk) = (u(&v["keys"]["privateKey"]), u(&v["keys"]["publicKey"]));
    assert_eq!(public_key_of(sk), pk);
    let nv = &v["note"];
    let n1 = Note { amount: u(&nv["amount"]), public_key: pk, blinding: u(&nv["blinding"]), asset: u(&nv["asset"]), index: Some(2) };
    assert_eq!(commitment_of(&n1), u(&nv["commitment"]));
    assert_eq!(nullifier_of(&n1, sk).unwrap(), u(&nv["nullifier"]));

    // A note TypeScript encrypted opens here; one encrypted here opens with the same keys.
    let other = &v["other"];
    let (enc_priv, other_pk) = (other["encryptionPrivate"].as_str().unwrap(), u(&other["publicKey"]));
    let opened = decrypt_note(nv["encryptedToOther"].as_str().unwrap(), enc_priv, pk, commitment_of(&n1), 2).unwrap();
    assert_eq!(opened, n1);
    assert!(decrypt_note(nv["encryptedToOther"].as_str().unwrap(), enc_priv, pk, commitment_of(&n1) + U256::from(1), 2).is_none());
    let mine = Note { public_key: other_pk, ..n1.clone() };
    let sealed = encrypt_note(&mine, other["encryptionPublic"].as_str().unwrap()).unwrap();
    assert_eq!(sealed.len(), 2 + 2 * 156);
    assert_eq!(decrypt_note(&sealed, enc_priv, other_pk, commitment_of(&mine), 2), Some(mine));

    let n2 = Note { amount: U256::from(3) * U256::from(10).pow(U256::from(18)), public_key: pk, blinding: U256::from(222), asset: n1.asset, index: Some(4) };
    let note_tree = MerkleTree::new(NOTE_LEVELS, vec![U256::from(5), U256::from(6), commitment_of(&n1), U256::from(8), commitment_of(&n2)]);
    let key_tree = MerkleTree::new(KEY_LEVELS, vec![pk, other_pk]);
    assert_eq!(note_tree.root(), u(&v["trees"]["noteRoot"]));
    assert_eq!(key_tree.root(), u(&v["trees"]["keyRoot"]));
    assert_eq!(MerkleTree::new(KEY_LEVELS, vec![]).root(), u(&v["trees"]["emptyRoot"]));
    let path: Vec<U256> = v["trees"]["path4"].as_array().unwrap().iter().map(u).collect();
    assert_eq!(note_tree.path(4).unwrap(), path);
    // Inserting one at a time gives the same tree.
    let mut grown = MerkleTree::new(NOTE_LEVELS, vec![]);
    for l in note_tree.leaves().to_vec() {
        grown.insert(&[l]);
    }
    assert_eq!(grown.root(), note_tree.root());

    // The transaction's signals and circuit input, signal for signal.
    let ten18 = U256::from(10).pow(U256::from(18));
    let built = build_transaction(BuildInput {
        private_key: sk,
        public_key: pk,
        inputs: vec![n1.clone(), n2],
        outputs: vec![
            Note { amount: U256::from(4) * ten18, public_key: other_pk, blinding: U256::from(333), asset: n1.asset, index: None },
            Note { amount: U256::from(6) * ten18, public_key: pk, blinding: U256::from(444), asset: n1.asset, index: None },
        ],
        encrypted: [format!("0x{}", "aa".repeat(156)), format!("0x{}", "bb".repeat(156))],
        ext_amount: I256::ZERO,
        asset: n1.asset,
        recipient: None,
        relayer: None,
        fee: U256::ZERO,
        note_tree: &note_tree,
        key_tree: &key_tree,
    })
    .unwrap();
    let want = &v["built"]["circuitInput"];
    for (name, values) in &built.circuit_input {
        let flat: Vec<U256> = match &want[name] {
            Value::Array(a) => a.iter().flat_map(|x| match x {
                Value::Array(inner) => inner.iter().map(u).collect::<Vec<_>>(),
                other => vec![u(other)],
            }).collect(),
            other => vec![u(other)],
        };
        assert_eq!(values, &flat, "circuit input {name}");
    }
    assert_eq!(built.signals.ext_data_hash, u(&v["built"]["signals"]["extDataHash"]));
}

#[test]
fn ext_data_amounts_refs_and_calls_match() {
    let v = v();
    let e = &v["ext"]["value"];
    let ext = ExtData {
        recipient: e["recipient"].as_str().unwrap().parse().unwrap(),
        extAmount: e["extAmount"].as_str().unwrap().parse().unwrap(),
        relayer: e["relayer"].as_str().unwrap().parse().unwrap(),
        fee: u(&e["fee"]),
        encryptedOutput1: vec![1u8, 2].into(),
        encryptedOutput2: vec![].into(),
    };
    assert_eq!(ext_data_hash(&ext), u(&v["ext"]["hash"]));
    assert_eq!(public_amount_of(I256::try_from(10).unwrap(), U256::ZERO).unwrap(), u(&v["publicAmount"]["deposit"]));
    assert_eq!(public_amount_of(I256::try_from(-10).unwrap(), U256::from(2)).unwrap(), u(&v["publicAmount"]["withdraw"]));
    assert_eq!(ref_of("invoice #42 — thanks").to_string(), v["ref"].as_str().unwrap());
    let to: Address = "0x3333333333333333333333333333333333333333".parse().unwrap();
    let calls = standard_payment_calls(to, "USDT", U256::from(5) * U256::from(10).pow(U256::from(18)), "invoice #42").unwrap();
    let bnb = standard_payment_calls(to, "BNB", U256::from(10).pow(U256::from(16)), "tip").unwrap();
    for (got, want) in [(calls, &v["standard"]), (bnb, &v["standardBnb"])] {
        let want = want.as_array().unwrap();
        assert_eq!(got.len(), want.len());
        for (g, w) in got.iter().zip(want) {
            assert_eq!(g.to.to_lowercase(), w["to"].as_str().unwrap().to_lowercase());
            assert_eq!(g.data, w["data"].as_str().unwrap());
            assert_eq!(g.value, w["value"].as_str().unwrap());
        }
    }
}

/// Proves the vector transaction with a real circuit build (ORVYN_SHIELDED_WASM / _ZKEY; skipped
/// without them), checks it with the zkey's verifying key, and that a changed signal fails. With
/// ORVYN_PROOF_OUT set, writes proof.json and public.json for `snarkjs groth16 verify`.
#[tokio::test]
async fn proves_with_the_circuit_build() {
    let (Ok(_), Ok(zkey)) = (std::env::var("ORVYN_SHIELDED_WASM"), std::env::var("ORVYN_SHIELDED_ZKEY")) else {
        eprintln!("skipped: set ORVYN_SHIELDED_WASM and ORVYN_SHIELDED_ZKEY to a circuit build");
        return;
    };
    let v = v();
    let (sk, pk, other_pk) = (u(&v["keys"]["privateKey"]), u(&v["keys"]["publicKey"]), u(&v["other"]["publicKey"]));
    let nv = &v["note"];
    let ten18 = U256::from(10).pow(U256::from(18));
    let asset = u(&nv["asset"]);
    let n1 = Note { amount: u(&nv["amount"]), public_key: pk, blinding: u(&nv["blinding"]), asset, index: Some(2) };
    let n2 = Note { amount: U256::from(3) * ten18, public_key: pk, blinding: U256::from(222), asset, index: Some(4) };
    let note_tree = MerkleTree::new(NOTE_LEVELS, vec![U256::from(5), U256::from(6), commitment_of(&n1), U256::from(8), commitment_of(&n2)]);
    let key_tree = MerkleTree::new(KEY_LEVELS, vec![pk, other_pk]);
    let built = build_transaction(BuildInput {
        private_key: sk,
        public_key: pk,
        inputs: vec![n1.clone(), n2],
        outputs: vec![
            Note { amount: U256::from(4) * ten18, public_key: other_pk, blinding: U256::from(333), asset, index: None },
            Note { amount: U256::from(6) * ten18, public_key: pk, blinding: U256::from(444), asset, index: None },
        ],
        encrypted: [format!("0x{}", "aa".repeat(156)), format!("0x{}", "bb".repeat(156))],
        ext_amount: I256::ZERO,
        asset,
        recipient: None,
        relayer: None,
        fee: U256::ZERO,
        note_tree: &note_tree,
        key_tree: &key_tree,
    })
    .unwrap();
    let started = std::time::Instant::now();
    let proof = prove_transaction(&built).await.unwrap();
    eprintln!("proved in {:?}", started.elapsed());
    let zkey = std::path::Path::new(&zkey);
    assert!(verify_proof(zkey, &proof).unwrap(), "the proof verifies");
    let mut tampered = proof.clone();
    tampered.publicAmount = U256::from(1);
    assert!(!verify_proof(zkey, &tampered).unwrap(), "a changed signal fails");
    if let Ok(dir) = std::env::var("ORVYN_PROOF_OUT") {
        let s = |x: U256| x.to_string();
        // snarkjs' order: G2 coordinates as (c0, c1).
        let json = serde_json::json!({
            "pi_a": [s(proof.a[0]), s(proof.a[1]), "1"],
            "pi_b": [[s(proof.b[0][1]), s(proof.b[0][0])], [s(proof.b[1][1]), s(proof.b[1][0])], ["1", "0"]],
            "pi_c": [s(proof.c[0]), s(proof.c[1]), "1"],
            "protocol": "groth16",
            "curve": "bn128",
        });
        let public: Vec<String> = signals_of(&proof).into_iter().map(s).collect();
        std::fs::write(format!("{dir}/proof.json"), json.to_string()).unwrap();
        std::fs::write(format!("{dir}/public.json"), serde_json::to_string(&public).unwrap()).unwrap();
    }
}
