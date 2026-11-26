//! Dream worlds: Orvyn games played in a world generated live, streamed from Orvyn Worlds
//! (crates/worlds) as the player holds keys.
//!
//! A world game is one file, `orvyn-world/1`: { "format", "title", "description", "model":
//! "dream" | "yume" | "worldplay", "prompt", "image" (an uploaded asset), "seconds", "size" }.
//!
//! - "dream" is Orvyn's built-in world (no GPU): free to offer.
//! - GPU models cost GPU time each minute, so their games must be paid or token-gated, and each
//!   player gets a daily allowance.
//! - Models whose licence doesn't cover every country are refused for players there (or whose
//!   country isn't known).
//! - Sessions are signed for one player, one world, a fixed time; the Worlds server reports the
//!   seconds used.

use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::Sha256;

use crate::db::{db, WorldSession};
use crate::env::env;
use crate::error::{Error, Result};
use crate::studio::tencent_excluded;

pub const WORLD_FORMAT: &str = "orvyn-world/1";

pub fn is_world(game: &Value) -> bool {
    game["format"] == WORLD_FORMAT
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorldModel {
    pub id: &'static str,
    pub name: &'static str,
    pub what: &'static str,
    pub gpu: bool,
    pub needs_image: bool,
    pub license: &'static str,
    /// Whether the licence excludes the EU, the UK and South Korea.
    #[serde(skip)]
    pub tencent: bool,
    pub notice: Option<&'static str>,
}

pub const WORLD_MODELS: &[WorldModel] = &[
    WorldModel { id: "dream", name: "Orvyn Dream", what: "Orvyn's own generated landscapes: fly through mountains, water and sky the prompt shapes.", gpu: false, needs_image: false, license: "Orvyn (MIT)", tencent: false, notice: None },
    WorldModel { id: "yume", name: "YUME 1.5", what: "Walk into a picture: the world model continues it as you move.", gpu: true, needs_image: true, license: "Apache-2.0", tencent: false, notice: Some("AI-generated world (YUME).") },
    WorldModel {
        id: "worldplay",
        name: "HY-World 1.5 WorldPlay (Tencent)",
        what: "A world that stays consistent as you explore it, from one picture.",
        gpu: true,
        needs_image: true,
        license: "Tencent HY-WorldPlay Community License",
        tencent: true,
        notice: Some("Powered by Tencent Hunyuan (HY-World 1.5), under the Tencent Hunyuan Community License Agreement. Provided by Orvyn; Tencent isn't affiliated with it and doesn't endorse it. AI-generated world."),
    },
];

pub fn world_model(id: &str) -> Option<&'static WorldModel> {
    WORLD_MODELS.iter().find(|m| m.id == id)
}

pub const MAX_SESSION_SECONDS: f64 = 600.0;
pub const MIN_SESSION_SECONDS: f64 = 30.0;
/// GPU seconds a player gets per day across all worlds.
pub const GPU_SECONDS_PER_DAY: f64 = 1800.0;
/// Sessions open at once per player.
pub const OPEN_PER_PLAYER: i64 = 1;
/// How long a session token can wait before connecting.
pub const CONNECT_SECONDS: i64 = 120;

fn world_error(status: u16, m: impl Into<String>) -> Error {
    match status {
        401 => Error::unauthorized(m),
        404 => Error::not_found(m),
        409 => Error::conflict(m),
        429 => Error::rate_limited(m),
        451 => Error::forbidden(m),
        503 => Error::unavailable(m),
        _ => Error::invalid(m),
    }
}

fn js_num(v: &Value) -> f64 {
    match v {
        Value::Null => f64::NAN,
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) if s.trim().is_empty() => 0.0,
        Value::String(s) => s.trim().parse().unwrap_or(f64::NAN),
        Value::Bool(b) => *b as i32 as f64,
        _ => f64::NAN,
    }
}

