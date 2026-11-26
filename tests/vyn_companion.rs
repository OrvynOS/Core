//! Vyn as a companion, end to end on a throwaway database. The AI provider, Expo push and
//! Telegram's Bot API are one stand-in server that plays scripted replies and records requests:
//! - memory: she remembers mid-turn (a server tool) and it's in the next conversation's prompt;
//!   card numbers aren't kept; the person can delete memories, nobody else can;
//! - reminders: a "remind me" turn schedules one in local time; when due, it runs, lands in "From
//!   Vyn" and is pushed; a one-off doesn't run twice; the daily check-in follows settings;
//! - time zones across the October DST change and weekdays-only schedules;
//! - read_web refuses private addresses;
//! - Telegram: linking with a one-time code, a payment approved with a button, an unlinked chat,
//!   reminders delivered to the chat;
//! - streaming: words arrive in pieces, in order, and the stored conversation is as without it.
mod support;

use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::routing::post;
use axum::{Json, Router};
use chrono::{TimeZone, Utc};
use orvyn_core::vyn::{self, telegram, time, web, TurnInput};
use serde_json::{json, Value};

#[derive(Clone, Default)]
struct Stub {
    /// Scripted model replies (SSE bodies), played in order.
    replies: Arc<Mutex<Vec<String>>>,
    /// The system prompt of each model request.
    prompts: Arc<Mutex<Vec<String>>>,
    pushes: Arc<Mutex<Vec<Value>>>,
    /// (method, body) of each Bot API call.
    telegram: Arc<Mutex<Vec<(String, Value)>>>,
}

impl Stub {
    fn script(&self, replies: Vec<String>) {
        *self.replies.lock().unwrap() = replies;
    }
    fn last_prompt(&self) -> String {
        self.prompts.lock().unwrap().last().cloned().unwrap_or_default()
    }
    fn telegram_since(&self, n: usize) -> Vec<(String, Value)> {
        self.telegram.lock().unwrap()[n..].to_vec()
    }
    fn telegram_len(&self) -> usize {
        self.telegram.lock().unwrap().len()
    }
}

fn sse(events: &[Value]) -> String {
    events.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap_or("message"))).collect()
}

fn call(id: &str, name: &str, input: Value) -> String {
    sse(&[
        json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "tool_use", "id": id, "name": name, "input": {} } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": input.to_string() } }),
        json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 5 } }),
    ])
}

fn say(text: &str) -> String {
    say_in_pieces(&[text])
}

