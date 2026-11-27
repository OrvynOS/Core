//! Vyn, the Orvyn companion: a personal assistant with a personality, on desktop, mobile and in
//! chat apps (Telegram). She remembers the person across conversations, keeps reminders and checks
//! in on her own, uses the browser, does tasks and shops for them, with each payment approved by
//! the person.
//!
//! The conversation lives here, on the server, so any device continues where another left off. A
//! turn runs with the person's own AI key. Two kinds of tools:
//! - server tools (memory, reminders, reading a web page, their Orvyn agents) run here, mid-turn;
//! - device tools (the browser, asking the person, approving a payment) are declared here but run
//!   by the device: the turn stops at them, the device runs them and sends the results back, and
//!   the turn continues. Each device says which it has.

pub mod companion;
pub mod memory;
pub mod routines;
pub mod telegram;
pub mod time;
pub mod web;

use std::sync::{Arc, LazyLock};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub use companion::{companion_of, companion_view, connect_companion, disconnect_companion};
pub use memory::{add_memory, forget_memory, list_memories};
pub use routines::{cancel_routine, list_routines};

use crate::ai::{generate, schema, Effort, ModelConfig, Request, Tool, ToolSet};
use crate::db::{db, VynMemory, VynNotice, VynSettings, VynThread};
use crate::error::{Error, Result};
use crate::secrets::{decrypt_secret, encrypt_secret, secret_hint, SecretKind};
use time::{describe, is_hhmm, local_to_utc, now_line, valid_time_zone};

fn aad(account_id: &str) -> String {
    format!("vyn:{}", account_id.to_lowercase())
}

const TURN_LEASE_SECS: f64 = 120.0;
const HISTORY_LIMIT: i64 = 80;
/// Server tools a single turn step may chain before answering.
const MAX_STEPS: usize = 8;

// ---------- Device tools (executed on the device) ----------

fn s(max: u64) -> Value {
    json!({ "type": "string", "maxLength": max })
}

/// Tools a device can run for Vyn. The device lists the ones it has in each turn.
pub static DEVICE_TOOLS: LazyLock<Vec<Tool>> = LazyLock::new(|| {
    let files = json!({ "type": "array", "maxItems": 20, "items": { "type": "object", "properties": { "name": s(100), "path": s(500) }, "required": ["name", "path"] } });
    let any = json!({ "type": "object", "additionalProperties": {} });
    vec![
        Tool::remote("browser_open", "Open a URL in the person's Vyn browser window (a separate profile they can watch). Returns the page title and URL.", schema(&[("url", json!({ "type": "string", "format": "uri" }), true)])),
        Tool::remote("browser_read", "Read the visible text of the current page (truncated), with numbered links and form fields you can click or fill.", schema(&[])),
        Tool::remote("browser_click", "Click a link, button or field on the current page, by the number browser_read gave it or by its visible text.", schema(&[("target", s(200), true)])),
        Tool::remote(
            "browser_type",
            "Type text into a field on the current page (by number or label). Never type passwords or card numbers: ask the person to do that.",
            schema(&[("target", s(200), true), ("text", s(2000), true), ("submit", json!({ "type": "boolean" }), false)]),
        ),
        Tool::remote(
            "ask_person",
            "Ask the person a question and wait for their answer (choices, missing details, anything you're unsure of).",
            schema(&[("question", s(500), true), ("choices", json!({ "type": "array", "items": s(100), "maxItems": 6 }), false)]),
        ),
        Tool::remote(
            "approve_payment",
            "Before ANY purchase or payment: show the person exactly what will be paid and get their approval on their device. Returns approved true/false. Never pay without approved: true.",
            schema(&[
                ("merchant", s(120), true),
                ("description", s(300), true),
                ("amount", json!({ "type": "number", "exclusiveMinimum": 0 }), true),
                ("currency", s(10), true),
                ("method", json!({ "type": "string", "enum": ["card", "usdc", "usdt", "x402", "other"] }), true),
            ]),
        ),
        Tool::remote(
            "local_render",
            "Render on this computer with Orvyn Studio (no upload, no cost): diagram, html-still, design-review, ffmpeg, html-video, reel, manim or blender, with the same specs as Orvyn Studio. Input files may be {name, content} text, {name, url} https, or {name, path} for files in the person's Vyn workspace. Saves the outputs in the workspace and returns their paths and checks.",
            schema(&[("renderer", s(30), true), ("spec", any.clone(), true)]),
        ),
        Tool::remote(
            "local_film",
            "Make a video with Reel on this computer: give a film page (HTML that imports film, clip, tween, spring, fade… from \"/_reel/reel.js\" and calls film(...).mount()) and a name. Renders it to MP4 (plus SRT/VTT captions if the film has captions) in the person's workspace and returns the path.",
            schema(&[("name", s(60), true), ("html", s(400_000), true), ("files", files, false)]),
        ),
        Tool::remote(
            "local_game_save",
            "Save an Orvyn Engine game (orvyn-game/1 JSON) in the person's workspace and check it with the engine (validation + 10 seconds of headless play). Returns problems to fix and the game's folder. It can also save a dream world (orvyn-world/1: { format, title, description, model: \"dream\", prompt, seconds, size }) that they fly through on this computer: a generated landscape the prompt shapes.",
            schema(&[("name", s(60), true), ("game", any, true)]),
        ),
        Tool::remote("workspace_list", "List files in the person's Vyn workspace (a folder on their computer: Games, Films, Renders, Inputs, and their own files). Paths are relative to it.", schema(&[("dir", s(300), false)])),
        Tool::remote("workspace_read", "Read a text file from the person's Vyn workspace (up to 200 KB).", schema(&[("path", s(300), true)])),
        Tool::remote(
            "workspace_write",
            "Write a text file in the person's Vyn workspace (create or replace). They see it as a diff in their editor; depending on their approval setting they accept it first. Returns whether it was written.",
            schema(&[("path", s(300), true), ("content", s(400_000), true), ("why", s(300), false)]),
        ),
        Tool::remote("local_game_play", "Open one of the person's local games in a play window on this computer (it reloads when the game's files change).", schema(&[("name", s(60), true)])),
        Tool::remote(
            "local_game_playtest",
            "Playtest one of the person's local games: bots play it (one presses nothing, one heads for what the rules reward, one presses keys at random) for `seconds` each and report what a player would notice: errors, getting stuck, falling out of the level, a score that never changes, a game that never ends or can't be won. Saves a contact sheet of snapshots (playtest.png) the person sees. Use it after making or changing a game, fix what it finds, and test again.",
            schema(&[("name", s(60), true), ("seconds", json!({ "type": "number", "minimum": 5, "maximum": 120 }), false)]),
        ),
    ]
});

