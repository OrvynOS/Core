//! Orvyn inference against a mock model host, on a throwaway database: credit is required, the
//! reply is capped to what the balance covers, the upstream cost times the markup is charged, the
//! upstream's name and routing never reach the client, chat turns are saved and charged, and API
//! keys work until revoked.
mod support;

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use orvyn_core::{credits, inference, Kind};
use serde_json::{json, Value};

const ME: &str = "0x00000000000000000000000000000000000000aa";

#[derive(Clone, Default)]
struct Mock {
    requests: Arc<Mutex<Vec<Value>>>,
}

async fn models() -> impl IntoResponse {
    Json(json!({ "data": [
        { "id": "z-ai/glm-5.3", "name": "Z.ai: GLM 5.3", "context_length": 1000000, "pricing": { "prompt": "0.000002", "completion": "0.00001" }, "supported_parameters": ["tools"] },
        { "id": "openai/gpt-oss-120b", "name": "OpenAI: gpt-oss-120b", "context_length": 500000, "pricing": { "prompt": "0.000002", "completion": "0.000006" }, "supported_parameters": ["tools"] },
        { "id": "someone/uncurated", "name": "Someone: Uncurated", "pricing": { "prompt": "0", "completion": "0" } },
    ] }))
}


/// The upstream's Zero Data Retention endpoint list.
async fn zdr() -> impl IntoResponse {
    Json(json!({ "data": [
        { "model_id": "z-ai/glm-5.3", "provider_name": "Amazon Bedrock", "tag": "amazon-bedrock", "safety_retention": false },
        { "model_id": "openai/gpt-oss-120b", "provider_name": "xAI", "tag": "fireworks", "safety_retention": false },
        { "model_id": "someone/uncurated", "provider_name": "X", "tag": "x", "safety_retention": false },
    ] }))
}

async fn chat(State(m): State<Mock>, Json(body): Json<Value>) -> impl IntoResponse {
    m.requests.lock().unwrap().push(body.clone());
    if body["model"] == "openai/gpt-oss-120b" {
        return (axum::http::StatusCode::PAYMENT_REQUIRED, Json(json!({ "error": { "message": "OpenRouter: insufficient credits on openrouter.ai" } }))).into_response();
    }
    let usage = json!({ "prompt_tokens": 100, "completion_tokens": 50, "total_tokens": 150, "cost": 0.0007, "cost_details": { "upstream": 0.0007 }, "is_byok": false });
    if body["stream"] == true {
        let chunks = [
            json!({ "id": "gen-1", "provider": "Anthropic", "choices": [{ "index": 0, "delta": { "role": "assistant", "content": "Hel" } }] }),
            json!({ "id": "gen-1", "provider": "Anthropic", "choices": [{ "index": 0, "delta": { "content": "lo!" }, "finish_reason": "stop" }] }),
            json!({ "id": "gen-1", "provider": "Anthropic", "choices": [], "usage": usage }),
        ];
        let mut sse: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
        sse.push_str("data: [DONE]\n\n");
        return ([("content-type", "text/event-stream")], sse).into_response();
    }
    Json(json!({ "id": "gen-2", "provider": "Anthropic", "model": body["model"], "choices": [{ "index": 0, "message": { "role": "assistant", "content": "Hi there." }, "finish_reason": "stop" }], "usage": usage })).into_response()
}

