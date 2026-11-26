//! Agents on a throwaway database (the chain is a stand-in RPC that reports empty wallets):
//! - Virtual Card records: only the owner links, updates or removes a card; a new card is off for
//!   the agent until the owner turns it on; limits are checked;
//! - agents without a token: live, open to anyone signed in; a token asked for later waits for gas
//!   money, closes holder chat until it exists, and can't be asked for twice;
//! - self-hosted agents connecting: the wallet's own fresh signature for the owner who approved it,
//!   no takeover, reconnecting replaces the key, public activity kinds only, never billed or hired
//!   out, Orvyn can't sign for them;
//! - Vyn as the person's companion agent: one per person, handles checked, left out of "your
//!   agents", ask_agent finds agents and says why it can't reach one, her prompt knows her handle.
mod support;

use std::sync::{Arc, Mutex};

use alloy::signers::local::PrivateKeySigner;
use alloy::signers::SignerSync;
use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use orvyn_core::cards::{self, CardUpdate, NewCard};
use orvyn_core::db::Agent;
use orvyn_core::external_agents::{self as ext, RegisterInput};
use orvyn_core::launchpad::TokenTax;
use orvyn_core::vyn::{self, companion};
use orvyn_core::{auth, lifecycle, worker};
use serde_json::{json, Value};

fn setup() {
    support::test_env(&[("BSC_RPC_URL", &support::zero_rpc())]);
}

