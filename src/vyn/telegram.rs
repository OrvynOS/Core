//! Vyn on Telegram. The person links a chat from the Vyn app (a one-time /start code), then talks
//! to Vyn there like on any device: questions come as buttons, payments as an Approve / Decline
//! card, and reminders and check-ins arrive in the chat.
//!
//! Env: VYN_TELEGRAM_BOT_TOKEN, VYN_TELEGRAM_BOT_USERNAME, VYN_TELEGRAM_WEBHOOK_SECRET.

use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine as _;
use serde_json::{json, Value};

use super::{run_turn, waiting_calls, ToolResult, TurnInput};
use crate::db::{db, VynChannel, VynLinkCode};
use crate::error::{Error, Kind, Result};

const TOOLS: &[&str] = &["ask_person", "approve_payment", super::APPROVE_BINANCE];
const CODE_TTL_SECS: f64 = 15.0 * 60.0;

fn var(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

pub fn telegram_enabled() -> bool {
    !var("VYN_TELEGRAM_BOT_TOKEN").is_empty() && !var("VYN_TELEGRAM_BOT_USERNAME").is_empty() && !var("VYN_TELEGRAM_WEBHOOK_SECRET").is_empty()
}

/// Check Telegram's webhook secret header in constant time.
pub fn webhook_authorized(header: Option<&str>) -> bool {
    let secret = var("VYN_TELEGRAM_WEBHOOK_SECRET");
    match header {
        Some(h) if !secret.is_empty() => h.len() == secret.len() && h.bytes().zip(secret.bytes()).fold(0u8, |a, (x, y)| a | (x ^ y)) == 0,
        _ => false,
    }
}

async fn tg(method: &str, body: Value) -> Result<Value> {
    let res = crate::http::client()
        // VYN_TELEGRAM_API_URL: a stand-in Bot API (tests).
        .post(format!("{}/bot{}/{method}", Some(var("VYN_TELEGRAM_API_URL")).filter(|u| !u.is_empty()).unwrap_or_else(|| "https://api.telegram.org".into()).trim_end_matches('/'), var("VYN_TELEGRAM_BOT_TOKEN")))
        .json(&body)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| Error::unavailable(format!("telegram {method}: {e}")))?;
    let status = res.status().as_u16();
    let data: Value = res.json().await.unwrap_or(Value::Null);
    if data["ok"] != true {
        return Err(Error::unavailable(format!("telegram {method}: {}", data["description"].as_str().map(String::from).unwrap_or_else(|| status.to_string()))));
    }
    Ok(data)
}

/// Send text, split to Telegram's 4096-character limit.
async fn say(chat_id: &str, text: &str, extra: Value) -> Result<()> {
    let chars: Vec<char> = text.chars().collect();
    let chunks: Vec<String> = chars.chunks(4000).map(|c| c.iter().collect()).collect();
    for (i, chunk) in chunks.iter().enumerate() {
        let mut body = json!({ "chat_id": chat_id, "text": chunk });
        if i == chunks.len() - 1 {
            if let (Some(b), Some(e)) = (body.as_object_mut(), extra.as_object()) {
                b.extend(e.clone());
            }
        }
        tg("sendMessage", body).await?;
    }
    Ok(())
}

// ---------- Linking ----------

pub async fn create_link_code(account_id: &str) -> Result<Value> {
    if !telegram_enabled() {
        return Err(Error::unavailable("Vyn on Telegram isn't set up on this server"));
    }
    let code = B64URL.encode(rand::random::<[u8; 12]>());
    let row: VynLinkCode = sqlx::query_as("insert into vyn_link_codes (code, account_id, kind, expires_at) values ($1, $2, 'telegram', now() + make_interval(secs => $3)) returning *")
        .bind(&code)
        .bind(account_id.to_lowercase())
        .bind(CODE_TTL_SECS)
        .fetch_one(db())
        .await?;
    Ok(json!({ "url": format!("https://t.me/{}?start={code}", var("VYN_TELEGRAM_BOT_USERNAME")), "expiresAt": row.expires_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true) }))
}