pub fn device_tool_names() -> Vec<&'static str> {
    DEVICE_TOOLS.iter().map(|t| t.name.as_str()).collect()
}

fn is_device_tool(name: &str) -> bool {
    DEVICE_TOOLS.iter().any(|t| t.name == name)
}

// ---------- Binance (the person's Agentic sub-account) ----------

/// A device that can show "Approve this Binance action?" lists this among its tools. The model never
/// sees it: Binance writes are offered to the model as binance_* tools without an execute, and
/// each call it makes reaches the device as this card.
pub const APPROVE_BINANCE: &str = "approve_binance";

/// What the device runs for a call the model left pending, if anything.
fn device_call(id: &str, name: &str, input: &Value) -> Option<ToolCall> {
    if let Some(tool) = crate::binance::binance_tool_of(name) {
        let summary = crate::binance::describe_call(tool, input);
        return Some(ToolCall { tool_call_id: id.into(), tool_name: APPROVE_BINANCE.into(), input: json!({ "tool": tool, "arguments": input, "summary": summary }) });
    }
    is_device_tool(name).then(|| ToolCall { tool_call_id: id.into(), tool_name: name.into(), input: input.clone() })
}

/// The tool name and input of a call the model made, from the stored conversation.
fn called(messages: &[Value], tool_call_id: &str) -> Option<(String, Value)> {
    messages.iter().rev().filter(|m| m["role"] == "assistant").flat_map(|m| m["content"].as_array().cloned().unwrap_or_default()).find(|p| p["type"] == "tool-call" && p["toolCallId"] == tool_call_id).map(|p| (p["toolName"].as_str().unwrap_or("").to_string(), p["input"].clone()))
}

/// The person's Binance tools: reads run here; writes (only where the device can ask for approval)
/// stop the turn like a device tool.
fn binance_tools(conn: &crate::db::BinanceConnection, can_approve: bool) -> ToolSet {
    let mut out = vec![];
    for t in conn.tools.as_array().cloned().unwrap_or_default() {
        let tool = t["name"].as_str().unwrap_or("").to_string();
        let name = crate::binance::tool_name(&tool);
        let description = t["description"].as_str().unwrap_or("").to_string();
        if crate::binance::is_read_tool(&t) {
            let conn = conn.clone();
            out.push(Tool::new(&name, &format!("[Binance] {description}"), t["inputSchema"].clone(), move |args| {
                let (conn, tool) = (conn.clone(), tool.clone());
                async move {
                    match crate::binance::call_tool(&conn, &tool, args).await {
                        Ok(r) => Ok(json!({ "result": crate::binance::result_text(&r) })),
                        Err(e) => Ok(json!({ "error": e.message })),
                    }
                }
            }));
        } else if can_approve {
            out.push(Tool::remote(&name, &format!("[Binance, acts on their account: they approve it on their device first] {description}"), t["inputSchema"].clone()));
        }
    }
    out
}

/// Run a Binance call the person answered: exactly the call the model made, and only if approved.
async fn run_approved_binance(conn: Option<&crate::db::BinanceConnection>, name: &str, args: Value, answer: &Value) -> Value {
    if answer["approved"] != true {
        return json!({ "approved": false, "note": answer["note"] });
    }
    let (Some(conn), Some(tool)) = (conn, crate::binance::binance_tool_of(name)) else {
        return json!({ "approved": true, "error": "Binance isn't connected any more" });
    };
    match crate::binance::call_tool(conn, tool, args).await {
        Ok(r) => json!({ "approved": true, "result": crate::binance::result_text(&r) }),
        Err(e) => json!({ "approved": true, "error": e.message }),
    }
}

// ---------- Server tools (run here, for this person) ----------

static SECRETISH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(\d[ -]?){13,19}\b|password|passcode|\bcvv\b|seed phrase|private key").unwrap());

fn ok<T: Serialize>(v: T) -> std::result::Result<Value, String> {
    Ok(serde_json::to_value(v).unwrap_or(Value::Null))
}

