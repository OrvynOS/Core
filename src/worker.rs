//! The job loop shared by the orchestrator (the runtime) and each agent's own service. Jobs live in
//! Postgres and are leased with row locks.
//!
//! - Orchestrator tasks run for every agent: launch, launch sync, billing, Agent ID checks,
//!   self-hosted tiers, marketplace jobs.
//! - Agent tasks run on the agent's own service (render hosting) or in the shared runtime: tax
//!   income, autopilot and every installed plugin's jobs. They skip while suspended.
//! - Timers (orchestrator only): Binance Pay reconciliation, Orvyn RNG and price feeds, Vyn's due
//!   reminders.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::bliss::{context_for, installed_plugins, plugin_tasks};
use crate::db::{db, log_agent, Agent, AgentLogKind};
use crate::env::{env, HostingMode};
use crate::error::Result;
use crate::hosting::agent_is_serving;
use crate::jobs::{claim_due_jobs, ensure_jobs, finish_job, Scope};

fn minutes(n: f64) -> f64 {
    n * 60.0
}

pub const ORCHESTRATOR_TASKS: &[&str] = &["launch", "sync-launch", "agent-id", "billing", "self-hosted-tier", "hires"];
pub const AGENT_TASKS: &[&str] = &["income", "autopilot"];

/// (seconds between runs, whether it applies to this agent) for a core task.
fn schedule(task: &str, a: &Agent) -> Option<(f64, bool)> {
    let live = a.status == "live";
    Some(match task {
        // Agents created with a token, and live agents whose owner asked for one later.
        "launch" => (30.0, a.launchpad != "none" && a.token_address.is_none() && (a.status == "awaiting_funds" || (live && a.launch_tx.is_none()))),
        // Watch the launchpad curve until the token graduates to PancakeSwap.
        "sync-launch" => (minutes(5.0), live && a.token_address.is_some() && !a.graduated),
        // Binance Agent ID (premium): still held by the agent or its owner?
        "agent-id" => (minutes(60.0), a.agent_id_name.is_some()),
        // Self-hosted agents aren't hosted or billed by Orvyn...
        "billing" => (minutes(5.0), a.hosting_status != "external"),
        // ...but they can buy a tier in the registry; mirror it while it's paid.
        "self-hosted-tier" => (minutes(10.0), a.hosting_status == "external" && a.registered && crate::billing::billing_enabled()),
        // Marketplace jobs: mirror the escrow and make the operator's calls.
        "hires" => (minutes(1.0), live && a.hosting_status != "external" && crate::hires::marketplace_enabled()),
        // Split new tax income into plugin budgets.
        "income" => (minutes(std::env::var("FEE_INTERVAL_MIN").ok().and_then(|v| v.parse().ok()).unwrap_or(10.0)), live && agent_is_serving(a)),
        // 1 minute to 3 hours (older agents may have a longer interval stored: clamped).
        "autopilot" => (minutes(a.autopilot_interval_min.clamp(1, 180) as f64), live && a.autopilot_enabled && agent_is_serving(a)),
        _ => return None,
    })
}

/// Whether a core task runs for this agent now (e.g. "launch", "billing", "hires").
pub fn applies(task: &str, agent: &Agent) -> bool {
    schedule(task, agent).is_some_and(|(_, applies)| applies)
}

async fn run_core(task: &str, agent: Agent) -> Result<()> {
    match task {
        "launch" => crate::lifecycle::try_launch(agent).await.map(drop),
        "sync-launch" => crate::lifecycle::sync_launch(agent).await.map(drop),
        "agent-id" => crate::agentid::recheck_agent_id(&agent).await.map(drop),
        "billing" => crate::subscription::run_billing(agent).await.map(drop),
        "self-hosted-tier" => crate::billing::sync_self_hosted_tier(agent).await.map(drop),
        "hires" => crate::hires::sync_agent_hires(&agent.id).await,
        "income" => crate::lifecycle::process_income(&agent).await,
        "autopilot" => crate::agent::autopilot::run_autopilot_session(Arc::new(agent)).await.map(drop),
        _ => Ok(()),
    }
}