pub async fn channels_for(account_id: &str) -> Result<Vec<Value>> {
    let rows: Vec<VynChannel> = sqlx::query_as("select * from vyn_channels where account_id = $1").bind(account_id.to_lowercase()).fetch_all(db()).await?;
    Ok(rows.iter().map(|c| json!({ "id": c.id, "kind": c.kind, "label": c.label, "createdAt": c.created_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true) })).collect())
}

pub async fn unlink(account_id: &str, id: i64) -> Result<bool> {
    let gone: Option<VynChannel> = sqlx::query_as("delete from vyn_channels where id = $1 and account_id = $2 returning *").bind(id as i32).bind(account_id.to_lowercase()).fetch_optional(db()).await?;
    if let Some(g) = &gone {
        if telegram_enabled() {
            let _ = say(&g.external_id, "You unlinked me from this chat. Take care! 🧸", json!({})).await;
        }
    }
    Ok(gone.is_some())
}

async fn new_thread(account_id: &str) -> Result<String> {
    let id = crate::lifecycle::uuid_v4();
    sqlx::query("insert into vyn_threads (id, account_id, title) values ($1, $2, 'Telegram')").bind(&id).bind(account_id).execute(db()).await?;
    Ok(id)
}

async fn link(code: &str, chat_id: &str, label: Option<&str>) -> Result<Option<String>> {
    let row: Option<VynLinkCode> = sqlx::query_as("delete from vyn_link_codes where code = $1 and kind = 'telegram' and expires_at > now() returning *").bind(code).fetch_optional(db()).await?;
    let Some(row) = row else { return Ok(None) };
    let thread = new_thread(&row.account_id).await?;
    sqlx::query(
        "insert into vyn_channels (account_id, kind, external_id, thread_id, label) values ($1, 'telegram', $2, $3, $4)
         on conflict (kind, external_id) do update set account_id = excluded.account_id, thread_id = excluded.thread_id, label = excluded.label",
    )
    .bind(&row.account_id)
    .bind(chat_id)
    .bind(&thread)
    .bind(label)
    .execute(db())
    .await?;
    Ok(Some(row.account_id))
}

async fn channel(chat_id: &str) -> Result<Option<VynChannel>> {
    Ok(sqlx::query_as("select * from vyn_channels where kind = 'telegram' and external_id = $1").bind(chat_id).fetch_optional(db()).await?)
}

// ---------- Conversation ----------

