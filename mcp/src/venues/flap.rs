//! Flap bonding curves on BNB Chain (the Flap Portal). Buys and sells go through the Portal's
//! swapExactInput with a minimum output; only BNB-quoted tokens still on the curve are supported.
//! Graduated tokens trade on PancakeSwap.

use alloy::primitives::{address, Address, Bytes, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use serde_json::{json, Value};

use crate::chain::*;
use crate::tools::*;

pub fn portal() -> Address {
    std::env::var("FLAP_PORTAL").ok().and_then(|v| v.parse().ok()).unwrap_or(address!("e2cE6ab80874Fa9Fa2aAE65D277Dd6B8e65C9De0"))
}

sol! {
    interface IPortal {
        struct TokenStateV5 { uint8 status; uint256 reserve; uint256 circulatingSupply; uint256 price; uint8 tokenVersion; uint256 r; uint256 h; uint256 k; uint256 dexSupplyThresh; address quoteTokenAddress; bool nativeToQuoteSwapEnabled; bytes32 extensionID; }
        struct QuoteExactInputParams { address inputToken; address outputToken; uint256 inputAmount; }
        struct SwapExactInputParams { address inputToken; address outputToken; uint256 inputAmount; uint256 minOutputAmount; bytes permitData; }
        function getTokenV5(address token) external view returns (TokenStateV5 state);
        function quoteExactInput(QuoteExactInputParams params) external returns (uint256 outputAmount);
        function swapExactInput(SwapExactInputParams params) external payable returns (uint256 outputAmount);
    }
}

// Status 1 is trading on the curve (checked against live tokens); anything else isn't tradable here.
const TRADING: u8 = 1;

pub const NAME: &str = "orvyn-flap";
pub const TITLE: &str = "Flap (Orvyn)";
pub const INSTRUCTIONS: &str = "Flap bonding curves on BNB Chain. flap_token_info shows a token's curve; flap_quote prices a buy (spend BNB) or a sell; flap_build_swap returns unsigned transactions for the account to sign and send. Nothing is signed or sent here. Graduated tokens trade on PancakeSwap.";

fn token() -> Value {
    string("The Flap token's address")
}
fn side() -> Value {
    enum_arg(&["buy", "sell"], Some("buy: spend `amount` BNB; sell: sell `amount` tokens for BNB"))
}

pub fn tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "flap_token_info",
            title: "Flap token",
            description: "A Flap token's bonding curve: status, reserve, circulating supply, price and quote currency.",
            properties: vec![("token", token())],
            required: &["token"],
            build: false,
        },
        Tool {
            name: "flap_quote",
            title: "Quote a Flap trade",
            description: "Output of a Flap curve buy (BNB in) or sell (tokens in, BNB out).",
            properties: vec![("token", token()), ("side", side()), ("amount", amount_arg())],
            required: &["token", "side", "amount"],
            build: false,
        },
        Tool {
            name: "flap_build_swap",
            title: "Build a Flap trade",
            description: "Unsigned transactions for a Flap curve buy (BNB attached) or sell (an exact-amount approval when needed, then the sell), each with a minimum output.",
            properties: vec![("token", token()), ("side", side()), ("amount", amount_arg()), ("account", account_arg()), ("slippage_bps", slippage_arg())],
            required: &["token", "side", "amount", "account"],
            build: true,
        },
    ]
}

struct Curve {
    token: Address,
    state: IPortal::TokenStateV5,
    view: Value,
}

async fn curve_token(input: &str) -> Res<Curve> {
    let token = parse_token(input)?;
    if token == NATIVE {
        return Err(ToolError::new("give the Flap token's address"));
    }
    let state = read(portal(), IPortal::getTokenV5Call { token }).await.ok().filter(|s| !(s.reserve.is_zero() && s.circulatingSupply.is_zero()));
    let Some(state) = state else { return Err(ToolError::new(format!("{} isn't a Flap token", checksum(token)))) };
    let view = json!({
        "token": checksum(token),
        "status": state.status,
        "tradingOnCurve": state.status == TRADING,
        "reserve": from_units(state.reserve, 18),
        "circulatingSupply": from_units(state.circulatingSupply, 18),
        "price": from_units(state.price, 18),
        "quoteToken": if state.quoteTokenAddress == NATIVE { "BNB".to_string() } else { checksum(state.quoteTokenAddress) },
    });
    Ok(Curve { token, state, view })
}

