//! Vyn: local times as Node's Intl gives them (DST gaps and overlaps included), page text, address
//! checks; and on a throwaway database, a turn against a mock model: a server tool runs mid-turn,
//! a device tool stops the turn, the device's answer continues it, reminders run on their own.
mod support;

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use chrono::{TimeZone, Utc};
use orvyn_core::vyn::{self, time, web};
use serde_json::{json, Value};

#[test]
fn times_match_node_intl() {
    let d = Utc.with_ymd_and_hms(2026, 11, 24, 14, 5, 0).unwrap();
    assert_eq!(time::describe(d, "Europe/Lisbon"), "Tue 24 Nov, 14:05");
    assert_eq!(time::now_line("Europe/Lisbon", d), "Tuesday, 24 November 2026 at 14:05 (Europe/Lisbon)");
    assert_eq!(time::describe(d, "Asia/Kolkata"), "Tue 24 Nov, 19:35");
    assert_eq!(time::now_line("America/New_York", Utc.with_ymd_and_hms(2026, 1, 3, 4, 7, 0).unwrap()), "Friday, 2 January 2026 at 23:07 (America/New_York)");
    // The Sunday DST starts in Lisbon, at 09:00 local (already summer time): 08:00 UTC.
    let next = time::next_daily("09:00", "Europe/Lisbon", Utc.with_ymd_and_hms(2026, 3, 28, 12, 0, 0).unwrap(), &[0]).unwrap();
    assert_eq!(next.to_rfc3339(), "2026-03-29T08:00:00+00:00");
    // A time in the spring gap and one in the autumn overlap resolve as Node's do.
    assert_eq!(time::zoned(2026, 3, 29, 1, 30, "Europe/Lisbon").to_rfc3339(), "2026-03-29T01:30:00+00:00");
    assert_eq!(time::zoned(2026, 10, 25, 1, 30, "Europe/Lisbon").to_rfc3339(), "2026-10-25T01:30:00+00:00");
    assert!(time::valid_time_zone("Africa/Lagos") && !time::valid_time_zone("Mars/Olympus"));
    assert!(time::is_hhmm("23:59") && !time::is_hhmm("24:00") && !time::is_hhmm("9:00"));
}

#[test]
fn pages_addresses_and_pending_calls() {
    let html = "<html><head><title> Hi &amp; bye </title><style>x{}</style></head><body><script>evil()</script><p>One</p><div>Two<br>Three &lt;3</div><!-- no --></body></html>";
    // As the TypeScript core: the title is only a tag here, so it runs into the first line.
    assert_eq!(web::page_text(html), "Hi & bye One\nTwo\nThree <3");
    for (ip, private) in [("10.1.2.3", true), ("169.254.169.254", true), ("100.64.0.1", true), ("8.8.8.8", false), ("::1", true), ("fd00::1", true), ("::ffff:127.0.0.1", true), ("2606:4700::1", false)] {
        assert_eq!(web::is_private_address(ip.parse().unwrap()), private, "{ip}");
    }
    let messages = vec![
        json!({ "role": "assistant", "content": [{ "type": "tool-call", "toolCallId": "a" }, { "type": "tool-call", "toolCallId": "b" }] }),
        json!({ "role": "tool", "content": [{ "type": "tool-result", "toolCallId": "a" }] }),
    ];
    assert_eq!(vyn::pending_tool_calls(&messages), vec!["b".to_string()]);
    assert_eq!(vyn::mood_of("All set!", &[], &[]), "happy");
    assert_eq!(vyn::mood_of("x", &["approve_payment".into()], &[]), "attentive");
}

#[derive(Clone, Default)]
struct Mock {
    replies: Arc<Mutex<Vec<String>>>,
    requests: Arc<Mutex<Vec<Value>>>,
}

async fn reply(State(m): State<Mock>, Json(body): Json<Value>) -> impl IntoResponse {
    m.requests.lock().unwrap().push(body);
    let next = m.replies.lock().unwrap().remove(0);
    ([("content-type", "text/event-stream")], next).into_response()
}

fn sse(events: &[Value]) -> String {
    events.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap_or("message"))).collect()
}

fn tool_use(id: &str, name: &str, input: Value) -> String {
    sse(&[
        json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "tool_use", "id": id, "name": name, "input": {} } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": input.to_string() } }),
        json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 5 } }),
    ])
}

fn text(t: &str) -> String {
    sse(&[
        json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": t } }),
        json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" }, "usage": { "output_tokens": 5 } }),
    ])
}

