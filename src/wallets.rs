//! Agent wallets: one EVM wallet per agent on BNB Smart Chain. It launches the agent's token (so it
//! receives the token's tax), holds its treasury, trades, and pays.
//!
//! Each is a Privy server wallet: the key lives in Privy's enclaves and is never exported. It's owned
//! by Orvyn's authorization key (a signature needs that key as well as the app secret) and carries
//! Orvyn's Privy wallet policy. Every signature first passes Orvyn's own wallet policy
//! (wallet_policy::guard). Stored metadata: { custody: "privy", walletId, accountAddress }.
//!
//! Self-hosted agents sign with their own key instead ({ custody: "local" }, AGENT_PRIVATE_KEY).

use std::time::Duration;

use alloy::consensus::{SignableTransaction, TxEip1559};
use alloy::eips::Encodable2718;
use alloy::network::TxSignerSync;
use alloy::primitives::{Address, Bytes, TxKind, B256, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::SignerSync;
use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::env::{env, HostingMode};
use crate::error::{Error, Result};
use crate::evm::{self, rpc_error};
use crate::privy;
use crate::wallet_policy::guard::{GuardedAsterWallet, GuardedWallet, RawSigner};
use crate::wallet_policy::{TxRequest, TypedData};

const DEFAULT_REQUEST_EXPIRY_MS: i64 = 15 * 60 * 1000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "custody", rename_all = "lowercase")]
pub enum WalletMetadata {
    #[serde(rename_all = "camelCase")]
    Privy { wallet_id: String, account_address: String },
    /// A self-hosted agent's key: "main" (AGENT_PRIVATE_KEY) or "aster" (AGENT_ASTER_KEY).
    #[serde(rename_all = "camelCase")]
    Local { account_address: String, #[serde(default, skip_serializing_if = "Option::is_none")] key: Option<String> },
}

impl WalletMetadata {
    pub fn from_value(v: &Value) -> Result<Self> {
        serde_json::from_value(v.clone()).map_err(|_| Error::unavailable("the agent has no usable wallet metadata"))
    }
    pub fn address(&self) -> &str {
        match self {
            Self::Privy { account_address, .. } | Self::Local { account_address, .. } => account_address,
        }
    }
}

// ---- Privy authorization signatures ----

/// RFC 8785 canonical JSON (keys sorted; the values Privy requests carry are strings, integers,
/// booleans, arrays and objects).
pub fn canonical_json(v: &Value) -> String {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            // JCS sorts by UTF-16 code units.
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            let parts: Vec<String> = keys.iter().map(|k| format!("{}:{}", Value::String((*k).clone()), canonical_json(&m[*k]))).collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(a) => format!("[{}]", a.iter().map(canonical_json).collect::<Vec<_>>().join(",")),
        other => other.to_string(),
    }
}

/// The bytes Privy expects signed for a request (an empty object body becomes "").
pub fn authorization_payload(method: &str, url: &str, body: &Value, headers: &Map<String, Value>) -> String {
    let body = match body {
        Value::Object(m) if m.is_empty() => Value::String(String::new()),
        other => other.clone(),
    };
    canonical_json(&json!({ "version": 1, "method": method, "url": url, "body": body, "headers": headers }))
}

/// P-256 signature (DER, base64) of sha256(payload) with an authorization key ("wallet-auth:" +
/// base64 PKCS#8), as Privy's SDK makes it (RFC 6979; S not normalized, as noble does for P-256).
pub fn authorization_signature(key: &str, payload: &str) -> Result<String> {
    use p256::ecdsa::signature::hazmat::PrehashSigner;
    use p256::pkcs8::DecodePrivateKey;
    let stripped: String = key.chars().filter(|c| !c.is_whitespace()).collect();
    let stripped = stripped.trim_start_matches("wallet-auth:").trim_start_matches("wallet-api:");
    let der = B64.decode(stripped).map_err(|_| Error::unavailable("PRIVY_AUTHORIZATION_KEY isn't a base64 PKCS#8 key"))?;
    let signing = p256::ecdsa::SigningKey::from_pkcs8_der(&der).map_err(|_| Error::unavailable("PRIVY_AUTHORIZATION_KEY isn't a P-256 key"))?;
    let digest = Sha256::digest(payload.as_bytes());
    let sig: p256::ecdsa::Signature = signing.sign_prehash(&digest).map_err(|_| Error::internal("couldn't sign the Privy request"))?;
    Ok(B64.encode(sig.to_der().as_bytes()))
}

