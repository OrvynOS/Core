//! The contracts' test fixtures, made with the same code the services run:
//!
//! - `deposit.json` (OrvynShieldedPoolReal.t.sol): a real OrvynPay private-mode deposit (1 BNB into
//!   a note of a freshly registered shielded key), proved with the circuit build, so the core's
//!   trees, extDataHash and proof format are checked against the contracts and the Groth16 verifier.
//!   It must come from the same zkey as contracts/src/pay/Groth16Verifier.sol.
//! - `rng.json` (OrvynRNG.t.sol): three node keys with payee-bound registration proofs, a payee
//!   change proof, a real past round (the nodes' aggregate plus the live drand quicknet signature),
//!   its output, a provable early signature, price reports with an equivocation, and a
//!   hash-to-curve vector.
//!
//! Both write to contracts/test/fixtures (or the given directory).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use alloy::primitives::{keccak256, U256};
use anyhow::{anyhow, Context, Result};
use orvyn_core::orvynpay::crypto::{new_shielded_keys, MerkleTree};
use orvyn_core::orvynpay::transaction::{KEY_LEVELS, NOTE_LEVELS};
use orvyn_core::orvynpay::{private_transaction, PoolView, PrivateKind, RegisteredKey};
use orvyn_core::rng::{self, bls, drand, DST_POSSESSION, DST_SIGNATURE};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

fn core_err(e: orvyn_core::Error) -> anyhow::Error {
    anyhow!(e.message)
}

fn out_dir(given: Option<&str>) -> Result<PathBuf> {
    if let Some(d) = given {
        return Ok(PathBuf::from(d));
    }
    let mut dir = std::env::current_dir()?;
    loop {
        // The monorepo's contracts/, or an orvyn-contracts checkout next to orvyn-core.
        for c in ["contracts", "orvyn-contracts"] {
            if dir.join(c).join("test").is_dir() {
                return Ok(dir.join(c).join("test/fixtures"));
            }
        }
        if !dir.pop() {
            anyhow::bail!("no contracts/test above the working directory; pass the output directory");
        }
    }
}

fn write(dir: PathBuf, name: &str, v: &Value) -> Result<PathBuf> {
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(name);
    std::fs::write(&path, serde_json::to_string_pretty(v)? + "\n")?;
    Ok(path)
}

pub async fn orvynpay(out: Option<&str>) -> Result<()> {
    // The circuit build (contracts/circuits/build.sh) unless ORVYN_SHIELDED_WASM / _ZKEY say otherwise.
    orvyn_core::orvynpay::transaction::artifacts().map_err(core_err).context("build the circuit first (contracts/circuits/build.sh), or set ORVYN_SHIELDED_WASM and ORVYN_SHIELDED_ZKEY")?;
    // makeAddr("alice") in Foundry: the test registers the key from this address.
    let alice: alloy::primitives::Address = "0x328809Bc894f92807417D2dAD6b7C998c1aFdac6".parse()?;
    let keys = new_shielded_keys();
    let view = PoolView {
        note_tree: MerkleTree::new(NOTE_LEVELS, vec![]),
        key_tree: MerkleTree::new(KEY_LEVELS, vec![keys.public_key]),
        encrypted: HashMap::new(),
        spent: HashSet::new(),
        keys_by_account: HashMap::from([(format!("{alice:#x}"), RegisteredKey { shielded_key: keys.public_key, encryption_key: keys.encryption_public.clone() })]),
        to_block: 0,
    };
    let empty_root = view.note_tree.root();
    println!("proving a 1 BNB deposit (this takes a while in a debug build)…");
    let tx = private_transaction(PrivateKind::Deposit, &keys, &view, "BNB", U256::from(10u64).pow(U256::from(18)), alice, None).await.map_err(core_err)?;
    let after = MerkleTree::new(NOTE_LEVELS, vec![tx.proof.outputCommitments[0], tx.proof.outputCommitments[1]]);
    let s = |v: U256| v.to_string();
    let p = &tx.proof;
    let fixture = json!({
        "account": alice.to_checksum(None),
        "shieldedKey": s(keys.public_key),
        "encryptionKey": keys.encryption_public,
        "emptyNoteRoot": s(empty_root),
        "keyRoot": s(view.key_tree.root()),
        "noteRootAfter": s(after.root()),
        "value": s(tx.value),
        "proof": {
            "a": p.a.iter().map(|x| s(*x)).collect::<Vec<_>>(),
            "b": p.b.iter().map(|r| r.iter().map(|x| s(*x)).collect::<Vec<_>>()).collect::<Vec<_>>(),
            "c": p.c.iter().map(|x| s(*x)).collect::<Vec<_>>(),
            "root": s(p.root),
            "keyRoot": s(p.keyRoot),
            "publicAmount": s(p.publicAmount),
            "extDataHash": s(p.extDataHash),
            "asset": s(p.asset),
            "inputNullifiers": p.inputNullifiers.iter().map(|x| s(*x)).collect::<Vec<_>>(),
            "outputCommitments": p.outputCommitments.iter().map(|x| s(*x)).collect::<Vec<_>>(),
        },
        "ext": {
            "recipient": tx.ext.recipient.to_checksum(None),
            "extAmount": tx.ext.extAmount.to_string(),
            "relayer": tx.ext.relayer.to_checksum(None),
            "fee": s(tx.ext.fee),
            "encryptedOutput1": tx.ext.encryptedOutput1.to_string(),
            "encryptedOutput2": tx.ext.encryptedOutput2.to_string(),
        },
    });
    let path = write(out_dir(out)?, "deposit.json", &fixture)?;
    println!("wrote {} (deposit of 1 BNB, key root {}…)", path.display(), &fixture["keyRoot"].as_str().unwrap_or("")[..12.min(fixture["keyRoot"].as_str().unwrap_or("").len())]);
    Ok(())
}

