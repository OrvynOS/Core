//! OrvynPay: the agent pays other agents in BNB, USDT or USDC, addressed by their Binance Agent ID
//! (`name.ai`), Orvyn handle or wallet. Premium (the agent must hold an Agent ID itself; so must
//! the payee). Standard payments are public, with a reference; private payments come out of the
//! agent's shielded balance in the OrvynPay pool. Limits: per payment and per day here, and the
//! wallet policy's daily payment caps.

use std::sync::Arc;

use alloy::primitives::{utils::format_units, Address, U256};
use async_trait::async_trait;
use serde_json::{json, Value};

use super::data::meta;
use crate::ai::{schema, Tool, ToolSet};
use crate::billing::Call;
use crate::bliss::settings::{boolean, number, SettingField};
use crate::bliss::{json_text, Ctx, Plugin, PluginMeta};
use crate::db::{db, log_agent, AgentLogKind};
use crate::env::env;
use crate::error::{Error, Result};
use crate::evm;
use crate::orvynpay::{self as op, PrivateKind, ShieldedKeys, PoolView};

fn amount_of(symbol: &str, amount: f64) -> Result<U256> {
    let (_, decimals) = op::token(symbol)?;
    alloy::primitives::utils::parse_units(&format!("{:.*}", decimals.min(8) as usize, amount), decimals).map(|u| u.get_absolute()).map_err(|_| Error::invalid("bad amount"))
}

fn check_limits(ctx: &Ctx, symbol: &str, amount: f64) -> Result<()> {
    if !crate::agentid::is_premium(&ctx.agent) {
        return Err(Error::forbidden("OrvynPay needs the agent to hold a verified Binance Agent ID (premium)"));
    }
    let (max_usd, max_bnb) = (ctx.num("maxPaymentUsd"), ctx.num("maxPaymentBnb"));
    if (symbol == "BNB" && amount > max_bnb) || (symbol != "BNB" && amount > max_usd) {
        let cap = if symbol == "BNB" { format!("{} BNB", crate::auth::js_number(max_bnb)) } else { format!("${}", crate::auth::js_number(max_usd)) };
        return Err(Error::forbidden(format!("at most {cap} per payment")));
    }
    Ok(())
}

async fn payments_today(agent_id: &str) -> Result<i64> {
    Ok(sqlx::query_scalar("select count(*) from agent_logs where agent_id = $1 and kind = 'payment' and created_at >= now() - interval '1 day'").bind(agent_id).fetch_one(db()).await?)
}

fn pool() -> Result<Address> {
    env().orvyn_shielded_pool.as_deref().and_then(|p| p.parse().ok()).ok_or_else(|| Error::unavailable("OrvynPay private mode isn't deployed on this instance (ORVYN_SHIELDED_POOL)"))
}

async fn send(ctx: &Ctx, calls: Vec<Call>) -> Result<Vec<String>> {
    let mut txs = vec![];
    for c in calls {
        let data = hex::decode(c.data.trim_start_matches("0x")).map_err(|_| Error::internal("bad calldata"))?;
        let h = crate::wallets::agent_send(&ctx.agent.id, &ctx.agent.evm_wallet, evm::parse_address(&c.to)?, data.into(), c.value.parse().unwrap_or_default()).await?;
        txs.push(format!("{h:#x}"));
    }
    Ok(txs)
}

fn self_address(ctx: &Ctx) -> Result<Address> {
    evm::parse_address(&ctx.agent.evm_address)
}

/// The agent's shielded keys, registered in the pool on first use.
async fn registered_keys(ctx: &Ctx) -> Result<(ShieldedKeys, PoolView)> {
    let keys = op::shielded_keys_of(&ctx.agent.id).await?.keys;
    let pool = pool()?;
    let view = op::pool_view(pool).await?;
    if !view.keys_by_account.contains_key(&format!("{:#x}", self_address(ctx)?)) {
        let txs = send(ctx, vec![op::register_key_call(pool, &keys)]).await?;
        op::mark_registered(&ctx.agent.id, &txs[0]).await?;
        return Ok((keys, op::pool_view(pool).await?));
    }
    Ok((keys, view))
}

async fn private_send(ctx: &Ctx, kind: PrivateKind, symbol: &str, amount: U256, to: Option<Address>) -> Result<Vec<String>> {
    let (keys, view) = registered_keys(ctx).await?;
    let tx = op::private_transaction(kind, &keys, &view, symbol, amount, self_address(ctx)?, to).await?;
    send(ctx, op::private_transaction_calls(pool()?, &tx, symbol)?).await
}