/// A signed request to Privy's wallet API (authorization signature when Orvyn's key is set).
async fn privy_signed(method: reqwest::Method, path: &str, body: Value, idempotency_key: Option<&str>) -> Result<Value> {
    let (app_id, _) = privy::credentials()?;
    let url = format!("{}{}", privy::api_url(), path);
    let expiry = (chrono::Utc::now().timestamp_millis() + DEFAULT_REQUEST_EXPIRY_MS).to_string();
    let mut headers = Map::new();
    headers.insert("privy-app-id".into(), app_id.into());
    if let Some(k) = idempotency_key {
        headers.insert("privy-idempotency-key".into(), k.into());
    }
    headers.insert("privy-request-expiry".into(), expiry.clone().into());
    let mut req = privy::request(method.clone(), path)?.json(&body).header("privy-request-expiry", &expiry);
    if let Some(k) = idempotency_key {
        req = req.header("privy-idempotency-key", k);
    }
    if let Some(key) = env().privy_authorization_key.as_deref() {
        let payload = authorization_payload(method.as_str(), &url, &body, &headers);
        req = req.header("privy-authorization-signature", authorization_signature(key, &payload)?);
    }
    privy::send(req).await
}

// ---- Signers ----

/// Signs with a Privy server wallet (wallet RPC). Only the guard calls it.
pub struct PrivySigner {
    wallet_id: String,
    address: String,
}

fn hex_quantity(v: U256) -> String {
    format!("{v:#x}")
}

async fn privy_rpc(wallet_id: &str, method: &str, params: Value) -> Result<Value> {
    let body = json!({ "method": method, "chain_type": "ethereum", "params": params });
    let out = privy_signed(reqwest::Method::POST, &format!("/v1/wallets/{wallet_id}/rpc"), body, None).await?;
    Ok(out.get("data").cloned().unwrap_or(Value::Null))
}

#[async_trait]
impl RawSigner for PrivySigner {
    fn address(&self) -> String {
        self.address.to_lowercase()
    }
    async fn sign_message(&self, message: &[u8]) -> Result<String> {
        let params = match std::str::from_utf8(message) {
            Ok(text) if !text.starts_with("0x") => json!({ "message": text, "encoding": "utf-8" }),
            _ => json!({ "message": hex::encode(message), "encoding": "hex" }),
        };
        let data = privy_rpc(&self.wallet_id, "personal_sign", params).await?;
        data["signature"].as_str().map(String::from).ok_or_else(|| Error::internal("Privy returned no signature"))
    }
    async fn sign_typed_data(&self, typed: &TypedData) -> Result<String> {
        let params = json!({ "typed_data": { "domain": typed.domain, "message": typed.message, "primary_type": typed.primary_type, "types": typed.types } });
        let data = privy_rpc(&self.wallet_id, "eth_signTypedData_v4", params).await?;
        data["signature"].as_str().map(String::from).ok_or_else(|| Error::internal("Privy returned no signature"))
    }
    async fn sign_transaction(&self, tx: &TxRequest) -> Result<String> {
        // As Privy's viem adapter sends it: only the fields that are set (and non-zero).
        let mut t = Map::new();
        t.insert("type".into(), if tx.gas_price.is_some() && tx.max_fee_per_gas.is_none() { 0 } else { 2 }.into());
        if let Some(to) = tx.to {
            t.insert("to".into(), to.to_string().into());
        }
        if let Some(n) = tx.nonce.filter(|n| *n != 0) {
            t.insert("nonce".into(), n.into());
        }
        if tx.chain_id != 0 {
            t.insert("chain_id".into(), tx.chain_id.into());
        }
        if !tx.data.is_empty() {
            t.insert("data".into(), tx.data.to_string().into());
        }
        if tx.value != U256::ZERO {
            t.insert("value".into(), hex_quantity(tx.value).into());
        }
        let q = |v: u128| format!("{v:#x}");
        if let Some(g) = tx.gas.filter(|g| *g != 0) {
            t.insert("gas_limit".into(), q(g as u128).into());
        }
        if let Some(g) = tx.gas_price.filter(|g| *g != 0) {
            t.insert("gas_price".into(), q(g).into());
        }
        if let Some(g) = tx.max_fee_per_gas.filter(|g| *g != 0) {
            t.insert("max_fee_per_gas".into(), q(g).into());
        }
        if let Some(g) = tx.max_priority_fee_per_gas.filter(|g| *g != 0) {
            t.insert("max_priority_fee_per_gas".into(), q(g).into());
        }
        let data = privy_rpc(&self.wallet_id, "eth_signTransaction", json!({ "transaction": t })).await?;
        data["signed_transaction"].as_str().map(String::from).ok_or_else(|| Error::internal("Privy returned no signed transaction"))
    }
}

