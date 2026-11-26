//! Token launches on BNB Chain from the agent's own wallet (four.meme or Flap), and live token
//! stats (DexScreener, the bonding curves, GeckoTerminal candles).

use alloy::dyn_abi::DynSolValue;
use alloy::primitives::{keccak256, Address, Bytes, B256, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::sol;
use alloy::sol_types::SolCall;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::db::Agent;
use crate::env::env;
use crate::error::{Error, Result};
use crate::evm::{self, rpc_error};
use crate::wallet_policy::guard::GuardedWallet;

/// four.meme tax token settings (fixed at launch).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TokenTax {
    pub fee_rate: u32,
    pub recipient_rate: u32,
    pub burn_rate: u32,
    pub divide_rate: u32,
    pub liquidity_rate: u32,
    pub min_sharing: u64,
}

/// 1% on every trade, all of it to the agent's wallet.
pub const DEFAULT_TAX: TokenTax = TokenTax { fee_rate: 1, recipient_rate: 100, burn_rate: 0, divide_rate: 0, liquidity_rate: 0, min_sharing: 100_000 };

/// A tax four.meme accepts.
pub fn validate_tax(tax: Option<&TokenTax>) -> Result<TokenTax> {
    let t = *tax.unwrap_or(&DEFAULT_TAX);
    if ![1, 3, 5, 10].contains(&t.fee_rate) {
        return Err(Error::invalid("The trade tax is 1%, 3%, 5% or 10%."));
    }
    let parts = [t.recipient_rate, t.burn_rate, t.divide_rate, t.liquidity_rate];
    if parts.iter().any(|v| *v > 100) {
        return Err(Error::invalid("Tax shares are whole percentages from 0 to 100."));
    }
    if parts.iter().sum::<u32>() != 100 {
        return Err(Error::invalid("The tax split (agent, burn, holders, liquidity) must add up to 100%."));
    }
    if t.recipient_rate == 0 {
        return Err(Error::invalid("Part of the tax must go to the agent: that's its income."));
    }
    let digits = t.min_sharing.to_string();
    let ok = digits.len() >= 6 && digits.as_bytes()[0] != b'0' && digits[1..].chars().all(|c| c == '0');
    if !ok {
        return Err(Error::invalid("Minimum holding for holder rewards must be like 100000 or 1000000 tokens."));
    }
    Ok(t)
}

pub fn tax_from_value(v: Option<&Value>) -> Option<TokenTax> {
    v.filter(|x| !x.is_null()).and_then(|x| serde_json::from_value(x.clone()).ok())
}

/// The launch fee to send with createToken (FOURMEME_LAUNCH_FEE_BNB), in wei.
pub fn launch_fee_wei() -> U256 {
    evm::to_wei(std::env::var("FOURMEME_LAUNCH_FEE_BNB").ok().and_then(|v| v.parse().ok()).unwrap_or(0.005))
}

/// Token names four.meme accepts (it limits length).
pub fn token_name(name: &str) -> String {
    crate::http::clip(name.trim(), 32)
}

pub fn token_url(token: &str) -> String {
    format!("https://four.meme/token/{token}")
}
pub fn flap_token_url(token: &str) -> String {
    format!("https://flap.sh/bnb/{token}")
}

// ---------- four.meme ----------

sol! {
    #[sol(rpc)]
    interface FourMemeHelper {
        function getTokenInfo(address token) view returns (uint256 version, address tokenManager, address quote, uint256 lastPrice, uint256 tradingFeeRate, uint256 minTradingFee, uint256 launchTime, uint256 offers, uint256 maxOffers, uint256 funds, uint256 maxFunds, bool liquidityAdded);
    }
    interface FourMemeManager {
        function createToken(bytes args, bytes signature) payable;
    }
}

const TRANSFER_TOPIC: &str = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

