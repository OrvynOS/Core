//! Uniswap v3 on BNB Chain (SwapRouter02), single-hop or two hops through WBNB/USDT.

use alloy::primitives::{address, Address, U256};
use serde_json::{json, Value};

use crate::chain::*;
use crate::tools::*;
use crate::v3::{best_v3_route, build_v3_swap, describe_v3, SwapInput, V3Route, V3Venue};

pub const ROUTER: Address = address!("B971eF87ede563556b2ED4b1C0b0019111Dd85d2");
pub const QUOTER: Address = address!("78D78E420Da98ad378D7799bE8f4AF69033EB077");

const VENUE: V3Venue = V3Venue { quoter: QUOTER, router: ROUTER, fees: &[100, 500, 3000, 10000] };

pub const NAME: &str = "orvyn-uniswap";
pub const TITLE: &str = "Uniswap (Orvyn)";
pub const INSTRUCTIONS: &str = "Uniswap v3 on BNB Chain. uniswap_quote prices a swap; uniswap_build_swap returns unsigned transactions (an approval if needed, then the swap) for the account to sign and send. Nothing is signed or sent here.";

pub fn tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "uniswap_quote",
            title: "Quote a Uniswap swap",
            description: "Best Uniswap v3 route on BNB Chain and its output for an exact input amount.",
            properties: vec![("token_in", token_arg()), ("token_out", token_arg()), ("amount_in", amount_arg())],
            required: &["token_in", "token_out", "amount_in"],
            build: false,
        },
        Tool {
            name: "uniswap_build_swap",
            title: "Build a Uniswap swap",
            description: "Unsigned BNB Chain transactions for an exact-input Uniswap v3 swap from `account` to `account`: an exact-amount approval when needed, then the swap with a minimum output and a deadline.",
            properties: vec![
                ("token_in", token_arg()),
                ("token_out", token_arg()),
                ("amount_in", amount_arg()),
                ("account", account_arg()),
                ("slippage_bps", slippage_arg()),
                ("deadline_seconds", deadline_arg()),
            ],
            required: &["token_in", "token_out", "amount_in", "account"],
            build: true,
        },
    ]
}

async fn route(token_in: Address, token_out: Address, amount_in: U256) -> Res<V3Route> {
    best_v3_route(&VENUE, token_in, token_out, amount_in).await?.ok_or_else(|| ToolError::new("No Uniswap v3 pool on BNB Chain for this pair"))
}

pub async fn call(name: &str, a: &Args<'_>) -> Res<Value> {
    let token_in = parse_token(a.str("token_in"))?;
    let token_out = parse_token(a.str("token_out"))?;
    let amount_text = a.str("amount_in");
    if name == "uniswap_quote" {
        let (mi, mo) = tokio::try_join!(token_meta(token_in), token_meta(token_out))?;
        let best = route(token_in, token_out, to_units(amount_text, mi.decimals)?).await?;
        return Ok(json!({
            "tokenIn": mi.json(),
            "tokenOut": mo.json(),
            "amountIn": amount_text,
            "amountOut": from_units(best.amount_out, mo.decimals),
            "amountOutRaw": best.amount_out.to_string(),
            "route": describe_v3(&best),
        }));
    }
    let account = parse_account(a.str("account"))?;
    let (mi, mo) = tokio::try_join!(token_meta(token_in), token_meta(token_out))?;
    let amount_in = to_units(amount_text, mi.decimals)?;
    require_balance(token_in, account, amount_in).await?;
    let best = route(token_in, token_out, amount_in).await?;
    let deadline = deadline_from(a.opt_int("deadline_seconds"));
    let approval = approval_if_needed(token_in, account, ROUTER, amount_in).await?;
    let built = build_measured(account, token_out, best.amount_out, a.opt_int("slippage_bps"), |min| {
        let label = format!("Swap {amount_text} {} for at least {} {} on Uniswap v3", mi.symbol, from_units(min, mo.decimals), mo.symbol);
        let swap = build_v3_swap(&VENUE, &best, &SwapInput { token_in, token_out, account, amount_in, min_amount_out: min, deadline, label });
        approval.iter().cloned().chain([swap]).collect()
    })
    .await?;
    let p = plan(Plan {
        venue: "uniswap-v3",
        account,
        calls: &built.calls,
        token_in: &mi,
        token_out: &mo,
        amount_in,
        amount_out: built.expected,
        min_amount_out: built.min_amount_out,
        route: describe_v3(&best),
        deadline: Some(deadline),
    });
    Ok(with_simulation(p, built.simulation))
}