/// Signs with a key held here (self-hosted agents). Only the guard calls it.
pub struct LocalSigner(pub PrivateKeySigner);

fn sig_hex(sig: alloy::primitives::Signature) -> String {
    format!("0x{}", hex::encode(sig.as_bytes()))
}

#[async_trait]
impl RawSigner for LocalSigner {
    fn address(&self) -> String {
        format!("{:#x}", self.0.address())
    }
    async fn sign_message(&self, message: &[u8]) -> Result<String> {
        self.0.sign_message_sync(message).map(sig_hex).map_err(|e| Error::internal(format!("signing: {e}")))
    }
    async fn sign_typed_data(&self, typed: &TypedData) -> Result<String> {
        let value = serde_json::to_value(typed)?;
        let dynamic: alloy::dyn_abi::TypedData = serde_json::from_value(value).map_err(|e| Error::invalid(format!("typed data: {e}")))?;
        let hash: B256 = dynamic.eip712_signing_hash().map_err(|e| Error::invalid(format!("typed data: {e}")))?;
        self.0.sign_hash_sync(&hash).map(sig_hex).map_err(|e| Error::internal(format!("signing: {e}")))
    }
    async fn sign_transaction(&self, tx: &TxRequest) -> Result<String> {
        let mut t = TxEip1559 {
            chain_id: tx.chain_id,
            nonce: tx.nonce.unwrap_or(0),
            gas_limit: tx.gas.unwrap_or(0),
            max_fee_per_gas: tx.max_fee_per_gas.or(tx.gas_price).unwrap_or(0),
            max_priority_fee_per_gas: tx.max_priority_fee_per_gas.unwrap_or(0),
            to: tx.to.map(TxKind::Call).unwrap_or(TxKind::Create),
            value: tx.value,
            access_list: Default::default(),
            input: tx.data.clone(),
        };
        let sig = self.0.sign_transaction_sync(&mut t).map_err(|e| Error::internal(format!("signing: {e}")))?;
        Ok(format!("0x{}", hex::encode(t.into_signed(sig).encoded_2718())))
    }
}

fn local_key(which: Option<&str>, expect: &str) -> Result<PrivateKeySigner> {
    let (var, what) = if which == Some("aster") { ("AGENT_ASTER_KEY", "AGENT_ASTER_KEY (0x…) is required") } else { ("AGENT_PRIVATE_KEY", "AGENT_PRIVATE_KEY (0x…) is required") };
    let key = std::env::var(var).ok().filter(|k| crate::env::is_private_key(k)).ok_or_else(|| Error::unavailable(what))?;
    let signer: PrivateKeySigner = key.parse().map_err(|_| Error::unavailable(what))?;
    if format!("{:#x}", signer.address()) != expect.to_lowercase() {
        return Err(Error::unavailable("the key doesn't match the agent's wallet address"));
    }
    Ok(signer)
}

/// The signer for a wallet, without the policy in front. Only for the guarded wallets below.
fn raw_signer(meta: &WalletMetadata) -> Result<Box<dyn RawSigner>> {
    Ok(match meta {
        WalletMetadata::Privy { wallet_id, account_address } => Box::new(PrivySigner { wallet_id: wallet_id.clone(), address: account_address.clone() }),
        WalletMetadata::Local { account_address, key } => Box::new(LocalSigner(local_key(key.as_deref(), account_address)?)),
    })
}

/// The agent's main wallet. Every signature passes Orvyn's wallet policy first.
pub fn agent_wallet(agent_id: &str, evm_wallet: &Value) -> Result<GuardedWallet> {
    Ok(GuardedWallet::new(agent_id, raw_signer(&WalletMetadata::from_value(evm_wallet)?)?))
}

/// The agent's Aster API wallet. Every signature passes the Aster rules first.
pub fn aster_wallet(agent_id: &str, meta: &Value) -> Result<GuardedAsterWallet> {
    Ok(GuardedAsterWallet::new(agent_id, raw_signer(&WalletMetadata::from_value(meta)?)?))
}

// ---- Creating wallets ----

#[derive(Debug, Clone)]
pub struct CreatedWallet {
    pub evm_wallet: WalletMetadata,
    /// Lowercase.
    pub evm_address: String,
}

