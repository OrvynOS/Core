//! Orvyn inference: many AI models behind one OpenAI-style gateway, paid from Orvyn credits.
//!
//! Every request (chat in the app, /v1/chat/completions with an Orvyn API key, agents on the
//! "orvyn" provider) runs here, on the API server:
//!   1. the caller is a signed-in account (session or `orv_sk_` key) with credit;
//!   2. the model must be in the curated list; `max_tokens` is capped to what the balance covers;
//!   3. the request goes upstream with Orvyn's own key (OPENROUTER_API_KEY), which never leaves the
//!      server and is never named to clients;
//!   4. the reply's upstream cost times ORVYN_INFERENCE_MARKUP is charged to the account.
//!
//! Prices come from the upstream's public model list, refreshed hourly.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::credits::{self, MICRO};
use crate::db::{db, ChatTurnMessage, ChatThread};
use crate::env::env;
use crate::error::{Error, Kind, Result};

pub const DEFAULT_MODEL: &str = "anthropic/claude-sonnet-5.5";
/// Orvyn's own model, listed as coming soon (not selectable yet).
pub const ORVYN_MODEL: &str = "orvyn/orvyn-1";

/// The models Orvyn offers, in the order the app lists them.
pub const MODELS: &[&str] = &[
    "anthropic/claude-sonnet-5.5",
    "anthropic/claude-opus-5.5",
    "anthropic/claude-fable-5.2",
    "anthropic/claude-haiku-5.5",
    "openai/gpt-6.2-sol",
    "openai/gpt-6.2-astra",
    "openai/gpt-6-luna",
    "google/gemini-4-neon",
    "google/gemini-4-hydrogen",
    "x-ai/grok-4.7",
    "deepseek/deepseek-v4.1-flash",
    "qwen/qwen4-max",
    "moonshotai/kimi-k3.1-pro",
    "z-ai/glm-5.3",
    "mistralai/mistral-large-4-0",
];

/// Requests need at least this much credit ($0.01).
const MIN_BALANCE_MICRO: i64 = 10_000;
const DEFAULT_MAX_TOKENS: u64 = 8192;
const MAX_TOKENS_CAP: u64 = 64_000;

fn upstream_base() -> String {
    std::env::var("ORVYN_INFERENCE_UPSTREAM").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "https://openrouter.ai/api/v1".into())
}

/// Where Orvyn's own processes reach the gateway (agents on the "orvyn" provider): the API's
/// /api/v1 paths, which every host serves.
pub fn gateway_base() -> String {
    format!("{}/api/v1", env().orvyn_api_url.trim_end_matches('/'))
}

fn upstream_key() -> Result<String> {
    env().openrouter_api_key.clone().ok_or_else(|| Error::unavailable("Orvyn models aren't available on this server yet."))
}

// ---------- Catalog and prices ----------

#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub maker: String,
    pub context_length: u64,
    /// Upstream USD per token.
    pub prompt: f64,
    pub completion: f64,
    pub tools: bool,
}

static PRICES: LazyLock<Mutex<Option<(Instant, HashMap<String, ModelInfo>)>>> = LazyLock::new(|| Mutex::new(None));

fn parse_price(v: &Value) -> f64 {
    v.as_str().and_then(|s| s.parse().ok()).or_else(|| v.as_f64()).unwrap_or(0.0).max(0.0)
}