#[tokio::test]
async fn a_turn_with_server_and_device_tools() {
    support::test_env(&[]);
    let db = support::shared_db().await;
    let replies = vec![
        // Turn 1: remember (server tool, runs here), then ask_person (device tool: the turn stops).
        tool_use("toolu_1", "remember", json!({ "text": "Their name is Ada", "kind": "fact" })),
        tool_use("toolu_2", "ask_person", json!({ "question": "Coffee or tea?", "choices": ["Coffee", "Tea"] })),
        // Turn 2: the device's answer comes back; she replies.
        text("Tea it is. Noted!"),
        // The reminder, run on her own.
        text("Morning Ada, time for tea."),
    ];
    let mock = Mock { replies: Arc::new(Mutex::new(replies)), ..Default::default() };
    let app = Router::new().route("/v1/messages", post(reply)).with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    unsafe { std::env::set_var("ANTHROPIC_BASE_URL", &url) };

    let account = "0xada0000000000000000000000000000000000001";
    // No settings yet: the turn asks for a key first (412).
    let thread = vyn::create_thread(account, None).await.unwrap();
    let id = thread["id"].as_str().unwrap().to_string();
    let input = |t: Option<&str>, results: Vec<vyn::ToolResult>| vyn::TurnInput { text: t.map(String::from), tool_results: results, tools: vec!["ask_person".into(), "approve_payment".into()], device: "desktop".into() };
    assert_eq!(vyn::run_turn(account, &id, input(Some("hi"), vec![]), None).await.unwrap_err().status(), 412);
    let key = orvyn_core::secrets::encrypt_secret("sk-ant-test", &format!("vyn:{account}"), orvyn_core::secrets::SecretKind::AiKey).unwrap();
    sqlx::query("insert into vyn_settings (account_id, ai_provider, ai_model, ai_key_encrypted, ai_key_hint, auto_approve_usd, persona, timezone) values ($1, 'anthropic', 'claude-test', $2, 'sk-…test', 0, $3, 'Europe/Lisbon')")
        .bind(account)
        .bind(key)
        .bind(json!({ "tone": "calm", "callMe": "Ada" }))
        .execute(db)
        .await
        .unwrap();

    let streamed = Arc::new(Mutex::new(String::new()));
    let s2 = streamed.clone();
    let on_text = move |t: &str| s2.lock().unwrap().push_str(t);
    let out = vyn::run_turn(account, &id, input(Some("I'm Ada. Help me pick a drink"), vec![]), Some(&on_text)).await.unwrap();
    assert!(!out.done);
    assert_eq!(out.mood, "curious");
    assert_eq!(out.tool_calls.len(), 1);
    assert_eq!((out.tool_calls[0].tool_name.as_str(), out.tool_calls[0].input["question"].as_str()), ("ask_person", Some("Coffee or tea?")));
    let memories = vyn::list_memories(account, 10).await.unwrap();
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0].text, "Their name is Ada");
    // The system prompt carried the persona and the person's time.
    let first = mock.requests.lock().unwrap()[0].clone();
    let system = first["system"].to_string();
    assert!(system.contains("Call them Ada.") && system.contains("Calm and gentle") && system.contains("(Europe/Lisbon)"), "{system}");
    // Only the device's tools are offered, plus the server tools.
    let names: Vec<String> = first["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
    assert!(names.contains(&"ask_person".into()) && names.contains(&"remember".into()) && !names.contains(&"browser_open".into()));

    // An answer for a call that isn't waiting is refused; the real one continues the turn.
    let wrong = vyn::ToolResult { tool_call_id: "nope".into(), tool_name: "ask_person".into(), output: json!({ "answer": "x" }) };
    assert!(vyn::run_turn(account, &id, input(None, vec![wrong]), None).await.is_err());
    assert_eq!(vyn::waiting_calls(&id).await.unwrap().len(), 1);
    let answer = vyn::ToolResult { tool_call_id: "toolu_2".into(), tool_name: "ask_person".into(), output: json!({ "answer": "Tea" }) };
    let out = vyn::run_turn(account, &id, input(None, vec![answer]), None).await.unwrap();
    assert!(out.done);
    assert_eq!(out.text, "Tea it is. Noted!");
    assert!(vyn::waiting_calls(&id).await.unwrap().is_empty());
    let threads = vyn::list_threads(account).await.unwrap();
    assert_eq!(threads[0]["title"], "I'm Ada. Help me pick a drink");
    let messages = vyn::thread_messages(account, &id).await.unwrap();
    // user, assistant (remember), tool, assistant (ask_person), tool (the device's answer), assistant.
    assert_eq!(messages.len(), 6);

    // A reminder that's due runs as her own turn and becomes a notice.
    sqlx::query("insert into vyn_routines (id, account_id, title, instruction, schedule, kind, next_run_at) values ('r1', $1, 'Tea', 'Remind them about tea', $2, 'user', now() - interval '1 minute')")
        .bind(account)
        .bind(json!({ "kind": "once", "at": "2026-01-01T00:00:00.000Z" }))
        .execute(db)
        .await
        .unwrap();
    assert_eq!(vyn::run_due_routines().await.unwrap(), 1);
    let notices = vyn::notices(account, 0).await.unwrap();
    assert_eq!(notices.len(), 1);
    assert_eq!((notices[0].title.as_str(), notices[0].text.as_str()), ("Tea", "Morning Ada, time for tea."));
    assert!(vyn::list_routines(account).await.unwrap().is_empty(), "one-off reminders end when they run");
    assert_eq!(vyn::run_due_routines().await.unwrap(), 0);
}
