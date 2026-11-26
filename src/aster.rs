//! Aster perps (Futures API V3): public market data, and requests signed the V3 way. Each request's
//! params (with `nonce`, `user` and `signer` appended) form a query string; the API wallet signs it
//! as EIP-712 typed data (domain AsterSignTransaction, chain id 1666), and the signature goes last.
//! The main wallet only signs account actions: approving its API wallet and withdrawals. Both sign
//! through the wallet policy.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::env::env;
use crate::error::{Error, Result};
use crate::wallet_policy::aster::{ACCOUNT_CHAIN_ID, SIGNING_CHAIN_ID};
use crate::wallet_policy::guard::{GuardedAsterWallet, GuardedWallet};
use crate::wallet_policy::TypedData;

const ZERO: &str = "0x0000000000000000000000000000000000000000";

fn base() -> String {
    env().aster_api_url.trim_end_matches('/').to_string()
}

static LAST_NONCE: Mutex<u128> = Mutex::new(0);

/// Microseconds, strictly increasing within this process (Aster rejects reused nonces).
pub fn aster_nonce() -> String {
    let now = chrono::Utc::now().timestamp_millis() as u128 * 1000;
    let mut last = LAST_NONCE.lock().unwrap();
    *last = if now > *last { now } else { *last + 1 };
    last.to_string()
}

pub type Params = Vec<(String, String)>;

/// application/x-www-form-urlencoded, as URLSearchParams writes it.
pub fn to_query(params: &Params) -> String {
    let mut s = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in params {
        s.append_pair(k, v);
    }
    s.finish()
}

async fn parse(res: reqwest::Response) -> Result<Value> {
    let status = res.status();
    let text = res.text().await.unwrap_or_default();
    let body: Value = serde_json::from_str(&text).map_err(|_| Error::internal(format!("Aster: HTTP {} {}", status.as_u16(), crate::http::clip(&text, 200))))?;
    // Errors are {code: negative, msg}; some successes are {code: 200, msg: "success"}.
    if !status.is_success() || body["code"].as_i64().is_some_and(|c| c < 0) {
        return Err(Error::internal(format!("Aster: {}", body["msg"].as_str().map(String::from).unwrap_or_else(|| format!("HTTP {}", status.as_u16())))));
    }
    Ok(body)
}

/// Unsigned market data.
pub async fn aster_public(path: &str, params: &Params) -> Result<Value> {
    let q = to_query(params);
    let url = if q.is_empty() { format!("{}{path}", base()) } else { format!("{}{path}?{q}", base()) };
    parse(crate::http::client().get(url).timeout(Duration::from_secs(15)).send().await.map_err(|e| Error::internal(format!("Aster: {e}")))?).await
}

fn message_typed(chain_id: u64, msg: &str) -> TypedData {
    serde_json::from_value(json!({
        "domain": { "name": "AsterSignTransaction", "version": "1", "chainId": chain_id, "verifyingContract": ZERO },
        "types": { "Message": [{ "name": "msg", "type": "string" }] },
        "primaryType": "Message",
        "message": { "msg": msg },
    }))
    .unwrap()
}

/// A request signed by the agent's Aster API wallet, for the account `user`.
pub async fn aster_signed(method: reqwest::Method, path: &str, params: Params, user: &str, signer: &GuardedAsterWallet) -> Result<Value> {
    let mut p = params;
    p.push(("nonce".into(), aster_nonce()));
    p.push(("user".into(), user.into()));
    p.push(("signer".into(), checksum(&signer.address())));
    let msg = to_query(&p);
    let signature = signer.sign_typed_data(&message_typed(SIGNING_CHAIN_ID, &msg)).await?;
    let res = crate::http::client()
        .request(method, format!("{}{path}?{msg}&signature={signature}", base()))
        .header("content-type", "application/x-www-form-urlencoded")
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| Error::internal(format!("Aster: {e}")))?;
    parse(res).await
}

pub fn checksum(a: &str) -> String {
    a.parse::<alloy::primitives::Address>().map(|x| x.to_string()).unwrap_or_else(|_| a.to_string())
}

/// Approve `api_wallet` to trade perps for the main wallet's account (no spot, no withdrawals)
/// until `expires_at` (ms). Signed by the main wallet.
pub async fn aster_approve_api_wallet(main: &GuardedWallet, api_wallet: &str, expires_at: i64, name: &str) -> Result<Value> {
    let params: Params = vec![
        ("user".into(), checksum(&main.address())),
        ("nonce".into(), aster_nonce()),
        ("agentName".into(), name.into()),
        ("agentAddress".into(), api_wallet.into()),
        ("expired".into(), expires_at.to_string()),
        ("signatureChainId".into(), ACCOUNT_CHAIN_ID.to_string()),
        ("canSpotTrade".into(), "false".into()),
        ("canPerpTrade".into(), "true".into()),
        ("canWithdraw".into(), "false".into()),
        ("ipWhitelist".into(), String::new()),
    ];
    let msg = to_query(&params);
    let signature = main.sign_typed_data(&message_typed(ACCOUNT_CHAIN_ID, &msg)).await?;
    let mut body = params;
    body.push(("signature".into(), signature));
    let res = crate::http::client()
        .post(format!("{}/fapi/v3/registerAndApproveAgent", base()))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(to_query(&body))
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| Error::internal(format!("Aster: {e}")))?;
    parse(res).await
}