async fn agent(id: &str) -> Agent {
    sqlx::query_as("select * from agents where id = $1").bind(id).fetch_one(orvyn_core::db::db()).await.unwrap()
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// A registration signed by `key` for `owner`.
fn signed(key: &PrivateKeySigner, owner: &str, name: &str, issued_at: &str) -> RegisterInput {
    let wallet = key.address().to_checksum(None);
    let message = ext::registration_message(&wallet, owner, name, issued_at).unwrap();
    let sig = key.sign_message_sync(message.as_bytes()).unwrap();
    RegisterInput { wallet, name: name.into(), symbol: None, description: None, handle: None, issued_at: issued_at.into(), signature: format!("0x{}", hex::encode(sig.as_bytes())) }
}

#[tokio::test]
async fn virtual_cards() {
    setup();
    let db = support::shared_db().await;
    let owner = "0x1111111111111111111111111111111111111111";
    let other = "0x3333333333333333333333333333333333333333";
    sqlx::query("insert into agents (id, name, symbol, owner_account_id, ai_provider, ai_model, ai_key_encrypted, ai_key_hint, evm_address, evm_wallet, launchpad, status) values ('a1', 'A', 'A', $1, 'x', 'x', 'x', 'x', '0x2222222222222222222222222222222222222222', '{}', 'none', 'live')")
        .bind(owner)
        .execute(db)
        .await
        .unwrap();
    let a = agent("a1").await;
    assert!(cards::agent_card("a1").await.unwrap().is_none(), "no card by default");
    let link = |id: &'static str| NewCard { card_id: id, environment: "sandbox", chain_id: "eip155:84532", asset: "usdc" };
    assert!(cards::link_card(&a, other, link("card_test123")).await.is_err(), "someone else can't link a card");
    let card = cards::link_card(&a, owner, link("card_test123")).await.unwrap();
    assert_eq!((&card["enabled"], &card["perPurchaseLimitUsd"], &card["monthlyLimitUsd"]), (&json!(false), &json!(50.0), &json!(200.0)), "a linked card is off for the agent: {card}");
    assert!(cards::update_card(&a, other, CardUpdate { enabled: Some(true), ..Default::default() }).await.is_err(), "someone else can't turn it on");
    let on = cards::update_card(&a, owner, CardUpdate { enabled: Some(true), per_purchase_limit_usd: Some(25.0), ..Default::default() }).await.unwrap();
    assert_eq!((&on["enabled"], &on["perPurchaseLimitUsd"]), (&json!(true), &json!(25.0)));
    assert!(cards::update_card(&a, owner, CardUpdate { monthly_limit_usd: Some(-5.0), ..Default::default() }).await.is_err(), "a negative limit is refused");
    let relinked = cards::link_card(&a, owner, link("card_replacement")).await.unwrap();
    assert_eq!((&relinked["enabled"], relinked["cardId"].as_str()), (&json!(false), Some("card_replacement")), "a replacement card starts off again");
    cards::unlink_card(&a, owner).await.unwrap();
    assert!(cards::agent_card("a1").await.unwrap().is_none());
}

#[tokio::test]
async fn agents_without_a_token() {
    setup();
    let db = support::shared_db().await;
    let wallet = format!("{:#x}", PrivateKeySigner::random().address());
    let owner = format!("{:#x}", PrivateKeySigner::random().address());
    sqlx::query(
        "insert into agents (id, name, symbol, icon, owner_account_id, ai_provider, ai_model, ai_key_encrypted, ai_key_hint, evm_address, evm_wallet, launchpad, status)
         values ('tokenless-1', 'Helper', 'HELP', 'data:image/png;base64,iVBORw0KGgo=', $1, 'x', 'x', 'x', 'x', $2, $3, 'none', 'live')",
    )
    .bind(&owner)
    .bind(&wallet)
    .bind(json!({ "custody": "privy", "walletId": "w", "accountAddress": wallet }))
    .execute(db)
    .await
    .unwrap();
    let a = agent("tokenless-1").await;
    assert!(a.status == "live" && a.token_address.is_none());
    let chat = auth::holder_status(a.token_address.as_deref(), &a.launchpad, a.min_holding_tokens, &owner).await.unwrap();
    assert!(chat.allowed, "anyone signed in may chat with it");
    assert!(!worker::applies("launch", &a), "the worker doesn't try to launch it");
    assert_eq!(lifecycle::try_launch(a.clone()).await.unwrap().launch_tx, None, "try_launch leaves it alone");

    let tax = TokenTax { fee_rate: 3, recipient_rate: 100, burn_rate: 0, divide_rate: 0, liquidity_rate: 0, min_sharing: 100_000 };
    let requested = lifecycle::launch_token_later(&a, "flap", Some(&tax)).await.unwrap();
    assert!(requested.status == "live" && requested.launchpad == "flap" && requested.token_address.is_none(), "the agent stays live: {}", requested.status);
    assert!(worker::applies("launch", &requested), "the worker now waits to launch it");
    let funding = lifecycle::launch_funding(&requested).await.unwrap();
    assert!(funding.launch_cost.is_zero() && !funding.ready, "no Flap launch fee; it waits for gas money ({} wei needed)", funding.required);
    let chat = auth::holder_status(None, &requested.launchpad, requested.min_holding_tokens, &owner).await.unwrap();
    assert!(!chat.allowed && chat.reason.as_deref() == Some("token not launched yet"), "holder chat closes until the token exists");

    sqlx::query("update agents set launch_tx = 'pending' where id = 'tokenless-1'").execute(db).await.unwrap();
    assert!(!worker::applies("launch", &agent("tokenless-1").await), "a launch in progress isn't picked up twice");
    sqlx::query("update agents set launch_tx = null, token_address = '0x99d9b7d2c826fe1bdb0cdb4389077ea350527777' where id = 'tokenless-1'").execute(db).await.unwrap();
    let e = lifecycle::launch_token_later(&agent("tokenless-1").await, "fourmeme", None).await.unwrap_err();
    assert_eq!(e.status(), 409, "a second token is refused: {}", e.message);
}

#[tokio::test]
async fn self_hosted_agents_connect() {
    setup();
    let db = support::shared_db().await;
    let (owner, other) = ("0x1111111111111111111111111111111111111111", "0x2222222222222222222222222222222222222222");
    let key = PrivateKeySigner::random();
    let impostor = PrivateKeySigner::random();

    let mut forged = signed(&impostor, owner, "Home Helper", &now_iso());
    forged.wallet = key.address().to_checksum(None);
    assert_eq!(ext::register_external_agent(owner, &forged).await.unwrap_err().status(), 403, "signed by a different key");
    assert_eq!(ext::register_external_agent(other, &signed(&key, owner, "Home Helper", &now_iso())).await.unwrap_err().status(), 403, "signed for a different owner");
    let old = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    assert!(ext::register_external_agent(owner, &signed(&key, owner, "Home Helper", &old)).await.is_err(), "an old signature");

    let first = ext::register_external_agent(owner, &signed(&key, owner, "Home Helper", &now_iso())).await.unwrap();
    let first_key = first["key"].as_str().unwrap().to_string();
    let id = first["agentId"].as_str().unwrap().to_string();
    assert!(first["handle"].is_string() && first_key.starts_with("orva_"));
    let row = agent(&id).await;
    assert!(row.status == "live" && row.hosting_status == "external" && row.evm_wallet["custody"] == "external", "live, external, no key held");
    assert!(orvyn_core::wallets::agent_wallet(&row.id, &row.evm_wallet).is_err(), "Orvyn can't sign for it");
    assert!(!worker::applies("billing", &row), "Orvyn doesn't bill it");
    assert!(!worker::applies("hires", &row), "Orvyn doesn't hire it out");

    let entries = [json!({ "kind": "trade", "message": "Bought 0.01 BNB of TOKEN" }), json!({ "kind": "mail", "message": "not a public kind" }), json!({ "kind": "payment", "message": "Paid 2 USDT to helper.ai" })];
    assert_eq!(ext::post_activity(&first_key, &entries).await.unwrap()["posted"], 2, "activity posts only take public kinds");

    assert_eq!(ext::register_external_agent(other, &signed(&key, other, "Home Helper", &now_iso())).await.unwrap_err().status(), 409, "another owner can't claim the same wallet");
    let again = ext::register_external_agent(owner, &signed(&key, owner, "Home Helper 2", &now_iso())).await.unwrap();
    assert_eq!(again["agentId"], first["agentId"], "reconnecting keeps the agent");
    assert_eq!(agent(&id).await.name, "Home Helper 2", "and renames it");
    let trade = [json!({ "kind": "trade", "message": "x" })];
    assert_eq!(ext::post_activity(&first_key, &trade).await.unwrap_err().status(), 401, "the old key stops working");
    let new_key = again["key"].as_str().unwrap();
    assert_eq!(ext::post_activity(new_key, &[json!({ "kind": "think", "message": "hello" })]).await.unwrap()["posted"], 1);

    assert_eq!(ext::revoke_external_agent_keys(other, &id).await.unwrap_err().status(), 404, "someone else can't disconnect it");
    ext::revoke_external_agent_keys(owner, &id).await.unwrap();
    assert_eq!(ext::post_activity(new_key, &trade).await.unwrap_err().status(), 401, "after disconnecting, its key is refused");

    // A wallet Orvyn hosts can't be taken over by registering it as self-hosted.
    let hosted = PrivateKeySigner::random();
    sqlx::query("insert into agents (id, name, symbol, owner_account_id, ai_provider, ai_model, ai_key_encrypted, ai_key_hint, evm_address, evm_wallet, status) values ('hosted', 'H', 'H', $1, 'x', 'x', 'x', 'x', $2, '{}', 'live')")
        .bind(owner)
        .bind(format!("{:#x}", hosted.address()))
        .execute(db)
        .await
        .unwrap();
    assert_eq!(ext::register_external_agent(owner, &signed(&hosted, owner, "Takeover", &now_iso())).await.unwrap_err().status(), 409, "a hosted agent's wallet can't be registered");
}

#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<Value>>>);

async fn hello(State(r): State<Recorder>, Json(body): Json<Value>) -> impl axum::response::IntoResponse {
    r.0.lock().unwrap().push(body);
    let events = [
        json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "Hi!" } }),
        json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" }, "usage": { "output_tokens": 1 } }),
    ];
    let sse: String = events.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap())).collect();
    ([("content-type", "text/event-stream")], sse)
}

