//! PancakeSwap on BNB Chain: v2 (with fee-on-transfer support, which tax tokens need) and v3
//! through the SmartRouter. Picks the better quote unless a version is asked for.

use alloy::primitives::{address, Address, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use serde_json::{json, Value};

use crate::chain::*;
use crate::tools::*;
use crate::v3::{best_v3_route, build_v3_swap, describe_v3, SwapInput, V3Route, V3Venue};

pub const V2_ROUTER: Address = address!("10ED43C718714eb63d5aA57B78B54704E256024E");
pub const SMART_ROUTER: Address = address!("13f4EA83D0bd40E75C8222255bc855a974568Dd4");
pub const V3_QUOTER: Address = address!("B048Bbc1Ee6b733FFfCFb9e9CeF7375518e25997");

const V3: V3Venue = V3Venue { quoter: V3_QUOTER, router: SMART_ROUTER, fees: &[100, 500, 2500, 10000] };

sol! {
    interface IV2Router {
        function getAmountsOut(uint256 amountIn, address[] path) external view returns (uint256[] amounts);
        function swapExactTokensForTokensSupportingFeeOnTransferTokens(uint256 amountIn, uint256 amountOutMin, address[] path, address to, uint256 deadline) external;
        function swapExactETHForTokensSupportingFeeOnTransferTokens(uint256 amountOutMin, address[] path, address to, uint256 deadline) external payable;
        function swapExactTokensForETHSupportingFeeOnTransferTokens(uint256 amountIn, uint256 amountOutMin, address[] path, address to, uint256 deadline) external;
    }
}

pub const NAME: &str = "orvyn-pancakeswap";
pub const TITLE: &str = "PancakeSwap (Orvyn)";
pub const INSTRUCTIONS: &str = "PancakeSwap on BNB Chain (v2 and v3). pancakeswap_quote prices a swap; pancakeswap_build_swap returns the unsigned transactions (an approval if needed, then the swap) for the account to sign and send. Nothing is signed or sent here. Graduated four.meme and Flap tokens trade here; tax tokens need slippage above their tax.";

pub fn tools() -> Vec<Tool> {
    let version = || enum_arg(&["auto", "v2", "v3"], Some("Pool version (default auto: the better quote)"));
    vec![
        Tool {
            name: "pancakeswap_quote",
            title: "Quote a PancakeSwap swap",
            description: "Best PancakeSwap route and output for an exact input amount (v2 and v3, direct or through WBNB/USDT).",
            properties: vec![("token_in", token_arg()), ("token_out", token_arg()), ("amount_in", amount_arg()), ("version", version())],
            required: &["token_in", "token_out", "amount_in"],
            build: false,
        },
        Tool {
            name: "pancakeswap_build_swap",
            title: "Build a PancakeSwap swap",
            description: "Unsigned BNB Chain transactions for an exact-input PancakeSwap swap from `account` to `account`: an exact-amount approval when needed, then the swap with a minimum output and a deadline.",
            properties: vec![
                ("token_in", token_arg()),
                ("token_out", token_arg()),
                ("amount_in", amount_arg()),
                ("account", account_arg()),
                ("slippage_bps", slippage_arg()),
                ("deadline_seconds", deadline_arg()),
                ("version", version()),
            ],
            required: &["token_in", "token_out", "amount_in", "account"],
            build: true,
        },
    ]
}

#[derive(Clone)]
struct V2Route {
    path: Vec<Address>,
    amount_out: U256,
}

#[derive(Clone)]
enum Route {
    V2(V2Route),
    V3(V3Route),
}

impl Route {
    fn amount_out(&self) -> U256 {
        match self {
            Route::V2(r) => r.amount_out,
            Route::V3(r) => r.amount_out,
        }
    }
    fn kind(&self) -> &'static str {
        match self {
            Route::V2(_) => "v2",
            Route::V3(_) => "v3",
        }
    }
    fn describe(&self) -> Value {
        match self {
            Route::V2(r) => json!({ "kind": "v2", "path": r.path.iter().map(|a| checksum(*a)).collect::<Vec<_>>(), "fee": "0.25% per hop" }),
            Route::V3(r) => describe_v3(r),
        }
    }
}

async fn best_v2_route(token_in: Address, token_out: Address, amount_in: U256) -> Res<Option<V2Route>> {
    let (a, b) = (pool_token(token_in), pool_token(token_out));
    if a == b {
        return Err(ToolError::new("tokenIn and tokenOut are the same (BNB and WBNB count as the same here)"));
    }
    let mut paths = vec![vec![a, b]];
    if a != WBNB && b != WBNB {
        paths.push(vec![a, WBNB, b]);
    }
    let calls = paths.iter().map(|p| (V2_ROUTER, IV2Router::getAmountsOutCall { amountIn: amount_in, path: p.clone() }.abi_encode())).collect();
    let mut best: Option<V2Route> = None;
    for (i, r) in multicall(calls).await?.into_iter().enumerate() {
        let Some(data) = r else { continue };
        let Ok(amounts) = IV2Router::getAmountsOutCall::abi_decode_returns(&data) else { continue };
        let Some(out) = amounts.last().copied() else { continue };
        if !out.is_zero() && best.as_ref().is_none_or(|b| out > b.amount_out) {
            best = Some(V2Route { path: paths[i].clone(), amount_out: out });
        }
    }
    Ok(best)
}

