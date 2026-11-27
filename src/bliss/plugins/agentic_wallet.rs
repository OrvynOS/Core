//! Binance Agentic Wallet: the agent works the on-chain wallet its owner signed it in to in the
//! Binance App (crate::agentic_wallet). Reads (addresses, balances, history, quotes, orders, the
//! App's limits) in chat and autopilot; swaps, limit orders and sends in autopilot only, each
//! checked against the owner's Orvyn limits before it reaches Binance, which then applies the
//! limits set in the Binance App.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::data::meta;
use crate::agentic_wallet::{self as baw, Limits, Write};
use crate::ai::{Tool, ToolSet};
use crate::bliss::settings::{boolean, list, number, SettingField, Settings};
use crate::bliss::{Ctx, Plugin, PluginMeta};
use crate::db::{log_agent, Agent, AgentLogKind};

pub struct AgenticWallet(PluginMeta);

pub fn agentic_wallet() -> Arc<dyn Plugin> {
    Arc::new(AgenticWallet(meta(
        baw::PLUGIN_ID,
        "Binance Agentic Wallet",
        "capability",
        "Swap, place limit orders and send from the owner's Binance Agentic Wallet (BSC, Ethereum, Base, Solana) within limits the owner sets.",
        "The owner signs the agent in to their Binance Agentic Wallet: Orvyn shows Binance's sign-in link and pairing code, and the owner confirms in the Binance App. The agent reads its addresses, balances, history and quotes, and in autopilot can swap, place and cancel limit orders, and (if the owner allows it) send to addresses in the owner's Binance address book. Orvyn checks every action against the owner's limits here (per order and per day in USD, which chains), and Binance applies the limits set in the Binance App (daily limit, which tokens, risky transactions). The agent never holds the wallet's keys, and the owner can sign it out at any time, here or in the Binance App.",
        &["trade", "read-market-data"],
    )))
}

