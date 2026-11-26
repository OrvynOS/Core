//! BNB Chain plumbing shared by every venue: the client, token metadata, amounts, allowances,
//! batched reads (Multicall3), dry runs (eth_simulateV1) and the shape every build tool returns
//! (unsigned calls for the caller's own wallet).

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, OnceLock};

use alloy::network::TransactionBuilder;
use alloy::primitives::{address, Address, Bytes, U256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::sol;
use alloy::sol_types::SolCall;
use serde_json::{json, Value};

pub const CHAIN_ID: u64 = 56;
pub const NATIVE: Address = Address::ZERO;
pub const WBNB: Address = address!("bb4CdB9CBd36B01bD1cBaEBF2De08d9173bc095c");
pub const USDT: Address = address!("55d398326f99059fF775485246999027B3197955");
pub const USDC: Address = address!("8AC76a51cc950d9822D68b83fE1Ad97B32Cd580d");
pub const MULTICALL3: Address = address!("cA11bde05977b3631167028862bE2a173976CA11");

sol! {
    interface IERC20 {
        function symbol() external view returns (string);
        function decimals() external view returns (uint8);
        function balanceOf(address owner) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
        function approve(address spender, uint256 amount) external returns (bool);
    }
    interface IMulticall3 {
        struct Call3 { address target; bool allowFailure; bytes callData; }
        struct Result3 { bool success; bytes returnData; }
        function aggregate3(Call3[] calldata calls) external payable returns (Result3[] memory returnData);
        function getEthBalance(address addr) external view returns (uint256 balance);
    }
}

/// A refusal the caller should see as is (bad input, no pool, not enough balance).
#[derive(Debug)]
pub struct ToolError(pub String);

impl ToolError {
    pub fn new(m: impl Into<String>) -> Self {
        ToolError(m.into())
    }
    /// An RPC or decoding failure, shortened to its first line (as viem's shortMessage).
    pub fn rpc(e: impl std::fmt::Display) -> Self {
        let s = e.to_string();
        ToolError(s.lines().next().unwrap_or("RPC error").chars().take(300).collect())
    }
}

pub type Res<T> = std::result::Result<T, ToolError>;

pub fn rpc_url() -> String {
    ["EVM_RPC_BSC", "BSC_RPC_URL"].iter().find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty())).unwrap_or_else(|| "https://bsc-dataseed.bnbchain.org".into())
}

pub fn bsc() -> DynProvider {
    static P: OnceLock<DynProvider> = OnceLock::new();
    P.get_or_init(|| ProviderBuilder::new().connect_http(rpc_url().parse().expect("BSC_RPC_URL")).erased()).clone()
}

pub fn checksum(a: Address) -> String {
    a.to_checksum(None)
}

/// eth_call, decoded.
pub async fn read<C: SolCall>(to: Address, call: C) -> Res<C::Return> {
    let tx = TransactionRequest::default().with_to(to).with_input(Bytes::from(call.abi_encode()));
    let out = bsc().call(tx).await.map_err(ToolError::rpc)?;
    C::abi_decode_returns(&out).map_err(ToolError::rpc)
}

/// Batched reads through Multicall3; None for each call that failed or returned nothing.
pub async fn multicall(calls: Vec<(Address, Vec<u8>)>) -> Res<Vec<Option<Bytes>>> {
    let call3 = calls.into_iter().map(|(target, data)| IMulticall3::Call3 { target, allowFailure: true, callData: Bytes::from(data) }).collect();
    let results = read(MULTICALL3, IMulticall3::aggregate3Call { calls: call3 }).await?;
    Ok(results.into_iter().map(|r| (r.success && !r.returnData.is_empty()).then_some(r.returnData)).collect())
}

