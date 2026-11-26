//! Invoices and receipts for hosting subscriptions, mirrored from the registry's payment log (one
//! row per payment), with the transaction hash attached when we see it. Every stored invoice is also
//! its receipt; the upcoming invoice for the next period is built on demand.

use alloy::primitives::{B256, U256};
use alloy::sol_types::SolEvent;
use serde_json::{json, Value};

use crate::billing::{billing_enabled, iso, month_price, registry_agent, registry_config, registry_contract, OrvynRegistry, NATIVE_TOKEN};
use crate::db::{db, Agent, Invoice};
use crate::env::env;
use crate::error::Result;
use crate::evm::{self, rpc_error, BSC};
use crate::tiers::Tier;

const MONTH_MS: i64 = 30 * 86_400_000;

/// The registry's "token" for Binance Pay payments (OrvynRegistry.BINANCE_PAY).
pub const BINANCE_PAY_TOKEN: &str = "0x0000000000000000000000000000000000b1a9ce";

fn symbol_of(token: &str) -> String {
    match token.to_lowercase().as_str() {
        t if t == BSC.usdt || t == BINANCE_PAY_TOKEN => "USDT".into(),
        t if t == BSC.usdc => "USDC".into(),
        NATIVE_TOKEN => "BNB".into(),
        _ => token.to_string(),
    }
}

/// Pull any payments the registry has that we don't yet. Returns how many were added.
pub async fn sync_invoices(agent_id: &str) -> Result<u64> {
    if !billing_enabled() {
        return Ok(0);
    }
    let Some(record) = registry_agent(agent_id).await? else { return Ok(0) };
    let have: i64 = sqlx::query_scalar("select count(*) from invoices where agent_id = $1").bind(agent_id).fetch_one(db()).await?;
    let mut from = have as u64;
    let mut added = 0;
    let reg = OrvynRegistry::new(registry_contract()?, evm::bsc());
    while from < record.payments as u64 {
        let page = reg.getPayments(agent_id.into(), U256::from(from), U256::from(100u8)).call().await.map_err(rpc_error)?;
        if page.is_empty() {
            break;
        }
        for (i, p) in page.iter().enumerate() {
            let token = format!("{:#x}", p.token);
            let at = |s: u64| chrono::DateTime::from_timestamp(s as i64, 0).unwrap_or_default();
            let r = sqlx::query(
                "insert into invoices (agent_id, payment_index, payer, token, symbol, amount, decimals, months, period_start, period_end, paid_at, block_number)
                 values ($1, $2, $3, $4, $5, $6, 18, $7, $8, $9, $10, $11) on conflict do nothing",
            )
            .bind(agent_id)
            .bind((from + i as u64) as i32)
            .bind(format!("{:#x}", p.payer))
            .bind(&token)
            .bind(symbol_of(&token))
            .bind(p.amount.to_string())
            .bind(p.months as i32)
            .bind(at(p.periodStart))
            .bind(at(p.paidUntil))
            .bind(at(p.paidAt))
            .bind(p.blockNumber.to_string())
            .execute(db())
            .await?;
            added += r.rows_affected();
        }
        from += page.len() as u64;
    }
    Ok(added)
}

/// Attach a payment transaction's hash to the invoices it created. Only `SubscriptionPaid` events
/// logged by the registry count, so a forged transaction can't create or alter an invoice.
pub async fn attach_payment_tx(agent_id: &str, tx: B256) -> Result<u64> {
    sync_invoices(agent_id).await?;
    let Some(receipt) = crate::wallets::receipt_of(tx).await? else { return Ok(0) };
    let registry = registry_contract()?;
    let mut attached = 0;
    for log in receipt.inner.logs() {
        if log.address() != registry {
            continue;
        }
        let Ok(event) = OrvynRegistry::SubscriptionPaid::decode_log(&log.inner) else { continue };
        if event.agentId != agent_id {
            continue;
        }
        let index: i32 = event.paymentIndex.try_into().unwrap_or(-1);
        let r = sqlx::query("update invoices set tx_hash = $3 where agent_id = $1 and payment_index = $2 and tx_hash is null")
            .bind(agent_id)
            .bind(index)
            .bind(format!("{tx:#x}"))
            .execute(db())
            .await?;
        attached += r.rows_affected();
    }
    Ok(attached)
}

pub async fn attach_payment_tx_hash(agent_id: &str, tx: &str) -> Result<u64> {
    attach_payment_tx(agent_id, tx.parse().map_err(|_| crate::Error::invalid("not a transaction hash"))?).await
}

pub async fn list_invoices(agent_id: &str) -> Result<Vec<Invoice>> {
    Ok(sqlx::query_as("select * from invoices where agent_id = $1 order by payment_index").bind(agent_id).fetch_all(db()).await?)
}

/// Human invoice number, stable per row: ORV-<year paid>-<6-digit id>.
pub fn invoice_number(inv: &Invoice) -> String {
    use chrono::Datelike;
    format!("ORV-{}-{:06}", inv.paid_at.year(), inv.id)
}

