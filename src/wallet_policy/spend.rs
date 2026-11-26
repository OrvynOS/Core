//! Daily caps on what an agent spends without its owner's signature (fees, hosting, launches,
//! trades, x402). A signature's spends are counted all at once, and only if every asset stays within
//! its cap; otherwise nothing is counted and nothing is signed.

use super::config::daily_cap;
use super::{refuse, Spends};
use crate::db::db;
use crate::error::Result;

pub async fn reserve_spends(agent_id: &str, spends: &Spends) -> Result<()> {
    let capped: Vec<_> = spends.iter().filter_map(|(asset, amount)| daily_cap(asset).map(|cap| (asset, *amount, cap))).collect();
    if capped.is_empty() {
        return Ok(());
    }
    let day = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let mut tx = db().begin().await?;
    for (asset, amount, cap) in capped {
        if amount > cap {
            return Err(refuse(format!("over the daily cap for {asset}")));
        }
        // Add to today's total only if it stays within the cap (in one statement).
        let row: Option<String> = sqlx::query_scalar(
            "insert into wallet_spend (agent_id, day, asset, amount) values ($1, $2, $3, $4::numeric)
             on conflict (agent_id, day, asset) do update set amount = wallet_spend.amount + $4::numeric
             where wallet_spend.amount + $4::numeric <= $5::numeric
             returning amount::text",
        )
        .bind(agent_id)
        .bind(&day)
        .bind(asset)
        .bind(amount.to_string())
        .bind(cap.to_string())
        .fetch_optional(&mut *tx)
        .await?;
        if row.is_none() {
            return Err(refuse(format!("over the daily cap for {asset}")));
        }
    }
    tx.commit().await?;
    Ok(())
}
