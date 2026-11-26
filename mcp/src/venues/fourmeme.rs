//! four.meme bonding curves on BNB Chain (TokenManager2). Quotes come from TokenManagerHelper3
//! (tryBuy / trySell), which also says what to approve and what to send. Graduated tokens trade
//! on PancakeSwap instead.

use alloy::primitives::{address, Address, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use serde_json::{json, Value};

use crate::chain::*;
use crate::tools::*;

pub const TOKEN_MANAGER: Address = address!("5c952063c7fc8610FFDB798152D69F0B9550762b");
pub const HELPER: Address = address!("F251F83e40a78868FcfA3FA4599Dad6494E46034");

sol! {
    interface IHelper3 {
        function getTokenInfo(address token) external view returns (uint256 version, address tokenManager, address quote, uint256 lastPrice, uint256 tradingFeeRate, uint256 minTradingFee, uint256 launchTime, uint256 offers, uint256 maxOffers, uint256 funds, uint256 maxFunds, bool liquidityAdded);
        function tryBuy(address token, uint256 amount, uint256 funds) external view returns (address tokenManager, address quote, uint256 estimatedAmount, uint256 estimatedCost, uint256 estimatedFee, uint256 amountMsgValue, uint256 amountApproval, uint256 amountFunds);
        function trySell(address token, uint256 amount) external view returns (address tokenManager, address quote, uint256 funds, uint256 fee);
    }
    interface IManager2 {
        function buyTokenAMAP(address token, uint256 funds, uint256 minAmount) external payable;
        function sellToken(uint256 origin, address token, uint256 amount, uint256 minFunds) external;
    }
}

pub const NAME: &str = "orvyn-fourmeme";
pub const TITLE: &str = "four.meme (Orvyn)";
pub const INSTRUCTIONS: &str = "four.meme bonding curves on BNB Chain. fourmeme_token_info shows a token's curve; fourmeme_quote prices a buy (spend BNB, or the token's quote currency) or a sell; fourmeme_build_buy and fourmeme_build_sell return unsigned transactions for the account to sign and send. Nothing is signed or sent here. Graduated tokens trade on PancakeSwap.";

fn token() -> Value {
    string("The four.meme token's address")
}

pub fn tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "fourmeme_token_info",
            title: "four.meme token",
            description: "A four.meme token's bonding curve: price, progress to graduation, quote currency, trading fee, and whether it has graduated.",
            properties: vec![("token", token())],
            required: &["token"],
            build: false,
        },
        Tool {
            name: "fourmeme_quote",
            title: "Quote a four.meme trade",
            description: "Buy: how many tokens `amount` of the quote currency (usually BNB) gets. Sell: how much `amount` tokens returns.",
            properties: vec![("token", token()), ("side", enum_arg(&["buy", "sell"], None)), ("amount", amount_arg())],
            required: &["token", "side", "amount"],
            build: false,
        },
        Tool {
            name: "fourmeme_build_buy",
            title: "Build a four.meme buy",
            description: "Unsigned transactions to buy a four.meme token with `amount` of its quote currency (BNB for most tokens) on the bonding curve, with a minimum token amount.",
            properties: vec![("token", token()), ("amount", amount_arg()), ("account", account_arg()), ("slippage_bps", slippage_arg())],
            required: &["token", "amount", "account"],
            build: true,
        },
        Tool {
            name: "fourmeme_build_sell",
            title: "Build a four.meme sell",
            description: "Unsigned transactions to sell `amount` of a four.meme token back to its bonding curve: an exact-amount approval when needed, then the sell with a minimum return.",
            properties: vec![("token", token()), ("amount", amount_arg()), ("account", account_arg()), ("slippage_bps", slippage_arg())],
            required: &["token", "amount", "account"],
            build: true,
        },
    ]
}

struct Curve {
    token: Address,
    quote: Address,
    manager: Address,
    graduated: bool,
    view: Value,
}

async fn curve_token(input: &str) -> Res<Curve> {
    let token = parse_token(input)?;
    if token == NATIVE {
        return Err(ToolError::new("give the four.meme token's address"));
    }
    let info = read(HELPER, IHelper3::getTokenInfoCall { token }).await.ok().filter(|i| i.tokenManager != NATIVE);
    let Some(i) = info else { return Err(ToolError::new(format!("{} isn't a four.meme token", checksum(token)))) };
    let num = |v: U256| v.to_string().parse::<f64>().unwrap_or(0.0);
    let view = json!({
        "token": checksum(token),
        "version": js_num(num(i.version)),
        "tokenManager": checksum(i.tokenManager),
        "quoteToken": if i.quote == NATIVE { "BNB".to_string() } else { checksum(i.quote) },
        "lastPrice": from_units(i.lastPrice, 18),
        "tradingFeePct": js_num(num(i.tradingFeeRate) / 100.0),
        "launchedAt": iso(i.launchTime.to::<u64>()),
        "curveProgressPct": if i.maxFunds.is_zero() { Value::Null } else { js_num(num(i.funds * U256::from(10_000) / i.maxFunds) / 100.0) },
        "raised": from_units(i.funds, 18),
        "target": from_units(i.maxFunds, 18),
        "tokensLeftOnCurve": from_units(i.offers, 18),
        "curveSupply": from_units(i.maxOffers, 18),
        "graduated": i.liquidityAdded,
    });
    Ok(Curve { token, quote: i.quote, manager: i.tokenManager, graduated: i.liquidityAdded, view })
}