/// Run one claimed job and schedule its next run (errors are logged to the agent's activity).
pub async fn run_job(agent_id: &str, task: &str) -> Result<()> {
    let agent: Option<Agent> = sqlx::query_as("select * from agents where id = $1").bind(agent_id).fetch_optional(db()).await?;
    let Some(agent) = agent else { return finish_job(agent_id, task, minutes(60.0), None).await };
    let mut every = minutes(60.0);
    let symbol = agent.symbol.clone();
    let r: Result<()> = async {
        if let Some((secs, applies)) = schedule(task, &agent) {
            every = secs;
            if applies {
                run_core(task, agent).await?;
            }
        } else if let Some((_, plugin, job)) = plugin_tasks().into_iter().find(|(t, _, _)| t == task) {
            let installed = Arc::new(installed_plugins(&agent.id, false).await?);
            if let Some(entry) = installed.iter().find(|p| p.plugin.meta().id == plugin.meta().id) {
                let serving = agent.status == "live" && agent_is_serving(&agent);
                let ctx = context_for(&Arc::new(agent), entry, &installed);
                every = (job.every)(&ctx);
                if serving {
                    (job.run)(ctx).await?;
                }
            }
        }
        Ok(())
    }
    .await;
    match r {
        Ok(()) => finish_job(agent_id, task, every, None).await,
        Err(e) => {
            eprintln!("[{task}] {symbol}: {}", e.message);
            let _ = log_agent(agent_id, AgentLogKind::Error, &format!("{task}: {}", e.message)).await;
            finish_job(agent_id, task, every, Some(&e.message)).await
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Role {
    /// The shared control plane.
    Orchestrator,
    /// One agent's own service.
    Agent,
    /// Local dev / shared hosting: everything.
    All,
}

pub struct WorkerOptions {
    pub role: Role,
    pub agent_id: Option<String>,
    pub concurrency: Option<usize>,
}

pub struct Worker {
    pub tasks: Vec<String>,
    stopping: Arc<AtomicBool>,
    active: Arc<AtomicUsize>,
    handles: Vec<tokio::task::JoinHandle<()>>,
}

impl Worker {
    /// Stop claiming, and wait for in-flight jobs (up to `timeout`).
    pub async fn stop(self, timeout: Duration) {
        self.stopping.store(true, Ordering::SeqCst);
        for h in &self.handles {
            h.abort();
        }
        let deadline = tokio::time::Instant::now() + timeout;
        while self.active.load(Ordering::SeqCst) > 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

fn every(period: Duration, label: &'static str, stopping: Arc<AtomicBool>, f: impl Fn() -> futures_util::future::BoxFuture<'static, Result<()>> + Send + 'static) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(period);
        loop {
            tick.tick().await;
            if stopping.load(Ordering::SeqCst) {
                break;
            }
            if let Err(e) = f().await {
                eprintln!("[{label}] {}", e.message);
            }
        }
    })
}

/// Start the loops: keep the jobs table in step, claim due jobs every 2 s, and (orchestrator)
/// the timers. Must be called inside a Tokio runtime.
pub fn start_worker(options: WorkerOptions) -> Worker {
    let concurrency = options.concurrency.unwrap_or_else(|| std::env::var("WORKER_CONCURRENCY").ok().and_then(|v| v.parse().ok()).unwrap_or(4));
    let lease = 15.0 * 60.0;
    // With render hosting the orchestrator leaves agent tasks to each agent's own service.
    let run_agent_tasks = options.role != Role::Orchestrator || env().hosting_mode == HostingMode::Shared;
    let run_orchestration = options.role != Role::Agent;
    let mut tasks: Vec<String> = vec![];
    if run_orchestration {
        tasks.extend(ORCHESTRATOR_TASKS.iter().map(|s| s.to_string()));
    }
    if run_agent_tasks {
        tasks.extend(AGENT_TASKS.iter().map(|s| s.to_string()));
    }
    let scope = Arc::new(Scope { agent_id: options.agent_id.clone(), tasks: Some(tasks.clone()), task_prefix: run_agent_tasks.then(|| "plugin:".to_string()) });
    let stopping = Arc::new(AtomicBool::new(false));
    let active = Arc::new(AtomicUsize::new(0));
    let mut handles = vec![];

    handles.push(every(Duration::from_secs(30), "ensureJobs", stopping.clone(), || {
        Box::pin(async {
            let core: Vec<&str> = ORCHESTRATOR_TASKS.iter().chain(AGENT_TASKS).copied().collect();
            let plugin: Vec<(String, String)> = plugin_tasks().into_iter().map(|(t, p, _)| (p.meta().id.clone(), t)).collect();
            let refs: Vec<(&str, &str)> = plugin.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
            ensure_jobs(&core, &refs).await
        })
    }));
    {
        let (stop, active, scope) = (stopping.clone(), active.clone(), scope.clone());
        handles.push(every(Duration::from_secs(2), "tick", stopping.clone(), move || {
            let (stop, active, scope) = (stop.clone(), active.clone(), scope.clone());
            Box::pin(async move {
                let busy = active.load(Ordering::SeqCst);
                if stop.load(Ordering::SeqCst) || busy >= concurrency {
                    return Ok(());
                }
                for job in claim_due_jobs((concurrency - busy) as i64, lease, &scope).await? {
                    active.fetch_add(1, Ordering::SeqCst);
                    let active = active.clone();
                    tokio::spawn(async move {
                        if let Err(e) = run_job(&job.agent_id, &job.task).await {
                            eprintln!("[{}] {}", job.task, e.message);
                        }
                        active.fetch_sub(1, Ordering::SeqCst);
                    });
                }
                Ok(())
            })
        }));
    }
    if run_orchestration {
        // Binance Pay: catch up on missed webhooks and retry credits.
        handles.push(every(Duration::from_secs(120), "binance-pay", stopping.clone(), || Box::pin(crate::binancepay::reconcile_orders())));
        // Orvyn RNG: hosted nodes sign the current round; closed rounds are finalised; on chain,
        // rounds contracts asked for are committed and fulfilled. Price feeds likewise.
        handles.push(every(Duration::from_secs(5), "rng", stopping.clone(), || {
            Box::pin(async {
                let now = crate::rng::now_s();
                crate::rng::contribute_hosted(now).await?;
                crate::rng::finalize_rounds(now).await?;
                if crate::rng::chain::rng_contract().is_some() {
                    crate::rng::chain::relay_rounds(now).await?;
                }
                crate::price::tick(now).await?;
                if crate::price::chain::price_feed_contract().is_some() {
                    crate::price::chain::relay_reports().await?;
                }
                Ok(())
            })
        }));
        // Vyn: reminders and daily check-ins that are due.
        handles.push(every(Duration::from_secs(30), "vyn routines", stopping.clone(), || Box::pin(async { crate::vyn::run_due_routines().await.map(drop) })));
    }
    Worker { tasks, stopping, active, handles }
}
