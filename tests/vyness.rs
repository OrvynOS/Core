//! Vyness against a mock model host, on a throwaway database: a key per computer (signing in again
//! on the same computer replaces it), Anthropic Messages requests (plain and streamed) charged from
//! Orvyn credits under "vyness", and nothing about the upstream reaching the coding agent.
mod support;

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use orvyn_core::{Kind, credits, inference};
use serde_json::{Value, json};

const ME: &str = "0x00000000000000000000000000000000000000ab";

#[derive(Clone, Default)]
struct Mock {
    requests: Arc<Mutex<Vec<(Value, Option<String>)>>>,
}

async fn models() -> impl IntoResponse {
    Json(json!({ "data": [
        { "id": "anthropic/claude-sonnet-5.5", "name": "Anthropic: Claude Sonnet 5.5", "context_length": 1000000, "pricing": { "prompt": "0.000002", "completion": "0.00001" }, "supported_parameters": ["tools"] },
        { "id": "qwen/qwen4-max", "name": "Qwen: Qwen4 Max", "context_length": 256000, "pricing": { "prompt": "0.000001", "completion": "0.000004" } },
        { "id": "z-ai/glm-5.3", "name": "Z.ai: GLM 5.3", "context_length": 200000, "pricing": { "prompt": "0.000001", "completion": "0.000003" } },
    ] }))
}

async fn messages(State(m): State<Mock>, headers: axum::http::HeaderMap, Json(body): Json<Value>) -> impl IntoResponse {
    let beta = headers.get("anthropic-beta").and_then(|v| v.to_str().ok()).map(String::from);
    m.requests.lock().unwrap().push((body.clone(), beta));
    if body["model"] == "qwen/qwen4-max" {
        return (axum::http::StatusCode::BAD_REQUEST, Json(json!({ "type": "error", "error": { "type": "invalid_request_error", "message": "0 endpoints out of 1 requested are available matching your guardrail restrictions and data policy" } }))).into_response();
    }
    let usage = json!({ "input_tokens": 80, "cache_read_input_tokens": 20, "output_tokens": 50, "cost": 0.0007, "cost_details": { "upstream": 0.0007 }, "is_byok": false });
    if body["stream"] == true {
        let events = [
            ("message_start", json!({ "type": "message_start", "provider": "Anthropic", "message": { "provider": "Claude Platform on AWS", "id": "msg_1", "role": "assistant", "content": [], "usage": { "input_tokens": 80, "cache_read_input_tokens": 20, "output_tokens": 1 } } })),
            ("content_block_start", json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } })),
            ("content_block_delta", json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "Hel" } })),
            ("content_block_delta", json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "lo!" } })),
            ("message_delta", json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" }, "usage": usage })),
            ("message_stop", json!({ "type": "message_stop" })),
        ];
        let sse: String = events.iter().map(|(k, v)| format!("event: {k}\ndata: {v}\n\n")).collect();
        return ([("content-type", "text/event-stream")], sse).into_response();
    }
    Json(json!({ "id": "msg_2", "type": "message", "role": "assistant", "provider": "Anthropic", "model": body["model"], "content": [{ "type": "text", "text": "Hi there." }], "stop_reason": "end_turn", "usage": usage })).into_response()
}