async fn fourmeme(path: &str, body: Option<Value>, form: Option<reqwest::multipart::Form>, token: Option<&str>) -> Result<Value> {
    let url = format!("{}{path}", env().fourmeme_api_url.trim_end_matches('/'));
    let mut req = if body.is_some() || form.is_some() { crate::http::client().post(&url) } else { crate::http::client().get(&url) };
    if let Some(t) = token {
        req = req.header("meme-web-access", t);
    }
    if let Some(b) = body {
        req = req.json(&b);
    }
    if let Some(f) = form {
        req = req.multipart(f);
    }
    let res = req.timeout(std::time::Duration::from_secs(30)).send().await.map_err(|e| Error::internal(format!("four.meme {path}: {e}")))?;
    let status = res.status();
    let v: Value = res.json().await.unwrap_or(Value::Null);
    let code_ok = v["code"].as_str() == Some("0") || v["code"].as_i64() == Some(0);
    if !status.is_success() || !code_ok {
        return Err(Error::internal(format!("four.meme {path}: {}", v["msg"].as_str().map(String::from).unwrap_or_else(|| format!("HTTP {}", status.as_u16())))));
    }
    Ok(v["data"].clone())
}

/// Sign in to four.meme's API as the agent's wallet.
pub async fn fourmeme_login(wallet: &GuardedWallet) -> Result<String> {
    let address = wallet.address().parse::<Address>().map(|a| a.to_string()).unwrap_or_else(|_| wallet.address());
    let nonce = fourmeme("/private/user/nonce/generate", Some(json!({ "accountAddress": address, "verifyType": "LOGIN", "networkCode": "BSC" })), None, None).await?;
    let nonce = nonce.as_str().map(String::from).unwrap_or_else(|| nonce.to_string());
    let signature = wallet.sign_message(format!("You are sign in Meme {nonce}").as_bytes()).await?;
    let token = fourmeme(
        "/private/user/login/dex",
        Some(json!({ "region": "WEB", "langType": "EN", "walletName": "MetaMask", "verifyInfo": { "address": address, "networkCode": "BSC", "signature": signature, "verifyType": "LOGIN" } })),
        None,
        None,
    )
    .await?;
    token.as_str().map(String::from).ok_or_else(|| Error::internal("four.meme login returned no token"))
}

/// Read a token image (URL or data: URI): bytes and MIME type.
async fn read_image(icon: &str, allowed: &[&str]) -> Result<(Vec<u8>, String)> {
    let (bytes, mime) = if let Some(rest) = icon.strip_prefix("data:") {
        let (meta, data) = rest.split_once(',').ok_or_else(|| Error::invalid("bad data URI"))?;
        let mime = meta.split(';').next().unwrap_or("image/png").to_string();
        use base64::Engine;
        (base64::engine::general_purpose::STANDARD.decode(data).map_err(|_| Error::invalid("bad data URI"))?, mime)
    } else {
        let res = crate::http::client().get(icon).timeout(std::time::Duration::from_secs(20)).send().await.map_err(|e| Error::internal(format!("token image: {e}")))?;
        if !res.status().is_success() {
            return Err(Error::invalid(format!("couldn't read the token image ({})", res.status().as_u16())));
        }
        let mime = res.headers().get("content-type").and_then(|h| h.to_str().ok()).unwrap_or("image/png").to_string();
        (res.bytes().await.map_err(|e| Error::internal(e.to_string()))?.to_vec(), mime)
    };
    if !allowed.contains(&mime.as_str()) {
        return Err(Error::invalid(format!("the token image must be {}", allowed.iter().map(|m| m.trim_start_matches("image/")).collect::<Vec<_>>().join(", "))));
    }
    Ok((bytes, mime))
}

pub struct LaunchInput<'a> {
    pub name: String,
    pub symbol: String,
    pub description: String,
    pub icon: Option<String>,
    pub links: Option<&'a Value>,
    pub tax: TokenTax,
}

pub struct Launched {
    pub token: String,
    pub request_id: Option<String>,
    pub tx_hash: String,
}

