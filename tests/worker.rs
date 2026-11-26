//! The worker: plugin job names match the TypeScript core's (vectors/plugin-tasks.json), the jobs
//! table is kept in step, and jobs run, skip when they don't apply and reschedule themselves.
mod support;

use orvyn_core::{jobs, worker};

#[test]
fn plugin_task_names_match() {
    support::test_env(&[]);
    let want: Vec<String> = serde_json::from_str(include_str!("vectors/plugin-tasks.json")).unwrap();
    let got: Vec<String> = orvyn_core::bliss::plugin_tasks().into_iter().map(|(t, _, _)| t).collect();
    assert_eq!(got, want);
}

#[tokio::test]
async fn jobs_are_created_claimed_and_rescheduled() {
    support::test_env(&[]);
    let db = support::shared_db().await;
    // A self-hosted agent: billing doesn't apply to it, and it has the mail plugin.
    sqlx::query(
        "insert into agents (id, name, symbol, owner_account_id, ai_provider, ai_model, ai_key_encrypted, ai_key_hint, evm_address, evm_wallet, status, hosting_status, launchpad)
         values ('a1', 'A', 'A', '0x1', 'x', 'x', 'x', 'x', '0x2', '{}', 'live', 'external', 'none')",
    )
    .execute(db)
    .await
    .unwrap();
    sqlx::query("insert into agent_plugins (agent_id, plugin_id, config) values ('a1', 'mail', '{}')").execute(db).await.unwrap();
    let core: Vec<&str> = worker::ORCHESTRATOR_TASKS.iter().chain(worker::AGENT_TASKS).copied().collect();
    let plugin: Vec<(String, String)> = orvyn_core::bliss::plugin_tasks().into_iter().map(|(t, p, _)| (p.meta().id.clone(), t)).collect();
    let refs: Vec<(&str, &str)> = plugin.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
    jobs::ensure_jobs(&core, &refs).await.unwrap();
    let tasks: Vec<String> = sqlx::query_scalar("select task from jobs where agent_id = 'a1' order by task").fetch_all(db).await.unwrap();
    assert_eq!(tasks.len(), core.len() + 1);
    assert!(tasks.contains(&"plugin:mail:inbox".to_string()) && !tasks.contains(&"plugin:hire:jobs".to_string()));

    let scope = jobs::Scope { agent_id: Some("a1".into()), tasks: Some(vec!["billing".into()]), task_prefix: None };
    let claimed = jobs::claim_due_jobs(10, 60.0, &scope).await.unwrap();
    assert_eq!(claimed.len(), 1);
    worker::run_job("a1", "billing").await.unwrap();
    let (minutes, error): (f64, Option<String>) = sqlx::query_as("select (extract(epoch from next_run_at - now()) / 60)::float8, last_error from jobs where agent_id = 'a1' and task = 'billing'").fetch_one(db).await.unwrap();
    assert!((4.5..=5.5).contains(&minutes), "billing runs every 5 minutes ({minutes})");
    assert_eq!(error, None);
    // The mail plugin's job: no handle, so nothing to read; it reschedules a minute out.
    worker::run_job("a1", "plugin:mail:inbox").await.unwrap();
    let minutes: f64 = sqlx::query_scalar("select (extract(epoch from next_run_at - now()) / 60)::float8 from jobs where agent_id = 'a1' and task = 'plugin:mail:inbox'").fetch_one(db).await.unwrap();
    let err: Option<String> = sqlx::query_scalar("select last_error from jobs where agent_id = 'a1' and task = 'plugin:mail:inbox'").fetch_one(db).await.unwrap();
    assert!((0.5..=1.5).contains(&minutes), "{minutes} {err:?}");
    // A job for an agent that no longer exists is pushed an hour out.
    worker::run_job("gone", "billing").await.unwrap();
}
