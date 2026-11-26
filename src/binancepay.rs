//! Binance Pay for hosting subscriptions. The payer pays a USDT order from their Binance account;
//! once Binance Pay confirms it (signed webhook, or the order query when a webhook is missed),
//! Orvyn's creditor wallet credits the months in the registry (OrvynRegistry.creditExternal), keyed
//! by the order so it credits once.
//!
//! Merchant API requests are signed HMAC-SHA512 (hex, upper case) over "timestamp\nnonce\nbody\n";
//! webhooks are signed RSA-SHA256 (base64) over the same layout with a key from the certificates API.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use alloy::primitives::{keccak256, B256, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::Sha512;

use crate::billing::{billing_enabled, month_price, registry_agent, registry_config, registry_contract, sync_subscription};
use crate::db::{db, log_agent, Agent, AgentLogKind, BinancePayOrder};
use crate::env::env;
use crate::error::{Error, Result};
use crate::evm::{self, rpc_error, BSC};
use crate::tiers::Tier;

pub use crate::invoices::BINANCE_PAY_TOKEN;

sol! {
    #[sol(rpc)]
    interface Credit {
        function creditExternal(string agentId, uint32 months, bytes32 ref, uint256 usdAmount);
        function externalRefs(bytes32 ref) view returns (bool);
        function isCreditor(address account) view returns (bool);
    }
}

const ORDER_TTL_MS: i64 = 60 * 60_000;

pub fn binance_pay_enabled() -> bool {
    let e = env();
    e.binance_pay_api_key.is_some() && e.binance_pay_secret.is_some() && e.orvyn_binance_pay_creditor_key.is_some() && billing_enabled()
}

fn config() -> Result<(String, String, String)> {
    let e = env();
    match (&e.binance_pay_api_key, &e.binance_pay_secret) {
        (Some(k), Some(s)) => Ok((k.clone(), s.clone(), e.binance_pay_api_url.trim_end_matches('/').to_string())),
        _ => Err(Error::unavailable("Binance Pay is not configured")),
    }
}

const NONCE_CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";

fn nonce() -> String {
    rand::random::<[u8; 32]>().iter().map(|b| NONCE_CHARS[*b as usize % NONCE_CHARS.len()] as char).collect()
}

/// HMAC-SHA512, upper-case hex, over "timestamp\nnonce\nbody\n".
pub fn sign_request(body: &str, secret: &str, timestamp: &str, n: &str) -> String {
    let mut mac = Hmac::<Sha512>::new_from_slice(secret.as_bytes()).expect("hmac");
    mac.update(format!("{timestamp}\n{n}\n{body}\n").as_bytes());
    hex::encode_upper(mac.finalize().into_bytes())
}

async fn api(path: &str, payload: &Value) -> Result<Value> {
    let (key, secret, url) = config()?;
    let body = payload.to_string();
    let timestamp = chrono::Utc::now().timestamp_millis().to_string();
    let n = nonce();
    let signature = sign_request(&body, &secret, &timestamp, &n);
    let res = crate::http::client()
        .post(format!("{url}{path}"))
        .header("content-type", "application/json")
        .header("BinancePay-Timestamp", &timestamp)
        .header("BinancePay-Nonce", &n)
        .header("BinancePay-Certificate-SN", &key)
        .header("BinancePay-Signature", signature)
        .body(body)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| Error::internal(format!("Binance Pay {path}: {e}")))?;
    let status = res.status();
    let json: Value = res.json().await.unwrap_or(Value::Null);
    if !status.is_success() || json["status"] != "SUCCESS" {
        let why = json["errorMessage"].as_str().or(json["code"].as_str()).map(String::from).unwrap_or_else(|| format!("HTTP {}", status.as_u16()));
        return Err(Error::internal(format!("Binance Pay {path}: {why}")));
    }
    Ok(json["data"].clone())
}