async fn create_privy_wallet(policy: Option<&str>, display: String, external: String) -> Result<WalletMetadata> {
    let e = env();
    let mut body = json!({ "chain_type": "ethereum", "display_name": crate::http::clip(&display, 64), "external_id": crate::http::clip(&external, 64) });
    if let Some(owner) = &e.privy_authorization_key_id {
        body["owner_id"] = owner.clone().into();
    }
    if let Some(p) = policy {
        body["policy_ids"] = json!([p]);
    }
    // Same agent, same wallet: a retried creation returns the first one.
    let wallet = privy::send(privy::request(reqwest::Method::POST, "/v1/wallets")?.header("privy-idempotency-key", &external).json(&body)).await?;
    let (Some(id), Some(address)) = (wallet["id"].as_str(), wallet["address"].as_str()) else {
        return Err(Error::internal("Privy returned no wallet"));
    };
    let address: Address = address.parse().map_err(|_| Error::internal("Privy returned a bad address"))?;
    Ok(WalletMetadata::Privy { wallet_id: id.into(), account_address: address.to_string() })
}

pub async fn create_agent_wallets(agent_id: &str) -> Result<CreatedWallet> {
    let e = env();
    if !e.privy_allow_app_owned_wallets {
        if e.privy_authorization_key_id.is_none() || e.privy_authorization_key.is_none() {
            return Err(Error::unavailable("Set PRIVY_AUTHORIZATION_KEY and PRIVY_AUTHORIZATION_KEY_ID (or PRIVY_ALLOW_APP_OWNED_WALLETS=true for development)"));
        }
        if e.privy_agent_policy_id.is_none() {
            return Err(Error::unavailable("Set PRIVY_AGENT_POLICY_ID (create it with `orvyn-admin privy-policy`)"));
        }
    }
    let meta = create_privy_wallet(e.privy_agent_policy_id.as_deref(), format!("Orvyn agent {agent_id}"), format!("orvyn-agent-{agent_id}")).await?;
    let evm_address = meta.address().to_lowercase();
    Ok(CreatedWallet { evm_wallet: meta, evm_address })
}

/// The agent's Aster API wallet: a second Privy wallet, approved on Aster to trade perps for the
/// agent's account (never to withdraw), with Orvyn's Aster wallet policy.
pub async fn create_aster_signer_wallet(agent_id: &str) -> Result<WalletMetadata> {
    let e = env();
    if e.hosting_mode == HostingMode::SelfHosted {
        let key = std::env::var("AGENT_ASTER_KEY").ok().filter(|k| crate::env::is_private_key(k));
        let Some(key) = key else { return Err(Error::unavailable("set AGENT_ASTER_KEY (a separate 0x key) to trade perps on a self-hosted agent")) };
        let signer: PrivateKeySigner = key.parse().map_err(|_| Error::unavailable("AGENT_ASTER_KEY isn't a key"))?;
        return Ok(WalletMetadata::Local { account_address: signer.address().to_string(), key: Some("aster".into()) });
    }
    if !e.privy_allow_app_owned_wallets && e.privy_aster_policy_id.is_none() {
        return Err(Error::unavailable("Set PRIVY_ASTER_POLICY_ID (create it with `orvyn-admin privy-policy`)"));
    }
    create_privy_wallet(e.privy_aster_policy_id.as_deref(), format!("Orvyn Aster {agent_id}"), format!("orvyn-aster-{agent_id}")).await
}

// ---- Sending from an agent's wallet ----

/// Fill in nonce, gas and fees for a call from `from` on BNB Chain.
pub async fn prepare(from: &str, to: Address, data: Bytes, value: U256) -> Result<TxRequest> {
    let p = evm::bsc();
    let from = evm::parse_address(from)?;
    let nonce = p.get_transaction_count(from).pending().await.map_err(rpc_error)?;
    let call = TransactionRequest::default().from(from).to(to).input(data.clone().into()).value(value);
    let gas = p.estimate_gas(call).await.map_err(rpc_error)?;
    let fees = p.estimate_eip1559_fees().await.map_err(rpc_error)?;
    Ok(TxRequest {
        chain_id: evm::BSC.chain_id,
        to: Some(to),
        value,
        data,
        nonce: Some(nonce),
        // A little headroom over the estimate, as wallets add.
        gas: Some(gas + gas / 5),
        max_fee_per_gas: Some(fees.max_fee_per_gas),
        max_priority_fee_per_gas: Some(fees.max_priority_fee_per_gas),
        gas_price: None,
        has_authorization_list: false,
    })
}