fn say_in_pieces(pieces: &[&str]) -> String {
    let mut events = vec![json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } })];
    events.extend(pieces.iter().map(|p| json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": p } })));
    events.push(json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" }, "usage": { "output_tokens": 5 } }));
    sse(&events)
}

async fn model(State(s): State<Stub>, Json(body): Json<Value>) -> impl axum::response::IntoResponse {
    s.prompts.lock().unwrap().push(body["system"].to_string());
    let next = {
        let mut r = s.replies.lock().unwrap();
        if r.is_empty() { say("(no more script)") } else { r.remove(0) }
    };
    ([("content-type", "text/event-stream")], next)
}

async fn push(State(s): State<Stub>, Json(body): Json<Value>) -> Json<Value> {
    s.pushes.lock().unwrap().push(body);
    Json(json!({ "data": [] }))
}

async fn bot(State(s): State<Stub>, Path((_bot, method)): Path<(String, String)>, Json(body): Json<Value>) -> Json<Value> {
    s.telegram.lock().unwrap().push((method, body));
    Json(json!({ "ok": true, "result": {} }))
}

fn input(text: &str, device: &str) -> TurnInput {
    TurnInput { text: Some(text.into()), tool_results: vec![], tools: vec![], device: device.into() }
}

async fn thread(account: &str) -> String {
    vyn::create_thread(account, None).await.unwrap()["id"].as_str().unwrap().to_string()
}

async fn settle() {
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
}

#[tokio::test]
async fn vyn_as_a_companion() {
    support::test_env(&[]);
    let db = support::shared_db().await;
    let stub = Stub::default();
    let url = support::serve_forever(
        Router::new()
            .route("/v1/messages", post(model))
            .route("/push", post(push))
            .route("/{bot}/{method}", post(bot))
            .with_state(stub.clone()),
    );
    for (k, v) in [
        ("ANTHROPIC_BASE_URL", url.clone()),
        ("EXPO_PUSH_URL", format!("{url}/push")),
        ("VYN_TELEGRAM_API_URL", url.clone()),
        ("VYN_TELEGRAM_BOT_TOKEN", "123:test".into()),
        ("VYN_TELEGRAM_BOT_USERNAME", "vyn_test_bot".into()),
        ("VYN_TELEGRAM_WEBHOOK_SECRET", "s3cret-s3cret".into()),
    ] {
        unsafe { std::env::set_var(k, v) };
    }
    let me = "0x1111111111111111111111111111111111111111";
    let key = orvyn_core::secrets::encrypt_secret("sk-test", &format!("vyn:{me}"), orvyn_core::secrets::SecretKind::AiKey).unwrap();
    sqlx::query("insert into vyn_settings (account_id, ai_provider, ai_model, ai_key_encrypted, ai_key_hint, persona, timezone) values ($1, 'anthropic', 'claude-sonnet-5-5', $2, 'sk-…test', $3, 'Europe/Lisbon')")
        .bind(me)
        .bind(key)
        .bind(json!({ "callMe": "Ada", "tone": "playful" }))
        .execute(db)
        .await
        .unwrap();

    // ---- Memory ----
    stub.script(vec![
        call("m1", "remember", json!({ "text": "Ada's mum's birthday is 12 December", "kind": "person" })),
        call("m2", "remember", json!({ "text": "Her card number is 4111 1111 1111 1111" })),
        say("Noted 🎂"),
        say("You told me: 12 December."),
    ]);
    let t = thread(me).await;
    let r1 = vyn::run_turn(me, &t, input("Mum's birthday is 12 Dec", "desktop"), None).await.unwrap();
    assert!(r1.done && r1.text == "Noted 🎂", "server tools run mid-turn and the turn ends in words: {}", r1.text);
    assert_eq!(r1.mood, "happy", "the mascot looks happy after remembering");
    let first_prompt = stub.prompts.lock().unwrap()[0].clone();
    assert!(first_prompt.contains("Call them Ada") && first_prompt.contains("Playful"), "the prompt carries the persona: {}", &first_prompt[..200.min(first_prompt.len())]);
    let memories = vyn::list_memories(me, 50).await.unwrap();
    assert!(memories.iter().any(|m| m.text.contains("12 December")), "the fact is remembered");
    assert!(!memories.iter().any(|m| m.text.contains("4111")), "a card number is not kept");
    let t2 = thread(me).await;
    vyn::run_turn(me, &t2, input("When is mum's birthday?", "mobile"), None).await.unwrap();
    let prompt = stub.last_prompt();
    assert!(prompt.contains("12 December") && prompt.contains("Europe/Lisbon"), "a new conversation knows the memory and the time zone");
    assert!(!vyn::forget_memory("0x2222222222222222222222222222222222222222", memories[0].id as i64).await.unwrap(), "nobody else can delete it");
    assert!(vyn::forget_memory(me, memories[0].id as i64).await.unwrap(), "the person can delete a memory");

    // ---- Time zones ----
    let at = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
    let iso = |d: Option<chrono::DateTime<Utc>>| d.map(|d| d.to_rfc3339()).unwrap_or_default();
    assert_eq!(iso(time::next_daily("09:00", "Europe/Lisbon", at("2026-10-23T12:00:00Z"), &[])), "2026-10-24T08:00:00+00:00", "09:00 Lisbon in summer");
    assert_eq!(iso(time::next_daily("09:00", "Europe/Lisbon", at("2026-10-25T12:00:00Z"), &[])), "2026-10-26T09:00:00+00:00", "09:00 Lisbon in winter");
    assert_eq!(iso(time::next_daily("07:30", "America/New_York", at("2026-11-27T20:00:00Z"), &[1, 2, 3, 4, 5])), "2026-11-30T12:30:00+00:00", "weekdays only skip the weekend");

    // ---- Reminders ----
    sqlx::query("insert into push_devices (token, account_id, platform) values ('ExponentPushToken[abcdefghijklmnop]', $1, 'ios')").bind(me).execute(db).await.unwrap();
    stub.script(vec![call("s1", "schedule", json!({ "title": "Call mum", "instruction": "Remind Ada to call her mum.", "at": "2030-01-01T10:00" })), say("I'll remind you on 1 Jan at 10:00.")]);
    let t3 = thread(me).await;
    vyn::run_turn(me, &t3, input("Remind me to call mum on 1 Jan 2030 at 10", "desktop"), None).await.unwrap();
    let routines = vyn::list_routines(me).await.unwrap();
    assert_eq!(routines.len(), 1);
    assert_eq!((routines[0].title.as_str(), routines[0].next_run_at), ("Call mum", Some(Utc.with_ymd_and_hms(2030, 1, 1, 10, 0, 0).unwrap())), "scheduled in local time (Lisbon is UTC in winter)");
    sqlx::query("update vyn_routines set next_run_at = now() - interval '1 second' where id = $1").bind(&routines[0].id).execute(db).await.unwrap();
    stub.script(vec![say("Ada! Time to call your mum 📞")]);
    assert_eq!(vyn::run_due_routines().await.unwrap(), 1, "the due reminder runs");
    let notices = vyn::notices(me, 0).await.unwrap();
    assert!(notices.last().unwrap().text.contains("call your mum"), "it's written to the inbox");
    settle().await;
    assert!(stub.pushes.lock().unwrap().iter().any(|p| p.to_string().contains("Call mum")), "and pushed to the phone");
    assert!(vyn::list_threads(me).await.unwrap().iter().any(|t| t["title"] == "From Vyn"), "in the From Vyn conversation");
    assert_eq!(vyn::run_due_routines().await.unwrap(), 0, "a one-off reminder doesn't run twice");
    assert!(vyn::list_routines(me).await.unwrap().is_empty());

    let settings = |check_in: Option<&str>| vyn::SettingsInput { ai_provider: "anthropic".into(), ai_model: "claude-sonnet-5-5".into(), api_key: None, persona: None, timezone: None, check_in_time: Some(check_in.map(String::from)) };
    vyn::save_settings(me, &settings(Some("08:30"))).await.unwrap();
    let routines = vyn::list_routines(me).await.unwrap();
    assert!(routines.len() == 1 && routines[0].kind == "checkin", "a check-in time creates one daily check-in");
    vyn::save_settings(me, &settings(None)).await.unwrap();
    assert!(vyn::list_routines(me).await.unwrap().is_empty(), "clearing it removes the check-in");

    // ---- read_web ----
    for url in ["http://127.0.0.1/", "http://169.254.169.254/latest/meta-data/", "http://localhost:3100/", "http://[::1]/", "file:///etc/passwd"] {
        assert!(web::read_web(url).await.is_err(), "read_web refuses {url}");
    }

    // ---- Telegram ----
    assert!(telegram::webhook_authorized(Some("s3cret-s3cret")) && !telegram::webhook_authorized(Some("nope")) && !telegram::webhook_authorized(None), "the webhook secret is checked");
    let link = telegram::create_link_code(me).await.unwrap();
    let link = link["url"].as_str().unwrap();
    let code = link.split("start=").nth(1).unwrap().to_string();
    assert!(link.starts_with("https://t.me/vyn_test_bot?start=") && code.len() > 10, "the link opens the bot with a code");
    telegram::handle_update(&json!({ "message": { "chat": { "id": 4242, "type": "private" }, "from": { "first_name": "Ada" }, "text": format!("/start {code}") } })).await;
    assert_eq!(telegram::channels_for(me).await.unwrap().len(), 1, "/start with the code links the chat");
    assert!(stub.telegram.lock().unwrap().last().unwrap().1["text"].as_str().unwrap().contains("linked"));
    telegram::handle_update(&json!({ "message": { "chat": { "id": 999, "type": "private" }, "text": format!("/start {code}") } })).await;
    assert_eq!(telegram::channels_for(me).await.unwrap().len(), 1, "the code works once");

    // A message: Vyn asks to approve a payment; the button approves it; she confirms.
    stub.script(vec![call("p1", "approve_payment", json!({ "merchant": "Flowers & Co", "description": "Bouquet", "amount": 32, "currency": "EUR", "method": "card" })), say("Ordered! They arrive Friday 💐")]);
    let n0 = stub.telegram_len();
    telegram::handle_update(&json!({ "message": { "chat": { "id": 4242, "type": "private" }, "text": "Send mum flowers, under €40" } })).await;
    let card = stub.telegram_since(n0).into_iter().find(|(m, b)| m == "sendMessage" && b["text"].as_str().unwrap_or("").starts_with("Approve this payment?"));
    assert!(card.as_ref().is_some_and(|(_, b)| b["reply_markup"].to_string().contains("p:y")), "a payment comes as an Approve / Decline card");
    let n1 = stub.telegram_len();
    telegram::handle_update(&json!({ "callback_query": { "id": "cb1", "data": "p:y", "message": { "chat": { "id": 4242 }, "message_id": 7 } } })).await;
    let after: Vec<String> = stub.telegram_since(n1).iter().map(|(_, b)| b["text"].as_str().unwrap_or("").to_string()).collect();
    assert!(after.iter().any(|t| t.contains("Approved")) && after.iter().any(|t| t.contains("arrive Friday")), "Approve continues the turn: {after:?}");
    telegram::handle_update(&json!({ "message": { "chat": { "id": 5555, "type": "private" }, "text": "hello?" } })).await;
    assert!(stub.telegram.lock().unwrap().last().unwrap().1["text"].as_str().unwrap().contains("Link Telegram"), "an unlinked chat is told how to link");

    let n2 = stub.telegram_len();
    let t4 = thread(me).await;
    vyn::send_notice(me, &t4, "Water the plants", "Your basil looks thirsty 🌿").await.unwrap();
    assert!(stub.telegram_since(n2).iter().any(|(_, b)| b["chat_id"] == "4242" && b["text"].as_str().unwrap_or("").contains("basil")), "reminders are delivered to the linked chat");
    let channel = telegram::channels_for(me).await.unwrap()[0]["id"].as_i64().unwrap();
    assert!(telegram::unlink(me, channel).await.unwrap(), "the person can unlink");

    // ---- Streaming ----
    let pieces = ["Hi! ", "I'll open ", "the shop ", "for you."];
    let mut events = vec![json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } })];
    events.extend(pieces.iter().map(|p| json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": p } })));
    events.extend([
        json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "tool_use", "id": "c1", "name": "browser_open", "input": {} } }),
        json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "input_json_delta", "partial_json": json!({ "url": "https://shop.example" }).to_string() } }),
        json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 5 } }),
    ]);
    stub.script(vec![sse(&events), say_in_pieces(&["It's ", "open."])]);
    let t5 = thread(me).await;
    let got = Arc::new(Mutex::new(Vec::<String>::new()));
    let g = got.clone();
    let on_text = move |d: &str| g.lock().unwrap().push(d.to_string());
    let shop = TurnInput { text: Some("Open the shop".into()), tool_results: vec![], tools: vec!["browser_open".into()], device: "desktop".into() };
    let s1 = vyn::run_turn(me, &t5, shop, Some(&on_text)).await.unwrap();
    assert_eq!(*got.lock().unwrap(), pieces, "the words arrive in pieces, in order");
    assert!(s1.text == pieces.concat() && s1.tool_calls[0].tool_name == "browser_open" && !s1.done, "the step's result: its text and the device tool to run");
    got.lock().unwrap().clear();
    let opened = vyn::ToolResult { tool_call_id: "c1".into(), tool_name: "browser_open".into(), output: json!({ "title": "Shop" }) };
    let s2 = vyn::run_turn(me, &t5, TurnInput { text: None, tool_results: vec![opened], tools: vec!["browser_open".into()], device: "desktop".into() }, Some(&on_text)).await.unwrap();
    assert!(got.lock().unwrap().concat() == "It's open." && s2.done, "the next step streams too and finishes the turn");
    let roles: Vec<String> = vyn::thread_messages(me, &t5).await.unwrap().iter().map(|m| m["message"]["role"].as_str().or(m["role"].as_str()).unwrap_or("").to_string()).collect();
    assert_eq!(roles, ["user", "assistant", "tool", "assistant"], "the conversation is stored as usual");
}