/// Run a turn and show its output in the chat; device tools become buttons.
async fn converse(chat_id: &str, account_id: &str, thread_id: &str, text: Option<String>, results: Vec<ToolResult>) -> Result<()> {
    let mut next = Some((text, results));
    for _ in 0..4 {
        let Some((text, results)) = next.take() else { break };
        let _ = tg("sendChatAction", json!({ "chat_id": chat_id, "action": "typing" })).await;
        let out = run_turn(account_id, thread_id, TurnInput { text, tool_results: results, tools: TOOLS.iter().map(|s| s.to_string()).collect(), device: "telegram".into() }, None).await?;
        if !out.text.trim().is_empty() {
            say(chat_id, out.text.trim(), json!({})).await?;
        }
        let mut unsupported = vec![];
        for call in &out.tool_calls {
            match call.tool_name.as_str() {
                "ask_person" => {
                    let q = call.input["question"].as_str().unwrap_or("");
                    let choices: Vec<&str> = call.input["choices"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                    let extra = if choices.is_empty() {
                        json!({})
                    } else {
                        json!({ "reply_markup": { "inline_keyboard": choices.iter().enumerate().map(|(i, c)| json!([{ "text": c.chars().take(60).collect::<String>(), "callback_data": format!("c:{i}") }])).collect::<Vec<_>>() } })
                    };
                    say(chat_id, q, extra).await?;
                }
                "approve_payment" => {
                    let p = &call.input;
                    let amount = p["amount"].as_f64().map(crate::auth::js_number).unwrap_or_default();
                    let text = format!(
                        "Approve this payment?\n\n{amount} {} to {}\n{}\nPaid with: {}",
                        p["currency"].as_str().unwrap_or(""),
                        p["merchant"].as_str().unwrap_or(""),
                        p["description"].as_str().unwrap_or(""),
                        p["method"].as_str().unwrap_or("")
                    );
                    say(chat_id, &text, json!({ "reply_markup": { "inline_keyboard": [[{ "text": "Approve", "callback_data": "p:y" }, { "text": "Decline", "callback_data": "p:n" }]] } })).await?;
                }
                "approve_binance" => {
                    let text = format!("Approve this Binance action?

{}

It runs on your Binance Agentic sub-account only if you approve.", call.input["summary"].as_str().unwrap_or(""));
                    say(chat_id, &text, json!({ "reply_markup": { "inline_keyboard": [[{ "text": "Approve", "callback_data": "b:y" }, { "text": "Decline", "callback_data": "b:n" }]] } })).await?;
                }
                _ => unsupported.push(ToolResult {
                    tool_call_id: call.tool_call_id.clone(),
                    tool_name: call.tool_name.clone(),
                    output: json!({ "error": "not available on Telegram; use read_web, or ask them to open Vyn on their computer" }),
                }),
            }
        }
        if !unsupported.is_empty() && unsupported.len() == out.tool_calls.len() {
            next = Some((None, unsupported));
        }
    }
    Ok(())
}

/// Answers for the waiting device calls, from a typed message or a button.
async fn answer(thread_id: &str, text: Option<&str>, button: Option<&str>) -> Result<Option<Vec<ToolResult>>> {
    let waiting = waiting_calls(thread_id).await?;
    if waiting.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        waiting
            .into_iter()
            .map(|call| {
                let output = match call.tool_name.as_str() {
                    "approve_payment" if button == Some("p:y") => json!({ "approved": true }),
                    // A Binance action (the server runs the call itself once approved).
                    n if crate::binance::binance_tool_of(n).is_some() => match (button, text) {
                        (Some("b:y"), _) => json!({ "approved": true }),
                        (_, Some(t)) => json!({ "approved": false, "note": format!("They wrote instead of approving: {}", t.chars().take(500).collect::<String>()) }),
                        _ => json!({ "approved": false }),
                    },
                    "approve_payment" => match text {
                        Some(t) => json!({ "approved": false, "note": format!("They wrote instead of approving: {}", t.chars().take(500).collect::<String>()) }),
                        None => json!({ "approved": false }),
                    },
                    "ask_person" => {
                        let picked = button.and_then(|b| b.strip_prefix("c:")).and_then(|i| i.parse::<usize>().ok()).and_then(|i| call.input["choices"].get(i)).and_then(Value::as_str).map(String::from);
                        json!({ "answer": picked.or(text.map(String::from)).unwrap_or_default() })
                    }
                    _ => json!({ "error": "not available on Telegram" }),
                };
                ToolResult { tool_call_id: call.tool_call_id, tool_name: call.tool_name, output }
            })
            .collect(),
    ))
}

const NOT_LINKED: &str = "Hi, I'm Vyn from Orvyn. To talk here, open the Vyn app, go to Settings and choose Link Telegram.";

/// Handle one webhook update. Never fails (Telegram would retry forever).
pub async fn handle_update(update: &Value) {
    let chat = if update["message"].is_object() { &update["message"]["chat"]["id"] } else { &update["callback_query"]["message"]["chat"]["id"] };
    let chat_id = match chat {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        _ => return,
    };
    if let Err(e) = handle(update, &chat_id).await {
        let reply = match (e.kind, e.status()) {
            (_, 412) => "I need an AI key before I can think 🙂 Add one in the Vyn app under Settings.",
            (Kind::Conflict, _) => "One sec, I'm still on your last message.",
            _ => "Sorry, something went wrong on my side. Try again in a moment.",
        };
        if e.status() >= 500 {
            eprintln!("[vyn telegram] {}", e.message);
        }
        let _ = say(&chat_id, reply, json!({})).await;
    }
}

