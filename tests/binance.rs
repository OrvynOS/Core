//! Binance MCP (agentic) on a throwaway database, against a stand-in Binance (OAuth metadata, a token
//! endpoint that checks PKCE, the MCP server, the public price API) and a scripted model:
//! - connecting: the consent URL carries PKCE, the resource and Orvyn's client id; the code is
//!   exchanged once; tokens are stored encrypted; only the owner connects, only with the plugin;
//! - agents: reads in chat, writes only in autopilot; every write checked against the owner's caps
//!   (per order, per day, products, symbols, leverage, transfers, unknown tools) before Binance;
//! - Vyn: reads run mid-turn; a write becomes an approval card on the device, and only the person's
//!   approval runs it, with exactly the arguments shown; devices that can't approve only read;
//! - expiry and disconnecting.
mod support;

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine as _;
use orvyn_core::ai::ToolSet;
use orvyn_core::binance;
use orvyn_core::bliss;
use orvyn_core::db::Agent;
use orvyn_core::vyn::{self, TurnInput};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

#[derive(Clone, Default)]
struct Stub {
    base: Arc<Mutex<String>>,
    /// PKCE challenge per code the stand-in consent screen issued.
    challenges: Arc<Mutex<Vec<(String, String)>>>,
    /// (tool, arguments, authorization) of each tools/call.
    calls: Arc<Mutex<Vec<(String, Value, String)>>>,
    replies: Arc<Mutex<Vec<String>>>,
    requests: Arc<Mutex<Vec<Value>>>,
}

async fn metadata(State(s): State<Stub>) -> Json<Value> {
    let base = s.base.lock().unwrap().clone();
    Json(json!({ "issuer": base, "authorization_endpoint": format!("{base}/authorize"), "token_endpoint": format!("{base}/token"), "token_endpoint_auth_methods_supported": ["none"], "code_challenge_methods_supported": ["S256"] }))
}

async fn token(State(s): State<Stub>, Form(f): Form<std::collections::HashMap<String, String>>) -> axum::response::Response {
    let code = f.get("code").cloned().unwrap_or_default();
    let challenge = s.challenges.lock().unwrap().iter().find(|(c, _)| *c == code).map(|(_, ch)| ch.clone());
    let verifier_ok = challenge.is_some_and(|ch| f.get("code_verifier").is_some_and(|v| B64URL.encode(Sha256::digest(v.as_bytes())) == ch));
    if f.get("grant_type").map(String::as_str) != Some("authorization_code") || !verifier_ok || f.get("resource").is_none() {
        return (axum::http::StatusCode::BAD_REQUEST, Json(json!({ "error": "invalid_grant", "error_description": "PKCE check failed" }))).into_response();
    }
    Json(json!({ "access_token": format!("tok-{code}"), "token_type": "Bearer", "expires_in": 3600, "scope": "market account trade" })).into_response()
}

fn tool(name: &str, description: &str, read_only: Option<bool>) -> Value {
    let mut t = json!({ "name": name, "description": description, "inputSchema": { "type": "object", "properties": {} } });
    if let Some(ro) = read_only {
        t["annotations"] = json!({ "readOnlyHint": ro });
    }
    t
}

