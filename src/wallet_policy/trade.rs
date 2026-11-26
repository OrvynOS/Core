//! Trading calls an agent's key may sign on BNB Chain: exactly what Orvyn's trading MCP servers
//! build, and nothing that could send the output anywhere but the agent's own wallet.
//!
//! - PancakeSwap v2 router: the three fee-on-transfer swaps, `to` = the agent.
//! - PancakeSwap v3 SmartRouter and Uniswap v3 SwapRouter02: `multicall(deadline, data)` whose calls
//!   are only exactInputSingle / exactInput paying the agent, or paying the router itself followed by
//!   unwrapWETH9 to the agent.
//! - four.meme TokenManager2: buyTokenAMAP and sellToken (proceeds go to the caller).
//! - Flap Portal: swapExactInput between BNB and a token (output goes to the caller), no permit.
//! - Approvals: only to those contracts, never unlimited.
//!
//! BNB and stablecoins spent on trades count against the daily trading caps. Selling tokens back
//! isn't capped: the proceeds come back to the agent.

use alloy::primitives::{Address, U256};
use alloy::sol;
use alloy::sol_types::SolInterface;

use super::config::cfg;
use super::{add_spend, addr, now_secs, refuse, usd_micros, Spends};
use crate::error::Result;

const NATIVE: &str = "0x0000000000000000000000000000000000000000";
const ADDRESS_THIS: &str = "0x0000000000000000000000000000000000000002";
const MAX_DEADLINE_SEC: u64 = 3600;
const MAX_HOPS: usize = 4;

sol! {
    interface V2 {
        function swapExactTokensForTokensSupportingFeeOnTransferTokens(uint256 amountIn, uint256 amountOutMin, address[] path, address to, uint256 deadline);
        function swapExactETHForTokensSupportingFeeOnTransferTokens(uint256 amountOutMin, address[] path, address to, uint256 deadline) payable;
        function swapExactTokensForETHSupportingFeeOnTransferTokens(uint256 amountIn, uint256 amountOutMin, address[] path, address to, uint256 deadline);
    }
    struct ExactInputSingleParams { address tokenIn; address tokenOut; uint24 fee; address recipient; uint256 amountIn; uint256 amountOutMinimum; uint160 sqrtPriceLimitX96; }
    struct ExactInputParams { bytes path; address recipient; uint256 amountIn; uint256 amountOutMinimum; }
    interface Router02 {
        function multicall(uint256 deadline, bytes[] data) payable returns (bytes[] results);
        function exactInputSingle(ExactInputSingleParams params) payable returns (uint256 amountOut);
        function exactInput(ExactInputParams params) payable returns (uint256 amountOut);
        function unwrapWETH9(uint256 amountMinimum, address recipient) payable;
    }
    interface FourMemeTrade {
        function buyTokenAMAP(address token, uint256 funds, uint256 minAmount) payable;
        function sellToken(uint256 origin, address token, uint256 amount, uint256 minFunds);
    }
    struct FlapSwapParams { address inputToken; address outputToken; uint256 inputAmount; uint256 minOutputAmount; bytes permitData; }
    interface Flap {
        function swapExactInput(FlapSwapParams params) payable returns (uint256 outputAmount);
    }
}

fn is_stable(token: &str) -> bool {
    cfg().stablecoins.contains(&token.to_lowercase())
}

pub fn trade_contracts() -> Vec<String> {
    let c = cfg();
    [c.pancake_v2_router.clone(), c.pancake_smart_router.clone(), c.uniswap_router.clone(), Some(c.fourmeme_manager.clone()), c.flap_portal.clone()].into_iter().flatten().collect()
}

fn check_deadline(deadline: U256) -> Result<()> {
    let now = U256::from(now_secs());
    if deadline < now {
        return Err(refuse("the swap's deadline has passed"));
    }
    if deadline > now + U256::from(MAX_DEADLINE_SEC) {
        return Err(refuse("swap deadlines may be at most an hour away"));
    }
    Ok(())
}

fn count_input(spends: &mut Spends, value: U256, token_in: &str, amount_in: U256) {
    if value > U256::ZERO {
        add_spend(spends, "trade-bnb", value);
    } else if is_stable(token_in) {
        add_spend(spends, "trade-usd", usd_micros(amount_in));
    }
}

fn must_pay_agent(recipient: &Address, agent: &str) -> Result<()> {
    if addr(recipient) != agent.to_lowercase() {
        return Err(refuse("swap output must go to the agent's own wallet"));
    }
    Ok(())
}