/// Launch the agent's tax token on four.meme from its own wallet.
pub async fn launch_on_fourmeme(agent: &Agent, wallet: &GuardedWallet, input: LaunchInput<'_>) -> Result<Launched> {
    let icon = input.icon.clone().ok_or_else(|| Error::invalid("four.meme needs a token image"))?;
    let tax = validate_tax(Some(&input.tax))?;
    let token = fourmeme_login(wallet).await?;
    let (bytes, mime) = read_image(&icon, &["image/png", "image/jpeg", "image/jpg", "image/gif", "image/bmp", "image/webp"]).await?;
    let ext = mime.split('/').nth(1).unwrap_or("png").to_string();
    let form = reqwest::multipart::Form::new().part("file", reqwest::multipart::Part::bytes(bytes).file_name(format!("icon.{ext}")).mime_str(&mime).map_err(|e| Error::invalid(e.to_string()))?);
    let img_url = fourmeme("/private/token/upload", None, Some(form), Some(&token)).await?;
    let config = fourmeme("/public/config", None, None, None).await?;
    let list: Vec<Value> = match &config {
        Value::Array(a) => a.clone(),
        other if other["raisedToken"].is_object() => vec![other["raisedToken"].clone()],
        _ => vec![],
    };
    let raised = list.iter().find(|t| t["symbol"].as_str().is_some_and(|s| s.eq_ignore_ascii_case("BNB"))).or(list.first()).cloned().ok_or_else(|| Error::internal("four.meme config has no raised token"))?;
    let link = |k: &str| input.links.and_then(|l| l[k].as_str()).unwrap_or("").to_string();
    let created = fourmeme(
        "/private/token/create",
        Some(json!({
            "name": input.name, "shortName": input.symbol,
            "desc": if input.description.is_empty() { format!("{} is an AI agent on Orvyn.", input.name) } else { input.description.clone() },
            "imgUrl": img_url, "launchTime": chrono::Utc::now().timestamp_millis(), "label": "AI", "lpTradingFee": 0.0025,
            "webUrl": link("website"), "twitterUrl": link("twitter"), "telegramUrl": link("telegram"),
            "preSale": "0", "onlyMPC": false,
            "raisedAmount": if raised["totalBAmount"].is_null() { json!(24) } else { raised["totalBAmount"].clone() },
            "raisedToken": raised,
            "tokenTaxInfo": { "feeRate": tax.fee_rate, "burnRate": tax.burn_rate, "divideRate": tax.divide_rate, "liquidityRate": tax.liquidity_rate, "recipientRate": tax.recipient_rate, "recipientAddress": agent.evm_address, "minSharing": tax.min_sharing },
        })),
        None,
        Some(&token),
    )
    .await?;
    let arg: Bytes = created["createArg"].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| Error::internal("four.meme returned no createArg"))?;
    let sig: Bytes = created["signature"].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| Error::internal("four.meme returned no signature"))?;
    let manager = evm::parse_address(&env().fourmeme_token_manager)?;
    let data = FourMemeManager::createTokenCall { args: arg, signature: sig }.abi_encode();
    let tx = crate::wallets::prepare(&wallet.address(), manager, data.into(), launch_fee_wei()).await?;
    let hash = crate::wallets::send_and_wait(wallet, &tx).await?;
    let receipt = crate::wallets::receipt_of(hash).await?.ok_or_else(|| Error::internal("no receipt"))?;
    // The new token mints its supply in this transaction: the first contract (not the manager) to
    // log a Transfer from the zero address is the token.
    let token_addr = receipt
        .inner
        .logs()
        .iter()
        .find(|l| {
            let t = l.topics();
            t.len() >= 2 && format!("{:#x}", t[0]) == TRANSFER_TOPIC && t[1] == B256::ZERO && l.address() != manager
        })
        .map(|l| format!("{:#x}", l.address()))
        .ok_or_else(|| Error::internal(format!("launch transaction {hash:#x} created no token")))?;
    Ok(Launched { token: token_addr, request_id: None, tx_hash: format!("{hash:#x}") })
}

#[derive(Debug, Clone)]
pub struct CurveInfo {
    pub last_price: U256,
    pub funds: U256,
    pub max_funds: U256,
    pub graduated: bool,
}

