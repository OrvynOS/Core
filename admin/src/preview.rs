//! A local preview database: a throwaway Postgres on 127.0.0.1 (default port 5433) with Orvyn's
//! migrations and a few demo agents, for running orvyn-api and the web app locally. Gone when it
//! stops (Ctrl-C).

use std::time::Duration;

use anyhow::{anyhow, Result};
use base64::Engine;
use serde_json::json;

pub async fn run(port: u16) -> Result<()> {
    let settings = postgresql_embedded::Settings { port, temporary: true, timeout: Some(Duration::from_secs(600)), ..Default::default() };
    let mut pg = postgresql_embedded::PostgreSQL::new(settings);
    pg.setup().await?;
    pg.start().await?;
    pg.create_database("orvyn").await?;
    let url = pg.settings().url("orvyn");
    let pool = orvyn_core::db::pool_for(&url, 2).map_err(|e| anyhow!(e.message))?;
    orvyn_core::db::migrate::migrate(&pool).await.map_err(|e| anyhow!(e.message))?;

    let owner = "0x1111111111111111111111111111111111111111";
    let icon = |color: &str, letter: &str| {
        let svg = format!(r##"<svg xmlns="http://www.w3.org/2000/svg" width="96" height="96"><rect width="96" height="96" rx="48" fill="{color}"/><text x="48" y="62" font-size="44" text-anchor="middle" fill="#0b0b0b" font-family="Arial" font-weight="700">{letter}</text></svg>"##);
        format!("data:image/svg+xml;base64,{}", base64::engine::general_purpose::STANDARD.encode(svg))
    };
    let agents = [
        ("demo-atlas", "atlas", "Atlas Research", "ATLAS", "Answers research requests for other agents and takes paid jobs on the marketplace. Runs without a token.", icon("#F0B90B", "A"), "none", "live", "0x2222222222222222222222222222222222222222", None),
        ("demo-nova", "nova", "Nova Trader", "NOVA", "Trades BNB Chain tokens and BTC perps on Aster within tight limits. Its tax token funds its budget.", icon("#FCD535", "N"), "fourmeme", "live", "0x3333333333333333333333333333333333333333", Some("0x99d9b7d2c826fe1bdb0cdb4389077ea350527777")),
        ("demo-orbit", "orbit", "Orbit", "ORBT", "A new agent waiting for launch funds before it launches its token on Flap.", icon("#E8A317", "O"), "flap", "awaiting_funds", "0x4444444444444444444444444444444444444444", None),
    ];
    for (id, handle, name, symbol, description, icon, launchpad, status, wallet, token) in &agents {
        sqlx::query(
            "insert into agents (id, handle, name, symbol, description, icon, owner_account_id, ai_provider, ai_model, ai_key_encrypted, ai_key_hint, evm_address, evm_wallet, launchpad, status, token_address, persona, strategy)
             values ($1,$2,$3,$4,$5,$6,$7,'anthropic','claude-sonnet-5-5','x','sk-…demo',$8,$9,$10,$11,$12,'Precise and brief.','Small, careful positions.')",
        )
        .bind(id)
        .bind(handle)
        .bind(name)
        .bind(symbol)
        .bind(description)
        .bind(icon)
        .bind(owner)
        .bind(wallet)
        .bind(json!({ "custody": "privy", "walletId": format!("demo-{handle}"), "accountAddress": wallet }))
        .bind(launchpad)
        .bind(status)
        .bind(token)
        .execute(&pool)
        .await?;
    }
    let extras = [
        "insert into agent_plugins (agent_id, plugin_id, config) values ('demo-nova','trader','{}'), ('demo-nova','aster','{}'), ('demo-atlas','hire','{}'), ('demo-atlas','opinion','{}'), ('demo-atlas','orvynpay','{}')",
        "insert into agent_logs (agent_id, kind, message) values
           ('demo-nova','launch','Launched $NOVA on four.meme (tax 3%)'),
           ('demo-nova','trade','Bought 0.02 BNB of a BNB Chain token on PancakeSwap'),
           ('demo-atlas','payment','Paid 2 USDT to helper.ai for a data request'),
           ('demo-atlas','think','Reviewed two research requests')",
        // An outside app to try "Sign in with Orvyn".
        "insert into oauth_clients (id, name, owner_account_id, type, redirect_uris, scopes, homepage, first_party) values
           ('orv_demo_market', 'AgentMarket (demo)', '0x1111111111111111111111111111111111111111', 'public', '[\"http://localhost:4000/callback\"]', '[\"openid\",\"profile\",\"email\",\"wallet\",\"agent\"]', 'https://example.com', false)",
    ];
    for q in extras {
        if let Err(e) = sqlx::query(q).execute(&pool).await {
            eprintln!("preview data: {e}");
        }
    }
    println!("preview database ready at {url} ({} demo agents). Run orvyn-api with DATABASE_URL set to it. Ctrl-C stops and deletes it.", agents.len());
    tokio::signal::ctrl_c().await?;
    pool.close().await;
    pg.stop().await?;
    Ok(())
}
