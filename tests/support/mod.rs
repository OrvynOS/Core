//! Test support: settings for tests, and a real Postgres with a fresh, migrated database per test.
//!
//! The server is postgresql_embedded's (downloaded once to ~/.theseus), kept in
//! target/tmp/test-postgres on port 55433 and reused between runs: the first run starts it and
//! leaves it running. Stop it with `pg_ctl -D target/tmp/test-postgres stop`.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use orvyn_core::db;
use orvyn_core::env::{set_env, Env};
use sqlx::PgPool;

pub const AI_SECRET: &str = "test-ai-secret-0123456789abcdef0123456789";
pub const SESSION_SECRET: &str = "test-session-secret-0123456789abcdef01234";
const PORT: u16 = 55433;
const PASSWORD: &str = "orvyn-test";

/// Settings for tests (installed once per test binary).
pub fn test_env(extra: &[(&str, &str)]) {
    static DONE: OnceLock<()> = OnceLock::new();
    DONE.get_or_init(|| {
        let mut vars: HashMap<String, String> = [
            ("DATABASE_URL", "postgres://postgres@localhost/unused"),
            ("AI_KEY_ENCRYPTION_SECRET", AI_SECRET),
            ("SESSION_SECRET", SESSION_SECRET),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        for (k, v) in extra {
            vars.insert(k.to_string(), v.to_string());
        }
        set_env(Env::parse(&vars).expect("test settings"));
    });
}

fn url(database: &str) -> String {
    format!("postgres://postgres:{PASSWORD}@127.0.0.1:{PORT}/{database}")
}

/// Start the test server unless it's already up.
async fn ensure_server() {
    static READY: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    READY
        .get_or_init(|| async {
            if PgPool::connect(&url("postgres")).await.is_ok() {
                return;
            }
            // Started on its own thread and runtime so it outlives each test's runtime.
            std::thread::spawn(|| {
                tokio::runtime::Runtime::new().unwrap().block_on(async {
                    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("test-postgres");
                    let settings = postgresql_embedded::Settings {
                        data_dir: dir,
                        port: PORT,
                        password: PASSWORD.into(),
                        temporary: false,
                        // The first run downloads Postgres and runs initdb.
                        timeout: Some(Duration::from_secs(600)),
                        ..Default::default()
                    };
                    let mut pg = postgresql_embedded::PostgreSQL::new(settings);
                    pg.setup().await.expect("postgres setup");
                    pg.start().await.expect("postgres start");
                    // Left running on purpose (reused by the next run).
                    std::mem::forget(pg);
                });
            })
            .join()
            .expect("postgres");
        })
        .await;
}

/// A new empty database, migrated, with a pool on it.
pub async fn fresh_db() -> PgPool {
    ensure_server().await;
    let admin = PgPool::connect(&url("postgres")).await.expect("connect");
    let name = format!("t{}", hex::encode(rand::random::<[u8; 8]>()));
    sqlx::query(&format!("create database {name}")).execute(&admin).await.expect("create database");
    admin.close().await;
    let pool = db::pool_for(&url(&name), 4).expect("pool");
    db::migrate::migrate(&pool).await.expect("migrate");
    pool
}

/// Serve `app` on 127.0.0.1 from its own thread and runtime, so it outlives the test's runtime
/// (settings are read once per test binary). Returns its base URL.
pub fn serve_forever(app: axum::Router) -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(format!("http://{}", listener.local_addr().unwrap())).unwrap();
            axum::serve(listener, app).await.unwrap();
        })
    });
    rx.recv().unwrap()
}

/// A BNB Chain JSON-RPC where wallets are empty and every contract call reverts (an unminted
/// Agent ID, no token), so code that reads the chain runs offline.
pub fn zero_rpc() -> String {
    use axum::routing::post;
    static URL: OnceLock<String> = OnceLock::new();
    URL.get_or_init(|| {
        serve_forever(axum::Router::new().route(
            "/",
            post(|axum::Json(body): axum::Json<serde_json::Value>| async move {
                let answer = |req: &serde_json::Value| match req["method"].as_str() {
                    Some("eth_call") => serde_json::json!({ "jsonrpc": "2.0", "id": req["id"], "error": { "code": 3, "message": "execution reverted" } }),
                    Some("eth_chainId") => serde_json::json!({ "jsonrpc": "2.0", "id": req["id"], "result": "0x38" }),
                    _ => serde_json::json!({ "jsonrpc": "2.0", "id": req["id"], "result": "0x0" }),
                };
                axum::Json(match &body {
                    serde_json::Value::Array(reqs) => serde_json::Value::Array(reqs.iter().map(answer).collect()),
                    req => answer(req),
                })
            }),
        ))
    })
    .clone()
}

/// A fresh database for code that uses `db()`, for this test (its thread) only.
pub async fn shared_db() -> &'static PgPool {
    let pool = fresh_db().await;
    db::use_database_on_this_thread(pool);
    db::db()
}