/// Sign through the policy, broadcast, and wait for success. Returns the transaction hash.
pub async fn send_and_wait(wallet: &GuardedWallet, tx: &TxRequest) -> Result<B256> {
    let signed = wallet.sign_transaction(tx).await?;
    let raw = hex::decode(signed.trim_start_matches("0x")).map_err(|_| Error::internal("bad signed transaction"))?;
    let pending = evm::bsc().send_raw_transaction(&raw).await.map_err(rpc_error)?;
    let hash = *pending.tx_hash();
    let receipt = pending.with_timeout(Some(Duration::from_secs(120))).get_receipt().await.map_err(rpc_error)?;
    if !receipt.status() {
        return Err(Error::internal(format!("transaction reverted ({hash})")));
    }
    Ok(hash)
}

/// Send a call (to, data, value) from the agent's wallet and wait for success.
pub async fn agent_send(agent_id: &str, evm_wallet: &Value, to: Address, data: Bytes, value: U256) -> Result<B256> {
    agent_send_with(agent_wallet(agent_id, evm_wallet)?, to, data, value).await
}

pub async fn agent_send_with(wallet: GuardedWallet, to: Address, data: Bytes, value: U256) -> Result<B256> {
    let tx = prepare(&wallet.address(), to, data, value).await?;
    send_and_wait(&wallet, &tx).await
}

/// Approve `spender` for at least `amount` of `token` from the agent's wallet, if needed.
pub async fn ensure_allowance(agent_id: &str, evm_wallet: &Value, owner: &str, token: Address, spender: Address, amount: U256) -> Result<Option<B256>> {
    let current = evm::IERC20::new(token, evm::bsc()).allowance(evm::parse_address(owner)?, spender).call().await.map_err(rpc_error)?;
    if current >= amount {
        return Ok(None);
    }
    use alloy::sol_types::SolCall;
    let data = evm::IERC20::approveCall { spender, amount }.abi_encode();
    agent_send(agent_id, evm_wallet, token, data.into(), U256::ZERO).await.map(Some)
}

/// Send BNB (NATIVE) or a token from the agent's wallet: only possible with the owner's grant
/// attached to `wallet` (the policy refuses transfers otherwise).
pub async fn transfer_from_agent(wallet: GuardedWallet, token: &str, to: Address, amount: U256) -> Result<B256> {
    if token.eq_ignore_ascii_case(evm::NATIVE) {
        return agent_send_with(wallet, to, Bytes::new(), amount).await;
    }
    use alloy::sol_types::SolCall;
    let data = evm::IERC20::transferCall { to, amount }.abi_encode();
    agent_send_with(wallet, evm::parse_address(token)?, data.into(), U256::ZERO).await
}

// ---- Orvyn's own keys (operator, creditor, sponsor, relayer) ----

/// Send from one of Orvyn's own keys (not an agent's: no agent policy applies) and wait.
pub async fn send_from_key(key: &str, to: Address, data: Bytes, value: U256) -> Result<B256> {
    send_from_key_with_gas(key, to, data, value, None).await
}

/// As `send_from_key`, with a fixed gas limit when estimates can't be trusted (BLS precompiles).
pub async fn send_from_key_with_gas(key: &str, to: Address, data: Bytes, value: U256, gas: Option<u64>) -> Result<B256> {
    let signer: PrivateKeySigner = key.parse().map_err(|_| Error::unavailable("bad Orvyn key"))?;
    let mut tx = prepare(&format!("{:#x}", signer.address()), to, data, value).await?;
    if gas.is_some() {
        tx.gas = gas;
    }
    let signed = LocalSigner(signer).sign_transaction(&tx).await?;
    let raw = hex::decode(signed.trim_start_matches("0x")).map_err(|_| Error::internal("bad signed transaction"))?;
    let pending = evm::bsc().send_raw_transaction(&raw).await.map_err(rpc_error)?;
    let hash = *pending.tx_hash();
    let receipt = pending.with_timeout(Some(Duration::from_secs(120))).get_receipt().await.map_err(rpc_error)?;
    if !receipt.status() {
        return Err(Error::internal(format!("transaction reverted ({hash})")));
    }
    Ok(hash)
}

/// The receipt of a transaction (logs included).
pub async fn receipt_of(hash: B256) -> Result<Option<alloy::rpc::types::TransactionReceipt>> {
    evm::bsc().get_transaction_receipt(hash).await.map_err(rpc_error)
}