async fn fetch_catalog() -> Result<HashMap<String, ModelInfo>> {
    let res = crate::http::client().get(format!("{}/models", upstream_base())).timeout(Duration::from_secs(20)).send().await.map_err(|e| Error::internal(format!("model list: {e}")))?;
    if !res.status().is_success() {
        return Err(Error::internal(format!("model list: {}", res.status())));
    }
    let body: Value = res.json().await.map_err(|e| Error::internal(format!("model list: {e}")))?;
    let mut out = HashMap::new();
    for m in body["data"].as_array().into_iter().flatten() {
        let Some(id) = m["id"].as_str() else { continue };
        if !MODELS.contains(&id) {
            continue;
        }
        let full = m["name"].as_str().unwrap_or(id);
        let (maker, name) = full.split_once(": ").map(|(a, b)| (a.to_string(), b.to_string())).unwrap_or_else(|| (id.split('/').next().unwrap_or("").to_string(), full.to_string()));
        out.insert(
            id.to_string(),
            ModelInfo {
                id: id.to_string(),
                name,
                maker,
                context_length: m["context_length"].as_u64().unwrap_or(0),
                prompt: parse_price(&m["pricing"]["prompt"]),
                completion: parse_price(&m["pricing"]["completion"]),
                tools: m["supported_parameters"].as_array().is_some_and(|p| p.iter().any(|x| x == "tools")),
            },
        );
    }
    Ok(out)
}

/// The curated models with their upstream prices (cached for an hour; a stale list is kept if a
/// refresh fails).
pub async fn catalog() -> Result<HashMap<String, ModelInfo>> {
    if let Some((at, m)) = PRICES.lock().unwrap().as_ref() {
        if at.elapsed() < Duration::from_secs(3600) {
            return Ok(m.clone());
        }
    }
    match fetch_catalog().await {
        Ok(m) if !m.is_empty() => {
            *PRICES.lock().unwrap() = Some((Instant::now(), m.clone()));
            Ok(m)
        }
        other => {
            if let Some((_, m)) = PRICES.lock().unwrap().as_ref() {
                return Ok(m.clone());
            }
            if let Err(e) = other {
                eprintln!("[inference] {}", e.message);
            }
            Err(Error::unavailable("Orvyn models are briefly unavailable. Try again in a minute."))
        }
    }
}

pub fn markup() -> f64 {
    env().inference_markup
}

/// The models as the app lists them: Orvyn's prices per million tokens, Orvyn Model (soon) first.
pub async fn models_view() -> Result<Value> {
    let cat = catalog().await?;
    let k = markup();
    let mut models = vec![json!({ "id": ORVYN_MODEL, "name": "Orvyn Model", "maker": "Orvyn", "soon": true })];
    for id in MODELS {
        let Some(m) = cat.get(*id) else { continue };
        models.push(json!({
            "id": m.id,
            "name": m.name,
            "maker": m.maker,
            "contextLength": m.context_length,
            "inputPerMillion": (m.prompt * k * 1e6 * 100.0).round() / 100.0,
            "outputPerMillion": (m.completion * k * 1e6 * 100.0).round() / 100.0,
            "tools": m.tools,
            "default": m.id == DEFAULT_MODEL,
        }));
    }
    Ok(json!({ "models": models, "defaultModel": DEFAULT_MODEL }))
}

/// GET /v1/models, OpenAI's shape.
pub async fn openai_models() -> Result<Value> {
    let cat = catalog().await?;
    let data: Vec<Value> = MODELS.iter().filter_map(|id| cat.get(*id)).map(|m| json!({ "id": m.id, "object": "model", "created": 0, "owned_by": m.maker, "context_length": m.context_length })).collect();
    Ok(json!({ "object": "list", "data": data }))
}

// ---------- The gateway ----------

/// What a request used and cost.
#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub model: String,
    pub micro: i64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// The reply's text (for chat).
    pub text: String,
}

/// Fields callers may set; anything else (upstream routing options and the like) is dropped.
const ALLOWED: &[&str] = &[
    "messages", "model", "max_tokens", "max_completion_tokens", "temperature", "top_p", "top_k", "stop", "tools", "tool_choice", "parallel_tool_calls", "response_format", "stream", "seed",
    "frequency_penalty", "presence_penalty", "repetition_penalty", "logit_bias", "reasoning", "user",
];

fn clean_message(m: &str) -> String {
    let m = crate::http::clip(m, 300);
    m.replace("OpenRouter", "the model host").replace("openrouter.ai", "the model host").replace("openrouter", "the model host")
}

