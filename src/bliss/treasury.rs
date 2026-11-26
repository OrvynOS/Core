//! Splits the agent's BNB between plugins. When income arrives, each plugin with a fee share gets
//! that percentage added to its budget (`state.pendingWei`); spending plugins draw from their own
//! budget, and whatever isn't earmarked stays free. Budgets change with atomic SQL so concurrent
//! jobs can't double-spend.

use alloy::primitives::{I256, U256};
use serde_json::Value;

use super::Installed;
use crate::db::{db, Agent};
use crate::error::Result;
use crate::evm;

pub fn pending_of(state: &Value) -> U256 {
    state["pendingWei"].as_str().and_then(|s| U256::from_str_radix(s, 10).ok()).unwrap_or_default()
}

/// Add (or, negative, take) wei to a plugin's budget, never below zero.
pub async fn adjust_budget(agent_id: &str, plugin_id: &str, delta_wei: I256) -> Result<()> {
    sqlx::query(
        "update agent_plugins set state = jsonb_set(state, '{pendingWei}', to_jsonb(greatest(0, coalesce((state->>'pendingWei')::numeric, 0) + $3::numeric)::text))
         where agent_id = $1 and plugin_id = $2",
    )
    .bind(agent_id)
    .bind(plugin_id)
    .bind(delta_wei.to_string())
    .execute(db())
    .await?;
    Ok(())
}

/// Route new income into plugin budgets.
pub async fn allocate_fee_income(agent_id: &str, installed: &[Installed], income_wei: U256) -> Result<()> {
    for p in installed {
        let pct = p.plugin.fee_share(&p.config).unwrap_or(0.0);
        if pct > 0.0 {
            let share = income_wei * U256::from((pct * 100.0).round() as u64) / U256::from(10_000u64);
            adjust_budget(agent_id, &p.plugin.meta().id, I256::from_raw(share)).await?;
        }
    }
    Ok(())
}

/// BNB above the gas reserve.
async fn usable(agent: &Agent) -> Result<U256> {
    let balance = evm::bnb_balance(&agent.evm_address).await?;
    Ok(balance.saturating_sub(evm::to_wei(agent.gas_reserve_bnb)))
}

/// BNB not reserved for gas and not earmarked for any plugin's budget.
pub async fn free_bnb(agent: &Agent, installed: &[Installed], exclude: Option<&str>) -> Result<U256> {
    let earmarked: U256 = installed.iter().filter(|p| Some(p.plugin.meta().id.as_str()) != exclude).map(|p| pending_of(&p.state)).fold(U256::ZERO, |a, b| a + b);
    Ok(usable(agent).await?.saturating_sub(earmarked))
}

/// What a plugin may spend now: its budget, capped by what the wallet holds.
pub async fn spendable(agent: &Agent, installed: &[Installed], plugin_id: &str) -> Result<U256> {
    let budget = installed.iter().find(|p| p.plugin.meta().id == plugin_id).map(|p| pending_of(&p.state)).unwrap_or_default();
    Ok(budget.min(usable(agent).await?))
}

/// After BNB leaves the wallet outside a plugin (the creator withdrew it), shrink budgets
/// proportionally so together they never claim more than the wallet has free.
pub async fn fit_budgets(agent: &Agent, installed: &[Installed]) -> Result<()> {
    let cap = usable(agent).await?;
    let total: U256 = installed.iter().map(|p| pending_of(&p.state)).fold(U256::ZERO, |a, b| a + b);
    if total <= cap {
        return Ok(());
    }
    for p in installed {
        let pending = pending_of(&p.state);
        if pending > U256::ZERO {
            let target = pending * cap / total;
            adjust_budget(&agent.id, &p.plugin.meta().id, I256::from_raw(target) - I256::from_raw(pending)).await?;
        }
    }
    Ok(())
}