/// Aster's withdrawal fee for USDT to BNB Chain, in USDT.
pub async fn aster_withdraw_fee() -> Result<String> {
    let res = crate::http::client()
        .get("https://www.asterdex.com/bapi/futures/v1/public/future/aster/estimate-withdraw-fee?chainId=56&network=EVM&currency=USDT&accountType=spot")
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| Error::internal(format!("Aster: {e}")))?;
    let body: Value = res.json().await.unwrap_or(Value::Null);
    match &body["data"]["gasCost"] {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(crate::environment::js_number_str(n)),
        _ => Err(Error::internal("couldn't get Aster's withdrawal fee")),
    }
}

/// Withdraw USDT from the Aster account to the main wallet on BNB Chain: the main wallet signs the
/// withdrawal, the API wallet signs the request.
pub async fn aster_withdraw_usdt(main: &GuardedWallet, api: &GuardedAsterWallet, amount: &str, fee: &str) -> Result<Value> {
    let user_nonce = (chrono::Utc::now().timestamp_millis() as u128 * 1000).to_string();
    let destination = checksum(&main.address());
    let typed: TypedData = serde_json::from_value(json!({
        "domain": { "name": "Aster", "version": "1", "chainId": ACCOUNT_CHAIN_ID, "verifyingContract": ZERO },
        "types": { "Action": [
            { "name": "type", "type": "string" }, { "name": "destination", "type": "address" }, { "name": "destination Chain", "type": "string" },
            { "name": "token", "type": "string" }, { "name": "amount", "type": "string" }, { "name": "fee", "type": "string" },
            { "name": "nonce", "type": "uint256" }, { "name": "aster chain", "type": "string" }
        ] },
        "primaryType": "Action",
        "message": { "type": "Withdraw", "destination": destination, "destination Chain": "BSC", "token": "USDT", "amount": amount, "fee": fee, "nonce": user_nonce, "aster chain": "Mainnet" },
    }))
    .unwrap();
    let user_signature = main.sign_typed_data(&typed).await?;
    let params: Params = vec![
        ("chainId".into(), "56".into()),
        ("asset".into(), "USDT".into()),
        ("amount".into(), amount.into()),
        ("fee".into(), fee.into()),
        ("receiver".into(), destination.clone()),
        ("userNonce".into(), user_nonce),
        ("userSignature".into(), user_signature),
    ];
    aster_signed(reqwest::Method::POST, "/fapi/v3/aster/user-withdraw", params, &destination, api).await
}

#[derive(Debug, Clone)]
pub struct AsterSymbol {
    pub symbol: String,
    pub status: String,
    pub tick_size: f64,
    pub step_size: f64,
    pub min_qty: f64,
    pub min_notional: f64,
}

static SYMBOLS: LazyLock<Mutex<Option<(Instant, HashMap<String, AsterSymbol>)>>> = LazyLock::new(Default::default);

pub async fn aster_symbols() -> Result<HashMap<String, AsterSymbol>> {
    if let Some((at, m)) = SYMBOLS.lock().unwrap().clone() {
        if at.elapsed() < Duration::from_secs(600) {
            return Ok(m);
        }
    }
    let info = aster_public("/fapi/v3/exchangeInfo", &vec![]).await?;
    let n = |v: &Value| v.as_str().and_then(|s| s.parse::<f64>().ok()).or_else(|| v.as_f64()).unwrap_or(0.0);
    let mut map = HashMap::new();
    for s in info["symbols"].as_array().cloned().unwrap_or_default() {
        let filters = s["filters"].as_array().cloned().unwrap_or_default();
        let f = |t: &str| filters.iter().find(|x| x["filterType"] == t).cloned().unwrap_or(json!({}));
        let sym = s["symbol"].as_str().unwrap_or("").to_string();
        map.insert(
            sym.clone(),
            AsterSymbol {
                symbol: sym,
                status: s["status"].as_str().unwrap_or("").into(),
                tick_size: n(&f("PRICE_FILTER")["tickSize"]),
                step_size: n(&f("LOT_SIZE")["stepSize"]),
                min_qty: n(&f("LOT_SIZE")["minQty"]),
                min_notional: n(&f("MIN_NOTIONAL")["notional"]),
            },
        );
    }
    *SYMBOLS.lock().unwrap() = Some((Instant::now(), map.clone()));
    Ok(map)
}

pub async fn aster_mark(symbol: &str) -> Result<Value> {
    let r = aster_public("/fapi/v3/premiumIndex", &vec![("symbol".into(), symbol.into())]).await?;
    let n = |v: &Value| v.as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
    Ok(json!({ "markPrice": n(&r["markPrice"]), "indexPrice": n(&r["indexPrice"]), "fundingRate": n(&r["lastFundingRate"]), "nextFundingTime": r["nextFundingTime"] }))
}

/// Round `value` down to a multiple of `step`, as a decimal string without float noise.
pub fn to_step(value: f64, step: f64) -> String {
    let decimals = (-step.log10()).ceil().max(0.0) as usize;
    let units = (value / step + 1e-9).floor();
    format!("{:.*}", decimals, units * step)
}