fn eip(sig: &str) -> Result<String> {
    bls::g1_eip(sig).map_err(core_err)
}

pub async fn rng(out: Option<&str>) -> Result<()> {
    let sign = |secret: &str, msg: &[u8], dst: &[u8]| bls::sign(secret, msg, dst).map_err(core_err);
    let addr = |s: &str| hex::decode(&s[2..]).unwrap();
    let u256 = |n: u64| U256::from(n).to_be_bytes::<32>().to_vec();

    // Fixed keys so the fixture is stable across runs.
    let secrets: Vec<String> = ["11", "22", "33"].iter().map(|b| b.repeat(32)).collect();
    let payees = ["0x00000000000000000000000000000000000000a1", "0x00000000000000000000000000000000000000a2", "0x00000000000000000000000000000000000000a3"];
    let mut nodes = vec![];
    for (i, secret) in secrets.iter().enumerate() {
        let pk_eip = bls::g2_eip(&bls::public_key_of(secret).map_err(core_err)?).map_err(core_err)?;
        let mut msg = b"orvyn-rng:v1:register:".to_vec();
        msg.extend(addr(payees[i]));
        msg.extend(hex::decode(&pk_eip)?);
        nodes.push(json!({ "publicKey": pk_eip, "payee": payees[i], "proof": eip(&sign(secret, &msg, DST_POSSESSION)?)? }));
    }
    let new_payee = "0x00000000000000000000000000000000000000b1";
    let mut msg = b"orvyn-rng:v1:payee:".to_vec();
    msg.extend(u256(0));
    msg.extend(u256(0));
    msg.extend(addr(new_payee));
    let payee_proof = eip(&sign(&secrets[0], &msg, DST_POSSESSION)?)?;

    // A round whose drand round is already out.
    let round = rng::round_of(rng::now_s()) - 5;
    let sigs: Vec<String> = secrets.iter().map(|s| rng::sign_round(s, round).map_err(core_err)).collect::<Result<_>>()?;
    let aggregate = bls::aggregate_signatures(&sigs).map_err(core_err)?;
    let drand_round = rng::drand_round_for(round);
    let b = drand::beacon(Some(drand_round)).await.map_err(core_err)?;
    let output = rng::output_of(round, &aggregate, &b.randomness);

    // A node signing a future round: the provable fault that gets it slashed.
    let early_round = round + 1000;
    let early_signature = eip(&rng::sign_round(&secrets[0], early_round).map_err(core_err)?)?;

    // Price reports: "orvyn-price:v1:" || feedId || round (uint64) || price (int256), 8 decimals.
    let pair = "BTC / USD";
    let (price, price_b) = (6_130_012_345_678i128, 6_130_112_345_678i128);
    let report = |p: i128| orvyn_core::price::report_message(pair, round, p);
    let report_sigs: Vec<String> = secrets.iter().map(|s| sign(s, &report(price), DST_SIGNATURE)).collect::<Result<_>>()?;
    let price_report = json!({
        "feedId": keccak256(pair.as_bytes()).to_string(),
        "price": price.to_string(),
        "aggregate3": eip(&bls::aggregate_signatures(&report_sigs).map_err(core_err)?)?,
        "aggregate2": eip(&bls::aggregate_signatures(&report_sigs[..2]).map_err(core_err)?)?,
        // Node 0 also signs a different price for the same feed and round: equivocation.
        "equivocation": { "priceA": price.to_string(), "sigA": eip(&report_sigs[0])?, "priceB": price_b.to_string(), "sigB": eip(&sign(&secrets[0], &report(price_b), DST_SIGNATURE)?)? },
    });

    // Hash-to-curve vector checked against the Solidity library on its own: signing with the
    // secret key 1 gives the hashed point itself.
    let htc_message = b"orvyn-rng:v1:12345";
    let one = format!("{:0>64}", "1");
    let htc = json!({ "message": hex::encode(htc_message), "dst": String::from_utf8_lossy(DST_SIGNATURE), "expected": eip(&sign(&one, htc_message, DST_SIGNATURE)?)? });

    let fixture = json!({
        "round": round,
        "roundClose": rng::round_close(round),
        "drandRound": drand_round,
        "drandTime": drand::GENESIS + (drand_round - 1) * drand::PERIOD,
        "drandSignature": eip(&b.signature)?,
        "drandRandomness": b.randomness,
        "nodes": nodes,
        "aggregateSignature": eip(&aggregate)?,
        "aggregateCompressed": aggregate,
        "output": output,
        "newPayee": new_payee,
        "payeeProof": payee_proof,
        "earlyRound": early_round,
        "earlySignature": early_signature,
        "hashToCurve": htc,
        "priceReport": price_report,
        "check": hex::encode(Sha256::digest(hex::decode(&b.signature)?)) == b.randomness,
    });
    let path = write(out_dir(out)?, "rng.json", &fixture)?;
    println!("wrote {}: round {round}, drand {drand_round}, output {}…", path.display(), &output[..16]);
    Ok(())
}