fn s(v: &Value, k: &str) -> Option<String> {
    match &v[k] {
        Value::String(x) if !x.trim().is_empty() => Some(x.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn need(v: &Value, k: &str) -> std::result::Result<String, String> {
    s(v, k).ok_or_else(|| format!("{k} is required"))
}

fn reply(r: std::result::Result<Value, String>, what: &str) -> std::result::Result<Value, String> {
    Ok(Value::String(match r {
        Ok(v) => crate::http::clip(&v.to_string(), 12_000),
        Err(e) => format!("Binance Agentic Wallet {what} failed: {e}"),
    }))
}

fn schema(props: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": props, "required": required })
}

const CHAIN: &str = "binanceChainId: 56 (BSC), 1 (Ethereum), 8453 (Base), CT_501 (Solana), 42161 (Arbitrum), 137 (Polygon)";
const TOKEN: &str = "token contract address (native coin: 0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE; SOL: So11111111111111111111111111111111111111111)";

/// Arguments for `baw`: the flags given, by name.
fn flags(args: &Value, keys: &[(&str, &str)]) -> Vec<String> {
    let mut out = vec![];
    for (key, flag) in keys {
        if let Some(v) = s(args, key) {
            out.push(format!("--{flag}"));
            out.push(if *flag == "binanceChainId" { baw::chain_id(&v) } else { v });
        }
    }
    out
}

fn read_tool(agent: &Arc<Agent>, name: &str, description: &str, input: Value, command: &'static [&'static str], keys: &'static [(&'static str, &'static str)]) -> Tool {
    let agent = agent.clone();
    Tool::new(name, &format!("[Binance Agentic Wallet] {description}"), input, move |args| {
        let agent = agent.clone();
        async move {
            let mut argv: Vec<String> = command.iter().map(|c| c.to_string()).collect();
            argv.extend(flags(&args, keys));
            let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
            reply(baw::call(&agent.id, &argv).await, command.join(" ").as_str())
        }
    })
}

#[async_trait]
impl Plugin for AgenticWallet {
    fn meta(&self) -> &PluginMeta {
        &self.0
    }
    fn settings(&self) -> Vec<SettingField> {
        vec![
            number("maxOrderUsd", "Max per action", Some("Largest swap, limit order or send the agent may make, in USD."), 50.0, 1.0, 100_000.0, Some(1.0), Some("USD")),
            number("maxDailyUsd", "Max per day", Some("All the agent's swaps, limit orders and sends in any 24 hours, in USD. Your daily limit in the Binance App applies too."), 200.0, 1.0, 1_000_000.0, Some(1.0), Some("USD")),
            list("chains", "Chains", Some("Where the agent may act: 56 (BSC), 1 (Ethereum), 8453 (Base), CT_501 (Solana), 42161 (Arbitrum), 137 (Polygon)."), vec!["56"], 8, true),
            boolean("allowLimitOrders", "Allow limit orders", Some("Buy or sell when a price is reached."), true),
            boolean("allowSends", "Allow sending", Some("Send the chain's coin, USDT or USDC to addresses in your Binance address book."), false),
        ]
    }
    fn validate(&self, config: &Settings, _others: &[crate::bliss::Installed], _tier: crate::tiers::Tier) -> Option<String> {
        let chains = config.get("chains").and_then(Value::as_array).cloned().unwrap_or_default();
        let known = ["56", "1", "8453", "CT_501", "42161", "137"];
        let bad: Vec<String> = chains.iter().filter_map(Value::as_str).map(baw::chain_id).filter(|c| !known.contains(&c.as_str())).collect();
        if !bad.is_empty() {
            return Some(format!("unknown chains: {} (use 56, 1, 8453, CT_501, 42161, 137)", bad.join(", ")));
        }
        let (order, day) = (config.get("maxOrderUsd").and_then(Value::as_f64).unwrap_or(0.0), config.get("maxDailyUsd").and_then(Value::as_f64).unwrap_or(0.0));
        (order > day).then(|| "the per-action limit can't be above the daily limit".into())
    }
    async fn knowledge(&self, ctx: &Ctx) -> Option<String> {
        if ctx.state["connected"] != true {
            return Some("## Binance Agentic Wallet\nThe owner hasn't signed you in to their Binance Agentic Wallet yet (or the session ended); say so if asked to use it.".into());
        }
        let l = Limits::from_settings(&ctx.config);
        let addresses = ctx.state["addresses"].as_array().map(|a| a.iter().filter_map(|x| Some(format!("{} {}", x["chain"].as_str()?, x["address"].as_str()?))).collect::<Vec<_>>().join(", ")).unwrap_or_default();
        Some(format!(
            "## Binance Agentic Wallet\nYou're signed in to the owner's Binance Agentic Wallet{}; use the agentic_wallet_* tools. Tokens are given by contract address; check balances and get a quote before you swap. Orvyn refuses actions outside the owner's limits: at most ${} per action and ${} per 24 hours (USD), chains {}{}{}. The Binance App applies the owner's own limits too, and may reject an action or ask the owner to confirm it. A swap's orderId means submitted, not done; a send's txHash means broadcast, not confirmed. Conditional orders (\"when the price reaches X\") are limit orders, never market swaps.",
            if addresses.is_empty() { String::new() } else { format!(" ({addresses})") },
            l.max_order_usd,
            l.max_daily_usd,
            l.chains.join(", "),
            if l.allow_limit_orders { ", limit orders allowed" } else { ", no limit orders" },
            if l.allow_sends { ", sends allowed (only to addresses in the owner's Binance address book; the chain's coin, USDT or USDC)" } else { ", no sends" },
        ))
    }
    fn mandate(&self, ctx: &Ctx) -> Option<String> {
        let l = Limits::from_settings(&ctx.config);
        Some(format!("Binance Agentic Wallet: ≤ ${} per action, ≤ ${} per day, chains {}, sends {} (enforced in code).", l.max_order_usd, l.max_daily_usd, l.chains.join("/"), if l.allow_sends { "on" } else { "off" }))
    }
    fn read_tools(&self, ctx: &Ctx) -> ToolSet {
        if ctx.state["connected"] != true {
            return vec![];
        }
        let a = &ctx.agent;
        vec![
            read_tool(a, "agentic_wallet_addresses", "The wallet's addresses on each chain.", schema(json!({}), &[]), &["wallet", "address"], &[]),
            read_tool(
                a,
                "agentic_wallet_balance",
                "Token balances (optionally one symbol, token or chain).",
                schema(json!({ "symbol": { "type": "string" }, "tokenAddress": { "type": "string", "description": TOKEN }, "chainId": { "type": "string", "description": CHAIN } }), &[]),
                &["wallet", "balance"],
                &[("symbol", "symbol"), ("tokenAddress", "tokenAddress"), ("chainId", "binanceChainId")],
            ),
            read_tool(a, "agentic_wallet_chains", "The chains the wallet supports.", schema(json!({}), &[]), &["wallet", "chains"], &[]),
            read_tool(
                a,
                "agentic_wallet_history",
                "Transaction history (type all, pending or confirmed; or one transaction by hash).",
                schema(json!({ "type": { "type": "string", "enum": ["all", "pending", "confirmed"] }, "size": { "type": "integer", "maximum": 100 }, "tx": { "type": "string" }, "chainId": { "type": "string", "description": CHAIN } }), &[]),
                &["wallet", "tx-history"],
                &[("type", "type"), ("size", "size"), ("tx", "tx"), ("chainId", "binanceChainId")],
            ),
            read_tool(a, "agentic_wallet_limits", "The limits the owner set in the Binance App (daily limit and quota left, which tokens, risky-transaction handling, when the session ends).", schema(json!({}), &[]), &["wallet", "settings"], &[]),
            read_tool(
                a,
                "agentic_wallet_quote",
                "A swap quote: what fromTokenQty of fromToken would get in toToken.",
                schema(json!({ "fromTokenQty": { "type": "string" }, "fromToken": { "type": "string", "description": TOKEN }, "toToken": { "type": "string", "description": TOKEN }, "chainId": { "type": "string", "description": CHAIN }, "slippage": { "type": "string", "description": "auto (default) or a percent, e.g. 1" } }), &["fromTokenQty", "fromToken", "toToken", "chainId"]),
                &["market-order", "quote"],
                &[("fromTokenQty", "fromTokenQty"), ("fromToken", "fromToken"), ("toToken", "toToken"), ("chainId", "binanceChainId"), ("slippage", "slippage")],
            ),
            read_tool(
                a,
                "agentic_wallet_orders",
                "Swaps (market orders) and their status: PENDING, FINISHED or FAILED.",
                schema(json!({ "orderId": { "type": "string" }, "status": { "type": "string", "enum": ["PENDING", "FINISHED", "FAILED"] }, "chainId": { "type": "string", "description": CHAIN } }), &[]),
                &["market-order", "list"],
                &[("orderId", "orderId"), ("status", "status"), ("chainId", "binanceChainId")],
            ),
            read_tool(
                a,
                "agentic_wallet_limit_orders",
                "Limit orders and their status (WORKING is waiting for its price).",
                schema(json!({ "strategyId": { "type": "string" }, "status": { "type": "string", "enum": ["WORKING", "TRIGGERED", "PENDING", "FINISHED", "FAILED", "EXPIRED", "CANCELED"] }, "chainId": { "type": "string", "description": CHAIN } }), &[]),
                &["limit-order", "list"],
                &[("strategyId", "strategyId"), ("status", "status"), ("chainId", "binanceChainId")],
            ),
        ]
    }
    fn has_actions(&self) -> bool {
        true
    }
    fn action_tools(&self, ctx: &Ctx) -> ToolSet {
        if ctx.state["connected"] != true {
            return vec![];
        }
        let limits = Limits::from_settings(&ctx.config);
        let mut tools = vec![
            action(ctx, &limits, "agentic_wallet_swap", "Swap fromTokenQty of fromToken for toToken now (a market order). Get a quote first.", schema(json!({ "fromTokenQty": { "type": "string" }, "fromToken": { "type": "string", "description": TOKEN }, "toToken": { "type": "string", "description": TOKEN }, "chainId": { "type": "string", "description": CHAIN }, "slippage": { "type": "string", "description": "auto (default) or a percent, e.g. 1" } }), &["fromTokenQty", "fromToken", "toToken", "chainId"]), Kind::Swap),
            action(ctx, &limits, "agentic_wallet_cancel_limit_order", "Cancel a working limit order.", schema(json!({ "strategyId": { "type": "string" } }), &["strategyId"]), Kind::Cancel),
        ];
        if limits.allow_limit_orders {
            let props = json!({ "triggerPrice": { "type": "string", "description": "the price in USD that triggers it" }, "fromTokenQty": { "type": "string" }, "fromToken": { "type": "string", "description": TOKEN }, "toToken": { "type": "string", "description": TOKEN }, "chainId": { "type": "string", "description": CHAIN }, "slippage": { "type": "string" } });
            tools.push(action(ctx, &limits, "agentic_wallet_limit_buy", "Buy toToken when its price falls to triggerPrice (USD). fromToken must be USDT, USDC or the chain's coin.", schema(props.clone(), &["triggerPrice", "fromTokenQty", "fromToken", "toToken", "chainId"]), Kind::LimitBuy));
            tools.push(action(ctx, &limits, "agentic_wallet_limit_sell", "Sell fromToken when its price rises to triggerPrice (USD). toToken must be USDT, USDC or the chain's coin.", schema(props, &["triggerPrice", "fromTokenQty", "fromToken", "toToken", "chainId"]), Kind::LimitSell));
        }
        if limits.allow_sends {
            tools.push(action(
                ctx,
                &limits,
                "agentic_wallet_send",
                "Send the chain's coin, USDT or USDC to an address in the owner's Binance address book (others are refused by Binance).",
                schema(json!({ "amount": { "type": "string" }, "recipient": { "type": "string" }, "tokenAddress": { "type": "string", "description": TOKEN }, "chainId": { "type": "string", "description": CHAIN } }), &["amount", "recipient", "tokenAddress", "chainId"]),
                Kind::Send,
            ));
        }
        tools
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Swap,
    LimitBuy,
    LimitSell,
    Cancel,
    Send,
}

fn action(ctx: &Ctx, limits: &Limits, name: &str, description: &str, input: Value, kind: Kind) -> Tool {
    let (agent, limits) = (ctx.agent.clone(), limits.clone());
    let description = format!("[Binance Agentic Wallet, acts on the wallet; checked against the owner's limits] {description}");
    Tool::new(name, &description, input, move |args| {
        let (agent, limits) = (agent.clone(), limits.clone());
        async move {
            let chain = s(&args, "chainId").map(|c| baw::chain_id(&c)).unwrap_or_default();
            let (write, argv): (Write, Vec<String>) = match kind {
                Kind::Swap | Kind::LimitBuy | Kind::LimitSell => {
                    let (from, to, qty) = (need(&args, "fromToken")?, need(&args, "toToken")?, need(&args, "fromTokenQty")?);
                    if chain.is_empty() {
                        return Err("chainId is required".into());
                    }
                    let mut argv: Vec<String> = match kind {
                        Kind::Swap => vec!["market-order".into(), "swap".into()],
                        Kind::LimitBuy => vec!["limit-order".into(), "buy".into(), "--triggerPrice".into(), need(&args, "triggerPrice")?],
                        _ => vec!["limit-order".into(), "sell".into(), "--triggerPrice".into(), need(&args, "triggerPrice")?],
                    };
                    argv.extend(["--fromTokenQty".into(), qty.clone(), "--fromToken".into(), from.clone(), "--toToken".into(), to.clone(), "--binanceChainId".into(), chain.clone()]);
                    if let Some(sl) = s(&args, "slippage") {
                        argv.extend(["--slippage".into(), sl]);
                    }
                    let w = if kind == Kind::Swap { Write::Swap { chain: chain.clone(), from, to, qty } } else { Write::Limit { chain: chain.clone(), from, to, qty } };
                    (w, argv)
                }
                Kind::Send => {
                    let (amount, recipient, token) = (need(&args, "amount")?, need(&args, "recipient")?, need(&args, "tokenAddress")?);
                    if chain.is_empty() {
                        return Err("chainId is required".into());
                    }
                    let argv = vec!["wallet".into(), "send".into(), "--amount".into(), amount.clone(), "--recipient".into(), recipient, "--binanceChainId".into(), chain.clone(), "--tokenAddress".into(), token.clone()];
                    (Write::Send { chain: chain.clone(), token, amount }, argv)
                }
                Kind::Cancel => (Write::Cancel, vec!["limit-order".into(), "cancel".into(), "--strategyId".into(), need(&args, "strategyId")?]),
            };
            let what = argv[..2].join(" ");
            let usd = match baw::check_write(&agent.id, &limits, &write).await {
                Ok(u) => u,
                Err(why) => {
                    let _ = log_agent(&agent.id, AgentLogKind::Trade, &format!("Binance Agentic Wallet {what} refused: {why}")).await;
                    return Ok(Value::String(json!({ "ok": false, "refused": why }).to_string()));
                }
            };
            let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
            let result = baw::call(&agent.id, &refs).await;
            let Ok(data) = result else { return reply(result, &what) };
            if let Some(u) = usd {
                baw::record_write(&agent.id, &what, u).await.map_err(|e| e.message)?;
            }
            let _ = log_agent(&agent.id, AgentLogKind::Trade, &crate::http::clip(&format!("Binance Agentic Wallet {what}{}: {}", usd.map(|u| format!(" (~${u:.2})")).unwrap_or_default(), data), 300)).await;
            // A swap is only submitted: wait for Binance to finish it.
            if kind == Kind::Swap {
                if let Some(order) = data["orderId"].as_str().map(String::from).or_else(|| data["orderId"].as_i64().map(|n| n.to_string())) {
                    return reply(Ok(baw::await_swap(&agent.id, &order).await), &what);
                }
            }
            reply(Ok(data), &what)
        }
    })
}
