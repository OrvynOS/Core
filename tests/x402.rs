//! x402: payments the TypeScript client signs pass the Rust checks (vectors/x402.json), tampering
//! is refused with the spec's reasons, and Rust-signed payments check out the same way.
mod support;

use alloy::primitives::U256;
use alloy::signers::local::PrivateKeySigner;
use orvyn_core::wallet_policy::set_now_for_tests;
use orvyn_core::wallets::LocalSigner;
use orvyn_core::x402::*;
use serde_json::Value;

/// The policy clock is one global: tests that pin it take turns.
static CLOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn payments_check_like_the_typescript_core() {
    let _clock = CLOCK.lock().await;
    support::test_env(&[]);
    let v: Value = serde_json::from_str(include_str!("vectors/x402.json")).unwrap();
    set_now_for_tests(v["nowMs"].as_i64().unwrap());
    let req: PaymentRequirements = serde_json::from_value(v["requirement"].clone()).unwrap();
    let payment: PaymentPayload = serde_json::from_value(v["payment"].clone()).unwrap();
    assert_eq!(check_payload_offline(&payment, &req).unwrap(), v["payer"].as_str().unwrap());
    // The header decodes to the same payment.
    let decoded: PaymentPayload = decode_header(v["header"].as_str()).unwrap();
    assert_eq!(decoded.payload, payment.payload);
    // Our requirements are the TypeScript core's.
    let ours = requirements_for("0x4444444444444444444444444444444444444444", &[PriceOption { asset: BSC_USDT.into(), amount: U256::from(2u64) * U256::from(10u64).pow(U256::from(18u8)) }], None);
    assert_eq!(serde_json::to_value(&ours[0]).unwrap(), v["requirement"]);
    assert!(matching_requirement(&payment, &ours).is_some());

    let reason = |p: &PaymentPayload, r: &PaymentRequirements| check_payload_offline(p, r).unwrap_err().reason;
    let mut more = req.clone();
    more.amount = "3000000000000000000".into();
    assert_eq!(reason(&payment, &more), "invalid_exact_evm_payload_amount");
    let mut elsewhere = req.clone();
    elsewhere.pay_to = "0x5555555555555555555555555555555555555555".into();
    assert_eq!(reason(&payment, &elsewhere), "invalid_exact_evm_payload_recipient_mismatch");
    let mut forged = payment.clone();
    forged.payload.as_mut().unwrap().permit2_authorization.permitted.amount = "9000000000000000000".into();
    assert_eq!(reason(&forged, &req), "invalid_exact_evm_payload_signature");
    set_now_for_tests(v["nowMs"].as_i64().unwrap() + 3_600_000);
    assert_eq!(reason(&payment, &req), "invalid_exact_evm_payload_deadline");
    set_now_for_tests(0);

    // Signed here: checks out, and the payer is the signer.
    let key = PrivateKeySigner::random();
    let mine = sign_permit2_payment_raw(&LocalSigner(key.clone()), &req, None).await.unwrap();
    assert_eq!(check_payload_offline(&mine, &req).unwrap(), key.address().to_string());
}

#[test]
fn choosing_what_to_pay() {
    let req = |asset: &str, amount: &str| requirements_for("0x4444444444444444444444444444444444444444", &[PriceOption { asset: asset.into(), amount: U256::from_str_radix(amount, 10).unwrap() }], None).remove(0);
    let required = payment_required(ResourceInfo { url: "u".into(), ..Default::default() }, vec![req(BSC_USDT, "5"), req(BSC_USDC, "3"), req("0xabcdefabcdefabcdefabcdefabcdefabcdefabcd", "1")], None);
    let mut policy = SpendPolicy::default();
    policy.max_per_request.insert(BSC_USDT.to_lowercase(), U256::from(10u8));
    assert_eq!(choose_requirement(&required, &policy).unwrap().amount, "5");
    policy.max_per_request.insert(BSC_USDC.into(), U256::from(10u8));
    assert_eq!(choose_requirement(&required, &policy).unwrap().amount, "3");
    policy.max_per_request.clear();
    policy.max_per_request.insert(BSC_USDC.into(), U256::from(2u8));
    policy.max_per_request.insert(BSC_USDT.into(), U256::from(4u8));
    assert!(choose_requirement(&required, &policy).is_none());
}