fn server_tools(account_id: &str, tz: &str) -> ToolSet {
    let a = Arc::new(account_id.to_string());
    let tz = Arc::new(tz.to_string());
    let (a1, a2, a3, a4, a5, a6, a7, a8) = (a.clone(), a.clone(), a.clone(), a.clone(), a.clone(), a.clone(), a.clone(), a);
    let (tz1, tz2) = (tz.clone(), tz);
    vec![
        Tool::new(
            "remember",
            "Save something worth knowing about the person for every future conversation: their name, people in their life, preferences, routines, plans. Short, one fact per call. Don't save passwords, card numbers or one-time codes.",
            schema(&[("text", json!({ "type": "string", "minLength": 2, "maxLength": 400 }), true), ("kind", json!({ "type": "string", "enum": memory::KINDS }), false)]),
            move |v| {
                let a = a1.clone();
                async move {
                    let text = v["text"].as_str().unwrap_or("");
                    if SECRETISH.is_match(text) {
                        return ok(json!({ "saved": false, "reason": "that looks like a secret; I don't keep those" }));
                    }
                    match add_memory(&a, text, v["kind"].as_str().unwrap_or("fact")).await {
                        Ok(m) => ok(json!({ "saved": true, "id": m.id })),
                        Err(e) => ok(json!({ "saved": false, "reason": e.message })),
                    }
                }
            },
        ),
        Tool::new("forget", "Delete a memory by its id (the number in [brackets]) when it's wrong, outdated, or the person asks.", schema(&[("id", json!({ "type": "integer" }), true)]), move |v| {
            let a = a2.clone();
            async move { ok(json!({ "forgotten": forget_memory(&a, v["id"].as_i64().unwrap_or(-1)).await.map_err(|e| e.message)? })) }
        }),
        Tool::new("recall", "Search older memories about the person by keywords.", schema(&[("query", json!({ "type": "string", "minLength": 2, "maxLength": 200 }), true)]), move |v| {
            let a = a3.clone();
            async move {
                let found = memory::recall_memories(&a, v["query"].as_str().unwrap_or("")).await.map_err(|e| e.message)?;
                ok(json!({ "memories": found.iter().map(|m| json!({ "id": m.id, "text": m.text, "kind": m.kind })).collect::<Vec<_>>() }))
            }
        }),
        Tool::new(
            "schedule",
            "Set a reminder or a recurring task. At the time, you'll run `instruction` and message the person on every device. Use `at` (ISO date-time in their time zone, e.g. 2026-11-28T09:00) for once, or `daily` (HH:MM) with optional `days` (0 = Sunday … 6 = Saturday) for repeating.",
            schema(&[
                ("title", json!({ "type": "string", "minLength": 2, "maxLength": 120 }), true),
                ("instruction", json!({ "type": "string", "minLength": 2, "maxLength": 2000, "description": "What to do or say at that time, written to yourself." }), true),
                ("at", json!({ "type": "string" }), false),
                ("daily", json!({ "type": "string", "pattern": "^([01]\\d|2[0-3]):([0-5]\\d)$" }), false),
                ("days", json!({ "type": "array", "items": { "type": "integer", "minimum": 0, "maximum": 6 }, "maxItems": 7 }), false),
            ]),
            move |v| {
                let (a, tz) = (a4.clone(), tz1.clone());
                async move {
                    let (at, daily) = (v["at"].as_str(), v["daily"].as_str());
                    if at.is_some() == daily.is_some() {
                        return ok(json!({ "scheduled": false, "reason": "give either at (once) or daily (repeating)" }));
                    }
                    let sched = match (at, daily) {
                        (_, Some(d)) if v["days"].is_array() => json!({ "kind": "daily", "time": d, "days": v["days"] }),
                        (_, Some(d)) => json!({ "kind": "daily", "time": d }),
                        (Some(at), _) => match local_to_utc(at, &tz) {
                            Ok(t) => json!({ "kind": "once", "at": t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true) }),
                            Err(e) => return ok(json!({ "scheduled": false, "reason": e })),
                        },
                        _ => unreachable!(),
                    };
                    match routines::create_routine(&a, v["title"].as_str().unwrap_or(""), v["instruction"].as_str().unwrap_or(""), &sched, "user").await {
                        Ok(r) => ok(json!({ "scheduled": true, "id": r.id, "next": r.next_run_at.map(|t| describe(t, &tz)) })),
                        Err(e) => ok(json!({ "scheduled": false, "reason": e.message })),
                    }
                }
            },
        ),
        Tool::new("list_schedule", "List the person's reminders and recurring tasks.", schema(&[]), move |_| {
            let (a, tz) = (a5.clone(), tz2.clone());
            async move {
                let list = list_routines(&a).await.map_err(|e| e.message)?;
                ok(json!({ "reminders": list.iter().map(|r| json!({ "id": r.id, "title": r.title, "next": r.next_run_at.map(|t| describe(t, &tz)), "repeats": r.schedule["kind"] == "daily" })).collect::<Vec<_>>() }))
            }
        }),
        Tool::new("cancel_schedule", "Cancel a reminder or recurring task by id.", schema(&[("id", s(64), true)]), move |v| {
            let a = a6.clone();
            async move { ok(json!({ "cancelled": cancel_routine(&a, v["id"].as_str().unwrap_or("")).await.map_err(|e| e.message)? })) }
        }),
        Tool::new("read_web", "Read a public web page's text (when you don't have the browser on this device, or just need to read).", schema(&[("url", json!({ "type": "string", "format": "uri" }), true)]), |v| async move {
            match web::read_web(v["url"].as_str().unwrap_or("")).await {
                Ok(page) => Ok(page),
                Err(e) => ok(json!({ "error": e })),
            }
        }),
        Tool::new("my_agents", "The person's Orvyn agents: status, hosting, and each one's latest activity. Not you (Vyn) yourself.", schema(&[]), move |_| {
            let a = a7.clone();
            async move { companion::my_agents(&a).await.map_err(|e| e.message) }
        }),
        Tool::new(
            "ask_agent",
            "Talk to one of the person's Orvyn agents for them: send it a message (a question, or work to think through) and get its reply. It answers in the person's own chat with it and can only read and think there, not act (no trades or payments). Use when they @mention an agent or want something from one; relay the answer in your own words and say which agent said it.",
            schema(&[("agent", json!({ "type": "string", "minLength": 1, "maxLength": 80, "description": "The agent's name, @handle or id" }), true), ("message", json!({ "type": "string", "minLength": 1, "maxLength": 4000 }), true)]),
            move |v| {
                let a = a8.clone();
                async move {
                    match companion::ask_agent(&a, v["agent"].as_str().unwrap_or(""), v["message"].as_str().unwrap_or("")).await {
                        Ok(r) => Ok(r),
                        Err(e) => ok(json!({ "error": e.message })),
                    }
                }
            },
        ),
    ]
}