fn check_path(path: &[Address]) -> Result<()> {
    if path.len() < 2 || path.len() > MAX_HOPS {
        return Err(refuse("swap paths may have 2-4 tokens"));
    }
    Ok(())
}

fn check_v2(data: &[u8], value: U256, agent: &str, spends: &mut Spends) -> Result<String> {
    let call = V2::V2Calls::abi_decode(data).map_err(|_| refuse("only fee-on-transfer exact-input swaps may be called on the PancakeSwap v2 router"))?;
    match call {
        V2::V2Calls::swapExactETHForTokensSupportingFeeOnTransferTokens(c) => {
            check_path(&c.path)?;
            must_pay_agent(&c.to, agent)?;
            check_deadline(c.deadline)?;
            if value == U256::ZERO {
                return Err(refuse("a BNB swap must attach the BNB"));
            }
            count_input(spends, value, NATIVE, value);
            Ok(format!("PancakeSwap v2: {value} wei BNB → {}", c.path.last().unwrap()))
        }
        V2::V2Calls::swapExactTokensForTokensSupportingFeeOnTransferTokens(c) => v2_token_swap(c.amountIn, &c.path, &c.to, c.deadline, false, value, agent, spends),
        V2::V2Calls::swapExactTokensForETHSupportingFeeOnTransferTokens(c) => v2_token_swap(c.amountIn, &c.path, &c.to, c.deadline, true, value, agent, spends),
    }
}

#[allow(clippy::too_many_arguments)]
fn v2_token_swap(amount_in: U256, path: &[Address], to: &Address, deadline: U256, for_eth: bool, value: U256, agent: &str, spends: &mut Spends) -> Result<String> {
    check_path(path)?;
    must_pay_agent(to, agent)?;
    check_deadline(deadline)?;
    if value != U256::ZERO {
        return Err(refuse("no BNB may be attached to a token swap"));
    }
    count_input(spends, U256::ZERO, &addr(&path[0]), amount_in);
    let out = if for_eth { "BNB".to_string() } else { path.last().unwrap().to_string() };
    Ok(format!("PancakeSwap v2: {amount_in} of {} → {out}", path[0]))
}

fn check_router02(name: &str, data: &[u8], value: U256, agent: &str, spends: &mut Spends) -> Result<String> {
    let outer = Router02::Router02Calls::abi_decode(data).map_err(|_| refuse(format!("only multicall(deadline, swaps) may be called on the {name} router")))?;
    let Router02::Router02Calls::multicall(outer) = outer else {
        return Err(refuse(format!("{name}: swaps must be wrapped in multicall with a deadline")));
    };
    check_deadline(outer.deadline)?;
    let inner = &outer.data;
    if inner.is_empty() || inner.len() > 2 {
        return Err(refuse(format!("{name}: one swap, optionally followed by unwrapping BNB")));
    }
    let mut to_router = false;
    let mut swaps = 0;
    let mut summary = vec![];
    for (i, d) in inner.iter().enumerate() {
        let call = Router02::Router02Calls::abi_decode(d).map_err(|_| refuse(format!("{name}: only exactInputSingle, exactInput and unwrapWETH9 are allowed")))?;
        let (recipient, amount_in, token_in, fname) = match call {
            Router02::Router02Calls::multicall(_) => return Err(refuse(format!("{name}: nested multicall isn't allowed"))),
            Router02::Router02Calls::unwrapWETH9(c) => {
                if i != inner.len() - 1 || !to_router {
                    return Err(refuse(format!("{name}: unwrapWETH9 only right after a swap paid to the router")));
                }
                must_pay_agent(&c.recipient, agent)?;
                to_router = false;
                summary.push("unwrap to BNB".to_string());
                continue;
            }
            Router02::Router02Calls::exactInputSingle(c) => (c.params.recipient, c.params.amountIn, c.params.tokenIn.to_string(), "exactInputSingle"),
            Router02::Router02Calls::exactInput(c) => {
                let path = &c.params.path;
                // The first 20 bytes of the path are the input token.
                if path.len() < 20 {
                    return Err(refuse("swap paths may have 2-4 tokens"));
                }
                if path.len() > 20 + (20 + 3) * (MAX_HOPS - 1) {
                    return Err(refuse("swap paths may have 2-4 tokens"));
                }
                (c.params.recipient, c.params.amountIn, format!("0x{}", hex::encode(&path[..20])), "exactInput")
            }
        };
        if i != 0 {
            return Err(refuse(format!("{name}: one swap per transaction")));
        }
        swaps += 1;
        if addr(&recipient) == ADDRESS_THIS {
            to_router = true;
        } else {
            must_pay_agent(&recipient, agent)?;
        }
        if value != U256::ZERO && value != amount_in {
            return Err(refuse(format!("{name}: attached BNB must equal the swap input")));
        }
        count_input(spends, value, &token_in, amount_in);
        summary.push(format!("{fname} {amount_in} of {}", if value > U256::ZERO { "BNB".to_string() } else { token_in }));
    }
    if swaps != 1 {
        return Err(refuse(format!("{name}: exactly one swap per transaction")));
    }
    if to_router {
        return Err(refuse(format!("{name}: output paid to the router must be unwrapped to the agent")));
    }
    Ok(format!("{name}: {}", summary.join(", ")))
}

