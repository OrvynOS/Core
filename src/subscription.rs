//! The orchestrator's billing pass for one agent: register it in the registry, mirror its
//! paid-until, run autopay, provision its host once live, and suspend or resume that host to match.

use crate::billing::{attest_environment, autopay, billing_enabled, iso, register_agent, settle_treasury_payment, sync_subscription, PayOutcome};
use crate::db::Agent;
use crate::error::Result;
use crate::hosting::{apply_plan, provision_host, resume_host, suspend_host};

pub async fn run_billing(input: Agent) -> Result<Agent> {
    let mut agent = input;
    // Held by Orvyn: keep it suspended; no on-chain actions, no autopay.
    if let Some(reason) = agent.held_reason.clone() {
        return suspend_host(agent, &format!("paused by Orvyn: {reason}")).await;
    }
    if agent.status == "live" {
        agent = crate::erc8004::mirror_to_erc8004(agent).await?;
    }
    if !billing_enabled() {
        // No registry configured (local dev): hosting only.
        if agent.status == "live" {
            agent = provision_host(agent).await?;
        }
        return Ok(agent);
    }
    agent = register_agent(agent).await?;
    let first = sync_subscription(agent).await?;
    let (mut active, mut ever_paid) = (first.active, first.ever_paid);
    agent = first.agent;
    // A new tier moves the agent to that tier's runtime plan.
    if first.tier_changed {
        let id = agent.id.clone();
        agent = match apply_plan(agent.clone()).await {
            Ok(a) => a,
            Err(e) => {
                eprintln!("[hosting] plan {id}: {e}");
                agent
            }
        };
    }
    // An owner-requested payment from the agent's treasury takes precedence over autopay.
    let pending = settle_treasury_payment(&agent).await?;
    let paid = if pending == PayOutcome::Paid {
        true
    } else if pending == PayOutcome::None && agent.billing_mode == "autopay" {
        autopay(&agent).await? == PayOutcome::Paid
    } else {
        false
    };
    if paid {
        let s = sync_subscription(agent).await?;
        (agent, active, ever_paid) = (s.agent, s.active, s.ever_paid);
    }
    // Invoices for payments made anywhere (wallets, other apps, the agent itself).
    if let Err(e) = crate::invoices::sync_invoices(&agent.id).await {
        eprintln!("[invoices] {}: {e}", agent.id);
    }
    if agent.status != "live" {
        return Ok(agent);
    }
    if active {
        agent = resume_host(provision_host(agent).await?).await?;
        // Record the environment it runs in (tier, plan, measured hardware) in the registry.
        let id = agent.id.clone();
        agent = match attest_environment(agent.clone()).await {
            Ok(a) => a,
            Err(e) => {
                eprintln!("[environment] {id}: {e}");
                agent
            }
        };
        Ok(agent)
    } else {
        let reason = if !ever_paid {
            "waiting for the first month of hosting. Pay in USDT, USDC, BNB or with Binance Pay to start.".to_string()
        } else {
            format!("subscription expired {}. Pay in USDT, USDC, BNB or with Binance Pay to resume.", agent.paid_until.map(iso).unwrap_or_default())
        };
        suspend_host(agent, &reason).await
    }
}