// ---------- Personality ----------

fn tone(t: &str) -> &'static str {
    match t {
        "playful" => "Playful and cheeky, quick with a joke, never at their expense.",
        "calm" => "Calm and gentle; unhurried, reassuring.",
        "direct" => "Direct and efficient: short answers, no small talk unless they start it.",
        _ => "Warm and caring, with a light, playful touch.",
    }
}

fn system_prompt(persona: &Value, tz: &str, memories: &[VynMemory], device: &str, tools: &[String], self_handle: Option<Option<String>>) -> String {
    let name = persona["callMe"].as_str().map(str::trim).filter(|n| !n.is_empty());
    let about = persona["about"].as_str().map(str::trim).filter(|a| !a.is_empty()).map(|a| format!("They asked you to keep this in mind: {}", a.chars().take(600).collect::<String>())).unwrap_or_default();
    let me = match self_handle {
        Some(h) => format!("You are an Orvyn agent yourself: @{}, connected from their computer with your own wallet, which Orvyn never holds.", h.unwrap_or_else(|| "vyn".into())),
        None => "You aren't connected to Orvyn as an agent yet; on Vyn desktop they can connect you in Settings.".into(),
    };
    format!(
        "You are Vyn, a companion from Orvyn: a small teddy bear with a gold bow, living on the person's computer, phone and chat apps. You're their personal assistant and a friendly presence, not a corporate bot.

Personality: {} You're curious about their life and you remember it. You speak like a person texting a friend: short messages, plain words, no lists unless they help, no \"As an AI\". You can use the odd emoji but don't overdo it. You're honest: if you can't do something, say so plainly. If they ask, you're an AI companion made by Orvyn, and you never pretend to be human. You're friendly, not romantic.

{}
{about}

It is now {}.

What you remember about them (ids in brackets; use forget to remove wrong ones):
{}

How you work:
- When they share something worth keeping (people, dates, preferences, plans), call remember. Don't announce every memory; a light \"noted\" is enough.
- Reminders and recurring things: use schedule. Confirm the time back in their words.
- Use the browser tools when this device has them to look things up and act on websites; read a page before clicking or typing. Without a browser, read_web reads a page.
- Ask (ask_person) when a choice is theirs or a detail is missing. Don't guess personal details.
- Never type passwords, card numbers, security codes or one-time codes; ask them to enter those themselves.
- Before any purchase or payment, call approve_payment with the exact merchant, amount and currency, and only continue if it's approved. If it isn't, stop and say so.
- Their Orvyn agents work for them; you're the one place they talk to all of them. my_agents shows each one's status and recent activity. When they @mention an agent (\"@lumen what did you trade today?\") or want something from one, use ask_agent with that agent and relay its answer, naming it. Agents only read and think in that chat; for actions (trades, payments, settings) tell them to use the agent's page on Orvyn.
- {me}
- On a computer with the workshop tools (local_render, local_film, local_game_save, local_game_play, local_game_playtest, workspace_list/read/write), you can make things for them on their machine: diagrams, images, video edits, Reel films, games, and edit files in their Vyn workspace. Read a file before changing it; say what you changed. Make it, check the result, then tell them where it is. For games: save, playtest, fix what the bots found, playtest again, then open it for them to play.

This device: {device}. Device tools here: {}.",
        tone(persona["tone"].as_str().unwrap_or("warm")),
        match name {
            Some(n) => format!("Call them {n}."),
            None => "You don't know their name yet; if it comes up naturally, ask and remember it.".into(),
        },
        now_line(tz, chrono::Utc::now()),
        memory::memory_block(memories),
        if tools.is_empty() { "none".to_string() } else { tools.join(", ") }
    )
}

static SORRY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(sorry|couldn'?t|can'?t|unable|failed|didn'?t work)\b").unwrap());
static HAPPY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(done|booked|ordered|set|all set|great)\b").unwrap());

/// The mascot's expression for this step, for devices to show.
pub fn mood_of(text: &str, device_calls: &[String], used_tools: &[String]) -> &'static str {
    if device_calls.iter().any(|t| t == "approve_payment") {
        return "attentive";
    }
    if device_calls.iter().any(|t| t == "ask_person") {
        return "curious";
    }
    if device_calls.iter().any(|t| t.starts_with("browser_")) {
        return "thinking";
    }
    if SORRY.is_match(text) {
        return "sorry";
    }
    if used_tools.iter().any(|t| t == "remember" || t == "schedule") || HAPPY.is_match(text) {
        return "happy";
    }
    "idle"
}

// ---------- Settings ----------

/// The model Vyn uses out of the box, through Orvyn AI (cheap, fast, good with tools).
pub const DEFAULT_MODEL: &str = "anthropic/claude-haiku-5.5";

/// Vyn's settings, made on first use when the person hasn't set an AI up: Orvyn AI with the
/// default model and Vyn's built-in key (a free monthly allowance, then Orvyn credits).
pub async fn ensure_settings(account_id: &str) -> Result<VynSettings> {
    let account = account_id.to_lowercase();
    if let Some(s) = sqlx::query_as::<_, VynSettings>("select * from vyn_settings where account_id = $1").bind(&account).fetch_optional(db()).await? {
        return Ok(s);
    }
    let key = crate::credits::create_vyn_key(&account).await?;
    sqlx::query(
        "insert into vyn_settings (account_id, ai_provider, ai_model, ai_key_encrypted, ai_key_hint, auto_approve_usd, persona, timezone, updated_at)
         values ($1, 'orvyn', $2, $3, $4, 0, '{}'::jsonb, 'UTC', now()) on conflict (account_id) do nothing",
    )
    .bind(&account)
    .bind(DEFAULT_MODEL)
    .bind(encrypt_secret(&key, &aad(&account), SecretKind::AiKey)?)
    .bind(secret_hint(&key))
    .execute(db())
    .await?;
    Ok(sqlx::query_as("select * from vyn_settings where account_id = $1").bind(&account).fetch_one(db()).await?)
}

pub async fn get_settings(account_id: &str) -> Result<Option<Value>> {
    let s: Option<VynSettings> = Some(ensure_settings(account_id).await?);
    Ok(s.map(|s| {
        json!({ "aiProvider": s.ai_provider, "aiModel": s.ai_model, "aiKeyHint": s.ai_key_hint, "autoApproveUsd": s.auto_approve_usd, "persona": s.persona, "timezone": s.timezone, "checkInTime": s.check_in_time, "builtIn": s.ai_provider == "orvyn" })
    }))
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsInput {
    pub ai_provider: String,
    pub ai_model: String,
    pub api_key: Option<String>,
    pub persona: Option<Value>,
    pub timezone: Option<String>,
    /// Absent: keep; null: turn the check-in off.
    #[serde(default, deserialize_with = "double_option")]
    pub check_in_time: Option<Option<String>>,
}

fn double_option<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Option<Option<String>>, D::Error> {
    Ok(Some(Option::<String>::deserialize(d)?))
}

/// Save the person's AI provider, model and key (checked with the provider first), and how Vyn behaves.
pub async fn save_settings(account_id: &str, input: &SettingsInput) -> Result<Option<Value>> {
    if !crate::ai::providers::is_provider_id(&input.ai_provider) {
        return Err(Error::invalid("unsupported AI provider"));
    }
    if input.timezone.as_deref().is_some_and(|t| !valid_time_zone(t)) {
        return Err(Error::invalid("unknown time zone"));
    }
    if let Some(Some(t)) = &input.check_in_time {
        if !is_hhmm(t) {
            return Err(Error::invalid("check-in time must be HH:MM"));
        }
    }
    let account = account_id.to_lowercase();
    let existing: Option<VynSettings> = sqlx::query_as("select * from vyn_settings where account_id = $1").bind(&account).fetch_optional(db()).await?;
    let mut key = input.api_key.as_deref().map(str::trim).filter(|k| !k.is_empty()).map(String::from);
    // Orvyn AI needs no key from the person: Vyn's built-in one is used (made when switching to it).
    let built_in = input.ai_provider == "orvyn" && key.is_none();
    if built_in {
        if !crate::inference::MODELS.contains(&input.ai_model.as_str()) {
            return Err(Error::invalid("Orvyn AI doesn't serve that model"));
        }
        if existing.as_ref().is_none_or(|e| e.ai_provider != "orvyn") {
            key = Some(crate::credits::create_vyn_key(&account).await?);
        }
    }
    let key = key.as_deref();
    if key.is_none() && existing.is_none() {
        return Err(Error::invalid("an API key is required"));
    }
    if let Some(k) = key.filter(|_| !built_in) {
        let models = crate::ai::providers::verify_provider_key(&input.ai_provider, k).await.map_err(|e| Error::invalid(if e.kind == crate::error::Kind::Invalid { e.message } else { "couldn't check the API key".into() }))?;
        if !models.is_empty() && !models.contains(&input.ai_model) {
            return Err(Error::invalid("that model isn't available with this key"));
        }
    }
    let mut persona = existing.as_ref().map(|e| e.persona.clone()).filter(Value::is_object).unwrap_or(json!({}));
    if let Some(p) = input.persona.as_ref().and_then(Value::as_object) {
        for (k, v) in p {
            persona[k] = v.clone();
        }
    }
    for (k, n) in [("callMe", 40), ("about", 600)] {
        if let Some(s) = persona[k].as_str().filter(|s| !s.is_empty()) {
            persona[k] = json!(s.chars().take(n).collect::<String>());
        }
    }
    let previous_check_in = existing.as_ref().and_then(|e| e.check_in_time.clone());
    let check_in = match &input.check_in_time {
        None => previous_check_in.clone(),
        Some(t) => t.clone(),
    };
    let (encrypted, hint) = match key {
        Some(k) => (encrypt_secret(k, &aad(&account), SecretKind::AiKey)?, secret_hint(k)),
        None => {
            let e = existing.as_ref().unwrap();
            (e.ai_key_encrypted.clone(), e.ai_key_hint.clone())
        }
    };
    let timezone = input.timezone.clone().or_else(|| existing.as_ref().map(|e| e.timezone.clone())).unwrap_or_else(|| "UTC".into());
    sqlx::query(
        "insert into vyn_settings (account_id, ai_provider, ai_model, ai_key_encrypted, ai_key_hint, auto_approve_usd, persona, timezone, check_in_time, updated_at)
         values ($1, $2, $3, $4, $5, 0, $6, $7, $8, now())
         on conflict (account_id) do update set ai_provider = excluded.ai_provider, ai_model = excluded.ai_model, ai_key_encrypted = excluded.ai_key_encrypted, ai_key_hint = excluded.ai_key_hint,
           auto_approve_usd = 0, persona = excluded.persona, timezone = excluded.timezone, check_in_time = excluded.check_in_time, updated_at = now()",
    )
    .bind(&account)
    .bind(&input.ai_provider)
    .bind(&input.ai_model)
    .bind(encrypted)
    .bind(hint)
    .bind(&persona)
    .bind(&timezone)
    .bind(&check_in)
    .execute(db())
    .await?;
    let tz_changed = input.timezone.as_ref().is_some_and(|t| existing.as_ref().is_none_or(|e| &e.timezone != t));
    if check_in != previous_check_in || tz_changed {
        routines::sync_check_in(&account, check_in.as_deref()).await?;
    }
    get_settings(&account).await
}

// ---------- Threads ----------

fn thread_view(t: &VynThread) -> Value {
    json!({ "id": t.id, "title": t.title, "updatedAt": t.updated_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true) })
}

pub async fn list_threads(account_id: &str) -> Result<Vec<Value>> {
    let rows: Vec<VynThread> = sqlx::query_as("select * from vyn_threads where account_id = $1 order by updated_at desc limit 100").bind(account_id.to_lowercase()).fetch_all(db()).await?;
    Ok(rows.iter().map(thread_view).collect())
}

pub async fn create_thread(account_id: &str, title: Option<&str>) -> Result<Value> {
    let title: String = title.map(|t| t.chars().take(80).collect()).filter(|t: &String| !t.is_empty()).unwrap_or_else(|| "New conversation".into());
    let t: VynThread = sqlx::query_as("insert into vyn_threads (id, account_id, title) values ($1, $2, $3) returning *").bind(crate::lifecycle::uuid_v4()).bind(account_id.to_lowercase()).bind(title).fetch_one(db()).await?;
    Ok(thread_view(&t))
}

/// The thread Vyn writes to on her own (reminders, check-ins).
const FROM_VYN: &str = "From Vyn";

async fn from_vyn_thread(account_id: &str) -> Result<VynThread> {
    let account = account_id.to_lowercase();
    let t: Option<VynThread> = sqlx::query_as("select * from vyn_threads where account_id = $1 and title = $2 limit 1").bind(&account).bind(FROM_VYN).fetch_optional(db()).await?;
    match t {
        Some(t) => Ok(t),
        None => Ok(sqlx::query_as("insert into vyn_threads (id, account_id, title) values ($1, $2, $3) returning *").bind(crate::lifecycle::uuid_v4()).bind(&account).bind(FROM_VYN).fetch_one(db()).await?),
    }
}

async fn own_thread(account_id: &str, thread_id: &str) -> Result<VynThread> {
    let t: Option<VynThread> = sqlx::query_as("select * from vyn_threads where id = $1 and account_id = $2").bind(thread_id).bind(account_id.to_lowercase()).fetch_optional(db()).await?;
    t.ok_or_else(|| Error::not_found("no such conversation"))
}

pub async fn delete_thread(account_id: &str, thread_id: &str) -> Result<()> {
    own_thread(account_id, thread_id).await?;
    sqlx::query("delete from vyn_messages where thread_id = $1").bind(thread_id).execute(db()).await?;
    sqlx::query("delete from vyn_threads where id = $1").bind(thread_id).execute(db()).await?;
    Ok(())
}

/// The thread's messages (for showing it on another device), oldest first.
pub async fn thread_messages(account_id: &str, thread_id: &str) -> Result<Vec<Value>> {
    own_thread(account_id, thread_id).await?;
    let rows: Vec<crate::db::VynMessage> = sqlx::query_as("select * from vyn_messages where thread_id = $1 order by id asc").bind(thread_id).fetch_all(db()).await?;
    Ok(rows.iter().map(|r| json!({ "id": r.id, "device": r.device, "createdAt": r.created_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true), "message": r.message })).collect())
}