/// The token's bonding-curve state from four.meme's helper contract.
pub async fn curve_info(token: &str) -> Result<CurveInfo> {
    let r = FourMemeHelper::new(evm::parse_address(&env().fourmeme_helper)?, evm::bsc()).getTokenInfo(evm::parse_address(token)?).call().await.map_err(rpc_error)?;
    Ok(CurveInfo { last_price: r.lastPrice, funds: r.funds, max_funds: r.maxFunds, graduated: r.liquidityAdded })
}

/// The agent's cumulative tax income (wei of BNB), or None while it can't be measured. four.meme
/// doesn't document how a tax token pays its recipient; until confirmed on a real tax token, income
/// isn't split or charged.
pub async fn tax_income_total(_agent: &Agent) -> Result<Option<U256>> {
    Ok(None)
}

// ---------- Flap ----------

fn flap_portal() -> Address {
    std::env::var("FLAP_PORTAL").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "0xe2cE6ab80874Fa9Fa2aAE65D277Dd6B8e65C9De0".into()).parse().expect("FLAP_PORTAL")
}
fn flap_tax_impl() -> Address {
    std::env::var("FLAP_TAX_TOKEN_IMPL").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "0x024f18294970B5c76c0691b87f138A0317156422".into()).parse().expect("FLAP_TAX_TOKEN_IMPL")
}

/// The token address the Portal would create for `salt` (EIP-1167 proxy of the implementation).
pub fn predict_token_address(salt: B256, implementation: Address, portal: Address) -> Address {
    let mut code = hex::decode("3d602d80600a3d3981f3363d3d373d3d3d363d73").unwrap();
    code.extend_from_slice(implementation.as_slice());
    code.extend(hex::decode("5af43d82803e903d91602b57fd5bf3").unwrap());
    portal.create2(salt, keccak256(&code))
}

/// A salt whose token address ends in 7777.
pub fn find_vanity_salt(seed: B256, suffix: &str, max_tries: u64) -> Result<(B256, Address)> {
    let (implementation, portal) = (flap_tax_impl(), flap_portal());
    let mut salt = keccak256(seed);
    for _ in 0..max_tries {
        let address = predict_token_address(salt, implementation, portal);
        if format!("{address:x}").ends_with(suffix) {
            return Ok((salt, address));
        }
        salt = keccak256(salt);
    }
    Err(Error::internal("no vanity salt found"))
}

/// Flap's newTokenV6 call for a launch (encoded with the dynamic ABI encoder: 26 fields).
pub fn flap_launch_call(name: &str, symbol: &str, meta: &str, tax: &TokenTax, beneficiary: Address, first_buy: U256, salt: B256) -> Result<Bytes> {
    let t = validate_tax(Some(tax))?;
    let bps = t.fee_rate * 100;
    let min_share = U256::from(t.min_sharing.max(if t.divide_rate > 0 { 10_000 } else { 0 })) * U256::from(10u64).pow(U256::from(18u8));
    let tax_duration: u64 = std::env::var("FLAP_TAX_DURATION_SEC").ok().and_then(|v| v.parse().ok()).unwrap_or(365 * 86_400);
    let u = |n: u64, bits: usize| DynSolValue::Uint(U256::from(n), bits);
    let zero = DynSolValue::Address(Address::ZERO);
    let params = DynSolValue::Tuple(vec![
        DynSolValue::String(name.into()),
        DynSolValue::String(symbol.into()),
        DynSolValue::String(meta.into()),
        u(1, 8),
        DynSolValue::FixedBytes(salt, 32),
        u(1, 8),
        zero.clone(),
        DynSolValue::Uint(first_buy, 256),
        DynSolValue::Address(beneficiary),
        DynSolValue::Bytes(vec![]),
        DynSolValue::FixedBytes(B256::ZERO, 32),
        DynSolValue::Bytes(vec![]),
        u(0, 8),
        u(0, 8),
        u(bps as u64, 16),
        u(bps as u64, 16),
        u(tax_duration, 64),
        u(3600, 64),
        u(t.recipient_rate as u64 * 100, 16),
        u(t.burn_rate as u64 * 100, 16),
        u(t.divide_rate as u64 * 100, 16),
        u(t.liquidity_rate as u64 * 100, 16),
        DynSolValue::Uint(min_share, 256),
        zero.clone(),
        zero,
        u(6, 8),
    ]);
    let mut data = crate::wallet_policy::rules::flap_new_token_v6_selector().to_vec();
    data.extend(DynSolValue::Tuple(vec![params]).abi_encode_params());
    Ok(data.into())
}

