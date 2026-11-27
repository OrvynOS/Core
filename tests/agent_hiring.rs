//! Agents hiring agents, on a throwaway database (payments on chain are covered by the wallet
//! policy test; here the chain isn't reachable): the owner's limits (per job, per day, kinds of
//! work, never itself), the "ask me first" flow (a request the owner approves or declines, only
//! by its owner), and the tools: finding agents is a read, hiring acts in copilot only.
mod support;

use std::sync::Arc;

use orvyn_core::agent_hiring::{self as ah, Limits};
use orvyn_core::ai::ToolSet;
use orvyn_core::bliss;
use orvyn_core::db::Agent;
use serde_json::{json, Value};

async fn run(tools: &ToolSet, name: &str, args: Value) -> String {
    let t = tools.iter().find(|t| t.name == name).unwrap_or_else(|| panic!("no tool {name}"));
    match t.execute.clone().expect("runs here")(args).await {
        Ok(Value::String(s)) => s,
        Ok(v) => v.to_string(),
        Err(e) => e,
    }
}

fn names(tools: &ToolSet) -> Vec<String> {
    tools.iter().map(|t| t.name.clone()).collect()
}

#[tokio::test]
async fn agents_hire_agents_within_their_owners_limits() {
    support::test_env(&[]);
    let db = support::shared_db().await;
    let (alice, bob) = ("0x1111111111111111111111111111111111111111", "0x2222222222222222222222222222222222222222");
    for (id, handle, owner, wallet) in [("atlas-1", "atlas", alice, "0x00000000000000000000000000000000000000a1"), ("nova-1", "nova", bob, "0x00000000000000000000000000000000000000b1"), ("pixel-1", "pixel", bob, "0x00000000000000000000000000000000000000c1")] {
        sqlx::query("insert into agents (id, handle, name, symbol, owner_account_id, ai_provider, ai_model, ai_key_encrypted, ai_key_hint, evm_address, evm_wallet, status, subscription_active) values ($1, $2, $2, 'X', $3, 'x', 'x', 'x', 'x', $4, '{}', 'live', true)")
            .bind(id)
            .bind(handle)
            .bind(owner)
            .bind(wallet)
            .execute(db)
            .await
            .unwrap();
    }
    let load = |id: &'static str| async move { sqlx::query_as::<_, Agent>("select * from agents where id = $1").bind(id).fetch_one(orvyn_core::db::db()).await.unwrap() };
    let (atlas, nova, pixel) = (load("atlas-1").await, load("nova-1").await, load("pixel-1").await);
    // Nova sells research for $20; Pixel sells diagrams for $40.
    bliss::install_plugin(&nova, "hire", &json!({ "category": "research", "offerTitle": "Token report", "offerDescription": "A due-diligence report on any BNB Chain token, with the data.", "priceUsd": 20 })).await.unwrap();
    bliss::install_plugin(&pixel, "hire", &json!({ "category": "diagrams", "offerTitle": "Architecture diagrams", "offerDescription": "Clean architecture diagrams with an editable Excalidraw file.", "priceUsd": 40 })).await.unwrap();

    // The plugin's own checks: per-job limit within the daily one, known kinds of work.
    assert!(bliss::install_plugin(&atlas, ah::PLUGIN_ID, &json!({ "maxJobUsd": 100, "maxDailyUsd": 50 })).await.is_err());
    assert!(bliss::install_plugin(&atlas, ah::PLUGIN_ID, &json!({ "categories": ["poetry"] })).await.is_err());
    bliss::install_plugin(&atlas, ah::PLUGIN_ID, &json!({ "mode": "ask", "maxJobUsd": 30, "maxDailyUsd": 50, "categories": ["research", "diagrams"] })).await.unwrap();
    let limits = ah::limits_of("atlas-1").await.unwrap().unwrap();
    assert_eq!((limits.mode.as_str(), limits.max_job_usd, limits.token.as_str()), ("ask", 30.0, "usdt"));

    // Limits: itself, over the per-job limit, a kind of work not allowed; by id or handle.
    assert!(ah::check_hire(&atlas, &limits, "atlas").await.unwrap_err().message.contains("itself"));
    assert!(ah::check_hire(&atlas, &limits, "pixel").await.unwrap_err().message.contains("$40.00, over your owner's limit of $30.00 per job"));
    let only_diagrams = Limits { categories: vec!["diagrams".into()], ..limits.clone() };
    assert!(ah::check_hire(&atlas, &only_diagrams, "nova").await.unwrap_err().message.contains("hasn't allowed hiring for research"));
    let (target, offer) = ah::check_hire(&atlas, &limits, "nova").await.unwrap();
    assert_eq!((target.id.as_str(), offer.price_usd), ("nova-1", 20.0));

    // Tools: finding agents and following jobs are reads; hiring and reviewing act (copilot only).
    let arc = Arc::new(atlas.clone());
    let tools = |surface: &'static str| {
        let arc = arc.clone();
        async move { bliss::compose_tools(&arc, &Arc::new(bliss::installed_plugins("atlas-1", false).await.unwrap()), surface).unwrap() }
    };
    let (chat, copilot) = (tools("chat").await, tools("autopilot").await);
    assert!(names(&chat).contains(&"find_agents_to_hire".into()) && !names(&chat).contains(&"hire_agent".into()), "{:?}", names(&chat));
    assert!(names(&copilot).contains(&"hire_agent".into()) && names(&copilot).contains(&"review_hired_job".into()));
    let found = run(&chat, "find_agents_to_hire", json!({ "category": "research" })).await;
    assert!(found.contains("nova-1") && !found.contains("pixel-1") && !found.contains("atlas-1"), "{found}");

    // Ask mode: hire_agent makes a request for the owner, nothing is paid.
    let out = run(&copilot, "hire_agent", json!({ "agentId": "nova", "brief": "A due-diligence report on CAKE: liquidity, holders, contract risks and price action over 30 days.", "reason": "Clients asked about CAKE" })).await;
    assert!(out.contains("waiting for your owner"), "{out}");
    assert!(run(&copilot, "hire_agent", json!({ "agentId": "nova", "brief": "too short", "reason": "x" })).await.contains("fuller brief"));
    assert!(run(&copilot, "hire_agent", json!({ "agentId": "pixel", "brief": "Three architecture diagrams of our agent stack, with the Excalidraw files.", "reason": "docs" })).await.contains("over your owner's limit"));
    let reqs = ah::proposals("atlas-1").await.unwrap();
    assert_eq!((reqs.len(), reqs[0].status.as_str(), reqs[0].price_usd), (1, "pending", 20.0));
    let no_hires: i64 = sqlx::query_scalar("select count(*) from hires where client_agent_id = 'atlas-1'").fetch_one(db).await.unwrap();
    assert_eq!(no_hires, 0, "nothing paid while it waits");

    // Only the owner decides; declining closes it; approving hires (here the chain is unreachable,
    // so the request is marked failed with the reason, and the owner sees it).
    assert_eq!(ah::decide(bob, &atlas, &reqs[0].id, true).await.unwrap_err().status(), 403);
    let declined = ah::decide(alice, &atlas, &reqs[0].id, false).await.unwrap();
    assert_eq!(declined.status, "declined");
    assert!(ah::decide(alice, &atlas, &reqs[0].id, true).await.is_err(), "decided once");
    run(&copilot, "hire_agent", json!({ "agentId": "nova", "brief": "A due-diligence report on BAKE: liquidity, holders, contract risks and price action over 30 days.", "reason": "Follow-up" })).await;
    let second = ah::proposals("atlas-1").await.unwrap().into_iter().find(|p| p.status == "pending").unwrap();
    let approved = ah::decide(alice, &atlas, &second.id, true).await.unwrap();
    assert_eq!(approved.status, "failed", "no escrow contract in this test");
    assert!(approved.error.is_some());

    // The agent can't review or message a job it didn't hire.
    assert!(run(&copilot, "review_hired_job", json!({ "jobId": "h-not-mine", "decision": "approve" })).await.contains("You didn't hire that job"));
}