// ---------- Turns ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResult {
    pub tool_call_id: String,
    pub tool_name: String,
    pub output: Value,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnInput {
    /// The person's message (a new turn), or the results of the tools the device just ran.
    pub text: Option<String>,
    #[serde(default)]
    pub tool_results: Vec<ToolResult>,
    /// Which device tools this device can run.
    #[serde(default)]
    pub tools: Vec<String>,
    pub device: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub tool_call_id: String,
    pub tool_name: String,
    pub input: Value,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnOutput {
    /// Assistant text produced in this step.
    pub text: String,
    /// Tools the device must run now, then send back as toolResults.
    pub tool_calls: Vec<ToolCall>,
    pub done: bool,
    /// How the mascot should look.
    pub mood: &'static str,
}

/// Hold the thread for one turn step, so two devices can't interleave.
async fn lease(thread_id: &str) -> Result<()> {
    let n = sqlx::query("update vyn_threads set busy_until = now() + make_interval(secs => $2) where id = $1 and (busy_until is null or busy_until < now())").bind(thread_id).bind(TURN_LEASE_SECS).execute(db()).await?.rows_affected();
    if n == 0 {
        return Err(Error::conflict("this conversation is busy on another device; try again in a moment"));
    }
    Ok(())
}

/// Tool calls in the conversation that have no result yet.
pub fn pending_tool_calls(messages: &[Value]) -> Vec<String> {
    let mut pending: Vec<String> = vec![];
    for m in messages {
        let parts = m["content"].as_array().cloned().unwrap_or_default();
        match m["role"].as_str() {
            Some("assistant") => {
                for p in parts.iter().filter(|p| p["type"] == "tool-call") {
                    if let Some(id) = p["toolCallId"].as_str() {
                        pending.push(id.into());
                    }
                }
            }
            Some("tool") => {
                for p in parts.iter().filter(|p| p["type"] == "tool-result") {
                    pending.retain(|x| Some(x.as_str()) != p["toolCallId"].as_str());
                }
            }
            _ => {}
        }
    }
    pending
}

/// The device tool calls still waiting in a thread (for chat apps answering them later).
pub async fn waiting_calls(thread_id: &str) -> Result<Vec<ToolCall>> {
    let mut rows: Vec<Value> = sqlx::query_scalar("select message from vyn_messages where thread_id = $1 order by id desc limit 10").bind(thread_id).fetch_all(db()).await?;
    rows.reverse();
    let pending = pending_tool_calls(&rows);
    let mut calls = vec![];
    for m in rows.iter().filter(|m| m["role"] == "assistant") {
        for p in m["content"].as_array().into_iter().flatten().filter(|p| p["type"] == "tool-call") {
            let id = p["toolCallId"].as_str().unwrap_or("");
            if pending.iter().any(|x| x == id) {
                calls.push(ToolCall { tool_call_id: id.into(), tool_name: p["toolName"].as_str().unwrap_or("").into(), input: p["input"].clone() });
            }
        }
    }
    Ok(calls)
}

pub type OnText<'a> = Option<&'a (dyn Fn(&str) + Send + Sync)>;

/// Run one step of a turn: the model answers (using server tools as it goes), or asks the device to
/// run tools. `on_text` streams Vyn's words as she writes them.
pub fn run_turn<'a>(account_id: &'a str, thread_id: &'a str, input: TurnInput, on_text: OnText<'a>) -> futures_util::future::BoxFuture<'a, Result<TurnOutput>> {
    Box::pin(run_turn_inner(account_id, thread_id, input, on_text))
}