fn upstream_error(status: u16, body: &str) -> Error {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let msg = v["error"]["message"].as_str().map(clean_message).unwrap_or_default();
    match status {
        400 | 404 | 413 | 422 => Error::invalid(if msg.is_empty() { "The model couldn't take that request.".into() } else { format!("The model couldn't take that request: {msg}") }),
        429 => Error::rate_limited("The model is busy. Try again in a moment, or pick another model."),
        // Orvyn's own upstream account (key or balance): never the person's fault.
        401 | 402 | 403 => {
            eprintln!("[inference] upstream refused Orvyn's key ({status}): {}", crate::http::clip(body, 300));
            Error::unavailable("Orvyn models are briefly unavailable. Try again shortly.")
        }
        _ => Error::unavailable("The model didn't answer. Try again, or pick another model."),
    }
}

fn cost_micro(info: &ModelInfo, upstream_cost: Option<f64>, input_tokens: u64, output_tokens: u64) -> i64 {
    let base = upstream_cost.filter(|c| c.is_finite() && *c >= 0.0).unwrap_or(input_tokens as f64 * info.prompt + output_tokens as f64 * info.completion);
    (base * markup() * MICRO).ceil() as i64
}

/// Read usage from a chunk or reply: (cost in USD, prompt tokens, completion tokens).
fn read_usage(u: &Value) -> (Option<f64>, u64, u64) {
    (u["cost"].as_f64(), u["prompt_tokens"].as_u64().unwrap_or(0), u["completion_tokens"].as_u64().unwrap_or(0))
}

/// What clients see of usage: Orvyn's charge, not the upstream's cost.
fn rewrite_usage(v: &mut Value, micro: i64) {
    if let Some(u) = v.get_mut("usage").and_then(Value::as_object_mut) {
        u.remove("cost_details");
        u.remove("is_byok");
        u.insert("cost".into(), json!(micro as f64 / MICRO));
    }
    if let Some(o) = v.as_object_mut() {
        o.remove("provider");
    }
}

/// Charge a finished request: Vyn's built-in key spends its monthly allowance first.
async fn settle(account: &str, source: &str, micro: i64, model: &str, input_tokens: u64, output_tokens: u64) -> Result<()> {
    if source == "vyn" {
        credits::charge_vyn(account, micro, model, input_tokens, output_tokens).await?;
    } else {
        credits::charge(account, micro, model, source, input_tokens, output_tokens).await?;
    }
    Ok(())
}