pub async fn invoice_by_number(agent_id: &str, number: &str) -> Result<Option<Invoice>> {
    let parts: Vec<&str> = number.split('-').collect();
    let ok = parts.len() == 3 && parts[0] == "ORV" && parts[1].len() == 4 && parts[1].chars().all(|c| c.is_ascii_digit()) && parts[2].len() >= 6 && parts[2].chars().all(|c| c.is_ascii_digit());
    if !ok {
        return Ok(None);
    }
    let Ok(id) = parts[2].parse::<i32>() else { return Ok(None) };
    let row: Option<Invoice> = sqlx::query_as("select * from invoices where agent_id = $1 and id = $2").bind(agent_id).bind(id).fetch_optional(db()).await?;
    Ok(row.filter(|r| invoice_number(r) == number))
}

pub fn seller() -> Value {
    let e = env();
    json!({ "name": e.orvyn_billing_entity, "address": e.orvyn_billing_address, "email": e.orvyn_billing_email, "taxId": e.orvyn_billing_tax_id })
}

/// Base units → "12.34" (cents, rounded down).
pub fn money(amount: U256, decimals: u8) -> String {
    let cents = amount * U256::from(100u8) / U256::from(10u64).pow(U256::from(decimals));
    format!("{}.{:02}", cents / U256::from(100u8), (cents % U256::from(100u8)).to::<u64>())
}

/// Everything a paid invoice / receipt shows.
pub fn receipt(inv: &Invoice, agent: &Agent) -> Result<Value> {
    let amount = U256::from_str_radix(&inv.amount, 10).unwrap_or_default();
    let method = if inv.token.to_lowercase() == BINANCE_PAY_TOKEN {
        "Binance Pay (USDT), credited in the Orvyn registry".to_string()
    } else {
        format!("{} on BNB Chain to {}", inv.symbol, registry_contract()?)
    };
    Ok(json!({
        "kind": "receipt",
        "number": invoice_number(inv),
        "status": "paid",
        "issuedAt": iso(inv.paid_at),
        "seller": seller(),
        "billTo": { "account": agent.owner_account_id, "details": if agent.billing_details.is_empty() { Value::Null } else { agent.billing_details.clone().into() } },
        "agent": { "id": agent.id, "name": agent.name, "symbol": agent.symbol },
        "line": {
            "description": format!("Orvyn agent hosting: {} (${})", agent.name, agent.symbol),
            "months": inv.months,
            "periodStart": iso(inv.period_start),
            "periodEnd": iso(inv.period_end),
            "unitPrice": money(amount / U256::from(inv.months.max(1) as u64), inv.decimals as u8),
        },
        "total": money(amount, inv.decimals as u8),
        "currency": inv.symbol,
        "payment": { "method": method, "payer": inv.payer, "paidAt": iso(inv.paid_at), "txHash": inv.tx_hash, "blockNumber": inv.block_number, "paymentIndex": inv.payment_index },
    }))
}

/// The next period's invoice, due when the current one ends. Built on demand, not stored.
pub async fn upcoming_invoice(agent: &Agent, months: u32) -> Result<Value> {
    let config = registry_config().await?;
    let record = registry_agent(&agent.id).await?;
    let tier = Tier::from_index(record.as_ref().map(|r| r.tier as u64).unwrap_or(0));
    let mut price = month_price(&config, tier, BSC.usdt);
    if price == U256::ZERO {
        price = month_price(&config, tier, BSC.usdc);
    }
    // The registry is the source of truth; the mirrored paidUntil can lag a fresh payment.
    let paid_until = record.map(|r| r.paid_until as i64 * 1000).unwrap_or_else(|| agent.paid_until.map(|t| t.timestamp_millis()).unwrap_or(0));
    let now = chrono::Utc::now();
    let start = chrono::DateTime::from_timestamp_millis(paid_until.max(now.timestamp_millis())).unwrap();
    let total = price * U256::from(months);
    Ok(json!({
        "kind": "invoice",
        "number": format!("ORV-DUE-{}", agent.id),
        "status": "due",
        "issuedAt": iso(now),
        "dueAt": iso(start),
        "seller": seller(),
        "billTo": { "account": agent.owner_account_id, "details": if agent.billing_details.is_empty() { Value::Null } else { agent.billing_details.clone().into() } },
        "agent": { "id": agent.id, "name": agent.name, "symbol": agent.symbol },
        "line": {
            "description": format!("Orvyn agent hosting ({}): {} (${})", tier.spec().name, agent.name, agent.symbol),
            "months": months,
            "periodStart": iso(start),
            "periodEnd": iso(chrono::DateTime::from_timestamp_millis(start.timestamp_millis() + months as i64 * MONTH_MS).unwrap()),
            "unitPrice": money(price, config.decimals),
        },
        "total": money(total, config.decimals),
        "currency": "USDT, USDC or BNB",
        "payTo": {
            "contract": config.contract,
            "how": format!("OrvynRegistry.pay(\"{}\", token, amount) on BNB Chain: approve the registry for {total} USDT or USDC first, or send BNB with token 0x0", agent.id),
        },
    }))
}