fn is_hex_address(s: &str) -> bool {
    s.len() == 42 && s.starts_with("0x") && s[2..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// viem's isAddress: hex of the right length, and a valid checksum when the case is mixed.
fn parse_address(s: &str) -> Option<Address> {
    if !is_hex_address(s) {
        return None;
    }
    let body = &s[2..];
    let mixed = body.bytes().any(|b| b.is_ascii_lowercase()) && body.bytes().any(|b| b.is_ascii_uppercase());
    if mixed { Address::parse_checksummed(s, None).ok() } else { s.parse().ok() }
}

/// "BNB", the zero address or a BEP-20 address (NATIVE for BNB).
pub fn parse_token(input: &str) -> Res<Address> {
    let t = input.trim();
    let lower = t.to_lowercase();
    match lower.as_str() {
        "bnb" | "native" | "0x0000000000000000000000000000000000000000" => return Ok(NATIVE),
        "usdt" => return Ok(USDT),
        "usdc" => return Ok(USDC),
        "wbnb" => return Ok(WBNB),
        _ => {}
    }
    parse_address(t).ok_or_else(|| ToolError::new(format!("\"{input}\" is not a token address (or BNB, WBNB, USDT, USDC)")))
}

pub fn parse_account(input: &str) -> Res<Address> {
    let a = parse_address(input.trim()).ok_or_else(|| ToolError::new(format!("\"{input}\" is not a wallet address")))?;
    if a == NATIVE {
        return Err(ToolError::new("the account can't be the zero address"));
    }
    Ok(a)
}

pub fn is_native(t: Address) -> bool {
    t == NATIVE
}

/// For pools: BNB trades as WBNB.
pub fn pool_token(t: Address) -> Address {
    if is_native(t) { WBNB } else { t }
}

#[derive(Clone, Debug)]
pub struct TokenMeta {
    pub address: Address,
    pub symbol: String,
    pub decimals: u8,
}

impl TokenMeta {
    pub fn json(&self) -> Value {
        json!({ "address": checksum(self.address), "symbol": self.symbol, "decimals": self.decimals })
    }
}

static META: LazyLock<Mutex<HashMap<Address, TokenMeta>>> = LazyLock::new(Default::default);

pub async fn token_meta(token: Address) -> Res<TokenMeta> {
    if is_native(token) {
        return Ok(TokenMeta { address: NATIVE, symbol: "BNB".into(), decimals: 18 });
    }
    if let Some(m) = META.lock().unwrap().get(&token) {
        return Ok(m.clone());
    }
    let (symbol, decimals) = tokio::join!(read(token, IERC20::symbolCall {}), read(token, IERC20::decimalsCall {}));
    let decimals = decimals.map_err(|_| ToolError::new(format!("{} is not a BEP-20 token on BNB Chain", checksum(token))))?;
    let meta = TokenMeta { address: token, symbol: symbol.unwrap_or_else(|_| "?".into()), decimals };
    META.lock().unwrap().insert(token, meta.clone());
    Ok(meta)
}

/// viem's parseUnits: a decimal string in whole units to base units, rounding extra digits.
pub fn parse_units(value: &str, decimals: u8) -> U256 {
    let d = decimals as usize;
    let (int, frac) = value.split_once('.').unwrap_or((value, "0"));
    let mut integer = U256::from_str_radix(if int.is_empty() { "0" } else { int }, 10).unwrap_or_default();
    let mut fraction = frac.trim_end_matches('0').to_string();
    let round = |s: &str| -> u64 {
        let n: f64 = s.parse().unwrap_or(0.0);
        (n + 0.5).floor() as u64
    };
    if d == 0 {
        if round(&format!("0.{fraction}")) == 1 {
            integer += U256::from(1);
        }
        fraction.clear();
    } else if fraction.len() > d {
        let (left, unit, right) = (&fraction[..d - 1], &fraction[d - 1..d], &fraction[d..]);
        let rounded = round(&format!("{unit}.{right}"));
        let mut f = if rounded > 9 {
            let bumped = U256::from_str_radix(if left.is_empty() { "0" } else { left }, 10).unwrap_or_default() + U256::from(1);
            format!("{:0>width$}0", bumped.to_string(), width = left.len())
        } else {
            format!("{left}{rounded}")
        };
        if f.len() > d {
            f = f[1..].to_string();
            integer += U256::from(1);
        }
        fraction = f[..d].to_string();
    } else {
        fraction = format!("{fraction:0<d$}");
    }
    let digits = format!("{integer}{fraction}");
    U256::from_str_radix(&digits, 10).unwrap_or_default()
}

pub fn to_units(amount: &str, decimals: u8) -> Res<U256> {
    let a = amount.trim();
    let (int, frac) = a.split_once('.').map_or((a, None), |(i, f)| (i, Some(f)));
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(int) || frac.is_some_and(|f| !digits(f)) {
        return Err(ToolError::new(format!("\"{amount}\" is not an amount (use a decimal like 0.5)")));
    }
    let v = parse_units(a, decimals);
    if v.is_zero() {
        return Err(ToolError::new("amount must be greater than 0"));
    }
    Ok(v)
}

/// viem's formatUnits: no trailing zeros ("1.5", "2").
pub fn from_units(v: U256, decimals: u8) -> String {
    let s = alloy::primitives::utils::format_units(v, decimals).unwrap_or_default();
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s }
}

pub const DEFAULT_SLIPPAGE_BPS: i64 = 100;
pub const MAX_SLIPPAGE_BPS: i64 = 3000;

pub fn min_out(amount_out: U256, slippage_bps: Option<i64>) -> Res<U256> {
    let s = slippage_bps.unwrap_or(DEFAULT_SLIPPAGE_BPS);
    if !(0..=MAX_SLIPPAGE_BPS).contains(&s) {
        return Err(ToolError::new(format!("slippage_bps must be 0-{MAX_SLIPPAGE_BPS}")));
    }
    Ok(amount_out * U256::from(10_000 - s) / U256::from(10_000))
}

/// An unsigned call for the caller's wallet.
#[derive(Clone, Debug)]
pub struct Call {
    pub to: Address,
    pub data: Bytes,
    /// Wei, as a decimal string.
    pub value: U256,
    pub description: String,
}

impl Call {
    pub fn json(&self) -> Value {
        json!({ "to": checksum(self.to), "data": self.data.to_string(), "value": self.value.to_string(), "description": self.description })
    }
}

/// Approve `spender` for exactly `amount` when the current allowance is lower (none for BNB).
pub async fn approval_if_needed(token: Address, owner: Address, spender: Address, amount: U256) -> Res<Vec<Call>> {
    if is_native(token) {
        return Ok(vec![]);
    }
    let current = read(token, IERC20::allowanceCall { owner, spender }).await?;
    if current >= amount {
        return Ok(vec![]);
    }
    let meta = token_meta(token).await?;
    Ok(vec![Call {
        to: token,
        data: IERC20::approveCall { spender, amount }.abi_encode().into(),
        value: U256::ZERO,
        description: format!("Approve {} {} for {}", from_units(amount, meta.decimals), meta.symbol, checksum(spender)),
    }])
}

pub async fn balance_of(token: Address, owner: Address) -> Res<U256> {
    if is_native(token) {
        bsc().get_balance(owner).await.map_err(ToolError::rpc)
    } else {
        read(token, IERC20::balanceOfCall { owner }).await
    }
}

/// Make sure the account can cover the input (a clear error beats a reverted transaction).
pub async fn require_balance(token: Address, account: Address, amount: U256) -> Res<()> {
    let have = balance_of(token, account).await?;
    if have < amount {
        let meta = token_meta(token).await?;
        return Err(ToolError::new(format!("{} holds {} {}, less than {}", checksum(account), from_units(have, meta.decimals), meta.symbol, from_units(amount, meta.decimals))));
    }
    Ok(())
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

pub fn deadline_from(seconds: Option<i64>) -> U256 {
    U256::from(now_secs() + seconds.unwrap_or(600).clamp(60, 3600) as u64)
}

pub fn iso(secs: u64) -> String {
    chrono::DateTime::from_timestamp(secs as i64, 0).unwrap_or_default().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// A JavaScript number as JSON: integers without ".0".
pub fn js_num(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9e15 { json!(n as i64) } else { json!(n) }
}

pub struct Plan<'a> {
    pub venue: &'a str,
    pub account: Address,
    pub calls: &'a [Call],
    pub token_in: &'a TokenMeta,
    pub token_out: &'a TokenMeta,
    pub amount_in: U256,
    pub amount_out: U256,
    pub min_amount_out: U256,
    pub route: Value,
    pub deadline: Option<U256>,
}

/// The shape every build tool returns.
pub fn plan(p: Plan) -> Value {
    let side = |m: &TokenMeta, v: U256| {
        let mut j = m.json();
        j["amount"] = json!(from_units(v, m.decimals));
        j["raw"] = json!(v.to_string());
        j
    };
    let mut out = json!({
        "chainId": CHAIN_ID,
        "venue": p.venue,
        "account": checksum(p.account),
        "calls": p.calls.iter().map(Call::json).collect::<Vec<_>>(),
        "quote": {
            "tokenIn": side(p.token_in, p.amount_in),
            "tokenOut": side(p.token_out, p.amount_out),
            "minAmountOut": from_units(p.min_amount_out, p.token_out.decimals),
            "minAmountOutRaw": p.min_amount_out.to_string(),
            "route": p.route,
        },
    });
    if let Some(d) = p.deadline {
        out["expiresAt"] = json!(iso(d.to::<u64>()));
    }
    out["notes"] = json!([
        "Unsigned transactions: send the calls in order from the account. Nothing has been signed or sent.",
        "The account receives the output; quotes can move, the minimum output protects the swap."
    ]);
    out
}

// ---------- Dry runs ----------

pub struct Simulation {
    /// "ok", "reverted" or "unavailable".
    pub status: &'static str,
    /// What the account received of the output token (base units), measured by the dry run.
    pub received: Option<U256>,
    pub detail: Option<String>,
}

fn hex_u256(v: U256) -> String {
    format!("{v:#x}")
}

/// Dry-run every call in order (eth_simulateV1, so an approval is applied before the swap) and
/// measure what the account receives of `token_out` (a balance read before and after the calls).
/// "unavailable" when the RPC can't simulate.
pub async fn simulate(account: Address, calls: &[Call], token_out: Address) -> Simulation {
    let balance_read = || {
        if is_native(token_out) {
            json!({ "from": account, "to": MULTICALL3, "data": Bytes::from(IMulticall3::getEthBalanceCall { addr: account }.abi_encode()) })
        } else {
            json!({ "from": account, "to": token_out, "data": Bytes::from(IERC20::balanceOfCall { owner: account }.abi_encode()) })
        }
    };
    let mut sim_calls = vec![balance_read()];
    sim_calls.extend(calls.iter().map(|c| json!({ "from": account, "to": c.to, "data": c.data, "value": hex_u256(c.value) })));
    sim_calls.push(balance_read());
    let params = json!([{ "blockStateCalls": [{ "calls": sim_calls }], "validation": false, "traceTransfers": false }, "latest"]);
    let unavailable = |d: String| Simulation { status: "unavailable", received: None, detail: Some(d.chars().take(200).collect()) };
    let res: Value = match bsc().raw_request::<_, Value>("eth_simulateV1".into(), params).await {
        Ok(v) => v,
        Err(e) => return unavailable(ToolError::rpc(e).0),
    };
    let Some(results) = res.get(0).and_then(|b| b["calls"].as_array()).filter(|r| r.len() == calls.len() + 2) else {
        return unavailable("the RPC returned no simulation".into());
    };
    let ok = |r: &Value| r["status"].as_str().is_some_and(|s| s == "0x1");
    for (i, r) in results[1..=calls.len()].iter().enumerate() {
        if !ok(r) {
            let msg: String = r["error"]["message"].as_str().unwrap_or("").chars().take(200).collect();
            return Simulation { status: "reverted", received: None, detail: Some(format!("call {} reverted: {msg}", i + 1)) };
        }
    }
    let balance = |r: &Value| -> Option<U256> {
        let data: Bytes = r["returnData"].as_str()?.parse().ok()?;
        (data.len() >= 32).then(|| U256::from_be_slice(&data[..32]))
    };
    match (balance(&results[0]), balance(&results[calls.len() + 1])) {
        (Some(before), Some(after)) => Simulation { status: "ok", received: Some(after.saturating_sub(before)), detail: None },
        _ => unavailable("couldn't read the balance in the simulation".into()),
    }
}

pub struct Measured {
    pub calls: Vec<Call>,
    pub expected: U256,
    pub min_amount_out: U256,
    pub simulation: Value,
}

/// Build with a minimum taken from a measured dry run (quotes can overstate: launchpad estimates,
/// tax tokens), then dry-run the final plan. Falls back to the quote when simulation is unavailable.
pub async fn build_measured(account: Address, token_out: Address, quoted: U256, slippage_bps: Option<i64>, build: impl Fn(U256) -> Vec<Call>) -> Res<Measured> {
    let probe = simulate(account, &build(U256::ZERO), token_out).await;
    // The smaller of the quote and the measured output: never stricter than what the chain returns.
    let measured = if probe.status == "ok" { probe.received.filter(|r| !r.is_zero()) } else { None };
    let expected = match measured {
        Some(m) if m < quoted => m,
        _ => quoted,
    };
    let min_amount_out = min_out(expected, slippage_bps)?;
    let calls = build(min_amount_out);
    let last = if probe.status == "unavailable" { probe } else { simulate(account, &calls, token_out).await };
    let mut simulation = json!({ "status": last.status, "measured": measured.is_some() });
    if let Some(r) = last.received {
        simulation["received"] = json!(r.to_string());
    }
    if let Some(d) = last.detail {
        simulation["detail"] = json!(d);
    }
    Ok(Measured { calls, expected, min_amount_out, simulation })
}

/// Merge a plan and its simulation (`{...plan, simulation}`).
pub fn with_simulation(mut plan: Value, simulation: Value) -> Value {
    plan["simulation"] = simulation;
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_as_viem() {
        assert_eq!(parse_units("1.5", 18), U256::from(1_500_000_000_000_000_000u128));
        assert_eq!(parse_units("0.1234567", 6), U256::from(123_457u64));
        assert_eq!(parse_units("0.9999999", 6), U256::from(1_000_000u64));
        assert_eq!(parse_units("2", 0), U256::from(2u64));
        assert_eq!(parse_units("2.5", 0), U256::from(3u64));
        assert_eq!(from_units(U256::from(1_500_000u64), 6), "1.5");
        assert_eq!(from_units(U256::from(2_000_000u64), 6), "2");
        assert!(to_units("1e5", 18).is_err() && to_units("0", 18).is_err() && to_units(".5", 18).is_err());
        assert_eq!(min_out(U256::from(10_000u64), None).unwrap(), U256::from(9_900u64));
        assert!(min_out(U256::from(1u64), Some(3001)).is_err());
    }

    #[test]
    fn tokens_and_accounts() {
        assert_eq!(parse_token("bnb").unwrap(), NATIVE);
        assert_eq!(parse_token(" USDT ").unwrap(), USDT);
        assert!(parse_token("0x55d398326f99059fF775485246999027B3197956").is_err(), "bad checksum");
        assert_eq!(parse_token("0x55d398326f99059ff775485246999027b3197955").unwrap(), USDT);
        assert!(parse_account("0x0000000000000000000000000000000000000000").is_err());
        assert_eq!(js_num(1.0), json!(1));
        assert_eq!(js_num(0.25), json!(0.25));
    }
}