/// Upload the token's image and details to Flap's IPFS service; returns the metadata CID.
pub async fn flap_upload_meta(icon: &str, description: &str, links: Option<&Value>, creator: &str) -> Result<String> {
    let (bytes, mime) = read_image(icon, &["image/png", "image/jpeg", "image/jpg", "image/gif", "image/webp"]).await?;
    let link = |k: &str| links.and_then(|l| l[k].as_str()).map(String::from);
    let ops = json!({
        "query": "mutation Create($file: Upload!, $meta: MetadataInput!) { create(file: $file, meta: $meta) }",
        "variables": { "file": null, "meta": { "description": crate::http::clip(description, 500), "website": link("website"), "twitter": link("twitter").or_else(|| link("x")), "telegram": link("telegram"), "creator": creator } },
    });
    let ext = mime.split('/').nth(1).unwrap_or("png").to_string();
    let form = reqwest::multipart::Form::new()
        .text("operations", ops.to_string())
        .text("map", r#"{"0":["variables.file"]}"#)
        .part("0", reqwest::multipart::Part::bytes(bytes).file_name(format!("icon.{ext}")).mime_str(&mime).map_err(|e| Error::invalid(e.to_string()))?);
    let url = std::env::var("FLAP_UPLOAD_URL").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "https://funcs.flap.sh/api/upload".into());
    let res = crate::http::client().post(url).multipart(form).timeout(std::time::Duration::from_secs(30)).send().await.map_err(|e| Error::internal(format!("Flap upload: {e}")))?;
    let status = res.status();
    let body: Value = res.json().await.unwrap_or(Value::Null);
    match body["data"]["create"].as_str() {
        Some(cid) if status.is_success() => Ok(cid.into()),
        _ => Err(Error::internal(format!("Flap metadata upload failed: {}", body["errors"][0]["message"].as_str().map(String::from).unwrap_or_else(|| format!("HTTP {}", status.as_u16()))))),
    }
}

/// Launch the agent's tax token on Flap from its own wallet.
pub async fn launch_on_flap(agent: &Agent, wallet: &GuardedWallet, name: &str) -> Result<Launched> {
    let icon = agent.icon.clone().ok_or_else(|| Error::invalid("Flap needs a token image"))?;
    let creator = wallet.address();
    let meta = flap_upload_meta(&icon, &agent.description, agent.links.as_ref(), &creator).await?;
    let tax = tax_from_value(agent.tax.as_ref()).unwrap_or(DEFAULT_TAX);
    let (salt, token) = tokio::task::spawn_blocking(|| find_vanity_salt(B256::from(rand::random::<[u8; 32]>()), "7777", 2_000_000)).await.map_err(|e| Error::internal(e.to_string()))??;
    let beneficiary = evm::parse_address(&creator)?;
    let data = flap_launch_call(name, &agent.symbol, &meta, &tax, beneficiary, U256::ZERO, salt)?;
    let portal = flap_portal();
    // Dry-run first, so a bad parameter costs nothing.
    evm::bsc().call(TransactionRequest::default().from(beneficiary).to(portal).input(data.clone().into())).await.map_err(|e| Error::invalid(format!("Flap would refuse this launch: {}", e.to_string().lines().next().unwrap_or(""))))?;
    let tx = crate::wallets::prepare(&creator, portal, data, U256::ZERO).await?;
    let hash = crate::wallets::send_and_wait(wallet, &tx).await?;
    let code = evm::bsc().get_code_at(token).await.map_err(rpc_error)?;
    if code.is_empty() {
        return Err(Error::internal(format!("the launch transaction succeeded but no token is at {token}")));
    }
    Ok(Launched { token: format!("{token:#x}"), request_id: Some(meta), tx_hash: format!("{hash:#x}") })
}