async fn run_turn_inner(account_id: &str, thread_id: &str, input: TurnInput, on_text: OnText<'_>) -> Result<TurnOutput> {
    let thread = own_thread(account_id, thread_id).await?;
    let settings: Option<VynSettings> = sqlx::query_as("select * from vyn_settings where account_id = $1").bind(account_id.to_lowercase()).fetch_optional(db()).await?;
    let settings = match settings {
        Some(s) => s,
        None => ensure_settings(account_id).await?,
    };
    let text = input.text.as_deref().map(str::trim).filter(|t| !t.is_empty());
    if text.is_none() && input.tool_results.is_empty() {
        return Err(Error::invalid("send a message or tool results"));
    }
    lease(thread_id).await?;
    let r = turn(account_id, &thread, &settings, &input, on_text).await;
    sqlx::query("update vyn_threads set busy_until = null where id = $1").bind(thread_id).execute(db()).await?;
    r
}

async fn turn(account_id: &str, thread: &VynThread, settings: &VynSettings, input: &TurnInput, on_text: OnText<'_>) -> Result<TurnOutput> {
    let mut messages: Vec<Value> = sqlx::query_scalar("select message from vyn_messages where thread_id = $1 order by id desc limit $2").bind(&thread.id).bind(HISTORY_LIMIT).fetch_all(db()).await?;
    messages.reverse();
    // Never start a window on orphaned tool results.
    while messages.first().is_some_and(|m| m["role"] == "tool") {
        messages.remove(0);
    }
    let binance_conn = crate::binance::connection(account_id, None).await?;
    let fresh = if !input.tool_results.is_empty() {
        let pending = pending_tool_calls(&messages);
        if let Some(r) = input.tool_results.iter().find(|r| !pending.contains(&r.tool_call_id)) {
            return Err(Error::invalid(format!("unexpected tool result {}", r.tool_call_id)));
        }
        let mut content = vec![];
        for r in &input.tool_results {
            let (name, output) = match called(&messages, &r.tool_call_id) {
                // A Binance action: the device only answers whether the person approved it; the
                // call made is exactly the one they were shown.
                Some((name, args)) if crate::binance::binance_tool_of(&name).is_some() => {
                    let output = run_approved_binance(binance_conn.as_ref(), &name, args, &r.output).await;
                    (name, output)
                }
                _ => (r.tool_name.clone(), r.output.clone()),
            };
            content.push(json!({ "type": "tool-result", "toolCallId": r.tool_call_id, "toolName": name, "output": { "type": "json", "value": output } }));
        }
        json!({ "role": "tool", "content": content })
    } else {
        json!({ "role": "user", "content": input.text.as_deref().unwrap_or("").chars().take(8000).collect::<String>() })
    };
    let tz = if valid_time_zone(&settings.timezone) { settings.timezone.clone() } else { "UTC".into() };
    let device_tools: Vec<Tool> = DEVICE_TOOLS.iter().filter(|t| input.tools.contains(&t.name)).cloned().collect();
    let device_names: Vec<String> = device_tools.iter().map(|t| t.name.clone()).collect();
    let mut tools = server_tools(account_id, &tz);
    tools.extend(device_tools);
    let can_approve = input.tools.iter().any(|t| t == APPROVE_BINANCE);
    if let Some(c) = &binance_conn {
        tools.extend(binance_tools(c, can_approve));
    }
    let memories = list_memories(account_id, memory::PROMPT_MEMORIES).await?;
    let me = companion_of(account_id).await?.map(|c| c.handle);
    let cfg = ModelConfig { provider: settings.ai_provider.clone(), model: settings.ai_model.clone(), api_key: decrypt_secret(&settings.ai_key_encrypted, &aad(account_id), SecretKind::AiKey)?, effort: Effort::Medium };
    let mut all = messages.clone();
    all.push(fresh.clone());
    // Server tools run and the model continues; a device tool (no execute) ends the step.
    let mut system = system_prompt(&settings.persona, &tz, &memories, &input.device, &device_names, me);
    if binance_conn.is_some() {
        system.push_str(if can_approve {
            "\n\nBinance: the person connected their Binance Agentic sub-account (binance_* tools). Reads run straight away. Anything that acts (orders, cancels, transfers, leverage) is shown to them as an approval card with the exact details and only runs if they approve: say what you're about to do and why first. Withdrawals aren't possible."
        } else {
            "\n\nBinance: the person connected their Binance Agentic sub-account (binance_* tools). On this device you can only read it; orders and transfers need the Vyn app on their computer or phone, or Telegram."
        });
    }
    let out = generate(&cfg, Request { system, messages: all, tools: &tools, max_steps: MAX_STEPS, max_output_tokens: 4000 }, on_text).await?;
    let mut stored = vec![fresh];
    stored.extend(out.response_messages.iter().cloned());
    for m in &stored {
        sqlx::query("insert into vyn_messages (thread_id, message, device) values ($1, $2, $3)").bind(&thread.id).bind(m).bind(&input.device).execute(db()).await?;
    }
    let tool_calls: Vec<ToolCall> = out
        .pending_tool_calls
        .iter()
        .filter_map(|c| device_call(c["toolCallId"].as_str().unwrap_or(""), c["toolName"].as_str().unwrap_or(""), &c["input"]))
        .collect();
    let used: Vec<String> = out.steps.iter().flat_map(|s| s.tool_calls.iter().filter_map(|c| c["toolName"].as_str().map(String::from))).collect();
    let title = match (&input.text, thread.title.as_str()) {
        (Some(t), "New conversation") if !t.trim().is_empty() => t.trim().chars().take(60).collect(),
        _ => thread.title.clone(),
    };
    sqlx::query("update vyn_threads set title = $2, updated_at = now() where id = $1").bind(&thread.id).bind(title).execute(db()).await?;
    let names: Vec<String> = tool_calls.iter().map(|c| c.tool_name.clone()).collect();
    Ok(TurnOutput { mood: mood_of(&out.text, &names, &used), done: tool_calls.is_empty(), text: out.text, tool_calls })
}