fn build_v2_swap(route: &V2Route, i: &SwapInput) -> Call {
    let (amount_in, min, path, to, deadline) = (i.amount_in, i.min_amount_out, route.path.clone(), i.account, i.deadline);
    let data = if is_native(i.token_in) {
        IV2Router::swapExactETHForTokensSupportingFeeOnTransferTokensCall { amountOutMin: min, path, to, deadline }.abi_encode()
    } else if is_native(i.token_out) {
        IV2Router::swapExactTokensForETHSupportingFeeOnTransferTokensCall { amountIn: amount_in, amountOutMin: min, path, to, deadline }.abi_encode()
    } else {
        IV2Router::swapExactTokensForTokensSupportingFeeOnTransferTokensCall { amountIn: amount_in, amountOutMin: min, path, to, deadline }.abi_encode()
    };
    Call { to: V2_ROUTER, data: data.into(), value: if is_native(i.token_in) { amount_in } else { U256::ZERO }, description: i.label.clone() }
}

async fn quote(token_in: Address, token_out: Address, amount_in: U256, version: &str) -> Res<(Route, Option<V2Route>, Option<V3Route>)> {
    let (v2, v3) = tokio::join!(
        async { if version == "v3" { Ok(None) } else { best_v2_route(token_in, token_out, amount_in).await } },
        async { if version == "v2" { Ok(None) } else { best_v3_route(&V3, token_in, token_out, amount_in).await } },
    );
    let (v2, v3) = (v2?, v3?);
    let best = match (&v2, &v3) {
        (None, Some(b)) => Route::V3(b.clone()),
        (Some(a), None) => Route::V2(a.clone()),
        (Some(a), Some(b)) => if b.amount_out > a.amount_out { Route::V3(b.clone()) } else { Route::V2(a.clone()) },
        (None, None) => return Err(ToolError::new(format!("No PancakeSwap {}pool for this pair", if version == "auto" { String::new() } else { format!("{version} ") }))),
    };
    Ok((best, v2, v3))
}

pub async fn call(name: &str, a: &Args<'_>) -> Res<Value> {
    let token_in = parse_token(a.str("token_in"))?;
    let token_out = parse_token(a.str("token_out"))?;
    let version = a.opt_str("version").unwrap_or("auto");
    let amount_text = a.str("amount_in");
    match name {
        "pancakeswap_quote" => {
            let (mi, mo) = tokio::try_join!(token_meta(token_in), token_meta(token_out))?;
            let amount_in = to_units(amount_text, mi.decimals)?;
            let (best, v2, v3) = quote(token_in, token_out, amount_in, version).await?;
            Ok(json!({
                "tokenIn": mi.json(),
                "tokenOut": mo.json(),
                "amountIn": amount_text,
                "amountOut": from_units(best.amount_out(), mo.decimals),
                "amountOutRaw": best.amount_out().to_string(),
                "route": best.describe(),
                "alternatives": { "v2": v2.map(|r| r.amount_out.to_string()), "v3": v3.map(|r| r.amount_out.to_string()) },
            }))
        }
        _ => {
            let account = parse_account(a.str("account"))?;
            let (mi, mo) = tokio::try_join!(token_meta(token_in), token_meta(token_out))?;
            let amount_in = to_units(amount_text, mi.decimals)?;
            require_balance(token_in, account, amount_in).await?;
            let (best, _, _) = quote(token_in, token_out, amount_in, version).await?;
            let deadline = deadline_from(a.opt_int("deadline_seconds"));
            let spender = if matches!(best, Route::V2(_)) { V2_ROUTER } else { SMART_ROUTER };
            let approval = approval_if_needed(token_in, account, spender, amount_in).await?;
            let built = build_measured(account, token_out, best.amount_out(), a.opt_int("slippage_bps"), |min| {
                let label = format!("Swap {amount_text} {} for at least {} {} on PancakeSwap {}", mi.symbol, from_units(min, mo.decimals), mo.symbol, best.kind());
                let input = SwapInput { token_in, token_out, account, amount_in, min_amount_out: min, deadline, label };
                let swap = match &best {
                    Route::V2(r) => build_v2_swap(r, &input),
                    Route::V3(r) => build_v3_swap(&V3, r, &input),
                };
                approval.iter().cloned().chain([swap]).collect()
            })
            .await?;
            let venue = format!("pancakeswap-{}", best.kind());
            let p = plan(Plan {
                venue: &venue,
                account,
                calls: &built.calls,
                token_in: &mi,
                token_out: &mo,
                amount_in,
                amount_out: built.expected,
                min_amount_out: built.min_amount_out,
                route: best.describe(),
                deadline: Some(deadline),
            });
            Ok(with_simulation(p, built.simulation))
        }
    }
}
