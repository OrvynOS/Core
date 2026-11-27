//! Binance Agentic Wallet on a throwaway database, against a stand-in `baw` (fixtures/fake-baw.mjs,
//! run with node):
//! - signing in: only the owner, only with the plugin; Binance's link and pairing code are shown
//!   while Orvyn waits for the owner's confirmation; a rejected sign-in is reported;
//! - one session per agent: each agent's `baw` runs with its own instance id and its own session,
//!   kept encrypted in the database and never left on disk;
//! - agents: reads in chat, actions only in autopilot, each checked against the owner's limits
//!   before it reaches Binance; a swap waits for Binance to finish it;
//! - an expired session marks the wallet signed out; disconnecting signs out on Binance too.
mod support;

use std::sync::Arc;
use std::time::Duration;

use orvyn_core::agentic_wallet as baw;
use orvyn_core::ai::ToolSet;
use orvyn_core::bliss;
use orvyn_core::db::Agent;
use serde_json::{json, Value};

async fn run(tools: &ToolSet, name: &str, args: Value) -> String {
    let t = tools.iter().find(|t| t.name == name).unwrap_or_else(|| panic!("no tool {name}"));
    match t.execute.clone().expect("a tool that runs here")(args).await {
        Ok(Value::String(s)) => s,
        Ok(v) => v.to_string(),
        Err(e) => e,
    }
}

fn names(tools: &ToolSet) -> Vec<String> {
    tools.iter().map(|t| t.name.clone()).collect()
}

fn cmd(c: &Value) -> String {
    format!("{} {}", c["argv"][0].as_str().unwrap_or(""), c["argv"][1].as_str().unwrap_or("")).trim().to_string()
}