/// USDT amount for `months` of `tier` at the registry's USDT price, rounded up to the cent.
pub async fn price_usd(months: u32, tier: Tier) -> Result<String> {
    let config = registry_config().await?;
    let unit = month_price(&config, tier, BSC.usdt);
    if unit == U256::ZERO {
        return Err(Error::unavailable(format!("the registry has no USDT price for {}", tier.as_str())));
    }
    let total = unit * U256::from(months);
    let cent = U256::from(10u64).pow(U256::from(16u8));
    let cents = (total + cent - U256::from(1u8)) / cent;
    Ok(cents_to_str(cents))
}

/// viem's formatUnits(cents, 2): no trailing zeros.
pub fn cents_to_str(cents: U256) -> String {
    let whole = cents / U256::from(100u8);
    let frac = (cents % U256::from(100u8)).to::<u64>();
    match frac {
        0 => whole.to_string(),
        f if f % 10 == 0 => format!("{whole}.{}", f / 10),
        f => format!("{whole}.{f:02}"),
    }
}

fn new_order_id() -> String {
    let t = radix36(chrono::Utc::now().timestamp_millis() as u64).to_uppercase();
    crate::http::clip(&format!("ORV{t}{}", hex::encode_upper(rand::random::<[u8; 6]>())), 32)
}

fn radix36(mut n: u64) -> String {
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = vec![];
    loop {
        out.push(digits[(n % 36) as usize]);
        n /= 36;
        if n == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).unwrap()
}

/// keccak256 of the order id: the registry's credit reference.
pub fn order_ref(order_id: &str) -> B256 {
    keccak256(order_id.as_bytes())
}

fn plural(m: i32) -> &'static str {
    if m > 1 { "s" } else { "" }
}

/// Create a Binance Pay order for `months` of the agent's hosting. Anyone may pay for any agent.
pub async fn create_order(agent: &Agent, months: i32, created_by: Option<&str>, return_url: Option<&str>, cancel_url: Option<&str>) -> Result<Value> {
    if !binance_pay_enabled() {
        return Err(Error::unavailable("Binance Pay is not configured"));
    }
    if !(1..=36).contains(&months) {
        return Err(Error::invalid("months must be 1-36"));
    }
    let tier = Tier::from_index(registry_agent(&agent.id).await?.map(|r| r.tier as u64).unwrap_or(0));
    let amount = price_usd(months as u32, tier).await?;
    let id = new_order_id();
    let expires_at = chrono::Utc::now() + chrono::Duration::milliseconds(ORDER_TTL_MS);
    let e = env();
    let webhook = e.binance_pay_webhook_url.clone().unwrap_or_else(|| format!("{}/api/binance-pay/webhook", e.orvyn_api_url.trim_end_matches('/')));
    let mut payload = json!({
        "env": { "terminalType": "WEB" },
        "merchantTradeNo": id,
        "orderAmount": amount,
        "currency": "USDT",
        "description": format!("Orvyn hosting: {}, {months} month{}", agent.name, plural(months)),
        "goodsDetails": [{ "goodsType": "02", "goodsCategory": "Z000", "referenceGoodsId": agent.id, "goodsName": format!("Orvyn hosting (${})", agent.symbol), "goodsDetail": format!("{months} month{}", plural(months)) }],
    });
    if let Some(u) = return_url {
        payload["returnUrl"] = u.into();
    }
    if let Some(u) = cancel_url {
        payload["cancelUrl"] = u.into();
    }
    if webhook.starts_with("https://") {
        payload["webhookUrl"] = webhook.into();
    }
    payload["orderExpireTime"] = expires_at.timestamp_millis().into();
    let data = api("/binancepay/openapi/v2/order", &payload).await?;
    sqlx::query("insert into binance_pay_orders (id, agent_id, months, amount_usd, prepay_id, checkout_url, created_by, expires_at) values ($1, $2, $3, $4, $5, $6, $7, $8)")
        .bind(&id)
        .bind(&agent.id)
        .bind(months)
        .bind(&amount)
        .bind(data["prepayId"].as_str())
        .bind(data["checkoutUrl"].as_str())
        .bind(created_by)
        .bind(expires_at)
        .execute(db())
        .await?;
    log_agent(&agent.id, AgentLogKind::Fees, &format!("Binance Pay order {id} created: {amount} USDT for {months} month{} of hosting", plural(months))).await?;
    Ok(json!({
        "orderId": id, "amountUsd": amount, "currency": "USDT", "months": months,
        "checkoutUrl": data["checkoutUrl"], "qrcodeLink": data["qrcodeLink"], "qrContent": data["qrContent"],
        "deeplink": data["deeplink"], "universalUrl": data["universalUrl"], "expiresAt": expires_at.timestamp_millis(),
    }))
}

