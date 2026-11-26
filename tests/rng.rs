//! Orvyn RNG and price feeds against the TypeScript core (vectors/rng.json): BLS signatures,
//! EIP-2537 encodings, outputs, draws, a real drand round, price messages; and a full round of
//! both networks on a throwaway database.
mod support;

use orvyn_core::price;
use orvyn_core::rng::{self, bls, chain, drand};
use serde_json::Value;

fn v() -> Value {
    support::test_env(&[]);
    serde_json::from_str(include_str!("vectors/rng.json")).unwrap()
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap()
}

#[test]
fn keys_signatures_and_encodings_match() {
    let v = v();
    let round = v["round"].as_i64().unwrap();
    for (i, k) in v["keys"].as_array().unwrap().iter().enumerate() {
        assert_eq!(bls::public_key_of(s(&k["secret"])).unwrap(), s(&k["publicKey"]));
        assert_eq!(rng::sign_round(s(&k["secret"]), round).unwrap(), s(&v["sigs"][i]));
        assert_eq!(rng::prove_key(s(&k["secret"]), s(&k["publicKey"])).unwrap(), s(&v["proofs"][i]));
        assert_eq!(bls::g2_eip(s(&k["publicKey"])).unwrap(), s(&v["g2Eip"][i]));
        assert!(bls::valid_public_key(s(&k["publicKey"])));
    }
    let sigs: Vec<String> = v["sigs"].as_array().unwrap().iter().map(|x| s(x).to_string()).collect();
    let aggregate = bls::aggregate_signatures(&sigs).unwrap();
    assert_eq!(aggregate, s(&v["aggregate"]));
    assert_eq!(bls::g1_eip(&aggregate).unwrap(), s(&v["g1Eip"]));
    let pks: Vec<String> = v["keys"].as_array().unwrap().iter().map(|k| s(&k["publicKey"]).to_string()).collect();
    assert!(bls::verify_aggregate(&aggregate, &rng::round_message(round), rng::DST_SIGNATURE, &pks));
    assert!(!bls::verify_aggregate(&aggregate, &rng::round_message(round + 1), rng::DST_SIGNATURE, &pks));
    assert_eq!(chain::registration_proof(s(&v["keys"][0]["secret"]), s(&v["payee"]), &pks[0]).unwrap(), s(&v["registrationProof"]));
    assert_eq!(rng::output_of(round, &aggregate, &"ab".repeat(32)), s(&v["output"]));
    assert_eq!(rng::drand_round_for(round), v["drandRoundFor"].as_i64().unwrap());
}

#[test]
fn drand_rounds_verify_and_draws_match() {
    let v = v();
    let b = &v["beacon"];
    let (round, sig) = (b["round"].as_i64().unwrap(), s(&b["signature"]));
    assert!(drand::verify_round(round, sig));
    assert!(!drand::verify_round(round + 1, sig));
    assert_eq!(drand::randomness_of(sig), s(&b["randomness"]));
    assert_eq!(drand::iso(drand::time_of_round(round)), s(&b["at"]));
    for d in v["draws"].as_array().unwrap() {
        let a = &d["args"];
        let out = rng::draw(s(&a[0]), s(&a[1]), a[2].as_u64().unwrap() as usize, a[3].as_i64().unwrap(), a[4].as_i64().unwrap(), a[5].as_bool().unwrap());
        let want: Vec<i64> = d["out"].as_array().unwrap().iter().map(|x| x.as_i64().unwrap()).collect();
        assert_eq!(out, want, "draw {a}");
    }
}

#[test]
fn price_messages_match() {
    let v = v();
    let p = &v["price"];
    let round = v["round"].as_i64().unwrap();
    assert_eq!(price::feed_id("BTC / USD").to_string(), s(&p["feedId"]));
    assert_eq!(hex::encode(price::observation_message("BTC / USD", round, 6512345678900)), s(&p["observation"]));
    let report = price::report_message("ETH / USD", round, 312345000000);
    assert_eq!(hex::encode(&report), s(&p["report"]));
    assert_eq!(price::sign_message(s(&v["keys"][1]["secret"]), &report).unwrap(), s(&p["reportSig"]));
    assert_eq!(price::median_e8(&[5, 1, 9, 3]).to_string(), s(&p["median"]));
    assert_eq!(price::to_e8(65123.456789123).to_string(), s(&p["toE8"]));
}

