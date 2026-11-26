//! Every row type reads its table: a row with a typed, non-null value in every column is read as
//! the type, so each field must name a real column with a compatible type.
mod support;

use orvyn_core::db::*;
use sqlx::postgres::PgRow;

/// A row with a typed, non-null value in every column of `table`, read as `T`.
async fn check<T: for<'r> sqlx::FromRow<'r, PgRow> + Send + Unpin>(table: &str, problems: &mut Vec<String>) {
    let cols: Vec<(String, String, String)> = sqlx::query_as(
        "select column_name::text, data_type::text, udt_name::text from information_schema.columns where table_schema = 'public' and table_name = $1 order by ordinal_position",
    )
    .bind(table)
    .fetch_all(db())
    .await
    .unwrap();
    if cols.is_empty() {
        problems.push(format!("{table}: no such table"));
        return;
    }
    let exprs: Vec<String> = cols
        .iter()
        .map(|(name, kind, udt)| {
            let v = match kind.as_str() {
                "ARRAY" => format!("'{{}}'::{}[]", udt.trim_start_matches('_')),
                "boolean" => "true".to_string(),
                "jsonb" | "json" => "'{}'::jsonb".to_string(),
                "bytea" => "decode('00', 'hex')".to_string(),
                "timestamp with time zone" => "now()".to_string(),
                "timestamp without time zone" => "now()::timestamp".to_string(),
                "date" => "current_date".to_string(),
                "text" | "character varying" | "USER-DEFINED" => "'x'::text".to_string(),
                other => format!("1::{other}"),
            };
            format!("{v} as \"{name}\"")
        })
        .collect();
    let sql = format!("select {}", exprs.join(", "));
    if let Err(e) = sqlx::query_as::<_, T>(&sql).fetch_one(db()).await {
        problems.push(format!("{table}: {e}"));
    }
}

#[tokio::test]
async fn row_types_match_the_tables() {
    support::test_env(&[]);
    support::shared_db().await;
    let mut p = vec![];
    macro_rules! tables {
        ($($t:ty => $n:literal),* $(,)?) => { $( check::<$t>($n, &mut p).await; )* };
    }
    tables! {
        Agent => "agents", Trade => "trades", AgentCall => "agent_calls", FeeClaim => "fee_claims", ChatMessage => "chat_messages",
        AgentLog => "agent_logs", Invoice => "invoices", BinancePayOrder => "binance_pay_orders", TriggerOrder => "trigger_orders",
        Withdrawal => "withdrawals", AgentPlugin => "agent_plugins", ExternalPlugin => "external_plugins", KnowledgeDoc => "knowledge_docs",
        Job => "jobs", MailMessage => "mail_messages", MailSettings => "mail_settings", Hire => "hires", HireMessage => "hire_messages",
        StudioRender => "studio_renders", JobFile => "job_files", User => "users", PushDevice => "push_devices", WalletLogEntry => "wallet_log",
        ShieldedKey => "shielded_keys", OauthClient => "oauth_clients", OauthGrant => "oauth_grants", OauthCode => "oauth_codes",
        OauthRefreshToken => "oauth_refresh_tokens", OauthDeviceCode => "oauth_device_codes", AgentCard => "agent_cards",
        CreditEntry => "credit_ledger", InferenceKey => "inference_keys", ChatThread => "chat_turn_threads", ChatTurnMessage => "chat_turn_messages",
        SocialProfile => "social_profiles", SocialPost => "social_posts", SocialFollow => "social_follows",
        VynSettings => "vyn_settings", VynMemory => "vyn_memories", VynRoutine => "vyn_routines", VynNotice => "vyn_notices",
        VynChannel => "vyn_channels", VynLinkCode => "vyn_link_codes", VynThread => "vyn_threads", VynMessage => "vyn_messages",
        ExternalAgentKey => "external_agent_keys", RngNode => "rng_nodes", RngPartial => "rng_partials", RngRound => "rng_rounds",
        PriceObservation => "price_observations", PriceReport => "price_reports", PriceReportSignature => "price_report_signatures",
        Game => "games", GameVersion => "game_versions", GameAsset => "game_assets", GamePayment => "game_payments",
        GameScore => "game_scores", WorldSession => "world_sessions", GameSave => "game_saves",
        BinanceConnection => "binance_connections", BinanceOauthState => "binance_oauth_states", BinanceAgentOrder => "binance_agent_orders",
    }
    assert!(p.is_empty(), "\n{}", p.join("\n"));
}