pub async fn get_order(order_id: &str) -> Result<Option<BinancePayOrder>> {
    Ok(sqlx::query_as("select * from binance_pay_orders where id = $1").bind(order_id).fetch_optional(db()).await?)
}

pub fn order_view(o: &BinancePayOrder) -> Value {
    json!({
        "orderId": o.id, "agentId": o.agent_id, "months": o.months, "amountUsd": o.amount_usd, "status": o.status,
        "checkoutUrl": if o.status == "pending" { o.checkout_url.clone() } else { None },
        "txHash": o.tx_hash, "expiresAt": o.expires_at.timestamp_millis(),
        "paidAt": o.paid_at.map(|t| t.timestamp_millis()), "creditedAt": o.credited_at.map(|t| t.timestamp_millis()),
    })
}

/// Binance Pay's own view of an order.
pub async fn query_order(order_id: &str) -> Result<Value> {
    api("/binancepay/openapi/v2/order/query", &json!({ "merchantTradeNo": order_id })).await
}

/// "12.5" vs "12.50": paid at least what was expected (8 decimals).
fn same_amount(paid: &str, expected: &str) -> bool {
    match (crate::wallet_policy::grants::to_units(paid, 8), crate::wallet_policy::grants::to_units(expected, 8)) {
        (Ok(p), Ok(e)) => p >= e,
        _ => false,
    }
}

async fn mark_paid(order_id: &str, amount: &str, currency: &str) -> Result<()> {
    let Some(order) = get_order(order_id).await? else { return Err(Error::not_found(format!("unknown order {order_id}"))) };
    if order.status == "credited" {
        return Ok(());
    }
    if currency != "USDT" || !same_amount(amount, &order.amount_usd) {
        sqlx::query("update binance_pay_orders set status = 'failed', error = $2 where id = $1")
            .bind(order_id)
            .bind(format!("paid {amount} {currency}, expected {} USDT", order.amount_usd))
            .execute(db())
            .await?;
        log_agent(&order.agent_id, AgentLogKind::Error, &format!("Binance Pay order {order_id}: paid {amount} {currency}, expected {} USDT. Not credited; contact support.", order.amount_usd)).await?;
        return Ok(());
    }
    if order.status != "paid" {
        sqlx::query("update binance_pay_orders set status = 'paid', paid_at = now() where id = $1").bind(order_id).execute(db()).await?;
    }
    credit_order(order_id).await.map(|_| ())
}