fn token_of(v: &Value) -> std::result::Result<String, String> {
    match v["token"].as_str() {
        Some(t @ ("USDT" | "USDC" | "BNB")) => Ok(t.to_string()),
        _ => Err("token must be USDT, USDC or BNB".into()),
    }
}

pub struct OrvynPayPlugin(PluginMeta);

pub fn orvynpay() -> Arc<dyn Plugin> {
    Arc::new(OrvynPayPlugin(meta(
        "orvynpay",
        "OrvynPay",
        "capability",
        "Pay other agents by their Agent ID (name.ai) in USDT, USDC or BNB, publicly or privately.",
        "The agent pays other agents that hold a Binance Agent ID, addressed by name.ai, their Orvyn handle or wallet. Standard payments are public and carry a reference; private payments come from the agent's shielded balance, so the amount and both parties stay hidden on chain. Premium: the agent needs its own Agent ID. Payments stay within your per-payment and daily limits, and Orvyn's wallet policy caps them again.",
        &["pay"],
    )))
}

#[async_trait]
impl Plugin for OrvynPayPlugin {
    fn meta(&self) -> &PluginMeta {
        &self.0
    }
    fn settings(&self) -> Vec<SettingField> {
        vec![
            number("maxPaymentUsd", "Max per payment (USDT/USDC)", None, 50.0, 1.0, 5000.0, Some(1.0), Some("USD")),
            number("maxPaymentBnb", "Max per payment (BNB)", None, 0.1, 0.001, 10.0, Some(0.001), Some("BNB")),
            number("maxPaymentsPerDay", "Max payments per day", None, 20.0, 1.0, 200.0, Some(1.0), None),
            boolean("allowPrivate", "Allow private payments", None, true),
        ]
    }
    fn mandate(&self, ctx: &Ctx) -> Option<String> {
        let n = |k| crate::auth::js_number(ctx.num(k));
        Some(format!(
            "OrvynPay: at most ${} (or {} BNB) per payment, {} payments a day, only to agents holding an Agent ID. Pay only for something you asked for and received, or agreed to buy.",
            n("maxPaymentUsd"),
            n("maxPaymentBnb"),
            n("maxPaymentsPerDay")
        ))
    }
    fn read_tools(&self, ctx: &Ctx) -> ToolSet {
        let c = ctx.clone();
        vec![
            Tool::new(
                "resolve_agent_payee",
                "Look up who a payment would go to: name.ai, an Orvyn handle or a 0x address. Says whether they can receive OrvynPay (and privately).",
                schema(&[("to", json!({ "type": "string" }), true)]),
                |v| async move {
                    let to = v["to"].as_str().unwrap_or("").to_string();
                    match op::resolve_payee(&to).await {
                        Ok(p) => {
                            let private = match pool() {
                                Ok(pool) => op::pool_view(pool).await.ok().is_some_and(|view| view.keys_by_account.contains_key(&format!("{:#x}", p.address))),
                                Err(_) => false,
                            };
                            json_text(json!({ "address": p.address.to_string(), "label": p.label, "canReceivePrivately": private }))
                        }
                        Err(e) => Ok(Value::String(format!("Can't pay {to}: {}", e.message))),
                    }
                },
            ),
            Tool::new("shielded_balance", "The agent's private (shielded) OrvynPay balance per token.", schema(&[]), move |_| {
                let ctx = c.clone();
                async move {
                    let r: Result<Value> = async {
                        let keys = op::shielded_keys_of(&ctx.agent.id).await?.keys;
                        let notes = op::unspent_notes(&op::pool_view(pool()?).await?, &keys);
                        let mut out = serde_json::Map::new();
                        for (symbol, _, decimals) in op::TOKENS {
                            out.insert(symbol.to_string(), json!(format_units(op::shielded_balance(&notes, op::asset_of(symbol)?), *decimals).unwrap_or_default()));
                        }
                        Ok(Value::Object(out))
                    }
                    .await;
                    match r {
                        Ok(v) => json_text(v),
                        Err(e) => Ok(Value::String(format!("Couldn't read the shielded balance: {}", e.message))),
                    }
                }
            }),
        ]
    }
    fn has_actions(&self) -> bool {
        true
    }
    fn action_tools(&self, ctx: &Ctx) -> ToolSet {
        let (c1, c2, c3) = (ctx.clone(), ctx.clone(), ctx.clone());
        let token = json!({ "type": "string", "enum": ["USDT", "USDC", "BNB"] });
        let positive = json!({ "type": "number", "exclusiveMinimum": 0 });
        vec![
            Tool::new(
                "pay_agent",
                "Pay another agent (name.ai, Orvyn handle or 0x address) in USDT, USDC or BNB. private=true pays from the shielded balance and hides the amount and parties.",
                schema(&[
                    ("to", json!({ "type": "string" }), true),
                    ("amount", positive.clone(), true),
                    ("token", token.clone(), true),
                    ("private", json!({ "type": "boolean" }), false),
                    ("reference", json!({ "type": "string", "maxLength": 32, "description": "Invoice, job or request id (public payments)" }), false),
                    ("reason", json!({ "type": "string", "minLength": 5, "maxLength": 300 }), true),
                ]),
                move |v| {
                    let ctx = c1.clone();
                    async move {
                        let symbol = token_of(&v)?;
                        let amount = v["amount"].as_f64().unwrap_or(0.0);
                        let private = v["private"].as_bool().unwrap_or(false);
                        let r: Result<Value> = async {
                            check_limits(&ctx, &symbol, amount)?;
                            let max = ctx.num("maxPaymentsPerDay");
                            if payments_today(&ctx.agent.id).await? as f64 >= max {
                                return Err(Error::forbidden(format!("the daily limit of {} payments is reached", crate::auth::js_number(max))));
                            }
                            let payee = op::resolve_payee(v["to"].as_str().unwrap_or("")).await?;
                            if payee.address == self_address(&ctx)? {
                                return Err(Error::invalid("an agent can't pay itself"));
                            }
                            let units = amount_of(&symbol, amount)?;
                            let txs = if private {
                                if !ctx.flag("allowPrivate") {
                                    return Err(Error::forbidden("the creator doesn't allow private payments"));
                                }
                                private_send(&ctx, PrivateKind::Transfer, &symbol, units, Some(payee.address)).await?
                            } else {
                                send(&ctx, op::standard_payment_calls(payee.address, &symbol, units, v["reference"].as_str().unwrap_or(""))?).await?
                            };
                            let msg = format!("Paid {}{} {symbol} to {}: {}", if private { "privately " } else { "" }, crate::auth::js_number(amount), payee.label, v["reason"].as_str().unwrap_or(""));
                            log_agent(&ctx.agent.id, AgentLogKind::Payment, &msg).await?;
                            Ok(json!({ "ok": true, "to": { "address": payee.address.to_string(), "label": payee.label }, "txs": txs }))
                        }
                        .await;
                        match r {
                            Ok(x) => json_text(x),
                            Err(e) => Ok(Value::String(format!("Payment refused or failed: {}", e.message))),
                        }
                    }
                },
            ),
            Tool::new(
                "shield_funds",
                "Move USDT, USDC or BNB from the agent's wallet into its shielded balance, to pay privately later.",
                schema(&[("amount", positive.clone(), true), ("token", token.clone(), true)]),
                move |v| {
                    let ctx = c2.clone();
                    async move {
                        let symbol = token_of(&v)?;
                        let amount = v["amount"].as_f64().unwrap_or(0.0);
                        let r: Result<Vec<String>> = async {
                            if !ctx.flag("allowPrivate") {
                                return Err(Error::forbidden("the creator doesn't allow private payments"));
                            }
                            check_limits(&ctx, &symbol, amount)?;
                            let txs = private_send(&ctx, PrivateKind::Deposit, &symbol, amount_of(&symbol, amount)?, None).await?;
                            log_agent(&ctx.agent.id, AgentLogKind::Payment, &format!("Shielded {} {symbol}", crate::auth::js_number(amount))).await?;
                            Ok(txs)
                        }
                        .await;
                        match r {
                            Ok(txs) => json_text(json!({ "ok": true, "txs": txs })),
                            Err(e) => Ok(Value::String(format!("Shielding failed: {}", e.message))),
                        }
                    }
                },
            ),
            Tool::new(
                "unshield_funds",
                "Move funds from the agent's shielded balance back to its own wallet.",
                schema(&[("amount", positive, true), ("token", token, true)]),
                move |v| {
                    let ctx = c3.clone();
                    async move {
                        let symbol = token_of(&v)?;
                        let amount = v["amount"].as_f64().unwrap_or(0.0);
                        let r: Result<Vec<String>> = async {
                            let txs = private_send(&ctx, PrivateKind::Withdraw, &symbol, amount_of(&symbol, amount)?, None).await?;
                            log_agent(&ctx.agent.id, AgentLogKind::Payment, &format!("Unshielded {} {symbol} to the agent's wallet", crate::auth::js_number(amount))).await?;
                            Ok(txs)
                        }
                        .await;
                        match r {
                            Ok(txs) => json_text(json!({ "ok": true, "txs": txs })),
                            Err(e) => Ok(Value::String(format!("Unshielding failed: {}", e.message))),
                        }
                    }
                },
            ),
        ]
    }
}
