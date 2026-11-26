//! The AI layer against mock providers: the tool loop runs tools and feeds results back, text
//! streams as it arrives, device tools stop the turn, messages keep the AI SDK's shape (thinking
//! signatures included), and provider errors are described without leaking the key.
mod support;

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use orvyn_core::ai::{describe_ai_error, generate, schema, user_message, Effort, ModelConfig, Request, Tool};
use serde_json::{json, Value};

/// A mock provider: answers each call with the next canned SSE body, and keeps the requests.
#[derive(Clone, Default)]
struct Mock {
    replies: Arc<Mutex<Vec<String>>>,
    requests: Arc<Mutex<Vec<Value>>>,
}

async fn reply(State(m): State<Mock>, Json(body): Json<Value>) -> impl IntoResponse {
    m.requests.lock().unwrap().push(body);
    let next = m.replies.lock().unwrap().remove(0);
    if let Some(status) = next.strip_prefix("STATUS:") {
        return (axum::http::StatusCode::from_u16(status.parse().unwrap()).unwrap(), "nope").into_response();
    }
    ([("content-type", "text/event-stream")], next).into_response()
}

async fn serve(replies: Vec<String>) -> (String, Mock) {
    let mock = Mock { replies: Arc::new(Mutex::new(replies)), ..Default::default() };
    let app = Router::new().route("/v1/messages", post(reply)).route("/chat/completions", post(reply)).with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, mock)
}

fn sse(events: &[Value]) -> String {
    events.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap_or("message"))).collect()
}

fn weather_tool(calls: Arc<Mutex<Vec<Value>>>) -> Tool {
    Tool::new("weather", "The weather in a city.", schema(&[("city", json!({ "type": "string" }), true)]), move |input| {
        let calls = calls.clone();
        async move {
            calls.lock().unwrap().push(input.clone());
            Ok(json!({ "city": input["city"], "sky": "clear", "c": 21 }))
        }
    })
}

#[tokio::test]
async fn anthropic_tool_loop_streams_and_keeps_thinking() {
    support::test_env(&[]);
    let first = sse(&[
        json!({ "type": "message_start", "message": { "usage": { "input_tokens": 12 } } }),
        json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "thinking", "thinking": "" } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "thinking_delta", "thinking": "Check the weather." } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "signature_delta", "signature": "sig-1" } }),
        json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "tool_use", "id": "toolu_1", "name": "weather", "input": {} } }),
        json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "input_json_delta", "partial_json": "{\"city\":" } }),
        json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "input_json_delta", "partial_json": "\"Lagos\"}" } }),
        json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 30 } }),
    ]);
    let second = sse(&[
        json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "Clear and " } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "21°C." } }),
        json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" }, "usage": { "output_tokens": 8 } }),
    ]);
    let (url, mock) = serve(vec![first, second]).await;
    // SAFETY: set before any request, in this test binary.
    unsafe { std::env::set_var("ANTHROPIC_BASE_URL", &url) };
    let calls = Arc::new(Mutex::new(vec![]));
    let tools = vec![weather_tool(calls.clone())];
    let streamed = Arc::new(Mutex::new(String::new()));
    let s2 = streamed.clone();
    let on_text = move |t: &str| s2.lock().unwrap().push_str(t);
    let cfg = ModelConfig { provider: "anthropic".into(), model: "claude-opus-5".into(), api_key: "sk-test".into(), effort: Effort::Medium };
    let out = generate(&cfg, Request { system: "Be brief.".into(), messages: vec![user_message("Weather in Lagos?")], tools: &tools, max_steps: 4, max_output_tokens: 4096 }, Some(&on_text)).await.unwrap();

    assert_eq!(out.text, "Clear and 21°C.");
    assert_eq!(*streamed.lock().unwrap(), "Clear and 21°C.");
    assert_eq!(calls.lock().unwrap().as_slice(), &[json!({ "city": "Lagos" })]);
    assert_eq!(out.steps.len(), 2);
    assert_eq!((out.input_tokens, out.output_tokens), (12, 38));
    // The AI SDK's message shapes: reasoning with its signature, the call, the result, the answer.
    let m = &out.response_messages;
    assert_eq!(m[0]["content"][0], json!({ "type": "reasoning", "text": "Check the weather.", "providerOptions": { "anthropic": { "signature": "sig-1" } } }));
    assert_eq!(m[0]["content"][1], json!({ "type": "tool-call", "toolCallId": "toolu_1", "toolName": "weather", "input": { "city": "Lagos" } }));
    assert_eq!(m[1]["content"][0]["output"], json!({ "type": "json", "value": { "city": "Lagos", "sky": "clear", "c": 21 } }));
    assert_eq!(m[2]["content"][0]["text"], "Clear and 21°C.");
    // The second request carried the thinking block (signed) and the tool result back.
    let reqs = mock.requests.lock().unwrap().clone();
    assert_eq!(reqs[0]["thinking"], json!({ "type": "adaptive" }));
    assert_eq!(reqs[0]["output_config"], json!({ "effort": "medium" }));
    assert_eq!(reqs[0]["tools"][0]["name"], "weather");
    let sent = &reqs[1]["messages"];
    assert_eq!(sent[1]["content"][0], json!({ "type": "thinking", "thinking": "Check the weather.", "signature": "sig-1" }));
    assert_eq!(sent[2]["content"][0]["type"], "tool_result");
    assert_eq!(sent[2]["content"][0]["tool_use_id"], "toolu_1");
}