#[tokio::test]
async fn gateway_credits_chat_and_keys() {
    let mock = Mock::default();
    let base = support::serve_forever(Router::new().route("/models", get(models)).route("/endpoints/zdr", get(zdr)).route("/chat/completions", post(chat)).with_state(mock.clone()));
    unsafe { std::env::set_var("ORVYN_INFERENCE_UPSTREAM", &base) };
    support::test_env(&[("OPENROUTER_API_KEY", "sk-or-test"), ("ORVYN_CREDITS_ADDRESS", "0x00000000000000000000000000000000000000cc")]);
    support::shared_db().await;

    // The catalog: Orvyn Model first (coming soon), curated models only, prices with the 20% markup.
    let view = inference::models_view().await.unwrap();
    let list = view["models"].as_array().unwrap();
    assert_eq!(list[0]["id"], inference::ORVYN_MODEL);
    assert_eq!(list[0]["soon"], true);
    assert!(list.iter().all(|m| m["id"] != "someone/uncurated"));
    let sonnet = list.iter().find(|m| m["id"] == "z-ai/glm-5.3").unwrap();
    assert_eq!(sonnet["name"], "GLM 5.3");
    assert_eq!(sonnet["maker"], "Z.ai");
    assert_eq!(sonnet["inputPerMillion"], 2.4);
    assert_eq!(sonnet["outputPerMillion"], 12.0);
    assert!(!serde_json::to_string(&view).unwrap().to_lowercase().contains("openrouter"));

    let ask = |model: &str| json!({ "model": model, "messages": [{ "role": "user", "content": "hi" }], "max_tokens": 100000, "provider": { "order": ["x"] }, "models": ["a", "b"] });

    // No credit: refused before anything goes upstream.
    let e = inference::complete(ME, "api", ask("z-ai/glm-5.3"), None).await.unwrap_err();
    assert_eq!(e.kind, Kind::PaymentRequired);
    assert!(mock.requests.lock().unwrap().is_empty());

    // $0.50 of credit: the reply is capped to what that buys, then charged at cost x 1.2.
    assert_eq!(credits::adjust(ME, 0.5).await.unwrap(), 500_000);
    let (reply, usage) = inference::complete(ME, "api", ask("z-ai/glm-5.3"), None).await.unwrap();
    let reply = reply.unwrap();
    assert_eq!(reply["choices"][0]["message"]["content"], "Hi there.");
    assert!(reply.get("provider").is_none());
    assert_eq!(reply["usage"]["cost"], 0.00084);
    assert!(reply["usage"].get("cost_details").is_none());
    assert_eq!(usage.micro, 840);
    assert_eq!(credits::balance(ME).await.unwrap(), 500_000 - 840);
    let sent = mock.requests.lock().unwrap().last().cloned().unwrap();
    assert!(sent.get("models").is_none() && sent["provider"].get("order").is_none(), "the caller's routing options are dropped: {sent}");
    assert_eq!((sent["provider"]["zdr"].as_bool(), sent["provider"]["data_collection"].as_str()), (Some(true), Some("deny")), "zero data retention only: {sent}");
    assert_eq!(sent["usage"], json!({ "include": true }));
    let max = sent["max_tokens"].as_u64().unwrap();
    assert!(max < 100000 && max > 30000, "capped to the balance: {max}");
    assert_ne!(sent["user"], ME, "the account never goes upstream");

    // Models outside the list, and Orvyn Model (soon), are refused.
    assert_eq!(inference::complete(ME, "api", ask("someone/uncurated"), None).await.unwrap_err().kind, Kind::Invalid);
    let soon = inference::complete(ME, "api", ask(inference::ORVYN_MODEL), None).await.unwrap_err();
    assert!(soon.message.contains("coming soon"));

    // Orvyn's own upstream account failing is "briefly unavailable", never the host's name.
    let e = inference::complete(ME, "api", ask("openai/gpt-oss-120b"), None).await.unwrap_err();
    assert_eq!(e.kind, Kind::Unavailable);
    assert!(!e.message.to_lowercase().contains("openrouter"));

    // A streamed chat turn: text arrives in pieces, the turn is saved and charged.
    let seen = Arc::new(Mutex::new(String::new()));
    let s2 = seen.clone();
    let on_text = move |t: &str| s2.lock().unwrap().push_str(t);
    let done = inference::chat_turn(ME, None, "Say hello\nplease", Some("z-ai/glm-5.3"), Some(&on_text)).await.unwrap();
    assert_eq!(*seen.lock().unwrap(), "Hello!");
    assert_eq!(done["message"]["content"], "Hello!");
    assert_eq!(done["thread"]["title"], "Say hello");
    let thread_id = done["thread"]["id"].as_str().unwrap().to_string();
    let (_, msgs) = inference::thread(ME, &thread_id).await.unwrap();
    assert_eq!(msgs.iter().map(|m| m.role.as_str()).collect::<Vec<_>>(), ["user", "assistant"]);
    assert_eq!(msgs[1].micro_usd, 840);
    // The next turn sends the history.
    inference::chat_turn(ME, Some(&thread_id), "Again", None, None).await.unwrap();
    let sent = mock.requests.lock().unwrap().last().cloned().unwrap();
    let roles: Vec<&str> = sent["messages"].as_array().unwrap().iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["system", "user", "assistant", "user"]);
    assert_eq!(credits::balance(ME).await.unwrap(), 500_000 - 3 * 840);
    let ledger = credits::history(ME, 10).await.unwrap();
    assert_eq!(ledger.iter().filter(|e| e.kind == "usage" && e.source.as_deref() == Some("chat")).count(), 2);
    // Someone else can't read it.
    assert_eq!(inference::thread("0x00000000000000000000000000000000000000bb", &thread_id).await.unwrap_err().kind, Kind::NotFound);

    // A failed turn leaves no trace.
    let before = inference::list_threads(ME).await.unwrap().len();
    assert!(inference::chat_turn(ME, None, "hi", Some("openai/gpt-oss-120b"), None).await.is_err());
    assert_eq!(inference::list_threads(ME).await.unwrap().len(), before);

    // API keys: work until revoked; only the hash is stored.
    let (key, secret) = credits::create_key(ME, "My agent").await.unwrap();
    assert!(secret.starts_with("orv_sk_") && key.hint.ends_with(&secret[secret.len() - 4..]));
    assert_ne!(key.key_hash, secret);
    assert_eq!(credits::account_for_key(&secret).await.unwrap().as_deref(), Some(ME));
    credits::revoke_key(ME, &key.id).await.unwrap();
    assert_eq!(credits::account_for_key(&secret).await.unwrap(), None);

    // Stablecoin top-ups are a transfer to the deposit address.
    let call = credits::deposit_call("USDT", 5.0).unwrap();
    assert_eq!(call["to"], "0x55d398326f99059ff775485246999027b3197955");
    assert!(call["data"].as_str().unwrap().starts_with("0xa9059cbb00000000000000000000000000000000000000000000000000000000000000cc"));
    assert_eq!(credits::deposit_call("USDT", 0.5).unwrap_err().kind, Kind::Invalid);
    assert_eq!(credits::record_deposit(ME, "0x1234").await.unwrap_err().kind, Kind::Invalid);
}