sol! {
    struct FlapTokenState { uint8 status; uint256 reserve; uint256 circulatingSupply; uint256 price; uint8 tokenVersion; uint256 r; uint256 h; uint256 k; uint256 dexSupplyThresh; address quoteTokenAddress; bool nativeToQuoteSwapEnabled; bytes32 extensionID; }
    #[sol(rpc)]
    interface FlapState {
        function getTokenV5(address token) view returns (FlapTokenState state);
    }
}

pub struct FlapCurve {
    pub graduated: bool,
    pub progress_pct: Option<f64>,
}

/// A Flap token's curve: progress towards migration and graduation.
pub async fn flap_curve_info(token: &str) -> Result<FlapCurve> {
    let s = FlapState::new(flap_portal(), evm::bsc()).getTokenV5(evm::parse_address(token)?).call().await.map_err(rpc_error)?;
    let progress = (s.dexSupplyThresh > U256::ZERO).then(|| ((s.circulatingSupply * U256::from(10_000u64) / s.dexSupplyThresh).to::<u128>() as f64 / 100.0).min(100.0));
    Ok(FlapCurve { graduated: s.status == 4, progress_pct: progress })
}

// ---------- Stats ----------

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Live stats for an agent's token.
pub async fn token_stats(agent: &Agent) -> Result<Option<Value>> {
    let Some(token) = agent.token_address.clone() else { return Ok(None) };
    let flap = agent.launchpad == "flap";
    let pair = crate::data::dexscreener::best_pair("bsc", &token).await.ok().flatten();
    let curve: Option<(bool, Option<f64>)> = if flap {
        flap_curve_info(&token).await.ok().map(|c| (c.graduated, c.progress_pct))
    } else {
        curve_info(&token).await.ok().map(|c| {
            let p = (c.max_funds > U256::ZERO).then(|| ((c.funds * U256::from(10_000u64) / c.max_funds).to::<u128>() as f64 / 100.0).min(100.0));
            (c.graduated, p)
        })
    };
    let p = pair.clone().unwrap_or(Value::Null);
    let graduated = curve.map(|c| c.0).unwrap_or(agent.graduated);
    Ok(Some(json!({
        "token": token,
        "priceUsd": num(&p["priceUsd"]), "priceBnb": num(&p["priceNative"]),
        "marketCapUsd": num(&p["marketCap"]).or_else(|| num(&p["fdv"])),
        "liquidityUsd": num(&p["liquidity"]["usd"]), "volume24hUsd": num(&p["volume"]["h24"]), "change24hPct": num(&p["priceChange"]["h24"]),
        "trades24h": p["txns"]["h24"].clone(),
        "curveProgressPct": if curve.is_some_and(|c| c.0) { Some(100.0) } else { curve.and_then(|c| c.1) },
        "graduated": graduated,
        "pairAddress": p["pairAddress"].clone(),
        "tradeUrl": p["url"].as_str().map(String::from).unwrap_or_else(|| if flap { flap_token_url(&token) } else { token_url(&token) }),
    })))
}

/// (timeframe, aggregate, points) for a price range.
pub fn price_range(range: &str) -> Option<(&'static str, u32, u32)> {
    match range {
        "24h" => Some(("hour", 1, 24)),
        "7d" => Some(("hour", 4, 42)),
        "30d" => Some(("day", 1, 30)),
        _ => None,
    }
}

pub async fn token_candles(pair: Option<&str>, range: &str) -> Vec<crate::data::geckoterminal::Candle> {
    let (Some(pair), Some((tf, agg, points))) = (pair, price_range(range)) else { return vec![] };
    crate::data::geckoterminal::ohlcv("bsc", pair, tf, agg, points).await.unwrap_or_default()
}