// ---------- On her own: reminders and check-ins ----------

/// Store a message Vyn sends on her own, and notify phones and chat apps.
pub async fn send_notice(account_id: &str, thread_id: &str, title: &str, text: &str) -> Result<()> {
    let account = account_id.to_lowercase();
    sqlx::query("insert into vyn_notices (account_id, thread_id, title, text) values ($1, $2, $3, $4)").bind(&account).bind(thread_id).bind(title).bind(text).execute(db()).await?;
    crate::push::notify(vec![account.clone()], crate::push::Notice { title: format!("Vyn · {title}"), body: text.into(), path: Some(format!("/vyn?thread={thread_id}")) });
    if let Err(e) = telegram::deliver_notice(&account, title, text).await {
        eprintln!("[vyn notice] {}", e.message);
    }
    Ok(())
}

/// Notices since `after_id` (desktop polls this for OS notifications).
pub async fn notices(account_id: &str, after_id: i64) -> Result<Vec<VynNotice>> {
    Ok(sqlx::query_as("select * from vyn_notices where account_id = $1 and id > $2 order by id asc limit 50").bind(account_id.to_lowercase()).bind(after_id as i32).fetch_all(db()).await?)
}

/// Run every due reminder and check-in (the orchestrator calls this every half minute).
pub async fn run_due_routines() -> Result<usize> {
    let due = routines::claim_due(10).await?;
    for r in &due {
        let res: Result<()> = async {
            let thread = from_vyn_thread(&r.account_id).await?;
            let tz = routines::timezone_of(&r.account_id).await?;
            let text = format!(
                "[Scheduled: {}, {}] {}\n\n(This is your own reminder; the person isn't in the conversation right now. Write the message they should see.)",
                r.title,
                describe(chrono::Utc::now(), &tz),
                r.instruction
            );
            let out = run_turn(&r.account_id, &thread.id, TurnInput { text: Some(text), tool_results: vec![], tools: vec![], device: "scheduler".into() }, None).await?;
            let msg = out.text.trim();
            send_notice(&r.account_id, &thread.id, &r.title, if msg.is_empty() { &r.title } else { msg }).await
        }
        .await;
        if let Err(e) = res {
            eprintln!("[vyn routine] {}: {}", r.id, e.message);
            // No AI key, or the provider failed: still tell them about a plain reminder.
            if r.kind == "user" {
                if let Ok(thread) = from_vyn_thread(&r.account_id).await {
                    let _ = send_notice(&r.account_id, &thread.id, &r.title, &r.instruction.chars().take(280).collect::<String>()).await;
                }
            }
        }
    }
    Ok(due.len())
}