async fn handle(update: &Value, chat_id: &str) -> Result<()> {
    let cq = &update["callback_query"];
    if cq.is_object() {
        let _ = tg("answerCallbackQuery", json!({ "callback_query_id": cq["id"] })).await;
        let Some(ch) = channel(chat_id).await? else { return Ok(()) };
        let data = cq["data"].as_str().unwrap_or("");
        // Show the choice on the card and remove its buttons.
        if let Some(mid) = cq["message"]["message_id"].as_i64() {
            let _ = tg("editMessageReplyMarkup", json!({ "chat_id": chat_id, "message_id": mid, "reply_markup": { "inline_keyboard": [] } })).await;
        }
        if ["p:y", "p:n", "b:y", "b:n"].contains(&data) {
            say(chat_id, if data.ends_with(":y") { "Approved ✓" } else { "Declined" }, json!({})).await?;
        }
        if let Some(results) = answer(&ch.thread_id, None, Some(data)).await? {
            converse(chat_id, &ch.account_id, &ch.thread_id, None, results).await?;
        }
        return Ok(());
    }
    let msg = &update["message"];
    let (Some(text), true) = (msg["text"].as_str(), msg["chat"]["type"] == "private") else { return Ok(()) };
    let text = text.trim();
    let first = msg["from"]["first_name"].as_str();
    if text.starts_with("/start") {
        let code = text.split_whitespace().nth(1);
        let label = msg["from"]["username"].as_str().or(first);
        let account = match code {
            Some(c) => link(c, chat_id, label).await?,
            None => None,
        };
        if account.is_some() {
            let hi = first.map(|n| format!(" {n}")).unwrap_or_default();
            return say(chat_id, &format!("Hi{hi}! It's Vyn 🧸 We're linked now. Talk to me here like in the app: questions, reminders, errands. I'll ask before paying for anything."), json!({})).await;
        }
        let linked = channel(chat_id).await?.is_some();
        return say(chat_id, if linked { "We're already linked. What can I do for you?" } else { NOT_LINKED }, json!({})).await;
    }
    let ch = channel(chat_id).await?;
    if text == "/stop" {
        if let Some(c) = &ch {
            sqlx::query("delete from vyn_channels where id = $1").bind(c.id).execute(db()).await?;
        }
        return say(chat_id, if ch.is_some() { "Unlinked. You can link me again any time from the Vyn app." } else { "We weren't linked." }, json!({})).await;
    }
    let Some(ch) = ch else { return say(chat_id, NOT_LINKED, json!({})).await };
    // The thread may have been deleted from another device: start a fresh one.
    let exists: Option<String> = sqlx::query_scalar("select id from vyn_threads where id = $1").bind(&ch.thread_id).fetch_optional(db()).await?;
    let thread_id = match exists {
        Some(t) => t,
        None => {
            let t = new_thread(&ch.account_id).await?;
            sqlx::query("update vyn_channels set thread_id = $2 where id = $1").bind(ch.id).bind(&t).execute(db()).await?;
            t
        }
    };
    match answer(&thread_id, Some(text), None).await? {
        Some(results) => converse(chat_id, &ch.account_id, &thread_id, None, results).await,
        None => converse(chat_id, &ch.account_id, &thread_id, Some(text.to_string()), vec![]).await,
    }
}

/// Reminders and check-ins also go to every linked chat.
pub async fn deliver_notice(account_id: &str, title: &str, text: &str) -> Result<()> {
    if !telegram_enabled() {
        return Ok(());
    }
    let chats: Vec<VynChannel> = sqlx::query_as("select * from vyn_channels where account_id = $1 and kind = 'telegram'").bind(account_id).fetch_all(db()).await?;
    let body = if text.starts_with(title) { text.to_string() } else { format!("⏰ {title}\n\n{text}") };
    for c in chats {
        if let Err(e) = say(&c.external_id, &body, json!({})).await {
            eprintln!("[vyn telegram notice] {}", e.message);
        }
    }
    Ok(())
}