/// four.meme buys and sells; None if the call isn't one (createToken is checked by the caller).
fn check_fourmeme_trade(data: &[u8], value: U256, spends: &mut Spends) -> Result<Option<String>> {
    let Ok(call) = FourMemeTrade::FourMemeTradeCalls::abi_decode(data) else { return Ok(None) };
    match call {
        FourMemeTrade::FourMemeTradeCalls::buyTokenAMAP(c) => {
            if value > U256::ZERO {
                if value < c.funds {
                    return Err(refuse("four.meme: attached BNB doesn't cover the buy"));
                }
                add_spend(spends, "trade-bnb", value);
            }
            let funds = if value > U256::ZERO { format!("{value} wei") } else { format!("{} of its quote token", c.funds) };
            Ok(Some(format!("four.meme buy of {} for {funds}", c.token)))
        }
        FourMemeTrade::FourMemeTradeCalls::sellToken(c) => {
            if c.origin != U256::ZERO {
                return Err(refuse("four.meme: sells use origin 0"));
            }
            if value != U256::ZERO {
                return Err(refuse("no BNB may be attached to a sell"));
            }
            Ok(Some(format!("four.meme sell of {} {}", c.amount, c.token)))
        }
    }
}

fn check_flap(data: &[u8], value: U256, spends: &mut Spends) -> Result<String> {
    let call = Flap::swapExactInputCall::abi_decode(data).map_err(|_| refuse("only swapExactInput may be called on the Flap Portal"))?;
    let p = call.params;
    if !p.permitData.is_empty() {
        return Err(refuse("Flap: permits aren't allowed"));
    }
    let buy = addr(&p.inputToken) == NATIVE;
    if !buy && addr(&p.outputToken) != NATIVE {
        return Err(refuse("Flap: trades are between BNB and a token"));
    }
    if buy {
        if value != p.inputAmount {
            return Err(refuse("Flap: attached BNB must equal the input"));
        }
        add_spend(spends, "trade-bnb", value);
        return Ok(format!("Flap buy of {} for {value} wei", p.outputToken));
    }
    if value != U256::ZERO {
        return Err(refuse("no BNB may be attached to a sell"));
    }
    Ok(format!("Flap sell of {} {}", p.inputAmount, p.inputToken))
}

use alloy::sol_types::SolCall;

/// The trading part of the policy: a summary when `to` is a trading contract (after checking the
/// call), None when it isn't one.
pub fn check_trade_call(to: &str, data: &[u8], value: U256, agent: &str, spends: &mut Spends) -> Result<Option<String>> {
    let c = cfg();
    let is = |a: &Option<String>| a.as_deref() == Some(to);
    if is(&c.pancake_v2_router) {
        return check_v2(data, value, agent, spends).map(Some);
    }
    if is(&c.pancake_smart_router) {
        return check_router02("PancakeSwap v3", data, value, agent, spends).map(Some);
    }
    if is(&c.uniswap_router) {
        return check_router02("Uniswap v3", data, value, agent, spends).map(Some);
    }
    if is(&c.flap_portal) {
        return check_flap(data, value, spends).map(Some);
    }
    if to == c.fourmeme_manager {
        return check_fourmeme_trade(data, value, spends);
    }
    Ok(None)
}

/// Approving a trading contract to take a token: allowed, but never unlimited.
/// `spender` as the call decoded it (checksummed), for the summary.
pub fn check_trade_approval(token: &str, spender: &str, amount: U256) -> Result<Option<String>> {
    if !trade_contracts().contains(&spender.to_lowercase()) {
        return Ok(None);
    }
    if amount == U256::MAX {
        return Err(refuse("approvals for trading must be for the exact amount, not unlimited"));
    }
    Ok(Some(format!("approve {amount} of {token} for {spender}")))
}