/// Credit a paid order in the registry (once: the registry refuses a used ref), then resume the agent.
pub async fn credit_order(order_id: &str) -> Result<BinancePayOrder> {
    let Some(order) = get_order(order_id).await? else { return Err(Error::not_found(format!("unknown order {order_id}"))) };
    if order.status == "credited" {
        return Ok(order);
    }
    if order.status != "paid" {
        return Err(Error::conflict(format!("order {order_id} is {}", order.status)));
    }
    let registry = registry_contract()?;
    let reference = order_ref(&order.id);
    let already = Credit::new(registry, evm::bsc()).externalRefs(reference).call().await.map_err(rpc_error)?;
    let mut tx_hash = order.tx_hash.clone();
    if !already {
        let key = env().orvyn_binance_pay_creditor_key.clone().ok_or_else(|| Error::unavailable("ORVYN_BINANCE_PAY_CREDITOR_KEY is not set"))?;
        let usd = crate::wallet_policy::grants::to_units(&order.amount_usd, 18)?;
        let data = Credit::creditExternalCall { agentId: order.agent_id.clone(), months: order.months as u32, r#ref: reference, usdAmount: usd }.abi_encode();
        let tx = crate::wallets::send_from_key(&key, registry, data.into(), U256::ZERO).await?;
        tx_hash = Some(format!("{tx:#x}"));
    }
    let credited: BinancePayOrder = sqlx::query_as("update binance_pay_orders set status = 'credited', credited_at = now(), tx_hash = $2, error = null where id = $1 returning *")
        .bind(order_id)
        .bind(&tx_hash)
        .fetch_one(db())
        .await?;
    log_agent(&order.agent_id, AgentLogKind::Fees, &format!("Binance Pay: {} USDT paid for {} month{} of hosting (order {})", order.amount_usd, order.months, plural(order.months), order.id)).await?;
    let agent: Option<Agent> = sqlx::query_as("select * from agents where id = $1").bind(&order.agent_id).fetch_optional(db()).await?;
    if let Some(agent) = agent {
        let synced = sync_subscription(agent).await?;
        if let Err(e) = crate::invoices::sync_invoices(&synced.agent.id).await {
            eprintln!("[binance-pay] invoices {e}");
        }
        if synced.active && synced.agent.status == "live" {
            let resumed = async { crate::hosting::resume_host(crate::hosting::provision_host(synced.agent).await?).await }.await;
            if let Err(e) = resumed {
                eprintln!("[binance-pay] resume {e}");
            }
        }
    }
    Ok(credited)
}

// ---------- Webhooks ----------

static CERTIFICATES: LazyLock<Mutex<Option<HashMap<String, String>>>> = LazyLock::new(Default::default);

async fn certificate(serial: &str, refresh: bool) -> Result<Option<String>> {
    let cached = CERTIFICATES.lock().unwrap().clone();
    let map = match cached {
        Some(m) if !refresh => m,
        _ => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Cert {
                cert_public: String,
                cert_serial: String,
            }
            let list: Vec<Cert> = serde_json::from_value(api("/binancepay/openapi/certificates", &json!({})).await?).unwrap_or_default();
            let m: HashMap<String, String> = list.into_iter().map(|c| (c.cert_serial, c.cert_public)).collect();
            *CERTIFICATES.lock().unwrap() = Some(m.clone());
            m
        }
    };
    Ok(map.get(serial).cloned())
}

/// RSA-SHA256 over "timestamp\nnonce\nbody\n" with a PEM public key (or certificate's key).
pub fn verify_signature(raw_body: &str, timestamp: &str, nonce: &str, signature_b64: &str, public_pem: &str) -> bool {
    use rsa::pkcs1v15::{Signature, VerifyingKey};
    use rsa::pkcs8::DecodePublicKey;
    use rsa::signature::Verifier;
    let key = rsa::RsaPublicKey::from_public_key_pem(public_pem.trim()).or_else(|_| {
        use rsa::pkcs1::DecodeRsaPublicKey;
        rsa::RsaPublicKey::from_pkcs1_pem(public_pem.trim())
    });
    let Ok(key) = key else { return false };
    let Ok(sig) = B64.decode(signature_b64).map_err(|_| ()).and_then(|b| Signature::try_from(b.as_slice()).map_err(|_| ())) else { return false };
    VerifyingKey::<sha2::Sha256>::new(key).verify(format!("{timestamp}\n{nonce}\n{raw_body}\n").as_bytes(), &sig).is_ok()
}