/// Problems in a world game file. `access` is the game's access kind, for the GPU cost rule.
pub fn check_world(world: &Value, uploaded: &[String], access: Option<&str>) -> Vec<String> {
    let mut p = vec![];
    let title = world["title"].as_str().map(str::trim).unwrap_or("");
    if title.is_empty() || title.chars().count() > 80 {
        p.push("give the world a title (up to 80 characters)".to_string());
    }
    let model = world_model(&match &world["model"] {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        v => v.to_string(),
    });
    if model.is_none() {
        p.push(format!("model must be one of {}", WORLD_MODELS.iter().map(|m| m.id).collect::<Vec<_>>().join(", ")));
    }
    let prompt = world["prompt"].as_str().map(str::trim).unwrap_or("");
    let n = prompt.chars().count();
    if !(3..=2000).contains(&n) {
        p.push("describe the world in the prompt (3–2000 characters)".into());
    }
    let image = world["image"].as_str();
    if let Some(m) = model.filter(|m| m.needs_image && image.is_none()) {
        p.push(format!("{} starts from a picture: upload one and name it in \"image\"", m.name));
    }
    if let Some(i) = image.filter(|i| !uploaded.iter().any(|u| u == i)) {
        p.push(format!("{i} hasn't been uploaded"));
    }
    let seconds = if world["seconds"].is_null() { 300.0 } else { js_num(&world["seconds"]) };
    if !seconds.is_finite() || !(MIN_SESSION_SECONDS..=MAX_SESSION_SECONDS).contains(&seconds) {
        p.push(format!("seconds (one session) must be {MIN_SESSION_SECONDS}–{MAX_SESSION_SECONDS}"));
    }
    if !world["size"].is_null() {
        let ok = world["size"].as_array().is_some_and(|s| s.len() == 2 && {
            let (w, h) = (js_num(&s[0]), js_num(&s[1]));
            (256.0..=1280.0).contains(&w) && (144.0..=720.0).contains(&h)
        });
        if !ok {
            p.push("size must be [width, height] within 256x144 and 1280x720".into());
        }
    }
    if let Some(m) = model.filter(|m| m.gpu && access == Some("free")) {
        p.push(format!("{} runs on a GPU, and every minute costs: make the world paid or token-gated (Publish tab)", m.name));
    }
    p
}

pub fn world_size(world: &Value) -> (f64, f64) {
    match world["size"].as_array() {
        Some(s) => (js_num(s.first().unwrap_or(&Value::Null)), js_num(s.get(1).unwrap_or(&Value::Null))),
        None => (640.0, 360.0),
    }
}

/// Why `model_id` can't be played by someone in `country`, or None.
pub fn world_model_blocked(model_id: &str, country: Option<&str>) -> Option<String> {
    let Some(m) = world_model(model_id) else { return Some("This world's model isn't available.".into()) };
    if !m.tencent {
        return None;
    }
    match country {
        None => Some(format!("{}'s licence doesn't cover every country, and we couldn't tell where you are, so this world can't be played here.", m.name)),
        Some(c) if tencent_excluded(c) => Some(format!("{}'s licence doesn't cover your country ({c}), so this world can't be played there.", m.name)),
        _ => None,
    }
}

fn sign(claims: &Value) -> Result<String> {
    let key = env().orvyn_worlds_key.clone().ok_or_else(|| world_error(503, "Worlds aren't set up on this deployment."))?;
    let body = B64URL.encode(claims.to_string());
    let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes()).unwrap();
    mac.update(format!("world:{body}").as_bytes());
    Ok(format!("{body}.{}", B64URL.encode(mac.finalize().into_bytes())))
}

/// JSON with the claims' fields in the order the TypeScript core wrote them (the Worlds server
/// only checks the signature over these exact bytes, so any order works; this keeps it stable).
fn claims(sid: &str, game: &str, account: &str, model: &str, prompt: &str, image: Option<&str>, seconds: f64, width: f64, height: f64, exp: i64) -> Value {
    // Whole numbers, as JavaScript writes them (the Worlds server reads them as integers).
    let int = |x: f64| json!(x.round() as i64);
    json!({ "sid": sid, "game": game, "account": account, "model": model, "prompt": prompt, "image": image, "seconds": int(seconds), "width": int(width), "height": int(height), "exp": exp })
}

pub struct OpenSession<'a> {
    pub game_id: &'a str,
    pub account_id: Option<&'a str>,
    pub world: &'a Value,
    pub country: Option<&'a str>,
    /// Where the Worlds server can download the start image (https).
    pub image_url: Option<&'a str>,
    pub preview: bool,
}

