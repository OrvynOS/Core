//! Postgres: the connection pool, migrations, and the row types (schema.rs).
//!
//! Migrations are the SQL files in `migrations/` with drizzle's journal (`migrations/meta/_journal.json`),
//! applied with drizzle's own bookkeeping (`drizzle.__drizzle_migrations`: sha256 of each file and
//! its journal time). A database migrated by the TypeScript core and one migrated here are the same.

pub mod migrate;
pub mod schema;

use std::sync::OnceLock;
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::PgPool;

use crate::env::env;
use crate::error::Result;
pub use schema::*;

static POOL: OnceLock<PgPool> = OnceLock::new();

/// Connection options for a URL. Render's internal hosts (no domain, e.g. `dpg-…-a`) and localhost
/// stay on the private network without TLS; other hosts need TLS.
pub fn connect_options(url: &str) -> Result<PgConnectOptions> {
    let parsed = url::Url::parse(url).map_err(|e| crate::error::Error::internal(format!("DATABASE_URL: {e}")))?;
    let host = parsed.host_str().unwrap_or("");
    let private = host == "localhost" || host == "127.0.0.1" || !host.contains('.');
    let options: PgConnectOptions = url.parse().map_err(|e| crate::error::Error::internal(format!("DATABASE_URL: {e}")))?;
    Ok(options.ssl_mode(if private { PgSslMode::Disable } else { PgSslMode::Require }))
}

/// A pool for a URL (lazy: connects on first use).
pub fn pool_for(url: &str, size: u32) -> Result<PgPool> {
    Ok(PgPoolOptions::new()
        .max_connections(size)
        .acquire_timeout(Duration::from_secs(15))
        .connect_lazy_with(connect_options(url)?))
}

thread_local! {
    static THREAD_POOL: std::cell::RefCell<Option<&'static PgPool>> = const { std::cell::RefCell::new(None) };
}

/// The database (DATABASE_URL, or one installed with `use_database`).
pub fn db() -> &'static PgPool {
    if let Some(p) = THREAD_POOL.with(|t| *t.borrow()) {
        return p;
    }
    POOL.get_or_init(|| pool_for(&env().database_url, env().database_pool_size).expect("DATABASE_URL"))
}

/// Use another database instead of DATABASE_URL (the self-hosted agent's own). Only before the
/// first `db()`.
pub fn use_database(pool: PgPool) -> bool {
    POOL.set(pool).is_ok()
}

/// Use a database on this thread only (tests: each runs on its own thread and runtime).
pub fn use_database_on_this_thread(pool: PgPool) {
    let leaked: &'static PgPool = Box::leak(Box::new(pool));
    THREAD_POOL.with(|t| *t.borrow_mut() = Some(leaked));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentLogKind {
    Launch,
    Fees,
    Trade,
    Think,
    Plugin,
    Withdraw,
    Mail,
    Identity,
    Payment,
    Error,
}

impl AgentLogKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Launch => "launch",
            Self::Fees => "fees",
            Self::Trade => "trade",
            Self::Think => "think",
            Self::Plugin => "plugin",
            Self::Withdraw => "withdraw",
            Self::Mail => "mail",
            Self::Identity => "identity",
            Self::Payment => "payment",
            Self::Error => "error",
        }
    }
}

/// Write to an agent's activity log. Errors also reach the creator's phone (throttled in push).
pub async fn log_agent(agent_id: &str, kind: AgentLogKind, message: &str) -> Result<()> {
    sqlx::query("insert into agent_logs (agent_id, kind, message) values ($1, $2, $3)")
        .bind(agent_id)
        .bind(kind.as_str())
        .bind(message)
        .execute(db())
        .await?;
    if kind == AgentLogKind::Error {
        let (id, msg) = (agent_id.to_string(), message.to_string());
        tokio::spawn(async move {
            let _ = crate::push::notify_agent_error(&id, &msg).await;
        });
    }
    Ok(())
}
