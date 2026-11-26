//! Concentrated-liquidity (v3) routing on a SwapRouter02-style router: PancakeSwap v3
//! (SmartRouter) and Uniswap v3 on BNB Chain share the interface. Quotes come from QuoterV2,
//! single-hop at every fee tier and two hops through WBNB or USDT, batched through Multicall3.

use alloy::primitives::{address, aliases::U24, Address, Bytes, U160, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use serde_json::{json, Value};

use crate::chain::{checksum, is_native, multicall, pool_token, Call, Res, ToolError, USDT, WBNB};

sol! {
    interface IQuoterV2 {
        struct QuoteExactInputSingleParams { address tokenIn; address tokenOut; uint256 amountIn; uint24 fee; uint160 sqrtPriceLimitX96; }
        function quoteExactInputSingle(QuoteExactInputSingleParams params) external returns (uint256 amountOut, uint160 sqrtPriceX96After, uint32 initializedTicksCrossed, uint256 gasEstimate);
        function quoteExactInput(bytes path, uint256 amountIn) external returns (uint256 amountOut, uint160[] sqrtPriceX96AfterList, uint32[] initializedTicksCrossedList, uint256 gasEstimate);
    }
    interface IRouter02 {
        struct ExactInputSingleParams { address tokenIn; address tokenOut; uint24 fee; address recipient; uint256 amountIn; uint256 amountOutMinimum; uint160 sqrtPriceLimitX96; }
        struct ExactInputParams { bytes path; address recipient; uint256 amountIn; uint256 amountOutMinimum; }
        function exactInputSingle(ExactInputSingleParams params) external payable returns (uint256 amountOut);
        function exactInput(ExactInputParams params) external payable returns (uint256 amountOut);
        function unwrapWETH9(uint256 amountMinimum, address recipient) external payable;
        function multicall(uint256 deadline, bytes[] data) external payable returns (bytes[] results);
    }
}

/// SwapRouter02's "the router itself" recipient, used before unwrapping WBNB to BNB.
const ADDRESS_THIS: Address = address!("0000000000000000000000000000000000000002");

pub struct V3Venue {
    pub quoter: Address,
    pub router: Address,
    pub fees: &'static [u32],
}

#[derive(Clone, Debug)]
pub struct V3Route {
    pub path: Vec<Address>,
    pub fees: Vec<u32>,
    pub amount_out: U256,
}

fn encode_path(tokens: &[Address], fees: &[u32]) -> Bytes {
    let mut out = Vec::with_capacity(tokens.len() * 23);
    for (i, t) in tokens.iter().enumerate() {
        out.extend_from_slice(t.as_slice());
        if let Some(f) = fees.get(i) {
            out.extend_from_slice(&f.to_be_bytes()[1..]);
        }
    }
    out.into()
}

pub async fn best_v3_route(venue: &V3Venue, token_in: Address, token_out: Address, amount_in: U256) -> Res<Option<V3Route>> {
    let (a, b) = (pool_token(token_in), pool_token(token_out));
    if a == b {
        return Err(ToolError::new("tokenIn and tokenOut are the same (BNB and WBNB count as the same here)"));
    }
    let mut candidates: Vec<(Vec<Address>, Vec<u32>)> = venue.fees.iter().map(|f| (vec![a, b], vec![*f])).collect();
    for mid in [WBNB, USDT] {
        if mid == a || mid == b {
            continue;
        }
        for f1 in venue.fees {
            for f2 in venue.fees {
                candidates.push((vec![a, mid, b], vec![*f1, *f2]));
            }
        }
    }
    let calls = candidates
        .iter()
        .map(|(path, fees)| {
            let data = if fees.len() == 1 {
                IQuoterV2::quoteExactInputSingleCall {
                    params: IQuoterV2::QuoteExactInputSingleParams { tokenIn: path[0], tokenOut: path[1], amountIn: amount_in, fee: U24::from(fees[0]), sqrtPriceLimitX96: U160::ZERO },
                }
                .abi_encode()
            } else {
                IQuoterV2::quoteExactInputCall { path: encode_path(path, fees), amountIn: amount_in }.abi_encode()
            };
            (venue.quoter, data)
        })
        .collect();
    let results = multicall(calls).await?;
    let mut best: Option<V3Route> = None;
    for (i, r) in results.into_iter().enumerate() {
        let Some(data) = r else { continue };
        // Both quote functions return amountOut first.
        if data.len() < 32 {
            continue;
        }
        let out = U256::from_be_slice(&data[..32]);
        if !out.is_zero() && best.as_ref().is_none_or(|b| out > b.amount_out) {
            best = Some(V3Route { path: candidates[i].0.clone(), fees: candidates[i].1.clone(), amount_out: out });
        }
    }
    Ok(best)
}

pub struct SwapInput {
    pub token_in: Address,
    pub token_out: Address,
    pub account: Address,
    pub amount_in: U256,
    pub min_amount_out: U256,
    pub deadline: U256,
    pub label: String,
}

/// One router call (a multicall with the deadline): BNB in is wrapped by the router; BNB out is
/// unwrapped to the account.
pub fn build_v3_swap(venue: &V3Venue, route: &V3Route, i: &SwapInput) -> Call {
    let native_out = is_native(i.token_out);
    let recipient = if native_out { ADDRESS_THIS } else { i.account };
    let swap: Bytes = if route.fees.len() == 1 {
        IRouter02::exactInputSingleCall {
            params: IRouter02::ExactInputSingleParams {
                tokenIn: route.path[0],
                tokenOut: route.path[1],
                fee: U24::from(route.fees[0]),
                recipient,
                amountIn: i.amount_in,
                amountOutMinimum: i.min_amount_out,
                sqrtPriceLimitX96: U160::ZERO,
            },
        }
        .abi_encode()
        .into()
    } else {
        IRouter02::exactInputCall { params: IRouter02::ExactInputParams { path: encode_path(&route.path, &route.fees), recipient, amountIn: i.amount_in, amountOutMinimum: i.min_amount_out } }
            .abi_encode()
            .into()
    };
    let mut data = vec![swap];
    if native_out {
        data.push(IRouter02::unwrapWETH9Call { amountMinimum: i.min_amount_out, recipient: i.account }.abi_encode().into());
    }
    Call {
        to: venue.router,
        data: IRouter02::multicallCall { deadline: i.deadline, data }.abi_encode().into(),
        value: if is_native(i.token_in) { i.amount_in } else { U256::ZERO },
        description: i.label.clone(),
    }
}

pub fn describe_v3(route: &V3Route) -> Value {
    json!({
        "kind": "v3",
        "path": route.path.iter().map(|a| checksum(*a)).collect::<Vec<_>>(),
        "feeTiers": route.fees.iter().map(|f| format!("{}%", *f as f64 / 10_000.0)).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_fee_tiers() {
        let p = encode_path(&[WBNB, USDT], &[500]);
        assert_eq!(p.len(), 43);
        assert_eq!(&p[20..23], &[0x00, 0x01, 0xf4]);
        let d = describe_v3(&V3Route { path: vec![WBNB, USDT], fees: vec![100, 2500, 3000, 10000], amount_out: U256::ZERO });
        assert_eq!(d["feeTiers"], json!(["0.01%", "0.25%", "0.3%", "1%"]));
    }
}