#[tokio::test]
async fn openai_compatible_and_device_tools() {
    support::test_env(&[]);
    let call = [
        json!({ "choices": [{ "delta": { "tool_calls": [{ "index": 0, "id": "call_a", "function": { "name": "open_app", "arguments": "{\"app\":" } }] } }] }),
        json!({ "choices": [{ "delta": { "tool_calls": [{ "index": 0, "function": { "arguments": "\"notes\"}" } }] }, "finish_reason": "tool_calls" }] }),
        json!({ "choices": [], "usage": { "prompt_tokens": 5, "completion_tokens": 7 } }),
    ];
    let body: String = call.iter().map(|e| format!("data: {e}\n\n")).collect::<String>() + "data: [DONE]\n\n";
    let (url, mock) = serve(vec![body, "STATUS:401".into()]).await;
    // SAFETY: set before any request, in this test binary.
    unsafe { std::env::set_var("ORVYN_AI_BASE_GROQ", &url) };
    let tools = vec![Tool::remote("open_app", "Open an app on the person's computer.", schema(&[("app", json!({ "type": "string" }), true)]))];
    let cfg = ModelConfig { provider: "groq".into(), model: "llama-x".into(), api_key: "gsk-secret-key".into(), effort: Effort::Low };
    let req = || Request { system: "sys".into(), messages: vec![user_message("open my notes")], tools: &tools, max_steps: 4, max_output_tokens: 1000 };
    let out = generate(&cfg, req(), None).await.unwrap();
    // A device tool: the turn stops at the call, for the device to run.
    assert_eq!(out.pending_tool_calls, vec![json!({ "type": "tool-call", "toolCallId": "call_a", "toolName": "open_app", "input": { "app": "notes" } })]);
    assert_eq!(out.response_messages.len(), 1);
    let sent = mock.requests.lock().unwrap()[0].clone();
    assert_eq!(sent["messages"][0], json!({ "role": "system", "content": "sys" }));
    assert_eq!(sent["tools"][0]["function"]["name"], "open_app");
    assert_eq!(sent["max_tokens"], 1000);
    // A rejected key: described for the creator, never echoed.
    let e = generate(&cfg, req(), None).await.unwrap_err();
    let described = describe_ai_error(&e);
    assert_eq!(described, "The AI provider rejected the agent's API key. The creator needs to update it.");
    assert!(!described.contains("gsk-secret-key"));
}