fn tradable(t: &Curve) -> Res<()> {
    if t.graduated {
        return Err(ToolError::new("This token has graduated to PancakeSwap: trade it with the PancakeSwap server (tax tokens need slippage above their tax)."));
    }
    if t.manager != TOKEN_MANAGER {
        return Err(ToolError::new(format!("This token uses an older four.meme manager ({}) that isn't supported", checksum(t.manager))));
    }
    Ok(())
}

pub async fn call(name: &str, a: &Args<'_>) -> Res<Value> {
    let t = curve_token(a.str("token")).await?;
    if name == "fourmeme_token_info" {
        return Ok(t.view);
    }
    tradable(&t)?;
    let (meta, quote_meta) = tokio::try_join!(token_meta(t.token), token_meta(t.quote))?;
    let amount = a.str("amount");
    match name {
        "fourmeme_quote" => {
            if a.str("side") == "buy" {
                let funds = to_units(amount, quote_meta.decimals)?;
                let r = read(HELPER, IHelper3::tryBuyCall { token: t.token, amount: U256::ZERO, funds }).await?;
                Ok(json!({
                    "side": "buy",
                    "spend": format!("{amount} {}", quote_meta.symbol),
                    "receive": format!("{} {}", from_units(r.estimatedAmount, meta.decimals), meta.symbol),
                    "cost": from_units(r.estimatedCost, quote_meta.decimals),
                    "fee": from_units(r.estimatedFee, quote_meta.decimals),
                    "curve": t.view,
                }))
            } else {
                let tokens = to_units(amount, meta.decimals)?;
                let r = read(HELPER, IHelper3::trySellCall { token: t.token, amount: tokens }).await?;
                Ok(json!({
                    "side": "sell",
                    "sell": format!("{amount} {}", meta.symbol),
                    "receive": format!("{} {}", from_units(r.funds.saturating_sub(r.fee), quote_meta.decimals), quote_meta.symbol),
                    "fee": from_units(r.fee, quote_meta.decimals),
                    "curve": t.view,
                }))
            }
        }
        "fourmeme_build_buy" => {
            let account = parse_account(a.str("account"))?;
            let funds = to_units(amount, quote_meta.decimals)?;
            let r = read(HELPER, IHelper3::tryBuyCall { token: t.token, amount: U256::ZERO, funds }).await?;
            if r.estimatedAmount.is_zero() {
                return Err(ToolError::new("The curve returns no tokens for that amount (sold out or too small)"));
            }
            require_balance(t.quote, account, if t.quote == NATIVE { r.amountMsgValue } else { r.amountApproval }).await?;
            let approval = if r.amountApproval.is_zero() { vec![] } else { approval_if_needed(t.quote, account, TOKEN_MANAGER, r.amountApproval).await? };
            // The helper's estimate can overstate what a buy returns (fees, token tax): the minimum
            // comes from a measured dry run when the RPC supports it.
            let built = build_measured(account, t.token, r.estimatedAmount, a.opt_int("slippage_bps"), |min| {
                let buy = Call {
                    to: TOKEN_MANAGER,
                    data: IManager2::buyTokenAMAPCall { token: t.token, funds: r.amountFunds, minAmount: min }.abi_encode().into(),
                    value: r.amountMsgValue,
                    description: format!("Buy at least {} {} for {amount} {} on four.meme", from_units(min, meta.decimals), meta.symbol, quote_meta.symbol),
                };
                approval.iter().cloned().chain([buy]).collect()
            })
            .await?;
            let p = plan(Plan {
                venue: "fourmeme",
                account,
                calls: &built.calls,
                token_in: &quote_meta,
                token_out: &meta,
                amount_in: funds,
                amount_out: built.expected,
                min_amount_out: built.min_amount_out,
                route: json!({ "kind": "bonding-curve", "curve": t.view }),
                deadline: None,
            });
            Ok(with_simulation(p, built.simulation))
        }
        _ => {
            let account = parse_account(a.str("account"))?;
            let tokens = to_units(amount, meta.decimals)?;
            require_balance(t.token, account, tokens).await?;
            let r = read(HELPER, IHelper3::trySellCall { token: t.token, amount: tokens }).await?;
            let net = r.funds.saturating_sub(r.fee);
            if net.is_zero() {
                return Err(ToolError::new("The curve returns nothing for that amount"));
            }
            let approval = approval_if_needed(t.token, account, TOKEN_MANAGER, tokens).await?;
            let built = build_measured(account, t.quote, net, a.opt_int("slippage_bps"), |min| {
                let sell = Call {
                    to: TOKEN_MANAGER,
                    data: IManager2::sellTokenCall { origin: U256::ZERO, token: t.token, amount: tokens, minFunds: min }.abi_encode().into(),
                    value: U256::ZERO,
                    description: format!("Sell {amount} {} for at least {} {} on four.meme", meta.symbol, from_units(min, quote_meta.decimals), quote_meta.symbol),
                };
                approval.iter().cloned().chain([sell]).collect()
            })
            .await?;
            let p = plan(Plan {
                venue: "fourmeme",
                account,
                calls: &built.calls,
                token_in: &meta,
                token_out: &quote_meta,
                amount_in: tokens,
                amount_out: built.expected,
                min_amount_out: built.min_amount_out,
                route: json!({ "kind": "bonding-curve", "curve": t.view }),
                deadline: None,
            });
            Ok(with_simulation(p, built.simulation))
        }
    }
}
