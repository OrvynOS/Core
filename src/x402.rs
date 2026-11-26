//! x402 v2 for Orvyn agents on BNB Smart Chain: pay for HTTP resources (client) and sell them
//! (server), with the `exact` scheme over Permit2 (BSC stablecoins lack EIP-3009).
//!
//! The payer signs a Permit2 `PermitWitnessTransferFrom` whose spender is the x402 exact Permit2
//! proxy and whose witness pins the receiver and `validAfter`. The proxy's `settle` pulls the funds
//! from the payer straight to the receiver; the facilitator only pays gas.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use alloy::primitives::{Address, Bytes, B256, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::SolCall;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::evm::{self, rpc_error};
use crate::wallet_policy::guard::{GuardedWallet, RawSigner};
use crate::wallet_policy::{now_ms, TypedData};
use crate::wallets::LocalSigner;

pub const X402_VERSION: u8 = 2;
pub const HEADER_PAYMENT_REQUIRED: &str = "PAYMENT-REQUIRED";
pub const HEADER_PAYMENT_SIGNATURE: &str = "PAYMENT-SIGNATURE";
pub const HEADER_PAYMENT_RESPONSE: &str = "PAYMENT-RESPONSE";
pub const BSC_NETWORK: &str = "eip155:56";
pub const BSC_USDT: &str = "0x55d398326f99059fF775485246999027B3197955";
pub const BSC_USDC: &str = "0x8AC76a51cc950d9822d68b83fE1Ad97B32Cd580d";
/// Uniswap Permit2, same address on every chain.
pub const PERMIT2: &str = "0x000000000022D473030F116dDEE9F6B43aC78BA3";

/// The x402 exact Permit2 proxy (X402_PERMIT2_PROXY overrides), checksummed.
pub fn x402_permit2_proxy() -> Address {
    crate::wallet_policy::x402::x402_permit2_proxy().parse().expect("proxy address")
}