/// Open a session for a player: checks the model's licence where they are, their daily GPU
/// allowance and open sessions, records it, and returns the Worlds URL to connect to.
pub async fn open_session(input: OpenSession<'_>) -> Result<Value> {
    let e = env();
    let (Some(url), Some(_)) = (e.orvyn_worlds_url.clone(), e.orvyn_worlds_key.clone()) else {
        return Err(world_error(503, "Worlds aren't set up on this deployment yet."));
    };
    let model = world_model(input.world["model"].as_str().unwrap_or("")).ok_or_else(|| world_error(400, "This world's model isn't available."))?;
    if let Some(why) = world_model_blocked(model.id, input.country) {
        return Err(world_error(451, why));
    }
    if model.gpu && input.account_id.is_none() {
        return Err(world_error(401, "Sign in to Orvyn to play this world."));
    }
    let account = input.account_id.map(str::to_lowercase).unwrap_or_else(|| format!("anon:{}", hex::encode(rand::random::<[u8; 6]>())));
    let asked = if input.world["seconds"].is_null() { 300.0 } else { js_num(&input.world["seconds"]) };
    let mut seconds = MAX_SESSION_SECONDS.min(MIN_SESSION_SECONDS.max(asked));
    if input.account_id.is_some() {
        let open: i64 = sqlx::query_scalar("select count(*) from world_sessions where account_id = $1 and ended_at is null and expires_at > now()").bind(&account).fetch_one(db()).await?;
        if open >= OPEN_PER_PLAYER {
            return Err(world_error(409, "You're already in a world. End it (or wait a minute) before starting another."));
        }
        if model.gpu {
            let used: f64 = sqlx::query_scalar("select coalesce(sum(seconds), 0)::float8 from world_sessions where account_id = $1 and gpu = true and created_at > now() - interval '1 day'").bind(&account).fetch_one(db()).await?;
            let left = GPU_SECONDS_PER_DAY - used;
            if left < 30.0 {
                return Err(world_error(429, "You've used today's time in GPU worlds. Come back tomorrow."));
            }
            seconds = seconds.min(left.floor());
        }
    }
    let sid = format!("ws{}", hex::encode(rand::random::<[u8; 9]>()));
    let (width, height) = world_size(input.world);
    let exp = chrono::Utc::now().timestamp() + CONNECT_SECONDS;
    sqlx::query("insert into world_sessions (id, game_id, account_id, model, gpu, country, preview, allowed_seconds, expires_at) values ($1, $2, $3, $4, $5, $6, $7, $8, to_timestamp($9))")
        .bind(&sid)
        .bind(input.game_id)
        .bind(&account)
        .bind(model.id)
        .bind(model.gpu)
        .bind(input.country)
        .bind(input.preview)
        .bind(seconds as i32)
        .bind((exp as f64) + seconds + 60.0)
        .execute(db())
        .await?;
    let prompt = match &input.world["prompt"] {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        v => v.to_string(),
    };
    let image = if model.needs_image { input.image_url } else { None };
    let token = sign(&claims(&sid, input.game_id, &account, model.id, &prompt, image, seconds, width, height, exp))?;
    let base = url.trim_end_matches('/');
    let base = base.strip_prefix("http").map(|rest| format!("ws{rest}")).unwrap_or_else(|| base.to_string());
    let encoded: String = url::form_urlencoded::byte_serialize(token.as_bytes()).collect();
    Ok(json!({ "sid": sid, "url": format!("{base}/play?token={encoded}"), "seconds": seconds as i64, "model": model.id, "notice": model.notice }))
}

/// The Worlds server's report that a session ended (signed with the shared key).
pub async fn record_usage(body: &str, signature: Option<&str>) -> Result<Value> {
    let key = env().orvyn_worlds_key.clone().ok_or_else(|| world_error(503, "Worlds aren't set up."))?;
    let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes()).unwrap();
    mac.update(format!("usage:{body}").as_bytes());
    let want = hex::encode(mac.finalize().into_bytes());
    let got = signature.unwrap_or("");
    if got.len() != want.len() || got.bytes().zip(want.bytes()).fold(0u8, |a, (x, y)| a | (x ^ y)) != 0 {
        return Err(world_error(401, "bad signature"));
    }
    let r: Value = serde_json::from_str(body).map_err(|_| world_error(400, "bad report"))?;
    let (Some(sid), Some(secs)) = (r["sid"].as_str(), r["seconds"].as_f64()) else { return Err(world_error(400, "bad report")) };
    let row: Option<WorldSession> = sqlx::query_as("select * from world_sessions where id = $1").bind(sid).fetch_optional(db()).await?;
    let row = row.ok_or_else(|| world_error(404, "unknown session"))?;
    // Never more than the session was allowed (plus a little for the connection).
    let seconds = secs.min(row.allowed_seconds as f64 + 5.0).max(0.0);
    sqlx::query("update world_sessions set seconds = $2, ended_at = now() where id = $1").bind(sid).bind(seconds).execute(db()).await?;
    Ok(json!({ "recorded": true, "seconds": seconds }))
}

/// A dream world's first view (JPEG), from Orvyn Worlds: its thumbnail when it has no picture.
pub async fn world_still(game_id: &str, world: &Value) -> Result<Option<Vec<u8>>> {
    let e = env();
    let (Some(url), Some(_)) = (e.orvyn_worlds_url.clone(), e.orvyn_worlds_key.clone()) else { return Ok(None) };
    if world["model"].as_str().unwrap_or("dream") != "dream" {
        return Ok(None);
    }
    let (width, height) = world_size(world);
    let prompt = world["prompt"].as_str().unwrap_or("");
    let token = sign(&claims(&format!("still-{game_id}"), game_id, "orvyn", "dream", prompt, None, 30.0, width, height, chrono::Utc::now().timestamp() + 60))?;
    let encoded: String = url::form_urlencoded::byte_serialize(token.as_bytes()).collect();
    let res = crate::http::client().get(format!("{}/still?token={encoded}", url.trim_end_matches('/'))).timeout(Duration::from_secs(30)).send().await;
    match res {
        Ok(r) if r.status().is_success() => Ok(r.bytes().await.ok().map(|b| b.to_vec())),
        _ => Ok(None),
    }
}