#[tokio::test]
async fn a_round_of_both_networks() {
    support::test_env(&[]);
    let db = support::shared_db().await;
    for id in ["n1", "n2", "n3"] {
        rng::join_hosted(id).await.unwrap();
    }
    // Joining again is a no-op; leaving and rejoining reactivates.
    let pk = rng::join_hosted("n1").await.unwrap()["publicKey"].clone();
    rng::leave("n3").await.unwrap();
    assert_eq!(rng::join_hosted("n1").await.unwrap()["publicKey"], pk);

    // A round in the near future, so the rows' real created_at is before its close.
    let r = rng::round_of(rng::now_s()) + 2;
    let start = rng::round_start(r) as f64;
    assert_eq!(rng::contribute_hosted(start + 5.0).await.unwrap(), 2);
    assert_eq!(rng::contribute_hosted(start + 6.0).await.unwrap(), 0);
    assert_eq!(rng::contribute_hosted(start + 51.0).await.unwrap(), 0, "closed");

    // A self-hosted node: wrong round and bad signatures are refused.
    let (secret, public_key) = bls::new_key();
    let proof = rng::prove_key(&secret, &public_key).unwrap();
    assert!(rng::join_external("x1", &public_key, &rng::prove_key(&secret, &pk.as_str().unwrap().to_string()).unwrap()).await.is_err());
    rng::join_external("x1", &public_key, &proof).await.unwrap();
    assert!(rng::submit_partial("x1", r, &rng::sign_round(&secret, r + 1).unwrap(), start + 7.0).await.is_err());
    assert!(rng::submit_partial("x1", r - 1, &rng::sign_round(&secret, r - 1).unwrap(), start + 7.0).await.is_err());
    rng::submit_partial("x1", r, &rng::sign_round(&secret, r).unwrap(), start + 7.0).await.unwrap();

    let fake = drand::Beacon { round: rng::drand_round_for(r), signature: "00".repeat(48), randomness: "cd".repeat(32), at: String::new() };
    let later = start + 120.0;
    assert_eq!(rng::finalize_rounds_with(start + 55.0, |_| { let b = fake.clone(); async move { Ok(b) } }).await.unwrap(), 0, "drand not out yet");
    assert_eq!(rng::finalize_rounds_with(later, |_| { let b = fake.clone(); async move { Ok(b) } }).await.unwrap(), 1);
    let record = rng::get_round(Some(r)).await.unwrap().unwrap();
    assert_eq!(record.participants.as_array().unwrap().len(), 3);
    assert_eq!(record.output, rng::output_of(r, &record.aggregate_signature, &"cd".repeat(32)));
    // Only the (fake) drand part fails verification.
    let check = rng::verify_record(&record);
    let problems: Vec<&str> = check["problems"].as_array().unwrap().iter().map(|p| p.as_str().unwrap()).collect();
    assert!(problems.contains(&"the drand signature doesn't verify"), "{problems:?}");
    assert!(!problems.iter().any(|p| p.contains("aggregate")), "{problems:?}");
    let stats = rng::node_stats("n1", 1440).await.unwrap().unwrap();
    assert_eq!(stats["roundsSigned"], 1);

    // Price network: observe, report, sign, finalise.
    let observed = price::observe_hosted(start + 5.0, |pair| async move { Some(if pair == "BTC / USD" { 65000.12345 } else { 100.0 }) }).await.unwrap();
    assert_eq!(observed, 2 * price::FEEDS.len());
    let btc = price::to_e8(65000.12345);
    // x1 observes 1% away: the report (lower median) is still the hosted nodes' price.
    let own = btc + btc / 100;
    let sig = price::sign_message(&secret, &price::observation_message("BTC / USD", r, own)).unwrap();
    price::submit_observation("x1", "BTC / USD", r, &own.to_string(), &sig, start + 6.0).await.unwrap();
    assert!(price::submit_observation("x1", "BTC / USD", r, &own.to_string(), &sig, start + 31.0).await.is_err());
    assert_eq!(price::build_reports(start + 31.0).await.unwrap(), price::FEEDS.len());
    assert_eq!(price::sign_hosted(start + 35.0).await.unwrap(), 2 * price::FEEDS.len());
    let report_sig = price::sign_message(&secret, &price::report_message("BTC / USD", r, btc)).unwrap();
    assert!(price::submit_report_signature("x1", "BTC / USD", r, &(btc + 1).to_string(), &report_sig, start + 36.0).await.is_err(), "not the report");
    price::submit_report_signature("x1", "BTC / USD", r, &btc.to_string(), &report_sig, start + 36.0).await.unwrap();
    assert_eq!(price::finalize_reports(start + 51.0).await.unwrap(), price::FEEDS.len());
    let rep = price::report("BTC / USD", r).await.unwrap().unwrap();
    assert_eq!(rep["priceE8"], btc.to_string());
    assert_eq!(rep["signers"], 3);
    assert_eq!(rep["verification"]["valid"], true, "{rep}");
    let acc = price::node_accuracy("x1", 1440).await.unwrap();
    assert_eq!(acc["observations"], 1);
    assert_eq!(acc["withinTolerancePct"], 0.0);
    let _ = db;
}