async fn mcp(State(s): State<Stub>, headers: HeaderMap, body: String) -> axum::response::Response {
    let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    if !auth.starts_with("Bearer tok-") {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }
    let req: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let result = match req["method"].as_str().unwrap_or("") {
        "initialize" => json!({ "protocolVersion": "2025-06-18", "capabilities": { "tools": {} }, "serverInfo": { "name": "binance", "version": "1" } }),
        "tools/list" => json!({ "tools": [
            tool("get_ticker", "Latest price for a symbol.", Some(true)),
            tool("get_account_balances", "Balances of the Agentic sub-account.", None),
            tool("new_spot_order", "Place a spot order.", Some(false)),
            tool("new_futures_order", "Place a USD-M futures order.", Some(false)),
            tool("cancel_spot_order", "Cancel a spot order.", Some(false)),
            tool("set_futures_leverage", "Set leverage for a futures symbol.", Some(false)),
            tool("wallet_transfer", "Move funds between the sub-account's wallets.", Some(false)),
            tool("enable_fast_mode", "Something new the agent shouldn't touch.", None),
        ] }),
        "tools/call" => {
            let (name, args) = (req["params"]["name"].as_str().unwrap_or("").to_string(), req["params"]["arguments"].clone());
            s.calls.lock().unwrap().push((name.clone(), args.clone(), auth));
            json!({ "content": [{ "type": "text", "text": json!({ "ok": true, "tool": name, "echo": args }).to_string() }] })
        }
        _ if req["id"].is_null() => return axum::http::StatusCode::ACCEPTED.into_response(),
        _ => json!({}),
    };
    Json(json!({ "jsonrpc": "2.0", "id": req["id"], "result": result })).into_response()
}

async fn price() -> Json<Value> {
    Json(json!({ "symbol": "BTCUSDT", "price": "40000.00" }))
}

fn sse(events: &[Value]) -> String {
    events.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap_or("message"))).collect()
}

fn model_call(id: &str, name: &str, input: Value) -> String {
    sse(&[
        json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "tool_use", "id": id, "name": name, "input": {} } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": input.to_string() } }),
        json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 5 } }),
    ])
}

fn model_say(t: &str) -> String {
    sse(&[
        json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": t } }),
        json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" }, "usage": { "output_tokens": 5 } }),
    ])
}

async fn model(State(s): State<Stub>, Json(body): Json<Value>) -> impl IntoResponse {
    s.requests.lock().unwrap().push(body);
    let next = {
        let mut r = s.replies.lock().unwrap();
        if r.is_empty() { model_say("(no more script)") } else { r.remove(0) }
    };
    ([("content-type", "text/event-stream")], next)
}

/// The stand-in consent screen: the person approves, and Binance redirects back with a code.
fn consent(stub: &Stub, url: &str, code: &str) -> String {
    let u = url::Url::parse(url).unwrap();
    let q: std::collections::HashMap<String, String> = u.query_pairs().into_owned().collect();
    stub.challenges.lock().unwrap().push((code.into(), q["code_challenge"].clone()));
    q["state"].clone()
}

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