/// Run one chat completion for `account`, charging it. With `on_chunk`, the request streams and
/// each chunk (already rewritten for the client) is handed over as it arrives; the reply is then
/// `None`. Without it, the whole reply comes back.
pub async fn complete(account: &str, source: &str, body: Value, on_chunk: Option<&(dyn Fn(&Value) + Send + Sync)>) -> Result<(Option<Value>, Usage)> {
    let key = upstream_key()?;
    let Some(obj) = body.as_object() else { return Err(Error::invalid("The request body must be a JSON object.")) };
    let model = obj.get("model").and_then(Value::as_str).unwrap_or(DEFAULT_MODEL).to_string();
    if model == ORVYN_MODEL {
        return Err(Error::invalid("Orvyn Model is coming soon. Pick another model for now."));
    }
    let cat = catalog().await?;
    let info = cat.get(&model).cloned().ok_or_else(|| Error::invalid(format!("Orvyn doesn't serve {model}. GET /v1/models lists the models.")))?;
    if !obj.get("messages").is_some_and(|m| m.as_array().is_some_and(|a| !a.is_empty())) {
        return Err(Error::invalid("messages must be a non-empty array."));
    }

    // Credit: refuse when it's out, and cap the reply to what the balance covers. Vyn's built-in
    // key also has this month's free allowance.
    let vyn = source == "vyn";
    let balance = credits::balance(account).await?.max(0) + if vyn { credits::vyn_allowance_left(account).await? } else { 0 };
    if balance < MIN_BALANCE_MICRO {
        return Err(Error::payment_required(if vyn { "Vyn's free allowance for this month is used up. Top up Orvyn credits to keep talking." } else { "You're out of Orvyn credits. Top up to keep going." }));
    }
    let k = markup();
    let input_chars = serde_json::to_string(&obj.get("messages")).map(|s| s.len()).unwrap_or(0) + serde_json::to_string(&obj.get("tools")).map(|s| s.len()).unwrap_or(0);
    let input_estimate = (input_chars as f64 / 3.0).ceil();
    let left = balance as f64 / MICRO - input_estimate * info.prompt * k;
    let per_out = (info.completion * k).max(1e-12);
    let affordable = if left > 0.0 { (left / per_out).floor() as u64 } else { 0 };
    if affordable < 64 {
        return Err(Error::payment_required("Not enough Orvyn credits for this request. Top up, shorten the conversation, or pick a cheaper model."));
    }
    let requested = obj.get("max_tokens").or_else(|| obj.get("max_completion_tokens")).and_then(Value::as_u64).unwrap_or(DEFAULT_MAX_TOKENS);
    let max_tokens = requested.clamp(1, MAX_TOKENS_CAP).min(affordable);

    let mut up = Map::new();
    for (key_name, v) in obj {
        if ALLOWED.contains(&key_name.as_str()) && key_name != "max_completion_tokens" {
            up.insert(key_name.clone(), v.clone());
        }
    }
    let stream = on_chunk.is_some();
    up.insert("model".into(), json!(model));
    up.insert("max_tokens".into(), json!(max_tokens));
    up.insert("stream".into(), json!(stream));
    up.insert("usage".into(), json!({ "include": true }));
    if stream {
        up.insert("stream_options".into(), json!({ "include_usage": true }));
    }
    // The person's account never goes upstream: a hash stands in for abuse reports.
    up.insert("user".into(), json!(hex::encode(&<sha2::Sha256 as sha2::Digest>::digest(format!("orvyn:{account}").as_bytes())[..12])));

    let req = crate::http::client()
        .post(format!("{}/chat/completions", upstream_base()))
        .bearer_auth(key)
        .header("HTTP-Referer", "https://orvyn.cash")
        .header("X-Title", "Orvyn")
        .timeout(Duration::from_secs(600))
        .json(&Value::Object(up));
    let res = req.send().await.map_err(|e| {
        eprintln!("[inference] upstream: {e}");
        Error::unavailable("The model didn't answer. Try again in a moment.")
    })?;
    let status = res.status().as_u16();
    if !(200..300).contains(&status) {
        let text = res.text().await.unwrap_or_default();
        return Err(upstream_error(status, &text));
    }

    let mut usage = Usage { model: model.clone(), ..Default::default() };
    let mut upstream_cost = None;
    let reply = if let Some(push) = on_chunk {
        use futures_util::StreamExt;
        let mut reader = crate::ai::SseReader::new();
        let mut bytes = res.bytes_stream();
        let mut failed: Option<String> = None;
        while let Some(chunk) = bytes.next().await {
            let Ok(chunk) = chunk else {
                failed = Some("The model's reply was cut off.".into());
                break;
            };
            for data in reader.push(&chunk) {
                if data.trim() == "[DONE]" {
                    continue;
                }
                let Ok(mut ev) = serde_json::from_str::<Value>(&data) else { continue };
                if let Some(e) = ev.get("error") {
                    failed = Some(e["message"].as_str().map(clean_message).unwrap_or_else(|| "The model stopped with an error.".into()));
                    continue;
                }
                if let Some(t) = ev["choices"][0]["delta"]["content"].as_str() {
                    usage.text.push_str(t);
                }
                if ev.get("usage").is_some_and(Value::is_object) {
                    let (c, i, o) = read_usage(&ev["usage"]);
                    upstream_cost = c.or(upstream_cost);
                    usage.input_tokens = i;
                    usage.output_tokens = o;
                    usage.micro = cost_micro(&info, upstream_cost, i, o);
                }
                rewrite_usage(&mut ev, usage.micro);
                push(&ev);
            }
        }
        if usage.input_tokens == 0 && usage.output_tokens == 0 {
            // No usage chunk (cut off): charge an estimate.
            usage.input_tokens = input_estimate as u64;
            usage.output_tokens = (usage.text.len() as f64 / 4.0).ceil() as u64;
            usage.micro = cost_micro(&info, None, usage.input_tokens, usage.output_tokens);
        }
        settle(account, source, usage.micro, &model, usage.input_tokens, usage.output_tokens).await?;
        if let Some(m) = failed.filter(|_| usage.text.is_empty()) {
            return Err(Error::new(Kind::Unavailable, m));
        }
        None
    } else {
        let mut v: Value = res.json().await.map_err(|e| Error::internal(format!("upstream reply: {e}")))?;
        let (c, i, o) = read_usage(&v["usage"]);
        usage.input_tokens = i;
        usage.output_tokens = o;
        usage.micro = cost_micro(&info, c, i, o);
        usage.text = v["choices"][0]["message"]["content"].as_str().unwrap_or("").to_string();
        settle(account, source, usage.micro, &model, i, o).await?;
        rewrite_usage(&mut v, usage.micro);
        Some(v)
    };
    Ok((reply, usage))
}