pub const WEBHOOK_OK: &str = r#"{"returnCode":"SUCCESS","returnMessage":null}"#;

/// Handle a Binance Pay webhook (`raw_body` exactly as received). False when the signature doesn't
/// verify (answer 401); true when handled.
pub async fn handle_webhook(raw_body: &str, get: impl Fn(&str) -> Option<String>) -> Result<bool> {
    let (Some(serial), Some(timestamp), Some(n), Some(signature)) = (get("BinancePay-Certificate-SN"), get("BinancePay-Timestamp"), get("BinancePay-Nonce"), get("BinancePay-Signature")) else {
        return Ok(false);
    };
    let key = match certificate(&serial, false).await? {
        Some(k) => Some(k),
        None => certificate(&serial, true).await?,
    };
    let Some(key) = key else { return Ok(false) };
    if !verify_signature(raw_body, &timestamp, &n, &signature, &key) {
        return Ok(false);
    }
    let event: Value = serde_json::from_str(raw_body)?;
    if event["bizType"] != "PAY" {
        return Ok(true);
    }
    let data: Value = match &event["data"] {
        Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null),
        other => other.clone(),
    };
    let Some(order_id) = data["merchantTradeNo"].as_str() else { return Ok(true) };
    if get_order(order_id).await?.is_none() {
        return Ok(true);
    }
    let text = |v: &Value| match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    match event["bizStatus"].as_str() {
        Some("PAY_SUCCESS") => {
            let fee = if data["totalFee"].is_null() { "0".to_string() } else { text(&data["totalFee"]) };
            mark_paid(order_id, &fee, &text(&data["currency"])).await?;
        }
        Some("PAY_CLOSED") => {
            sqlx::query("update binance_pay_orders set status = 'closed' where id = $1 and status = 'pending'").bind(order_id).execute(db()).await?;
        }
        _ => {}
    }
    Ok(true)
}

/// Catch up on missed webhooks and failed credits.
pub async fn reconcile_orders() -> Result<()> {
    if !binance_pay_enabled() {
        return Ok(());
    }
    let open: Vec<BinancePayOrder> = sqlx::query_as("select * from binance_pay_orders where status in ('pending', 'paid') and created_at < now() - interval '1 minute' limit 50").fetch_all(db()).await?;
    for order in open {
        let r = async {
            if order.status == "paid" {
                credit_order(&order.id).await?;
                return Ok::<_, Error>(());
            }
            let remote = query_order(&order.id).await?;
            let status = remote["status"].as_str().unwrap_or("");
            if status == "PAID" {
                mark_paid(&order.id, remote["orderAmount"].as_str().unwrap_or("0"), remote["currency"].as_str().unwrap_or("")).await?;
            } else if ["CANCELED", "EXPIRED", "ERROR"].contains(&status) || (status == "INITIAL" && order.expires_at.timestamp_millis() < chrono::Utc::now().timestamp_millis() - ORDER_TTL_MS) {
                sqlx::query("update binance_pay_orders set status = 'closed', error = $2 where id = $1").bind(&order.id).bind(status).execute(db()).await?;
            }
            Ok(())
        }
        .await;
        if let Err(e) = r {
            sqlx::query("update binance_pay_orders set error = $2 where id = $1").bind(&order.id).bind(crate::http::clip(&e.message, 500)).execute(db()).await?;
        }
    }
    Ok(())
}

/// Is the creditor wallet appointed in the registry? (deploy check)
pub async fn creditor_ready() -> Result<bool> {
    let Some(key) = env().orvyn_binance_pay_creditor_key.clone() else { return Ok(false) };
    if !billing_enabled() {
        return Ok(false);
    }
    let signer: alloy::signers::local::PrivateKeySigner = key.parse().map_err(|_| Error::unavailable("bad creditor key"))?;
    Credit::new(registry_contract()?, evm::bsc()).isCreditor(signer.address()).call().await.map_err(rpc_error)
}