fn tradable(t: &Curve) -> Res<()> {
    if t.state.status != TRADING {
        return Err(ToolError::new(format!("This token isn't trading on the Flap curve (status {}); if it has graduated, trade it with the PancakeSwap server.", t.state.status)));
    }
    if t.state.quoteTokenAddress != NATIVE {
        return Err(ToolError::new(format!("This token's curve is quoted in {}; only BNB-quoted Flap tokens are supported", checksum(t.state.quoteTokenAddress))));
    }
    Ok(())
}

/// (input token, output token, output amount)
async fn quote(token: Address, side: &str, amount: U256, circulating: U256) -> Res<(Address, Address, U256)> {
    if side == "sell" && amount > circulating {
        return Err(ToolError::new(format!("Only {} tokens are in circulation on the Flap curve; sell less than that", from_units(circulating, 18))));
    }
    let (input, output) = if side == "buy" { (NATIVE, token) } else { (token, NATIVE) };
    let out = read(portal(), IPortal::quoteExactInputCall { params: IPortal::QuoteExactInputParams { inputToken: input, outputToken: output, inputAmount: amount } }).await.map_err(|_| {
        ToolError::new(if side == "sell" { "The Flap curve can't take that sell; try a smaller amount" } else { "The Flap curve can't fill that buy; try a smaller amount" })
    })?;
    Ok((input, output, out))
}

pub async fn call(name: &str, a: &Args<'_>) -> Res<Value> {
    let t = curve_token(a.str("token")).await?;
    if name == "flap_token_info" {
        return Ok(t.view);
    }
    tradable(&t)?;
    let meta = token_meta(t.token).await?;
    let side = a.str("side");
    let amount = a.str("amount");
    if name == "flap_quote" {
        let amount_in = to_units(amount, 18)?;
        let (_, _, out) = quote(t.token, side, amount_in, t.state.circulatingSupply).await?;
        return Ok(if side == "buy" {
            json!({ "side": side, "spend": format!("{amount} BNB"), "receive": format!("{} {}", from_units(out, meta.decimals), meta.symbol), "curve": t.view })
        } else {
            json!({ "side": side, "sell": format!("{amount} {}", meta.symbol), "receive": format!("{} BNB", from_units(out, 18)), "curve": t.view })
        });
    }
    let account = parse_account(a.str("account"))?;
    let bnb = token_meta(NATIVE).await?;
    let amount_in = to_units(amount, if side == "buy" { 18 } else { meta.decimals })?;
    let (input, output, out) = quote(t.token, side, amount_in, t.state.circulatingSupply).await?;
    if out.is_zero() {
        return Err(ToolError::new("The curve returns nothing for that amount"));
    }
    require_balance(input, account, amount_in).await?;
    let approval = if side == "sell" { approval_if_needed(t.token, account, portal(), amount_in).await? } else { vec![] };
    let built = build_measured(account, output, out, a.opt_int("slippage_bps"), |min| {
        let swap = Call {
            to: portal(),
            data: IPortal::swapExactInputCall { params: IPortal::SwapExactInputParams { inputToken: input, outputToken: output, inputAmount: amount_in, minOutputAmount: min, permitData: Bytes::new() } }
                .abi_encode()
                .into(),
            value: if side == "buy" { amount_in } else { U256::ZERO },
            description: if side == "buy" {
                format!("Buy at least {} {} for {amount} BNB on Flap", from_units(min, meta.decimals), meta.symbol)
            } else {
                format!("Sell {amount} {} for at least {} BNB on Flap", meta.symbol, from_units(min, 18))
            },
        };
        approval.iter().cloned().chain([swap]).collect()
    })
    .await?;
    let (token_in, token_out) = if side == "buy" { (&bnb, &meta) } else { (&meta, &bnb) };
    let p = plan(Plan {
        venue: "flap",
        account,
        calls: &built.calls,
        token_in,
        token_out,
        amount_in,
        amount_out: built.expected,
        min_amount_out: built.min_amount_out,
        route: json!({ "kind": "bonding-curve", "curve": t.view }),
        deadline: None,
    });
    Ok(with_simulation(p, built.simulation))
}