/// A facilitator that checks payments offline and settles without a chain.
struct Mock {
    settle_ok: bool,
}

#[async_trait::async_trait]
impl Facilitator for Mock {
    async fn verify(&self, payload: &PaymentPayload, req: &PaymentRequirements) -> orvyn_core::Result<VerifyResponse> {
        Ok(match check_payload_offline(payload, req) {
            Ok(payer) => VerifyResponse { is_valid: true, invalid_reason: None, payer: Some(payer) },
            Err(e) => VerifyResponse { is_valid: false, invalid_reason: Some(e.reason), payer: None },
        })
    }
    async fn settle(&self, _: &PaymentPayload, req: &PaymentRequirements) -> orvyn_core::Result<SettlementResponse> {
        Ok(SettlementResponse { success: self.settle_ok, error_reason: (!self.settle_ok).then(|| "insufficient_funds".into()), payer: None, transaction: "0xabc".into(), network: req.network.clone(), amount: None })
    }
}

#[tokio::test]
async fn the_paywall_admits_paid_requests_and_settles_after() {
    let _clock = CLOCK.lock().await;
    support::test_env(&[]);
    let v: Value = serde_json::from_str(include_str!("vectors/x402.json")).unwrap();
    set_now_for_tests(v["nowMs"].as_i64().unwrap());
    let price = [PriceOption { asset: BSC_USDT.into(), amount: U256::from(2u64) * U256::from(10u64).pow(U256::from(18u8)) }];
    let header = v["header"].as_str().unwrap();
    let wall = |settle_ok: bool| Paywall::new("0x4444444444444444444444444444444444444444", &price, std::sync::Arc::new(Mock { settle_ok })).description("Daily market report");

    // No payment: a 402 offering the price.
    let Gate::PaymentRequired(r) = wall(true).admit("https://example.com/report", None).await.unwrap() else { panic!("not a 402") };
    assert_eq!(r.accepts.len(), 1);
    assert_eq!(r.resource.description.as_deref(), Some("Daily market report"));
    assert!(r.error.is_none());
    // A payment for a different offer: refused.
    let other = Paywall::new("0x5555555555555555555555555555555555555555", &price, std::sync::Arc::new(Mock { settle_ok: true }));
    let Gate::PaymentRequired(r) = other.admit("https://example.com/report", Some(header)).await.unwrap() else { panic!("admitted a payment for someone else") };
    assert!(r.error.unwrap().contains("doesn't match"));
    // Paid: admitted with the payer, then settled.
    let Gate::Admitted(a) = wall(true).admit("https://example.com/report", Some(header)).await.unwrap() else { panic!("not admitted") };
    assert_eq!(a.payer.to_lowercase(), v["payer"].as_str().unwrap().to_lowercase());
    let settled = wall(true).settle(a).await.unwrap().expect("settled");
    assert!(settled.success && decode_header::<SettlementResponse>(Some(&encode_header(&settled))).is_some());
    // A failed settlement becomes a 402 with the reason.
    let Gate::Admitted(a) = wall(false).admit("https://example.com/report", Some(header)).await.unwrap() else { panic!("not admitted") };
    assert_eq!(wall(false).settle(a).await.unwrap().unwrap_err().error.as_deref(), Some("insufficient_funds"));
    // A payer check can refuse.
    let picky = wall(true).payer_check(std::sync::Arc::new(|_payer: String| Box::pin(async { false }) as futures_util::future::BoxFuture<'static, bool>));
    let Gate::PaymentRequired(r) = picky.admit("https://example.com/report", Some(header)).await.unwrap() else { panic!("payer check ignored") };
    assert_eq!(r.error.as_deref(), Some("payer_not_allowed"));
}