#[tokio::test]
async fn vyness_keys_and_messages() {
    let mock = Mock::default();
    let base = support::serve_forever(Router::new().route("/models", get(models)).route("/messages", post(messages)).with_state(mock.clone()));
    unsafe { std::env::set_var("ORVYN_INFERENCE_UPSTREAM", &base) };
    support::test_env(&[("OPENROUTER_API_KEY", "sk-or-test")]);
    let db = support::shared_db().await;

    // A key per computer: signing in again on the same one replaces it.
    let first = credits::create_vyness_key(ME, "studio-pc").await.unwrap();
    let second = credits::create_vyness_key(ME, "studio-pc").await.unwrap();
    let laptop = credits::create_vyness_key(ME, "laptop").await.unwrap();
    assert!(second.starts_with("orv_sk_"));
    assert_eq!(credits::key_owner(&first).await.unwrap(), None, "replaced");
    assert_eq!(credits::key_owner(&second).await.unwrap(), Some((ME.to_string(), Some("vyness".to_string()))));
    assert!(credits::key_owner(&laptop).await.unwrap().is_some());
    let names: Vec<String> = sqlx::query_scalar("select name from inference_keys where account_id = $1 and revoked_at is null order by name").bind(ME).fetch_all(db).await.unwrap();
    assert_eq!(names, ["Vyness · laptop", "Vyness · studio-pc"]);

    let ask = |stream: bool| json!({ "model": "anthropic/claude-sonnet-5.5[1m]", "max_tokens": 64000, "stream": stream, "messages": [{ "role": "user", "content": "hi" }], "provider": { "order": ["x"] }, "metadata": { "user_id": ME } });

    // No credit: refused before anything goes upstream.
    let e = inference::messages(ME, "vyness", ask(false), None, None).await.unwrap_err();
    assert_eq!(e.kind, Kind::PaymentRequired);
    assert!(mock.requests.lock().unwrap().is_empty());

    // A plain request: charged at cost x 1.2 under "vyness", with the upstream scrubbed out.
    credits::adjust(ME, 1.0).await.unwrap();
    let (reply, usage) = inference::messages(ME, "vyness", ask(false), Some("prompt-caching-2024-07-31"), None).await.unwrap();
    let reply = reply.unwrap();
    assert_eq!(reply["content"][0]["text"], "Hi there.");
    assert!(reply.get("provider").is_none());
    assert_eq!(reply["usage"], json!({ "input_tokens": 80, "cache_read_input_tokens": 20, "output_tokens": 50 }));
    assert_eq!((usage.input_tokens, usage.output_tokens, usage.micro), (100, 50, 840));
    let (sent, beta) = mock.requests.lock().unwrap().last().cloned().unwrap();
    assert_eq!(sent["model"], "anthropic/claude-sonnet-5.5", "the [1m] hint is dropped");
    assert!(sent.get("provider").is_none(), "routing options are dropped: {sent}");
    assert_ne!(sent["metadata"]["user_id"], ME, "the account never goes upstream");
    assert_eq!(beta.as_deref(), Some("prompt-caching-2024-07-31"));

    // Streamed: every event reaches the agent, none mention the upstream, and it's charged once.
    let seen = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
    let s2 = seen.clone();
    let on_event = move |k: &str, v: &Value| s2.lock().unwrap().push((k.to_string(), v.clone()));
    let (none, usage) = inference::messages(ME, "vyness", ask(true), None, Some(&on_event)).await.unwrap();
    assert!(none.is_none());
    assert_eq!((usage.text.as_str(), usage.micro), ("Hello!", 840));
    let seen = seen.lock().unwrap();
    assert_eq!(seen.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), ["message_start", "content_block_start", "content_block_delta", "content_block_delta", "message_delta", "message_stop"]);
    let all = serde_json::to_string(&*seen).unwrap();
    assert!(!all.contains("cost") && !all.contains("provider") && !all.contains("is_byok"), "{all}");

    assert_eq!(credits::balance(ME).await.unwrap(), 1_000_000 - 2 * 840);
    let spent: i64 = sqlx::query_scalar("select coalesce(sum(micro_usd), 0)::bigint from credit_ledger where account_id = $1 and source = 'vyness'").bind(ME).fetch_one(db).await.unwrap();
    assert_eq!(spent, -2 * 840);

    // A model blocked on Orvyn's upstream account: a clear message, and it's hidden everywhere.
    let listed = |v: &Value, id: &str| v["data"].as_array().unwrap().iter().any(|m| m["id"] == id);
    assert!(listed(&inference::openai_models().await.unwrap(), "qwen/qwen4-max"));
    let blocked = json!({ "model": "qwen/qwen4-max", "max_tokens": 10, "messages": [{ "role": "user", "content": "hi" }] });
    let e = inference::messages(ME, "vyness", blocked.clone(), None, None).await.unwrap_err();
    assert_eq!(e.kind, Kind::Invalid);
    assert!(e.message.contains("isn't available on Orvyn AI") && !e.message.contains("guardrail"), "{}", e.message);
    assert!(!listed(&inference::openai_models().await.unwrap(), "qwen/qwen4-max"));
    assert!(inference::models_view().await.unwrap()["models"].as_array().unwrap().iter().all(|m| m["id"] != "qwen/qwen4-max"));
    let before = mock.requests.lock().unwrap().len();
    assert_eq!(inference::messages(ME, "vyness", blocked, None, None).await.unwrap_err().kind, Kind::Invalid);
    assert_eq!(mock.requests.lock().unwrap().len(), before, "a hidden model doesn't go upstream again");
    // And by hand: ORVYN_AI_HIDDEN_MODELS.
    unsafe { std::env::set_var("ORVYN_AI_HIDDEN_MODELS", "z-ai/glm-5.3") };
    assert!(!listed(&inference::openai_models().await.unwrap(), "z-ai/glm-5.3"));
    unsafe { std::env::remove_var("ORVYN_AI_HIDDEN_MODELS") };
    assert!(listed(&inference::openai_models().await.unwrap(), "z-ai/glm-5.3"));

    // Models Orvyn AI doesn't serve are refused.
    let bad = json!({ "model": "someone/else", "max_tokens": 10, "messages": [{ "role": "user", "content": "hi" }] });
    assert_eq!(inference::messages(ME, "vyness", bad, None, None).await.unwrap_err().kind, Kind::Invalid);
}