fn calls(log: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(log).unwrap_or_default().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

async fn wait_for(agent: &str, status: &str) -> Value {
    for _ in 0..100 {
        let v = baw::view(agent).await.unwrap();
        if v["status"] == status {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("{agent} never became {status}: {}", baw::view(agent).await.unwrap());
}

#[tokio::test]
async fn binance_agentic_wallet_for_agents() {
    if std::process::Command::new("node").arg("--version").output().is_err() {
        eprintln!("skipped: needs node");
        return;
    }
    support::test_env(&[]);
    let db = support::shared_db().await;
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-baw.mjs");
    let log = std::env::temp_dir().join(format!("fake-baw-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&log);
    for (k, v) in [("BAW_BIN", fixture.to_string_lossy().to_string()), ("FAKE_BAW_LOG", log.to_string_lossy().to_string())] {
        unsafe { std::env::set_var(k, v) };
    }

    let owner = "0x1111111111111111111111111111111111111111";
    for id in ["wallet-1", "wallet-2"] {
        sqlx::query("insert into agents (id, handle, name, symbol, owner_account_id, ai_provider, ai_model, ai_key_encrypted, ai_key_hint, evm_address, evm_wallet, status) values ($1, $1, 'W', 'W', $2, 'x', 'x', 'x', 'x', '0x00000000000000000000000000000000000000b1', '{}', 'live')")
            .bind(id)
            .bind(owner)
            .execute(db)
            .await
            .unwrap();
    }
    let agent: Agent = sqlx::query_as("select * from agents where id = 'wallet-1'").fetch_one(db).await.unwrap();
    let other: Agent = sqlx::query_as("select * from agents where id = 'wallet-2'").fetch_one(db).await.unwrap();

    // ---- Signing in ----
    assert!(baw::start_connect(owner, &agent).await.unwrap_err().message.contains("install the Binance Agentic Wallet plugin"), "the plugin comes first");
    let limits = json!({ "maxOrderUsd": 100, "maxDailyUsd": 150, "chains": ["BSC"], "allowLimitOrders": false, "allowSends": true });
    bliss::install_plugin(&agent, baw::PLUGIN_ID, &limits).await.unwrap();
    bliss::install_plugin(&other, baw::PLUGIN_ID, &limits).await.unwrap();
    assert_eq!(baw::start_connect("0x2222222222222222222222222222222222222222", &agent).await.unwrap_err().status(), 403, "only the owner signs it in");

    let started = baw::start_connect(owner, &agent).await.unwrap();
    assert_eq!(started["status"], "pending");
    assert_eq!(started["pending"]["url"], "https://app.binance.com/uni-qr/test");
    assert_eq!(started["pending"]["pairingCode"], "a1b2c3", "the owner sees Binance's pairing code");
    let done = wait_for("wallet-1", "connected").await;
    assert_eq!(done["addresses"], json!([{ "chain": "BSC", "address": "0x00000000000000000000000000000000000000aa" }, { "chain": "Solana", "address": "So1aaa" }]));
    assert_eq!(done["limits"]["dailyLimit"], 500, "the Binance App's own limits are shown");
    assert!(done["pending"].is_null());

    // The session is the agent's own: stored encrypted, its own instance, nothing left on disk.
    let stored: String = sqlx::query_scalar("select session_encrypted from binance_wallet_sessions where agent_id = 'wallet-1'").fetch_one(db).await.unwrap();
    assert!(stored.starts_with("v1:") && !stored.contains("sess-1"), "the session is stored encrypted");
    let log1 = calls(&log);
    assert_eq!(log1.iter().map(|c| cmd(c)).collect::<Vec<_>>()[..3], ["auth signin", "auth verify", "wallet status"]);
    assert!(log1.iter().all(|c| !std::path::Path::new(c["dir"].as_str().unwrap()).exists()), "each command's directory is removed");
    let instance = log1[0]["instance"].as_str().unwrap().to_string();
    assert!(log1.iter().all(|c| c["instance"] == instance.as_str()) && instance.len() == 64);

    // A second agent: its own instance, and a rejected sign-in is reported.
    unsafe { std::env::set_var("FAKE_BAW_QR", "qr-wrong") };
    baw::start_connect(owner, &other).await.unwrap();
    let rejected = wait_for("wallet-2", "disconnected").await;
    assert!(rejected["error"].as_str().unwrap().contains("AUTH_REJECTED"), "{rejected}");
    unsafe { std::env::remove_var("FAKE_BAW_QR") };
    let theirs = calls(&log).into_iter().rev().find(|c| c["argv"][0] == "auth").unwrap();
    assert_ne!(theirs["instance"], instance.as_str(), "every agent has its own session");

    // ---- Tools ----
    let arc = Arc::new(agent.clone());
    let tools_for = |surface: &'static str| {
        let arc = arc.clone();
        async move { bliss::compose_tools(&arc, &Arc::new(bliss::installed_plugins("wallet-1", false).await.unwrap()), surface).unwrap() }
    };
    let (chat, auto) = (tools_for("chat").await, tools_for("autopilot").await);
    assert!(names(&chat).contains(&"agentic_wallet_balance".into()) && !names(&chat).contains(&"agentic_wallet_swap".into()), "reads in chat: {:?}", names(&chat));
    assert!(names(&auto).contains(&"agentic_wallet_swap".into()) && names(&auto).contains(&"agentic_wallet_send".into()), "actions in autopilot");
    assert!(!names(&auto).contains(&"agentic_wallet_limit_buy".into()), "limit orders are off");
    assert!(run(&chat, "agentic_wallet_balance", json!({ "chainId": "BSC" })).await.contains("1.5"));
    let last = calls(&log).pop().unwrap();
    assert_eq!(last["argv"], json!(["wallet", "balance", "--binanceChainId", "56"]), "the chain is passed as baw names it");

    let refused = |out: &str| out.contains("\"refused\"");
    let bnb = "0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE";
    let usdt = "0x55d398326f99059fF775485246999027B3197955";
    let swap = |qty: &str, chain: &str| json!({ "fromTokenQty": qty, "fromToken": bnb, "toToken": usdt, "chainId": chain });
    // 0.1 BNB quotes at $60.
    let out = run(&auto, "agentic_wallet_swap", swap("0.1", "56")).await;
    assert!(!refused(&out) && out.contains("FINISHED"), "a $60 swap goes through and is followed to the end: {out}");
    assert!(refused(&run(&auto, "agentic_wallet_swap", swap("0.2", "56")).await), "over the per-order limit ($120 > $100)");
    assert!(refused(&run(&auto, "agentic_wallet_swap", swap("0.01", "CT_501")).await), "a chain the owner didn't allow");
    // Sends: USDT at $1; the day now has $60.
    let send = |amount: &str, token: &str| json!({ "amount": amount, "recipient": "0x00000000000000000000000000000000000000cc", "tokenAddress": token, "chainId": "56" });
    assert!(run(&auto, "agentic_wallet_send", send("80", usdt)).await.contains("0xsent"), "a $80 USDT send ($140 in the day)");
    assert!(refused(&run(&auto, "agentic_wallet_send", send("20", usdt)).await), "over the daily limit ($140 + $20 > $150)");
    assert!(refused(&run(&auto, "agentic_wallet_send", send("1", "0x0000000000000000000000000000000000000001")).await), "only the chain's coin, USDT or USDC");
    assert!(!refused(&run(&auto, "agentic_wallet_cancel_limit_order", json!({ "strategyId": "s1" })).await), "cancelling is always allowed");
    let acted: Vec<String> = calls(&log).iter().filter(|c| ["swap", "send", "cancel"].contains(&c["argv"][1].as_str().unwrap())).map(|c| cmd(c)).collect();
    assert_eq!(acted, ["market-order swap", "wallet send", "limit-order cancel"], "only the allowed actions reached Binance");

    // ---- Signed out on Binance's side ----
    assert!(baw::call("wallet-1", &["expire"]).await.unwrap_err().contains("SESSION_EXPIRED"));
    assert_eq!(baw::view("wallet-1").await.unwrap()["status"], "disconnected", "an expired session marks the wallet signed out");
    assert!(run(&chat, "agentic_wallet_balance", json!({})).await.contains("isn't connected"));

    // ---- Signing in again, then disconnecting ----
    baw::start_connect(owner, &agent).await.unwrap();
    wait_for("wallet-1", "connected").await;
    assert!(baw::disconnect(owner, &agent).await.unwrap());
    assert_eq!(calls(&log).pop().unwrap()["argv"], json!(["auth", "signout"]), "disconnecting signs out on Binance");
    assert!(baw::session("wallet-1").await.unwrap().is_none());
    let state: Value = sqlx::query_scalar("select state from agent_plugins where agent_id = 'wallet-1' and plugin_id = $1").bind(baw::PLUGIN_ID).fetch_one(db).await.unwrap();
    assert_eq!(state["connected"], false);
    let _ = std::fs::remove_file(&log);
}