#[tokio::test]
async fn vyn_as_an_agent() {
    setup();
    let db = support::shared_db().await;
    let me = "0x3333333333333333333333333333333333333333";
    let with = |mut i: RegisterInput, handle: Option<&str>| {
        i.symbol = Some("VYN".into());
        i.handle = handle.map(String::from);
        i
    };
    assert_eq!(companion::companion_view(companion::companion_of(me).await.unwrap().as_ref())["connected"], false, "not connected at first");

    let phone = PrivateKeySigner::random();
    let e = companion::connect_companion(me, &with(signed(&phone, "0x4444444444444444444444444444444444444444", "Vyn", &now_iso()), None)).await.unwrap_err();
    assert_eq!(e.status(), 403, "a signature for someone else is refused");

    let laptop = PrivateKeySigner::random();
    for (handle, why) in [("vyn", "reserved"), ("Bad Handle!", "letters")] {
        let e = companion::connect_companion(me, &with(signed(&laptop, me, "Vyn", &now_iso()), Some(handle))).await.unwrap_err();
        assert!(e.message.contains(why), "the handle {handle:?} is refused: {}", e.message);
    }
    assert!(companion::companion_of(me).await.unwrap().is_none(), "a refused handle leaves her unconnected");
    let r = companion::connect_companion(me, &with(signed(&laptop, me, "Vyn", &now_iso()), Some("vyn-me"))).await.unwrap();
    assert!(r["key"].as_str().unwrap().starts_with("orva_") && r["handle"] == "vyn-me", "she connects under the handle chosen: {r}");
    let her_id = r["agentId"].as_str().unwrap().to_string();
    let her = companion::companion_of(me).await.unwrap().unwrap();
    assert!(her.companion && her.hosting_status == "external" && her.evm_wallet["custody"] == "external");
    assert!(her.description.to_lowercase().contains("companion"), "her description says what she is");
    let again = companion::connect_companion(me, &with(signed(&laptop, me, "Vyn", &now_iso()), None)).await.unwrap();
    assert!(again["agentId"] == r["agentId"] && again["handle"] == "vyn-me", "reconnecting from the same computer keeps her");
    let e = companion::connect_companion(me, &with(signed(&phone, me, "Vyn", &now_iso()), None)).await.unwrap_err();
    assert_eq!(e.status(), 409, "one Vyn per person: {}", e.message);

    // The person's other agents.
    for (handle, name, wallet, hosting, active) in [("lumen", "Lumen Research", "0x6666666666666666666666666666666666666666", "none", false), ("forge", "Pixel Forge", "0x7777777777777777777777777777777777777777", "external", true)] {
        sqlx::query(
            "insert into agents (id, handle, name, symbol, owner_account_id, ai_provider, ai_model, ai_key_encrypted, ai_key_hint, evm_address, evm_wallet, launchpad, status, hosting_status, subscription_active)
             values ($1, $2, $3, 'X', $4, 'anthropic', 'm', '', '', $5, $6, 'none', 'live', $7, $8)",
        )
        .bind(lifecycle::uuid_v4())
        .bind(handle)
        .bind(name)
        .bind(me)
        .bind(wallet)
        .bind(json!({ "custody": "external", "accountAddress": wallet }))
        .bind(hosting)
        .bind(active)
        .execute(db)
        .await
        .unwrap();
    }
    let mine = companion::my_agents(me).await.unwrap();
    let names: Vec<&str> = mine["agents"].as_array().unwrap().iter().map(|a| a["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["Lumen Research", "Pixel Forge"], "my_agents lists the agents, not Vyn herself");
    assert_eq!(companion::find_agent(me, "@lumen").await.unwrap().map(|a| a.name), Some("Lumen Research".into()));
    assert_eq!(companion::find_agent(me, "pixel").await.unwrap().and_then(|a| a.handle), Some("forge".into()), "found by name prefix");
    assert!(companion::find_agent(me, &her_id).await.unwrap().is_none(), "Vyn isn't one of the agents she asks");
    assert!(companion::find_agent("0x5555555555555555555555555555555555555555", "lumen").await.unwrap().is_none(), "someone else's agent isn't found");
    let unknown = companion::ask_agent(me, "nobody", "hi").await.unwrap();
    assert!(unknown["error"].as_str().unwrap().contains("No agent"));
    let paused = companion::ask_agent(me, "lumen", "what did you do today?").await.unwrap();
    assert!(paused["error"].is_string() && paused["agent"] == "Lumen Research", "an agent that isn't serving says why: {paused}");
    let own_server = companion::ask_agent(me, "forge", "hi").await.unwrap();
    assert!(own_server["error"].as_str().unwrap().contains("self-hosted"));

    // Her prompt knows who she is and has the agent tools.
    let rec = Recorder::default();
    let url = support::serve_forever(Router::new().route("/v1/messages", post(hello)).with_state(rec.clone()));
    unsafe { std::env::set_var("ANTHROPIC_BASE_URL", &url) };
    let key = orvyn_core::secrets::encrypt_secret("sk-test", &format!("vyn:{me}"), orvyn_core::secrets::SecretKind::AiKey).unwrap();
    sqlx::query("insert into vyn_settings (account_id, ai_provider, ai_model, ai_key_encrypted, ai_key_hint, persona) values ($1, 'anthropic', 'm', $2, '', '{}')").bind(me).bind(key).execute(db).await.unwrap();
    let thread = vyn::create_thread(me, None).await.unwrap();
    let input = vyn::TurnInput { text: Some("@lumen how's it going?".into()), tool_results: vec![], tools: vec![], device: "desktop".into() };
    vyn::run_turn(me, thread["id"].as_str().unwrap(), input, None).await.unwrap();
    let request = rec.0.lock().unwrap()[0].clone();
    assert!(request["system"].to_string().contains("@vyn-me"), "her prompt names her Orvyn handle");
    let tools: Vec<&str> = request["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(tools.contains(&"my_agents") && tools.contains(&"ask_agent"), "{tools:?}");

    companion::disconnect_companion(me).await.unwrap();
    assert!(companion::companion_of(me).await.unwrap().is_none(), "after disconnecting she's not the companion");
    assert!(ext::agent_for_key(r["key"].as_str().unwrap()).await.is_err(), "her old key is refused");
    let e = companion::connect_companion(me, &with(signed(&phone, me, "Vyn", &now_iso()), Some("vyn-me"))).await.unwrap_err();
    assert!(e.message.contains("taken"), "a handle that's taken is refused: {}", e.message);
    let on_phone = companion::connect_companion(me, &with(signed(&phone, me, "Vyn", &now_iso()), Some("vyn-me-phone"))).await.unwrap();
    assert_ne!(on_phone["agentId"], r["agentId"], "then she can connect from another device");
    assert!(!agent(&her_id).await.companion, "the old page stays");
}
