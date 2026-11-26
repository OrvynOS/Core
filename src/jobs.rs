//! Postgres-backed per-agent job scheduling. Every agent has one row per task; a worker claims due
//! rows with FOR UPDATE SKIP LOCKED and holds a lease while it runs them, so any number of runtime
//! instances can share the work and a crashed worker's lease simply expires.

use crate::db::db;
use crate::error::Result;

/// Create missing job rows: core tasks for every agent, plugin tasks only for agents with that
/// plugin installed.
pub async fn ensure_jobs(core_tasks: &[&str], plugin_tasks: &[(&str, &str)]) -> Result<()> {
    if !core_tasks.is_empty() {
        sqlx::query("insert into jobs (agent_id, task) select a.id, t.task from agents a cross join unnest($1::text[]) as t(task) on conflict do nothing")
            .bind(core_tasks.iter().map(|s| s.to_string()).collect::<Vec<_>>())
            .execute(db())
            .await?;
    }
    if plugin_tasks.is_empty() {
        return Ok(());
    }
    let (plugins, tasks): (Vec<String>, Vec<String>) = plugin_tasks.iter().map(|(p, t)| (p.to_string(), t.to_string())).unzip();
    sqlx::query(
        "insert into jobs (agent_id, task)
         select ap.agent_id, t.task from agent_plugins ap join unnest($1::text[], $2::text[]) as t(plugin_id, task) on t.plugin_id = ap.plugin_id
         on conflict do nothing",
    )
    .bind(plugins)
    .bind(tasks)
    .execute(db())
    .await?;
    Ok(())
}

/// What a worker claims: the orchestrator its own tasks for every agent; an agent's host only that
/// agent's tasks.
#[derive(Debug, Clone, Default)]
pub struct Scope {
    pub agent_id: Option<String>,
    pub tasks: Option<Vec<String>>,
    pub task_prefix: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedJob {
    pub agent_id: String,
    pub task: String,
}

pub async fn claim_due_jobs(limit: i64, lease_seconds: f64, scope: &Scope) -> Result<Vec<ClaimedJob>> {
    let filter_tasks = scope.tasks.is_some() || scope.task_prefix.is_some();
    let rows: Vec<(String, String)> = sqlx::query_as(
        "update jobs set locked_until = now() + make_interval(secs => $2)
         where (agent_id, task) in (
           select agent_id, task from jobs
           where next_run_at <= now() and (locked_until is null or locked_until < now())
             and ($3::text is null or agent_id = $3)
             and (not $4 or task = any(coalesce($5::text[], '{}')) or ($6::text is not null and task like $6 || '%'))
           order by next_run_at
           limit $1
           for update skip locked
         )
         returning agent_id, task",
    )
    .bind(limit)
    .bind(lease_seconds)
    .bind(&scope.agent_id)
    .bind(filter_tasks)
    .bind(&scope.tasks)
    .bind(&scope.task_prefix)
    .fetch_all(db())
    .await?;
    Ok(rows.into_iter().map(|(agent_id, task)| ClaimedJob { agent_id, task }).collect())
}

pub async fn finish_job(agent_id: &str, task: &str, every_seconds: f64, error: Option<&str>) -> Result<()> {
    sqlx::query("update jobs set locked_until = null, next_run_at = now() + make_interval(secs => $3), last_error = $4 where agent_id = $1 and task = $2")
        .bind(agent_id)
        .bind(task)
        .bind(every_seconds)
        .bind(error)
        .execute(db())
        .await?;
    Ok(())
}