// ---------- Wire types ----------

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ResourceInfo {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PaymentRequirements {
    pub scheme: String,
    /// CAIP-2, e.g. "eip155:56".
    pub network: String,
    /// Atomic units, decimal.
    pub amount: String,
    pub asset: String,
    pub pay_to: String,
    pub max_timeout_seconds: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<serde_json::Map<String, Value>>,
}

impl PaymentRequirements {
    pub fn is_permit2(&self) -> bool {
        self.scheme == "exact" && self.extra.as_ref().and_then(|e| e.get("assetTransferMethod")).and_then(Value::as_str) == Some("permit2")
    }
    pub fn amount_u256(&self) -> Result<U256> {
        U256::from_str_radix(&self.amount, 10).map_err(|_| Error::invalid("invalid amount"))
    }
    fn same(&self, b: &PaymentRequirements) -> bool {
        self.scheme == b.scheme && self.network == b.network && self.asset.eq_ignore_ascii_case(&b.asset) && self.pay_to.eq_ignore_ascii_case(&b.pay_to) && self.amount == b.amount
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaymentRequired {
    pub x402_version: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub resource: ResourceInfo,
    pub accepts: Vec<PaymentRequirements>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TokenAmount {
    pub token: String,
    pub amount: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WitnessData {
    pub to: String,
    pub valid_after: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Permit2Authorization {
    pub from: String,
    pub permitted: TokenAmount,
    pub spender: String,
    pub nonce: String,
    pub deadline: String,
    pub witness: WitnessData,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Permit2Payload {
    pub signature: String,
    pub permit2_authorization: Permit2Authorization,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaymentPayload {
    pub x402_version: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<ResourceInfo>,
    pub accepted: PaymentRequirements,
    pub payload: Option<Permit2Payload>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct VerifyResponse {
    pub is_valid: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payer: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SettlementResponse {
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payer: Option<String>,
    pub transaction: String,
    pub network: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount: Option<String>,
}

/// An x402 failure with its spec reason code.
#[derive(Debug, Clone)]
pub struct X402Error {
    pub message: String,
    pub reason: String,
}

impl From<X402Error> for Error {
    fn from(e: X402Error) -> Self {
        Error::payment_required(format!("{} ({})", e.message, e.reason))
    }
}

fn fail<T>(message: &str, reason: &str) -> std::result::Result<T, X402Error> {
    Err(X402Error { message: message.into(), reason: reason.into() })
}

pub fn encode_header<T: Serialize>(value: &T) -> String {
    B64.encode(serde_json::to_vec(value).unwrap())
}

pub fn decode_header<T: for<'de> Deserialize<'de>>(header: Option<&str>) -> Option<T> {
    serde_json::from_slice(&B64.decode(header?).ok()?).ok()
}

// ---------- Permit2 ----------

sol! {
    struct TokenPermissions { address token; uint256 amount; }
    struct PermitTransferFrom { TokenPermissions permitted; uint256 nonce; uint256 deadline; }
    struct Witness { address to; uint256 validAfter; }
    interface X402Proxy {
        function settle(PermitTransferFrom permit, address owner, Witness witness, bytes signature);
    }
    interface Permit2Nonces {
        function nonceBitmap(address owner, uint256 wordPos) view returns (uint256);
    }
}

fn now_secs() -> u64 {
    (now_ms() / 1000) as u64
}

/// The EIP-712 data a payer signs for an authorization.
pub fn typed_data(auth: &Permit2Authorization, chain_id: u64) -> TypedData {
    serde_json::from_value(json!({
        "domain": { "name": "Permit2", "chainId": chain_id, "verifyingContract": PERMIT2 },
        "types": {
            "PermitWitnessTransferFrom": [
                { "name": "permitted", "type": "TokenPermissions" }, { "name": "spender", "type": "address" },
                { "name": "nonce", "type": "uint256" }, { "name": "deadline", "type": "uint256" }, { "name": "witness", "type": "Witness" }
            ],
            "TokenPermissions": [{ "name": "token", "type": "address" }, { "name": "amount", "type": "uint256" }],
            "Witness": [{ "name": "to", "type": "address" }, { "name": "validAfter", "type": "uint256" }]
        },
        "primaryType": "PermitWitnessTransferFrom",
        "message": {
            "permitted": { "token": auth.permitted.token, "amount": auth.permitted.amount },
            "spender": auth.spender, "nonce": auth.nonce, "deadline": auth.deadline,
            "witness": { "to": auth.witness.to, "validAfter": auth.witness.valid_after }
        }
    }))
    .unwrap()
}

/// The EIP-712 signing hash of typed data.
pub fn typed_data_hash(typed: &TypedData) -> Result<B256> {
    let dynamic: alloy::dyn_abi::TypedData = serde_json::from_value(serde_json::to_value(typed)?).map_err(|e| Error::invalid(format!("typed data: {e}")))?;
    dynamic.eip712_signing_hash().map_err(|e| Error::invalid(format!("typed data: {e}")))
}

fn chain_for(network: &str) -> std::result::Result<u64, X402Error> {
    if network == BSC_NETWORK { Ok(56) } else { fail(&format!("Unsupported network {network}"), "invalid_network") }
}

fn checksum(a: &str) -> String {
    a.parse::<Address>().map(|x| x.to_string()).unwrap_or_else(|_| a.to_string())
}

/// Sign a payment for `requirements` with a wallet (an agent's, through its policy).
pub async fn sign_permit2_payment(wallet: &GuardedWallet, requirements: &PaymentRequirements, resource: Option<ResourceInfo>) -> Result<PaymentPayload> {
    sign_with(|t| async move { wallet.sign_typed_data(&t).await }, &wallet.address(), requirements, resource).await
}

/// Sign a payment with any signer (no policy: Orvyn's own keys and tests).
pub async fn sign_permit2_payment_raw(signer: &dyn RawSigner, requirements: &PaymentRequirements, resource: Option<ResourceInfo>) -> Result<PaymentPayload> {
    sign_with(|t| async move { signer.sign_typed_data(&t).await }, &signer.address(), requirements, resource).await
}

async fn sign_with<F, Fut>(sign: F, address: &str, requirements: &PaymentRequirements, resource: Option<ResourceInfo>) -> Result<PaymentPayload>
where
    F: FnOnce(TypedData) -> Fut,
    Fut: std::future::Future<Output = Result<String>>,
{
    if !requirements.is_permit2() {
        return Err(X402Error { message: "Not an exact/permit2 requirement".into(), reason: "unsupported_scheme".into() }.into());
    }
    let chain_id = chain_for(&requirements.network)?;
    let t = now_secs();
    let auth = Permit2Authorization {
        from: checksum(address),
        permitted: TokenAmount { token: checksum(&requirements.asset), amount: requirements.amount.clone() },
        spender: x402_permit2_proxy().to_string(),
        nonce: U256::from_be_bytes(rand::random::<[u8; 32]>()).to_string(),
        deadline: (t + requirements.max_timeout_seconds).to_string(),
        // A little slack for clock skew between payer and chain.
        witness: WitnessData { to: checksum(&requirements.pay_to), valid_after: t.saturating_sub(60).to_string() },
    };
    let signature = sign(typed_data(&auth, chain_id)).await?;
    let mut accepted = requirements.clone();
    accepted.extra.get_or_insert_with(Default::default).insert("assetTransferMethod".into(), "permit2".into());
    Ok(PaymentPayload { x402_version: 2, resource, accepted, payload: Some(Permit2Payload { signature, permit2_authorization: auth }), extensions: None })
}

fn big(s: &str, reason: &str) -> std::result::Result<U256, X402Error> {
    U256::from_str_radix(s, 10).or_else(|_| fail("Malformed number", reason))
}

/// Checks that need no chain access: the payload matches the requirements and the signature
/// recovers to `from`. Returns the payer (checksummed).
pub fn check_payload_offline(payload: &PaymentPayload, req: &PaymentRequirements) -> std::result::Result<String, X402Error> {
    if payload.x402_version != 2 {
        return fail("x402Version must be 2", "invalid_x402_version");
    }
    if !req.is_permit2() {
        return fail("Requirement isn't exact/permit2", "unsupported_scheme");
    }
    let a = &payload.accepted;
    if a.scheme != req.scheme || a.network != req.network {
        return fail("Scheme or network mismatch", "invalid_network");
    }
    let Some(p) = &payload.payload else { return fail("Missing permit2Authorization", "invalid_exact_evm_payload") };
    let auth = &p.permit2_authorization;
    let eq = |x: &str, y: &str| x.eq_ignore_ascii_case(y);
    if !eq(&auth.permitted.token, &req.asset) {
        return fail("Token mismatch", "invalid_exact_evm_payload_asset_mismatch");
    }
    if !eq(&auth.witness.to, &req.pay_to) {
        return fail("Recipient mismatch", "invalid_exact_evm_payload_recipient_mismatch");
    }
    if !eq(&auth.spender, &x402_permit2_proxy().to_string()) {
        return fail("Spender isn't the x402 Permit2 proxy", "invalid_exact_evm_payload_spender");
    }
    if big(&auth.permitted.amount, "invalid_exact_evm_payload_amount")? < big(&req.amount, "invalid_exact_evm_payload_amount")? {
        return fail("Amount below the requirement", "invalid_exact_evm_payload_amount");
    }
    let t = U256::from(now_secs());
    if big(&auth.deadline, "invalid_exact_evm_payload_deadline")? <= t + U256::from(6u8) {
        return fail("Authorization expired", "invalid_exact_evm_payload_deadline");
    }
    if big(&auth.witness.valid_after, "invalid_exact_evm_payload_valid_after")? > t {
        return fail("Authorization not yet valid", "invalid_exact_evm_payload_valid_after");
    }
    let chain_id = chain_for(&req.network)?;
    let hash = typed_data_hash(&typed_data(auth, chain_id)).or_else(|_| fail("Malformed authorization", "invalid_exact_evm_payload"))?;
    let sig = hex::decode(p.signature.trim_start_matches("0x")).unwrap_or_default();
    let signer = evm::sig::recover(hash, &sig).map(|s| s.to_string());
    if !signer.is_some_and(|s| eq(&s, &auth.from)) {
        return fail("Signature doesn't match the payer", "invalid_exact_evm_payload_signature");
    }
    Ok(checksum(&auth.from))
}

/// Full verification: offline checks, then Permit2 allowance, balance and nonce on chain.
pub async fn verify_permit2_payment(payload: &PaymentPayload, req: &PaymentRequirements) -> Result<VerifyResponse> {
    let payer = match check_payload_offline(payload, req) {
        Ok(p) => p,
        Err(e) => return Ok(VerifyResponse { is_valid: false, invalid_reason: Some(e.reason), payer: None }),
    };
    let auth = &payload.payload.as_ref().unwrap().permit2_authorization;
    let amount = U256::from_str_radix(&auth.permitted.amount, 10).unwrap_or_default();
    let nonce = U256::from_str_radix(&auth.nonce, 10).unwrap_or_default();
    let (owner, asset) = (evm::parse_address(&payer)?, evm::parse_address(&req.asset)?);
    let p = evm::bsc();
    let token = evm::IERC20::new(asset, p.clone());
    let permit2: Address = PERMIT2.parse().unwrap();
    let allowance = token.allowance(owner, permit2).call().await.map_err(rpc_error)?;
    let balance = token.balanceOf(owner).call().await.map_err(rpc_error)?;
    let bitmap: U256 = {
        let data = Permit2Nonces::nonceBitmapCall { owner, wordPos: nonce >> 8 }.abi_encode();
        let out = p.call(TransactionRequest::default().to(permit2).input(Bytes::from(data).into())).await.map_err(rpc_error)?;
        Permit2Nonces::nonceBitmapCall::abi_decode_returns(&out).map_err(rpc_error)?
    };
    let reason = if allowance < amount {
        Some("permit2_allowance_required")
    } else if balance < amount {
        Some("insufficient_funds")
    } else if (bitmap >> (nonce & U256::from(255u8)).to::<usize>()) & U256::from(1u8) == U256::from(1u8) {
        Some("invalid_exact_evm_payload_nonce")
    } else {
        None
    };
    Ok(VerifyResponse { is_valid: reason.is_none(), invalid_reason: reason.map(String::from), payer: Some(payer) })
}

fn settle_call(payload: &PaymentPayload) -> Result<(Address, Bytes)> {
    let p = payload.payload.as_ref().ok_or_else(|| Error::invalid("no payload"))?;
    let a = &p.permit2_authorization;
    let n = |s: &str| U256::from_str_radix(s, 10).map_err(|_| Error::invalid("bad number"));
    let call = X402Proxy::settleCall {
        permit: PermitTransferFrom { permitted: TokenPermissions { token: evm::parse_address(&a.permitted.token)?, amount: n(&a.permitted.amount)? }, nonce: n(&a.nonce)?, deadline: n(&a.deadline)? },
        owner: evm::parse_address(&a.from)?,
        witness: Witness { to: evm::parse_address(&a.witness.to)?, validAfter: n(&a.witness.valid_after)? },
        signature: hex::decode(p.signature.trim_start_matches("0x")).map_err(|_| Error::invalid("bad signature"))?.into(),
    };
    Ok((x402_permit2_proxy(), call.abi_encode().into()))
}

/// Settle a verified payment by calling the proxy from `gas_payer` (it only pays gas).
pub async fn settle_permit2_payment(gas_payer: &PrivateKeySigner, payload: &PaymentPayload, req: &PaymentRequirements) -> Result<SettlementResponse> {
    let verified = verify_permit2_payment(payload, req).await?;
    let base = |success: bool, reason: Option<&str>, tx: String| SettlementResponse {
        success,
        error_reason: reason.map(String::from),
        payer: verified.payer.clone(),
        transaction: tx,
        network: req.network.clone(),
        amount: None,
    };
    if !verified.is_valid {
        return Ok(base(false, verified.invalid_reason.as_deref(), String::new()));
    }
    let (to, data) = settle_call(payload)?;
    let p = evm::bsc();
    let from = gas_payer.address();
    if p.call(TransactionRequest::default().from(from).to(to).input(data.clone().into())).await.is_err() {
        return Ok(base(false, Some("settlement_simulation_failed"), String::new()));
    }
    let tx = crate::wallets::prepare(&format!("{from:#x}"), to, data, U256::ZERO).await?;
    let signed = LocalSigner(gas_payer.clone()).sign_transaction(&tx).await?;
    let pending = p.send_raw_transaction(&hex::decode(signed.trim_start_matches("0x")).unwrap()).await.map_err(rpc_error)?;
    let hash = format!("{:#x}", pending.tx_hash());
    let receipt = pending.with_timeout(Some(Duration::from_secs(120))).get_receipt().await.map_err(rpc_error)?;
    if !receipt.status() {
        return Ok(base(false, Some("settlement_reverted"), hash));
    }
    let mut ok = base(true, None, hash);
    ok.amount = Some(payload.payload.as_ref().unwrap().permit2_authorization.permitted.amount.clone());
    Ok(ok)
}

// ---------- Payer ----------

/// What an agent will pay per request, per token (atomic units). Tokens not listed are never paid.
#[derive(Debug, Clone, Default)]
pub struct SpendPolicy {
    pub max_per_request: HashMap<String, U256>,
    /// When Permit2 isn't approved yet, approve it for this much (at least the payment); none:
    /// refuse instead.
    pub approve_up_to: HashMap<String, U256>,
}

fn lookup(map: &HashMap<String, U256>, token: &str) -> Option<U256> {
    map.iter().find(|(k, _)| k.eq_ignore_ascii_case(token)).map(|(_, v)| *v)
}

/// The cheapest offer this payer can accept: exact/permit2 on BSC, a listed token, within limits.
pub fn choose_requirement(required: &PaymentRequired, policy: &SpendPolicy) -> Option<PaymentRequirements> {
    let mut options: Vec<&PaymentRequirements> = required
        .accepts
        .iter()
        .filter(|r| r.is_permit2() && r.network == BSC_NETWORK && lookup(&policy.max_per_request, &r.asset).zip(r.amount_u256().ok()).is_some_and(|(limit, a)| a <= limit))
        .collect();
    options.sort_by_key(|r| r.amount_u256().unwrap_or(U256::MAX));
    options.first().map(|r| (*r).clone())
}

pub struct Paid {
    pub requirement: PaymentRequirements,
    pub settlement: Option<SettlementResponse>,
}

/// GET/POST that pays x402 challenges from an agent's wallet (through its policy). The request is
/// sent twice when payment is needed. `confirm` has the final say (budgets, logging).
pub async fn fetch_with_x402<C, Fut>(
    wallet: &GuardedWallet,
    evm_wallet: &Value,
    policy: &SpendPolicy,
    request: reqwest::RequestBuilder,
    confirm: C,
) -> Result<(reqwest::Response, Option<Paid>)>
where
    C: FnOnce(PaymentRequirements, PaymentRequired) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let retry = request.try_clone().ok_or_else(|| Error::invalid("the request body can't be replayed"))?;
    let first = request.send().await.map_err(|e| Error::internal(format!("fetch: {e}")))?;
    if first.status().as_u16() != 402 {
        return Ok((first, None));
    }
    let header: Option<PaymentRequired> = decode_header(first.headers().get(HEADER_PAYMENT_REQUIRED).and_then(|h| h.to_str().ok()));
    let required = match header {
        Some(r) => Some(r),
        None => first.json::<PaymentRequired>().await.ok(),
    };
    let Some(required) = required.filter(|r| !r.accepts.is_empty()) else {
        return Err(X402Error { message: "402 without payment requirements".into(), reason: "invalid_payment_required".into() }.into());
    };
    let Some(requirement) = choose_requirement(&required, policy) else {
        let offered: Vec<String> = required
            .accepts
            .iter()
            .map(|r| {
                let method = r.extra.as_ref().and_then(|e| e.get("assetTransferMethod")).and_then(Value::as_str).unwrap_or("eip3009");
                format!("{} of {} via {}/{method} on {}", r.amount, r.asset, r.scheme, r.network)
            })
            .collect();
        return Err(X402Error { message: format!("No acceptable payment option (offered: {})", offered.join("; ")), reason: "no_acceptable_requirement".into() }.into());
    };
    if !confirm(requirement.clone(), required.clone()).await {
        return Err(X402Error { message: "Payment refused by policy".into(), reason: "refused_by_policy".into() }.into());
    }
    let amount = requirement.amount_u256()?;
    let owner = wallet.address();
    let asset = evm::parse_address(&requirement.asset)?;
    let permit2: Address = PERMIT2.parse().unwrap();
    let allowance = evm::IERC20::new(asset, evm::bsc()).allowance(evm::parse_address(&owner)?, permit2).call().await.map_err(rpc_error)?;
    if allowance < amount {
        let Some(up_to) = lookup(&policy.approve_up_to, &requirement.asset) else {
            return Err(X402Error { message: format!("Permit2 isn't approved for {}", requirement.asset), reason: "permit2_allowance_required".into() }.into());
        };
        crate::wallets::ensure_allowance(&wallet.agent_id, evm_wallet, &owner, asset, permit2, up_to.max(amount)).await?;
    }
    let payment = sign_permit2_payment(wallet, &requirement, Some(required.resource.clone())).await?;
    let response = retry.header(HEADER_PAYMENT_SIGNATURE, encode_header(&payment)).send().await.map_err(|e| Error::internal(format!("fetch: {e}")))?;
    let settlement: Option<SettlementResponse> = decode_header(response.headers().get(HEADER_PAYMENT_RESPONSE).and_then(|h| h.to_str().ok()));
    if response.status().as_u16() == 402 {
        let reason = settlement.and_then(|s| s.error_reason).unwrap_or_else(|| "payment_rejected".into());
        return Err(X402Error { message: format!("Payment rejected: {reason}"), reason }.into());
    }
    Ok((response, Some(Paid { requirement, settlement })))
}

// ---------- Seller (the paywall's decisions; the HTTP server applies them) ----------

#[derive(Debug, Clone)]
pub struct PriceOption {
    pub asset: String,
    pub amount: U256,
}

pub fn requirements_for(pay_to: &str, price: &[PriceOption], max_timeout_seconds: Option<u64>) -> Vec<PaymentRequirements> {
    price
        .iter()
        .map(|p| PaymentRequirements {
            scheme: "exact".into(),
            network: BSC_NETWORK.into(),
            amount: p.amount.to_string(),
            asset: checksum(&p.asset),
            pay_to: checksum(pay_to),
            max_timeout_seconds: max_timeout_seconds.unwrap_or(300),
            extra: Some(serde_json::Map::from_iter([("assetTransferMethod".to_string(), json!("permit2"))])),
        })
        .collect()
}

/// A 402 body (also sent base64 in PAYMENT-REQUIRED).
pub fn payment_required(resource: ResourceInfo, accepts: Vec<PaymentRequirements>, error: Option<&str>) -> PaymentRequired {
    PaymentRequired { x402_version: X402_VERSION, error: error.map(String::from), resource, accepts, extensions: None }
}

/// The offer a payment claims to accept, if it's one of ours.
pub fn matching_requirement<'a>(payload: &PaymentPayload, accepts: &'a [PaymentRequirements]) -> Option<&'a PaymentRequirements> {
    accepts.iter().find(|r| r.same(&payload.accepted))
}

/// A facilitator: verifies and settles payments.
#[async_trait::async_trait]
pub trait Facilitator: Send + Sync {
    async fn verify(&self, payload: &PaymentPayload, req: &PaymentRequirements) -> Result<VerifyResponse>;
    async fn settle(&self, payload: &PaymentPayload, req: &PaymentRequirements) -> Result<SettlementResponse>;
}

/// A standard x402 facilitator over HTTP (POST /verify, POST /settle).
pub struct RemoteFacilitator {
    pub base_url: String,
    pub bearer: Option<String>,
}

impl RemoteFacilitator {
    /// Orvyn's hosted facilitator (/api/x402 on the API), with the internal key.
    pub fn orvyn() -> Self {
        let e = crate::env::env();
        Self { base_url: format!("{}/api/x402/", e.orvyn_api_url.trim_end_matches('/')), bearer: e.orvyn_internal_api_key.clone() }
    }
    async fn post<T: for<'de> Deserialize<'de>>(&self, path: &str, payload: &PaymentPayload, req: &PaymentRequirements) -> Result<T> {
        let base = if self.base_url.ends_with('/') { self.base_url.clone() } else { format!("{}/", self.base_url) };
        let mut r = crate::http::client().post(format!("{base}{path}")).json(&json!({ "x402Version": X402_VERSION, "paymentPayload": payload, "paymentRequirements": req }));
        if let Some(b) = &self.bearer {
            r = r.bearer_auth(b);
        }
        let res = r.send().await.map_err(|e| Error::internal(format!("facilitator: {e}")))?;
        if !res.status().is_success() && res.status().as_u16() != 402 {
            return Err(Error::internal(format!("Facilitator {path} answered {}", res.status())));
        }
        res.json().await.map_err(|e| Error::internal(format!("facilitator: {e}")))
    }
}

#[async_trait::async_trait]
impl Facilitator for RemoteFacilitator {
    async fn verify(&self, payload: &PaymentPayload, req: &PaymentRequirements) -> Result<VerifyResponse> {
        self.post("verify", payload, req).await
    }
    async fn settle(&self, payload: &PaymentPayload, req: &PaymentRequirements) -> Result<SettlementResponse> {
        self.post("settle", payload, req).await
    }
}

/// Binance Agent ID (`name.ai`) registry on BSC.
pub fn agent_id_registry() -> String {
    std::env::var("ORVYN_AGENT_ID_REGISTRY").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "0x60B9262456dE011F7d15a54CdD8c48bAaa8B022F".into())
}

/// Whether `payer` holds a Binance Agent ID (for Agent ID-only services).
pub async fn agent_id_holder(payer: &str) -> bool {
    let (Ok(registry), Ok(owner)) = (evm::parse_address(&agent_id_registry()), evm::parse_address(payer)) else { return false };
    evm::IERC20::new(registry, evm::bsc()).balanceOf(owner).call().await.map(|b| b > U256::ZERO).unwrap_or(false)
}

// ---------- Orvyn's facilitator ----------

pub struct FacilitatorConfig {
    pub keys: Vec<String>,
    pub assets: Vec<String>,
    pub max_amount: U256,
    pub min_gas_wei: U256,
}

pub fn facilitator_config_from_env() -> FacilitatorConfig {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let ether = |v: Option<String>, d: &str| alloy::primitives::utils::parse_ether(&v.unwrap_or_else(|| d.into())).unwrap_or_default();
    FacilitatorConfig {
        keys: var("X402_FACILITATOR_KEYS").unwrap_or_default().split(',').map(|k| k.trim().to_string()).filter(|k| !k.is_empty()).collect(),
        assets: var("X402_FACILITATOR_ASSETS")
            .unwrap_or_else(|| format!("{BSC_USDT},{BSC_USDC}"))
            .split(',')
            .map(|a| a.trim().to_lowercase())
            .filter(|a| !a.is_empty())
            .collect(),
        max_amount: ether(var("X402_FACILITATOR_MAX_AMOUNT"), "1000"),
        min_gas_wei: ether(var("X402_FACILITATOR_MIN_GAS_BNB"), "0.005"),
    }
}

struct GasWallet {
    key: PrivateKeySigner,
    /// Settlements on one wallet run one at a time, so its nonces never collide.
    lock: tokio::sync::Mutex<()>,
    pending: std::sync::atomic::AtomicUsize,
}

/// Orvyn's x402 facilitator: verifies payments and settles them from a pool of gas wallets.
pub struct OrvynFacilitator {
    config: FacilitatorConfig,
    wallets: Vec<Arc<GasWallet>>,
    /// Settlements by payer+nonce, so a retried /settle doesn't pay gas twice.
    settled: Mutex<HashMap<String, (SettlementResponse, i64)>>,
    in_flight: tokio::sync::Mutex<()>,
}

impl OrvynFacilitator {
    pub fn new(config: FacilitatorConfig) -> Result<Self> {
        let wallets = config
            .keys
            .iter()
            .map(|k| k.parse::<PrivateKeySigner>().map(|key| Arc::new(GasWallet { key, lock: Default::default(), pending: Default::default() })))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| Error::unavailable("X402_FACILITATOR_KEYS has a bad key"))?;
        Ok(Self { config, wallets, settled: Default::default(), in_flight: Default::default() })
    }
    pub fn enabled(&self) -> bool {
        !self.wallets.is_empty()
    }

    /// GET /supported
    pub fn supported(&self) -> Value {
        json!({
            "kinds": [{ "x402Version": X402_VERSION, "scheme": "exact", "network": BSC_NETWORK, "extra": { "assetTransferMethod": "permit2" } }],
            "extensions": [],
            "signers": { "eip155:*": self.wallets.iter().map(|w| w.key.address().to_string()).collect::<Vec<_>>() },
        })
    }

    fn refuse(&self, req: &PaymentRequirements) -> Option<&'static str> {
        if req.network != BSC_NETWORK {
            return Some("invalid_network");
        }
        if !req.is_permit2() {
            return Some("unsupported_scheme");
        }
        if !self.config.assets.contains(&req.asset.to_lowercase()) {
            return Some("unsupported_asset");
        }
        match req.amount_u256() {
            Ok(a) if a > U256::ZERO && a <= self.config.max_amount => None,
            Ok(_) => Some("amount_out_of_range"),
            Err(_) => Some("invalid_amount"),
        }
    }

    pub async fn verify(&self, payload: &PaymentPayload, req: &PaymentRequirements) -> Result<VerifyResponse> {
        if let Some(r) = self.refuse(req) {
            return Ok(VerifyResponse { is_valid: false, invalid_reason: Some(r.into()), payer: None });
        }
        verify_permit2_payment(payload, req).await
    }

    pub async fn settle(&self, payload: &PaymentPayload, req: &PaymentRequirements) -> Result<SettlementResponse> {
        let fail = |r: &str| SettlementResponse { success: false, error_reason: Some(r.into()), payer: None, transaction: String::new(), network: req.network.clone(), amount: None };
        if let Some(r) = self.refuse(req) {
            return Ok(fail(r));
        }
        let Some(auth) = payload.payload.as_ref().map(|p| &p.permit2_authorization) else { return Ok(fail("invalid_exact_evm_payload")) };
        let key = format!("{}:{}", auth.from.to_lowercase(), auth.nonce);
        // One settlement per payment at a time; successes are kept until the authorization expires.
        let _guard = self.in_flight.lock().await;
        let now = chrono::Utc::now().timestamp_millis();
        {
            let mut settled = self.settled.lock().unwrap();
            settled.retain(|_, (_, until)| *until > now);
            if let Some((s, _)) = settled.get(&key) {
                return Ok(s.clone());
            }
        }
        if !self.enabled() {
            return Err(Error::unavailable("no gas wallets configured (X402_FACILITATOR_KEYS)"));
        }
        let code = evm::bsc().get_code_at(x402_permit2_proxy()).await.map_err(rpc_error)?;
        if code.is_empty() {
            return Err(Error::unavailable(format!("x402 Permit2 proxy {} isn't deployed on chain 56; set X402_PERMIT2_PROXY", x402_permit2_proxy())));
        }
        let mut usable = vec![];
        for w in &self.wallets {
            let bal = evm::bsc().get_balance(w.key.address()).await.unwrap_or_default();
            if bal >= self.config.min_gas_wei {
                usable.push(w.clone());
            }
        }
        usable.sort_by_key(|w| w.pending.load(std::sync::atomic::Ordering::SeqCst));
        let Some(w) = usable.first().cloned() else { return Err(Error::unavailable("all gas wallets are below the minimum BNB balance")) };
        w.pending.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let result = {
            let _one = w.lock.lock().await;
            settle_permit2_payment(&w.key, payload, req).await
        };
        w.pending.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        let result = result?;
        if result.success {
            let deadline_ms = auth.deadline.parse::<i64>().unwrap_or(0) * 1000;
            self.settled.lock().unwrap().insert(key, (result.clone(), deadline_ms.max(now) + 60_000));
        }
        Ok(result)
    }

    /// Gas wallets and their BNB, for monitoring.
    pub async fn health(&self) -> Vec<Value> {
        let mut out = vec![];
        for w in &self.wallets {
            let bal = evm::bsc().get_balance(w.key.address()).await.ok();
            out.push(json!({
                "address": w.key.address().to_string(),
                "balanceWei": bal.map(|b| b.to_string()).unwrap_or_else(|| "-1".into()),
                "pending": w.pending.load(std::sync::atomic::Ordering::SeqCst),
                "low": bal.is_none_or(|b| b < self.config.min_gas_wei),
            }));
        }
        out
    }
}

static FACILITATOR: LazyLock<Option<Arc<OrvynFacilitator>>> = LazyLock::new(|| OrvynFacilitator::new(facilitator_config_from_env()).ok().map(Arc::new));

/// The process-wide facilitator, configured from the environment.
pub fn facilitator() -> Result<Arc<OrvynFacilitator>> {
    FACILITATOR.clone().ok_or_else(|| Error::unavailable("X402_FACILITATOR_KEYS has a bad key"))
}

#[async_trait::async_trait]
impl Facilitator for OrvynFacilitator {
    async fn verify(&self, payload: &PaymentPayload, req: &PaymentRequirements) -> Result<VerifyResponse> {
        OrvynFacilitator::verify(self, payload, req).await
    }
    async fn settle(&self, payload: &PaymentPayload, req: &PaymentRequirements) -> Result<SettlementResponse> {
        OrvynFacilitator::settle(self, payload, req).await
    }
}

impl RemoteFacilitator {
    /// A facilitator at `base_url` (e.g. Orvyn's: `https://api.orvyn.cash/api/x402/`), with an
    /// API key when it needs one (Orvyn's does, for verify and settle).
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self { base_url: base_url.into(), bearer: api_key }
    }
}

// ---------- Paywall (sell a resource per request) ----------

/// Decides who may pay (e.g. [`agent_id_holder`]): payers it refuses get a 402.
pub type PayerCheck = Arc<dyn Fn(String) -> futures_util::future::BoxFuture<'static, bool> + Send + Sync>;

/// Sell a resource per request, in any HTTP framework: [`Paywall::admit`] the request, run your
/// handler, then [`Paywall::settle`] if it succeeded (failed work isn't charged).
pub struct Paywall {
    pub accepts: Vec<PaymentRequirements>,
    pub facilitator: Arc<dyn Facilitator>,
    pub description: Option<String>,
    pub mime_type: Option<String>,
    pub payer_check: Option<PayerCheck>,
}

/// What to do with a request.
pub enum Gate {
    /// Answer 402 with this (as JSON, and base64 in the `PAYMENT-REQUIRED` header).
    PaymentRequired(PaymentRequired),
    /// Paid (verified, not yet settled): serve it, then settle.
    Admitted(Admitted),
}

pub struct Admitted {
    pub payer: String,
    resource: ResourceInfo,
    payload: PaymentPayload,
    requirement: PaymentRequirements,
}

impl Paywall {
    /// Sell for `price` (any of the options), paid to `pay_to`, through `facilitator`.
    pub fn new(pay_to: &str, price: &[PriceOption], facilitator: Arc<dyn Facilitator>) -> Self {
        Self { accepts: requirements_for(pay_to, price, None), facilitator, description: None, mime_type: None, payer_check: None }
    }
    pub fn description(mut self, d: impl Into<String>) -> Self {
        self.description = Some(d.into());
        self
    }
    pub fn mime_type(mut self, m: impl Into<String>) -> Self {
        self.mime_type = Some(m.into());
        self
    }
    pub fn payer_check(mut self, check: PayerCheck) -> Self {
        self.payer_check = Some(check);
        self
    }

    fn required(&self, resource: ResourceInfo, error: Option<&str>) -> PaymentRequired {
        payment_required(resource, self.accepts.clone(), error)
    }

    /// `url`: the resource's URL; `payment_signature`: the request's `PAYMENT-SIGNATURE` header.
    pub async fn admit(&self, url: &str, payment_signature: Option<&str>) -> Result<Gate> {
        let resource = ResourceInfo { url: url.into(), description: self.description.clone(), mime_type: self.mime_type.clone(), ..Default::default() };
        let Some(payload) = decode_header::<PaymentPayload>(payment_signature) else {
            return Ok(Gate::PaymentRequired(self.required(resource, None)));
        };
        let Some(requirement) = matching_requirement(&payload, &self.accepts).cloned() else {
            return Ok(Gate::PaymentRequired(self.required(resource, Some("accepted requirement doesn't match any offer"))));
        };
        let verified = self.facilitator.verify(&payload, &requirement).await?;
        let Some(payer) = verified.payer.filter(|_| verified.is_valid) else {
            return Ok(Gate::PaymentRequired(self.required(resource, Some(verified.invalid_reason.as_deref().unwrap_or("invalid_payment")))));
        };
        if let Some(check) = &self.payer_check {
            if !check(payer.clone()).await {
                return Ok(Gate::PaymentRequired(self.required(resource, Some("payer_not_allowed"))));
            }
        }
        Ok(Gate::Admitted(Admitted { payer, resource, payload, requirement }))
    }

    /// Settle an admitted payment (after the handler succeeded). Ok: send the settlement base64 in
    /// the `PAYMENT-RESPONSE` header ([`encode_header`]). Err: answer 402 with it instead.
    pub async fn settle(&self, admitted: Admitted) -> Result<std::result::Result<SettlementResponse, PaymentRequired>> {
        let settlement = self.facilitator.settle(&admitted.payload, &admitted.requirement).await?;
        if settlement.success {
            return Ok(Ok(settlement));
        }
        Ok(Err(self.required(admitted.resource, Some(settlement.error_reason.as_deref().unwrap_or("settlement_failed")))))
    }
}

/// A [`PayerCheck`] that only lets Binance Agent ID holders pay (and so use the service).
pub fn agent_id_holders() -> PayerCheck {
    Arc::new(|payer: String| Box::pin(async move { agent_id_holder(&payer).await }))
}