#[tokio::test]
async fn binance_for_agents_and_vyn() {
    support::test_env(&[]);
    let db = support::shared_db().await;
    let stub = Stub::default();
    let url = support::serve_forever(
        Router::new()
            .route("/.well-known/oauth-authorization-server", get(metadata))
            .route("/token", post(token))
            .route("/mcp/agentic", post(mcp))
            .route("/api/v3/ticker/price", get(price))
            .route("/v1/messages", post(model))
            .with_state(stub.clone()),
    );
    *stub.base.lock().unwrap() = url.clone();
    for (k, v) in [("BINANCE_MCP_ISSUER", url.clone()), ("BINANCE_MCP_URL", format!("{url}/mcp/agentic")), ("BINANCE_SPOT_API", url.clone()), ("BINANCE_FUTURES_API", url.clone()), ("ANTHROPIC_BASE_URL", url.clone()), ("NEXT_PUBLIC_API_URL", "https://api.orvyn.test".into()), ("ORVYN_APP_URL", "https://app.orvyn.test".into())] {
        unsafe { std::env::set_var(k, v) };
    }
    let meta = binance::client_metadata();
    assert_eq!((meta["client_id"].as_str(), meta["redirect_uris"][0].as_str(), meta["token_endpoint_auth_method"].as_str()), (Some("https://api.orvyn.test/api/binance/client.json"), Some("https://api.orvyn.test/api/binance/callback"), Some("none")));

    // ---- An agent ----
    let owner = "0x1111111111111111111111111111111111111111";
    sqlx::query("insert into agents (id, handle, name, symbol, owner_account_id, ai_provider, ai_model, ai_key_encrypted, ai_key_hint, evm_address, evm_wallet, status) values ('trader-1', 'trader1', 'T', 'T', $1, 'x', 'x', 'x', 'x', '0x00000000000000000000000000000000000000b1', '{}', 'live')")
        .bind(owner)
        .execute(db)
        .await
        .unwrap();
    let agent: Agent = sqlx::query_as("select * from agents where id = 'trader-1'").fetch_one(db).await.unwrap();
    assert!(binance::start_connect(owner, Some(&agent), None).await.unwrap_err().message.contains("install the Binance plugin"), "the plugin comes first");
    let caps = json!({ "maxOrderUsd": 50, "maxDailyUsd": 80, "products": ["spot"], "maxLeverage": 2, "symbols": ["BTCUSDT", "ETHUSDT"], "allowTransfers": false });
    bliss::install_plugin(&agent, "binance", &caps).await.unwrap();
    assert_eq!(binance::start_connect("0x2222222222222222222222222222222222222222", Some(&agent), None).await.unwrap_err().status(), 403, "only the owner connects it");

    let consent_url = binance::start_connect(owner, Some(&agent), None).await.unwrap();
    let q: std::collections::HashMap<String, String> = url::Url::parse(&consent_url).unwrap().query_pairs().into_owned().collect();
    assert!(consent_url.starts_with(&format!("{url}/authorize?")) && q["response_type"] == "code" && q["code_challenge_method"] == "S256" && q["client_id"] == meta["client_id"] && q["resource"] == format!("{url}/mcp/agentic") && q["redirect_uri"] == "https://api.orvyn.test/api/binance/callback", "{consent_url}");
    let state = consent(&stub, &consent_url, "agent");
    assert_eq!(binance::finish_connect(&state, "agent").await.unwrap(), "https://app.orvyn.test/agents/trader-1?binance=connected");
    assert!(binance::finish_connect(&state, "agent").await.is_err(), "a sign-in is used once");
    let stored: String = sqlx::query_scalar("select access_token_encrypted from binance_connections where agent_id = 'trader-1'").fetch_one(db).await.unwrap();
    assert!(!stored.contains("tok-agent") && stored.starts_with("v1:"), "the token is stored encrypted");
    // A wrong verifier (someone else's code) is refused by Binance and nothing is stored for it.
    let other = binance::start_connect(owner, Some(&agent), None).await.unwrap();
    let other_state = url::Url::parse(&other).unwrap().query_pairs().find(|(k, _)| k == "state").unwrap().1.to_string();
    stub.challenges.lock().unwrap().push(("forged".into(), "not-the-challenge".into()));
    assert!(binance::finish_connect(&other_state, "forged").await.unwrap_err().message.contains("PKCE"));

    let arc = Arc::new(agent.clone());
    let tools_for = |surface: &'static str| {
        let arc = arc.clone();
        async move { bliss::compose_tools(&arc, &Arc::new(bliss::installed_plugins("trader-1", false).await.unwrap()), surface).unwrap() }
    };
    let (chat, auto) = (tools_for("chat").await, tools_for("autopilot").await);
    assert!(names(&chat).contains(&"binance_get_ticker".into()) && names(&chat).contains(&"binance_get_account_balances".into()), "reads (marked, or named as reads) in chat: {:?}", names(&chat));
    assert!(!names(&chat).contains(&"binance_new_spot_order".into()) && names(&auto).contains(&"binance_new_spot_order".into()), "writes only in autopilot");
    let r = run(&chat, "binance_get_ticker", json!({ "symbol": "BTCUSDT" })).await;
    assert!(r.contains("\"tool\":\"get_ticker\"") && stub.calls.lock().unwrap().last().unwrap().2 == "Bearer tok-agent", "a read goes to Binance with the agent's token: {r}");

    let n = || stub.calls.lock().unwrap().len();
    let refused = |out: &str| out.contains("\"refused\"");
    let before = n();
    let order = |args: Value| {
        let auto = auto.clone();
        async move { run(&auto, "binance_new_spot_order", args).await }
    };
    assert!(!refused(&order(json!({ "symbol": "BTCUSDT", "side": "BUY", "type": "MARKET", "quoteOrderQty": 30 })).await), "a $30 spot buy goes through");
    assert!(refused(&order(json!({ "symbol": "BTCUSDT", "side": "BUY", "type": "MARKET", "quoteOrderQty": 60 })).await), "over the per-order limit");
    assert!(refused(&order(json!({ "symbol": "DOGEUSDT", "side": "BUY", "type": "MARKET", "quoteOrderQty": 10 })).await), "a symbol not on the list");
    assert!(refused(&order(json!({ "symbol": "BTCUSDT", "side": "BUY", "type": "MARKET" })).await), "an order Orvyn can't size");
    // 0.001 BTC at the market ($40): $70 in the day, under $80.
    assert!(!refused(&order(json!({ "symbol": "BTCUSDT", "side": "BUY", "type": "MARKET", "quantity": "0.001" })).await), "sized from the market price");
    assert!(refused(&order(json!({ "symbol": "ETHUSDT", "side": "BUY", "type": "MARKET", "quoteOrderQty": 20 })).await), "over the daily limit ($70 + $20 > $80)");
    assert!(refused(&run(&auto, "binance_new_futures_order", json!({ "symbol": "BTCUSDT", "side": "BUY", "quoteOrderQty": 5 })).await), "futures aren't allowed");
    assert!(refused(&run(&auto, "binance_set_futures_leverage", json!({ "symbol": "BTCUSDT", "leverage": 5 })).await), "leverage over the limit");
    assert!(refused(&run(&auto, "binance_wallet_transfer", json!({ "asset": "USDT", "amount": 10 })).await), "transfers need the owner's yes");
    assert!(refused(&run(&auto, "binance_enable_fast_mode", json!({})).await), "a write Orvyn doesn't know is refused");
    assert!(!refused(&run(&auto, "binance_cancel_spot_order", json!({ "symbol": "BTCUSDT", "orderId": 1 })).await), "cancelling is always allowed");
    let sent: Vec<String> = stub.calls.lock().unwrap()[before..].iter().map(|c| c.0.clone()).collect();
    assert_eq!(sent, ["new_spot_order", "new_spot_order", "cancel_spot_order"], "only the allowed calls reached Binance");

    // ---- Vyn ----
    let me = "0x3333333333333333333333333333333333333333";
    let key = orvyn_core::secrets::encrypt_secret("sk-test", &format!("vyn:{me}"), orvyn_core::secrets::SecretKind::AiKey).unwrap();
    sqlx::query("insert into vyn_settings (account_id, ai_provider, ai_model, ai_key_encrypted, ai_key_hint, persona) values ($1, 'anthropic', 'm', $2, '', '{}')").bind(me).bind(key).execute(db).await.unwrap();
    let consent_url = binance::start_connect(me, None, None).await.unwrap();
    let state = consent(&stub, &consent_url, "vyn");
    assert_eq!(binance::finish_connect(&state, "vyn").await.unwrap(), "https://app.orvyn.test/vyn?binance=connected");
    assert_eq!(binance::connection_view(binance::connection(me, None).await.unwrap().as_ref())["connected"], true);

    let thread = vyn::create_thread(me, None).await.unwrap()["id"].as_str().unwrap().to_string();
    let desktop = |text: Option<&str>, results: Vec<vyn::ToolResult>| TurnInput { text: text.map(String::from), tool_results: results, tools: vec!["ask_person".into(), vyn::APPROVE_BINANCE.into()], device: "desktop".into() };
    let buy = json!({ "symbol": "ETHUSDT", "side": "BUY", "type": "MARKET", "quoteOrderQty": 25 });
    *stub.replies.lock().unwrap() = vec![model_call("t1", "binance_get_account_balances", json!({})), model_call("t2", "binance_new_spot_order", buy.clone()), model_say("Bought $25 of ETH.")];
    let before = n();
    let out = vyn::run_turn(me, &thread, desktop(Some("Buy $25 of ETH"), vec![]), None).await.unwrap();
    assert_eq!(stub.calls.lock().unwrap()[before].0, "get_account_balances", "a read runs mid-turn");
    assert_eq!(n(), before + 1, "the order hasn't been sent");
    assert_eq!(out.tool_calls.len(), 1);
    let card = &out.tool_calls[0];
    assert!(card.tool_name == vyn::APPROVE_BINANCE && card.input["tool"] == "new_spot_order" && card.input["arguments"] == buy && card.input["summary"].as_str().unwrap().contains("quoteOrderQty: 25"), "the device gets an approval card: {:?}", card.input);
    // The device can only answer yes or no: arguments it sends back are ignored.
    let yes = vyn::ToolResult { tool_call_id: card.tool_call_id.clone(), tool_name: vyn::APPROVE_BINANCE.into(), output: json!({ "approved": true, "arguments": { "quoteOrderQty": 5000 } }) };
    let done = vyn::run_turn(me, &thread, desktop(None, vec![yes]), None).await.unwrap();
    let (tool, args, auth) = stub.calls.lock().unwrap().last().cloned().unwrap();
    assert!(tool == "new_spot_order" && args == buy && auth == "Bearer tok-vyn", "approved: exactly the call shown runs, with Vyn's token");
    assert!(done.done && done.text.contains("Bought"));

    *stub.replies.lock().unwrap() = vec![model_call("t3", "binance_new_spot_order", buy.clone()), model_say("Okay, I won't.")];
    let out = vyn::run_turn(me, &thread, desktop(Some("Buy another $25"), vec![]), None).await.unwrap();
    let before = n();
    let no = vyn::ToolResult { tool_call_id: out.tool_calls[0].tool_call_id.clone(), tool_name: vyn::APPROVE_BINANCE.into(), output: json!({ "approved": false }) };
    vyn::run_turn(me, &thread, desktop(None, vec![no]), None).await.unwrap();
    assert_eq!(n(), before, "declined: nothing reaches Binance");

    // A device that can't show the card only gets the reads.
    *stub.replies.lock().unwrap() = vec![model_say("Here's your balance.")];
    let requests_before = stub.requests.lock().unwrap().len();
    vyn::run_turn(me, &thread, TurnInput { text: Some("What do I hold?".into()), tool_results: vec![], tools: vec![], device: "scheduler".into() }, None).await.unwrap();
    let request = stub.requests.lock().unwrap()[requests_before].clone();
    let offered: Vec<&str> = request["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).filter(|n| n.starts_with("binance_")).collect();
    assert!(offered.contains(&"binance_get_ticker") && !offered.contains(&"binance_new_spot_order"), "reads only: {offered:?}");

    // ---- Expiry and disconnecting ----
    sqlx::query("update binance_connections set expires_at = now() - interval '1 second' where agent_id = 'trader-1'").execute(db).await.unwrap();
    assert!(run(&tools_for("chat").await, "binance_get_ticker", json!({})).await.contains("reconnect"), "expired access says to reconnect");
    assert!(binance::disconnect(owner, Some("trader-1")).await.unwrap());
    assert!(names(&tools_for("autopilot").await).iter().all(|n| !n.starts_with("binance_")), "after disconnecting the agent has no Binance tools");
    assert!(binance::connection(owner, Some("trader-1")).await.unwrap().is_none() && binance::connection(me, None).await.unwrap().is_some(), "Vyn's connection is separate");
}