// ---------- Chat in the app ----------

const HISTORY: i64 = 40;
const MAX_MESSAGE_CHARS: usize = 20_000;

fn t(v: &chrono::DateTime<chrono::Utc>) -> String {
    v.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub fn thread_view(th: &ChatThread) -> Value {
    json!({ "id": th.id, "title": th.title, "model": th.model, "createdAt": t(&th.created_at), "updatedAt": t(&th.updated_at) })
}

pub fn message_view(m: &ChatTurnMessage) -> Value {
    json!({ "id": m.id, "role": m.role, "content": m.content, "model": m.model, "usd": credits::usd(m.micro_usd), "inputTokens": m.input_tokens, "outputTokens": m.output_tokens, "createdAt": t(&m.created_at) })
}

pub async fn list_threads(account: &str) -> Result<Vec<ChatThread>> {
    Ok(sqlx::query_as("select * from chat_turn_threads where account_id = $1 order by updated_at desc limit 200").bind(account.to_lowercase()).fetch_all(db()).await?)
}

async fn owned_thread(account: &str, id: &str) -> Result<ChatThread> {
    sqlx::query_as("select * from chat_turn_threads where id = $1 and account_id = $2").bind(id).bind(account.to_lowercase()).fetch_optional(db()).await?.ok_or_else(|| Error::not_found("No such chat."))
}

pub async fn thread(account: &str, id: &str) -> Result<(ChatThread, Vec<ChatTurnMessage>)> {
    let th = owned_thread(account, id).await?;
    let msgs = sqlx::query_as("select * from chat_turn_messages where thread_id = $1 order by id").bind(&th.id).fetch_all(db()).await?;
    Ok((th, msgs))
}

pub async fn delete_thread(account: &str, id: &str) -> Result<()> {
    let th = owned_thread(account, id).await?;
    sqlx::query("delete from chat_turn_messages where thread_id = $1").bind(&th.id).execute(db()).await?;
    sqlx::query("delete from chat_turn_threads where id = $1").bind(&th.id).execute(db()).await?;
    Ok(())
}

fn title_from(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("New chat");
    let mut s: String = line.chars().take(60).collect();
    if line.chars().count() > 60 {
        s.push('…');
    }
    s
}

/// One chat turn: save the message, ask the model (streaming text to `on_text`), save and charge
/// the reply. `thread_id` None starts a new chat. Returns the thread, the reply and the balance.
pub async fn chat_turn(account: &str, thread_id: Option<&str>, text: &str, model: Option<&str>, on_text: Option<&(dyn Fn(&str) + Send + Sync)>) -> Result<Value> {
    let account = account.to_lowercase();
    let text = text.trim();
    if text.is_empty() || text.chars().count() > MAX_MESSAGE_CHARS {
        return Err(Error::invalid(format!("Write a message (up to {MAX_MESSAGE_CHARS} characters).")));
    }
    let model = model.unwrap_or(DEFAULT_MODEL).to_string();
    if model == ORVYN_MODEL {
        return Err(Error::invalid("Orvyn Model is coming soon. Pick another model for now."));
    }
    if !MODELS.contains(&model.as_str()) {
        return Err(Error::invalid("Pick one of Orvyn's models."));
    }
    // Check credit before saving anything.
    if credits::balance(&account).await? < MIN_BALANCE_MICRO {
        return Err(Error::payment_required("You're out of Orvyn credits. Top up to keep chatting."));
    }
    let th: ChatThread = match thread_id {
        Some(id) => owned_thread(&account, id).await?,
        None => sqlx::query_as("insert into chat_turn_threads (id, account_id, title, model) values ($1, $2, $3, $4) returning *")
            .bind(crate::lifecycle::uuid_v4())
            .bind(&account)
            .bind(title_from(text))
            .bind(&model)
            .fetch_one(db())
            .await?,
    };
    let mut history: Vec<ChatTurnMessage> = sqlx::query_as("select * from chat_turn_messages where thread_id = $1 order by id desc limit $2").bind(&th.id).bind(HISTORY).fetch_all(db()).await?;
    history.reverse();
    let user_msg: ChatTurnMessage = sqlx::query_as("insert into chat_turn_messages (thread_id, role, content) values ($1, 'user', $2) returning *").bind(&th.id).bind(text).fetch_one(db()).await?;

    let today = chrono::Utc::now().format("%A %-d %B %Y");
    let mut messages = vec![json!({ "role": "system", "content": format!("You are a helpful assistant in Orvyn Chat. Answer clearly and directly; use Markdown when it helps. Today is {today} (UTC).") })];
    for m in &history {
        messages.push(json!({ "role": m.role, "content": m.content }));
    }
    messages.push(json!({ "role": "user", "content": text }));

    let push = |ev: &Value| {
        if let (Some(f), Some(t)) = (on_text, ev["choices"][0]["delta"]["content"].as_str()) {
            f(t);
        }
    };
    let result = complete(&account, "chat", json!({ "model": model, "messages": messages }), Some(&push)).await;
    let usage = match result {
        Ok((_, u)) => u,
        Err(e) => {
            // Nothing answered: take the message back so the chat stays as it was.
            sqlx::query("delete from chat_turn_messages where id = $1").bind(user_msg.id).execute(db()).await.ok();
            if thread_id.is_none() {
                sqlx::query("delete from chat_turn_threads where id = $1").bind(&th.id).execute(db()).await.ok();
            }
            return Err(e);
        }
    };
    let reply: ChatTurnMessage = sqlx::query_as("insert into chat_turn_messages (thread_id, role, content, model, micro_usd, input_tokens, output_tokens) values ($1, 'assistant', $2, $3, $4, $5, $6) returning *")
        .bind(&th.id)
        .bind(&usage.text)
        .bind(&model)
        .bind(usage.micro)
        .bind(usage.input_tokens.min(i32::MAX as u64) as i32)
        .bind(usage.output_tokens.min(i32::MAX as u64) as i32)
        .fetch_one(db())
        .await?;
    let th: ChatThread = sqlx::query_as("update chat_turn_threads set model = $2, updated_at = now() where id = $1 returning *").bind(&th.id).bind(&model).fetch_one(db()).await?;
    let balance = credits::balance(&account).await?;
    Ok(json!({ "thread": thread_view(&th), "message": message_view(&reply), "userMessage": message_view(&user_msg), "usd": credits::usd(balance) }))
}
