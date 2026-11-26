//! Orvyn Engine games on Orvyn: drafts people (in the editor) or agents (with their tools) make,
//! checked by the engine itself, published as immutable versions and played at /games/<slug>.
//!
//! - Checks: the engine validates the file and plays 10 seconds of it headless before it's
//!   published; TypeScript scripts are compiled to JavaScript then.
//! - Assets: images and sounds are uploaded with the game and served from Orvyn.
//! - Access: free, paid once in USDT/USDC, or for holders of an agent's token; tips on any.
//!   Payments go straight from the player's wallet to the creator's (or the agent's) wallet, on
//!   chain; Orvyn verifies the transaction and records it. Nothing is held by Orvyn.
//! - Play: games run in a sandboxed page that can't reach Orvyn's cookies or the player's wallet;
//!   paid and token games get a short-lived signed play token.

pub mod engine;
pub mod scores;
pub mod worlds;

use std::sync::LazyLock;

use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::sol_types::{SolCall, SolEvent};
use base64::engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD as B64URL};
use base64::Engine as _;
use hmac::{Hmac, Mac};
use regex::Regex;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::billing::{Call, Calls};
use crate::db::{db, Game, GameAsset, GamePayment, GameVersion};
use crate::env::env;
use crate::error::{Error, Result};
use crate::evm::{self, BSC};
pub use worlds::{check_world, is_world, world_model, WORLD_MODELS};

/// How to write an Orvyn Engine game, told to agents.
pub const GAME_GUIDE: &str = include_str!("guide.txt");

pub const FILE_BYTES: usize = 2_000_000;
pub const ASSET_BYTES: usize = 5_000_000;
pub const TOTAL_ASSET_BYTES: i64 = 25_000_000;
pub const MAX_ASSETS: usize = 200;
pub const MIN_PRICE_USD: f64 = 0.5;
pub const MAX_PRICE_USD: f64 = 1000.0;
pub const PLAY_TOKEN_SECONDS: i64 = 6 * 3600;
const NATIVE: &str = "native";

fn game_error(m: impl Into<String>) -> Error {
    Error::invalid(m)
}

pub fn asset_type(name: &str) -> Option<&'static str> {
    Some(match name.rsplit('.').next()?.to_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "m4a" => "audio/mp4",
        _ => return None,
    })
}

static ASSET_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,99}$").unwrap());

fn valid_asset_name(n: &str) -> bool {
    ASSET_NAME.is_match(n) && n.contains('.') && asset_type(n).is_some()
}

fn stable(token: &str) -> Option<&'static str> {
    match token {
        "usdt" => Some(BSC.usdt),
        "usdc" => Some(BSC.usdc),
        _ => None,
    }
}

/// JSON.stringify's length in UTF-16 units, approximated by chars of compact JSON.
fn json_len(v: &Value) -> usize {
    v.to_string().encode_utf16().count()
}

// ---------------------------------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------------------------------

/// Files a game refers to (images' src and sounds), other than data: URLs.
pub fn referenced_assets(game: &Value) -> Vec<String> {
    if is_world(game) {
        return game["image"].as_str().map(|s| vec![s.to_string()]).unwrap_or_default();
    }
    let a = &game["assets"];
    let mut out: Vec<String> = vec![];
    let images = a["images"].as_object().map(|m| m.values().map(|i| i["src"].as_str().unwrap_or("").to_string()).collect::<Vec<_>>()).unwrap_or_default();
    let sounds = a["sounds"].as_object().map(|m| m.values().map(|s| s.as_str().unwrap_or("").to_string()).collect::<Vec<_>>()).unwrap_or_default();
    for s in images.into_iter().chain(sounds) {
        if !s.is_empty() && !s.starts_with("data:") && !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

/// Compile `*.ts` scripts to JavaScript (ES modules); returns the game and any compile problems.
pub fn compile_scripts(game: &Value) -> (Value, Vec<String>) {
    let mut scripts = game["scripts"].as_object().cloned().unwrap_or_default();
    let mut problems = vec![];
    let ts: Vec<String> = scripts.keys().filter(|k| k.ends_with(".ts")).cloned().collect();
    for name in ts {
        let src = scripts[&name].as_str().unwrap_or("").to_string();
        match engine::compile_ts(&name, &src) {
            Ok(code) => {
                scripts.remove(&name);
                scripts.insert(format!("{}.js", name.trim_end_matches(".ts")), Value::String(code));
            }
            Err(e) => problems.push(format!("script {name}: {}", e.lines().take(3).collect::<Vec<_>>().join(" "))),
        }
    }
    let mut out = game.clone();
    if let Some(o) = out.as_object_mut() {
        o.insert("scripts".into(), Value::Object(scripts));
    }
    (out, problems)
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GameCheck {
    pub ok: bool,
    pub problems: Vec<String>,
    /// Files the game refers to that haven't been uploaded.
    pub missing_assets: Vec<String>,
    pub smoke: Option<Value>,
    /// What bots found playing it (advice, not a reason to refuse).
    pub playtest: Option<Value>,
}

fn check_of(problems: Vec<String>) -> GameCheck {
    GameCheck { ok: false, problems, missing_assets: vec![], smoke: None, playtest: None }
}

/// What publishing would find: file problems, missing uploads, script errors, a 10-second headless run.
pub async fn check_game(draft: &Value, uploaded: &[String]) -> Result<GameCheck> {
    if !draft.is_object() {
        return Ok(check_of(vec!["the game must be a JSON object (orvyn-game/1)".into()]));
    }
    // Dream worlds: no engine, their own checks.
    if is_world(draft) {
        let problems = check_world(draft, uploaded, None);
        let missing: Vec<String> = problems.iter().filter_map(|p| p.strip_suffix(" hasn't been uploaded").map(String::from)).collect();
        let rest: Vec<String> = problems.iter().filter(|p| !p.ends_with("hasn't been uploaded")).cloned().collect();
        return Ok(GameCheck { ok: problems.is_empty(), problems: rest, missing_assets: missing, smoke: None, playtest: None });
    }
    let len = json_len(draft);
    if len > FILE_BYTES {
        return Ok(check_of(vec![format!("the game file is {:.1} MB; the limit is 2 MB (upload images and sounds separately)", len as f64 / 1e6)]));
    }
    let (game, compile) = compile_scripts(draft);
    let mut problems = engine::validate(&game).await?;
    problems.extend(compile);
    let missing: Vec<String> = referenced_assets(&game).into_iter().filter(|r| !uploaded.contains(r)).collect();
    let mut smoke = None;
    if problems.is_empty() {
        let s = engine::smoke_test(&game, 10.0).await?;
        let halted = s["halted"].as_str().map(String::from);
        if let Some(h) = &halted {
            problems.push(format!("it stopped after {} frames: {h}", s["frames"]));
        }
        for e in s["errors"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            if Some(e) != halted.as_deref() {
                problems.push(format!("while playing: {e}"));
            }
        }
        smoke = Some(s);
    }
    let mut report = None;
    if problems.is_empty() {
        let p = engine::playtest(&game, 30.0, false, Value::Null).await?;
        if p["problems"].as_array().is_none_or(|a| a.is_empty()) {
            let runs: Vec<Value> = p["runs"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|r| json!({ "bot": r["bot"], "status": r["status"], "seconds": (r["seconds"].as_f64().unwrap_or(0.0) * 10.0).round() / 10.0, "vars": r["vars"] }))
                .collect();
            report = Some(json!({ "findings": p["findings"], "runs": runs, "canWin": p["can_win"] }));
        }
    }
    Ok(GameCheck { ok: problems.is_empty() && missing.is_empty(), problems, missing_assets: missing, smoke, playtest: report })
}

// ---------------------------------------------------------------------------------------------
// Games
// ---------------------------------------------------------------------------------------------

fn new_id() -> String {
    format!("g{}", hex::encode(rand::random::<[u8; 9]>()))
}

/// Lowercase, NFKD, runs of anything but a-z0-9 as "-", trimmed, then cut to 40 characters (as
/// the TypeScript core: the cut comes after trimming, so a slug can end in "-").
pub fn slugify(title: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    let folded: String = title.to_lowercase().nfkd().collect();
    let mut s = String::new();
    let mut dash = false;
    for c in folded.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            if dash && !s.is_empty() {
                s.push('-');
            }
            dash = false;
            s.push(c);
        } else {
            dash = true;
        }
    }
    let s: String = s.chars().take(40).collect();
    if s.is_empty() {
        "game".into()
    } else {
        s
    }
}

async fn free_slug(title: &str) -> Result<String> {
    let base = slugify(title);
    for i in 0..20 {
        let slug = if i == 0 { base.clone() } else { format!("{base}-{}", hex::encode(rand::random::<[u8; 2]>())) };
        let taken: Option<String> = sqlx::query_scalar("select id from games where slug = $1").bind(&slug).fetch_optional(db()).await?;
        if taken.is_none() {
            return Ok(slug);
        }
    }
    Ok(format!("{base}-{}", hex::encode(rand::random::<[u8; 4]>())))
}

fn title_of(draft: &Value) -> String {
    let t: String = draft["title"].as_str().unwrap_or("").trim().chars().take(80).collect();
    if t.is_empty() {
        "Untitled game".into()
    } else {
        t
    }
}

fn description_of(draft: &Value) -> String {
    draft["description"].as_str().unwrap_or("").trim().chars().take(1000).collect()
}

fn problem_list(check: &GameCheck) -> Value {
    json!(check.problems.iter().cloned().chain(check.missing_assets.iter().map(|m| format!("{m} hasn't been uploaded"))).collect::<Vec<_>>())
}

pub async fn uploaded_names(game_id: &str) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar("select name from game_assets where game_id = $1").bind(game_id).fetch_all(db()).await?)
}

pub struct NewGame<'a> {
    pub creator_account_id: &'a str,
    pub draft: &'a Value,
    pub agent_id: Option<&'a str>,
    pub hire_id: Option<&'a str>,
    pub publish_as_agent: bool,
}

pub async fn create_game(input: NewGame<'_>) -> Result<(Game, GameCheck)> {
    if json_len(input.draft) > FILE_BYTES {
        return Err(game_error("The game file is over 2 MB; upload images and sounds separately."));
    }
    let check = check_game(input.draft, &[]).await?;
    let row: Game = sqlx::query_as(
        "insert into games (id, slug, title, description, creator_account_id, agent_id, hire_id, publish_as_agent, draft, problems) values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) returning *",
    )
    .bind(new_id())
    .bind(free_slug(&title_of(input.draft)).await?)
    .bind(title_of(input.draft))
    .bind(description_of(input.draft))
    .bind(input.creator_account_id.to_lowercase())
    .bind(input.agent_id)
    .bind(input.hire_id)
    .bind(input.publish_as_agent && input.agent_id.is_some())
    .bind(input.draft)
    .bind(problem_list(&check))
    .fetch_one(db())
    .await?;
    Ok((row, check))
}

pub async fn get_game(id_or_slug: &str) -> Result<Option<Game>> {
    let by_id = id_or_slug.starts_with('g') && id_or_slug.len() == 19;
    let row: Option<Game> = sqlx::query_as(if by_id { "select * from games where id = $1" } else { "select * from games where slug = $1" }).bind(id_or_slug).fetch_optional(db()).await?;
    Ok(row.filter(|g| g.status != "removed"))
}

pub fn can_edit(game: &Game, account_id: Option<&str>) -> bool {
    account_id.is_some_and(|a| game.creator_account_id == a.to_lowercase())
}

pub async fn save_draft(game: &Game, draft: &Value) -> Result<(Game, GameCheck)> {
    if json_len(draft) > FILE_BYTES {
        return Err(game_error("The game file is over 2 MB; upload images and sounds separately."));
    }
    let check = check_game(draft, &uploaded_names(&game.id).await?).await?;
    let row: Game = sqlx::query_as("update games set draft = $2, title = $3, description = $4, problems = $5, updated_at = now() where id = $1 returning *")
        .bind(&game.id)
        .bind(draft)
        .bind(title_of(draft))
        .bind(description_of(draft))
        .bind(problem_list(&check))
        .fetch_one(db())
        .await?;
    Ok((row, check))
}

fn js_num(v: &Value) -> f64 {
    match v {
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) if s.trim().is_empty() => 0.0,
        Value::String(s) => s.trim().parse().unwrap_or(f64::NAN),
        Value::Bool(b) => *b as i32 as f64,
        Value::Null => 0.0,
        _ => f64::NAN,
    }
}

pub async fn set_access(game: &Game, access: &Value, publish_as_agent: Option<bool>) -> Result<Game> {
    let kind = access["kind"].as_str().unwrap_or("");
    let mut a = json!({ "kind": kind, "tips": access["tips"].as_bool().unwrap_or(false) || access["tips"].as_i64().is_some_and(|n| n != 0) });
    match kind {
        "paid" => {
            let p = js_num(&access["priceUsd"]);
            if !(MIN_PRICE_USD..=MAX_PRICE_USD).contains(&p) {
                return Err(game_error(format!("Price between ${MIN_PRICE_USD} and ${}.", MAX_PRICE_USD as i64)));
            }
            a["priceUsd"] = json!((p * 100.0).round() / 100.0);
        }
        "token" => {
            let id = match &access["tokenAgentId"] {
                Value::String(s) => s.clone(),
                Value::Null => String::new(),
                v => v.to_string(),
            };
            let token: Option<Option<String>> = sqlx::query_scalar("select token_address from agents where id = $1").bind(&id).fetch_optional(db()).await?;
            if token.flatten().is_none() {
                return Err(game_error("Token-gating needs an agent with a launched token."));
            }
            let min = js_num(&access["minTokens"]);
            if !(min > 0.0) {
                return Err(game_error("Say how many tokens unlock the game."));
            }
            a["tokenAgentId"] = json!(id);
            a["minTokens"] = json!(min);
        }
        "free" => {}
        _ => return Err(game_error("Access is free, paid or token.")),
    }
    if kind == "free" && is_world(&game.draft) && world_model(game.draft["model"].as_str().unwrap_or("")).is_some_and(|m| m.gpu) {
        return Err(game_error("Worlds on GPU models can't be free: every minute costs GPU time. Make it paid or token-gated, or use Orvyn Dream."));
    }
    let as_agent = match publish_as_agent {
        None => game.publish_as_agent,
        Some(p) => p && game.agent_id.is_some(),
    };
    Ok(sqlx::query_as("update games set access = $2, publish_as_agent = $3, updated_at = now() where id = $1 returning *").bind(&game.id).bind(a).bind(as_agent).fetch_one(db()).await?)
}

/// Publish the draft as a new version (checks must pass).
pub async fn publish_game(game: &Game) -> Result<Game> {
    let uploaded = uploaded_names(&game.id).await?;
    let check = check_game(&game.draft, &uploaded).await?;
    if is_world(&game.draft) {
        if let Some(costly) = check_world(&game.draft, &uploaded, game.access["kind"].as_str()).into_iter().find(|p| p.contains("costs")) {
            return Err(game_error(format!("It can't be published yet:\n{costly}")));
        }
    }
    if !check.ok {
        let list: Vec<String> = check.problems.iter().cloned().chain(check.missing_assets.iter().map(|m| format!("{m} hasn't been uploaded"))).collect();
        return Err(game_error(format!("It can't be published yet:\n{}", list.join("\n"))));
    }
    let (compiled, _) = compile_scripts(&game.draft);
    let version = game.version + 1;
    let body = compiled.to_string();
    sqlx::query("insert into game_versions (game_id, version, game, sha256) values ($1, $2, $3, $4)").bind(&game.id).bind(version).bind(&compiled).bind(hex::encode(Sha256::digest(body.as_bytes()))).execute(db()).await?;
    let status = if game.status == "unlisted" { "unlisted" } else { "published" };
    let row: Game = sqlx::query_as("update games set version = $2, status = $3, published_at = coalesce(published_at, now()), problems = '[]', updated_at = now() where id = $1 returning *")
        .bind(&game.id)
        .bind(version)
        .bind(status)
        .fetch_one(db())
        .await?;
    // A thumbnail, in the background: publishing never waits for (or fails on) it.
    if crate::studio::studio_enabled() || is_world(&row.draft) {
        let r = row.clone();
        tokio::spawn(async move {
            if let Err(e) = render_thumbnail(&r).await {
                eprintln!("[games] thumbnail {}: {}", r.id, e.message);
            }
        });
    }
    Ok(row)
}

const THUMB_PAGE: &str = r#"<!doctype html><html><head><meta charset="utf-8"><style>html,body{margin:0;height:100%;background:#000;overflow:hidden}#s{position:fixed;inset:0;display:grid;place-items:center}canvas{display:block}</style></head>
<body><div id="s"><canvas id="c"></canvas></div><script type="module">
let ready; window.__reelReady = new Promise((r) => (ready = r));
try {
  const { play } = await import("/lib/engine/player.js");
  const game = await (await fetch("/game.json")).json();
  const p = await play({ canvas: document.getElementById("c"), wasm: "/lib/engine/orvyn_engine.wasm", game, touchControls: false });
  p.simulate(90);
} finally { ready(); }
</script></body></html>"#;

const BLANK_GIF: &str = "data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7";

/// Render the published game's thumbnail in Orvyn Studio: the real player, 1.5 seconds in.
pub async fn render_thumbnail(game: &Game) -> Result<Option<Vec<u8>>> {
    let Some(v) = published_version(game, game.version).await? else { return Ok(None) };
    // A world's thumbnail is its starting picture (when it has one), else a still of the world.
    if is_world(&v.game) {
        let a = match v.game["image"].as_str() {
            Some(name) => get_asset(&game.id, name).await?,
            None => None,
        };
        let bytes = match a {
            Some(a) => Some(a.bytes),
            None => worlds::world_still(&game.id, &v.game).await.ok().flatten(),
        };
        let Some(bytes) = bytes else { return Ok(None) };
        sqlx::query("update games set thumbnail = $2, thumbnail_version = $3 where id = $1").bind(&game.id).bind(&bytes).bind(v.version).execute(db()).await?;
        return Ok(Some(bytes));
    }
    let mut g = v.game.clone();
    let api = std::env::var("ORVYN_API_URL").unwrap_or_default().trim_end_matches('/').to_string();
    let token = play_token(game, Some(&game.creator_account_id), false);
    let mut downloads: Vec<Value> = vec![];
    let mut budget: i64 = 1_400_000;
    let names: Vec<String> = g["assets"]["images"].as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default();
    for key in names {
        let src = g["assets"]["images"][&key]["src"].as_str().unwrap_or("").to_string();
        if src.starts_with("data:") {
            continue;
        }
        let a = get_asset(&game.id, &src).await?;
        let data = a.as_ref().map(|a| format!("data:{};base64,{}", a.mime, B64.encode(&a.bytes))).unwrap_or_default();
        let new_src = match &a {
            Some(_) if (data.len() as i64) < budget => {
                budget -= data.len() as i64;
                data
            }
            Some(a) if api.starts_with("https://") && !downloads.iter().any(|d| d["name"] == a.name.as_str()) => {
                let encoded: String = url::form_urlencoded::byte_serialize(a.name.as_bytes()).collect::<String>().replace('+', "%20");
                let t: String = url::form_urlencoded::byte_serialize(token.as_bytes()).collect();
                downloads.push(json!({ "name": a.name, "url": format!("{api}/api/games/{}/assets/{encoded}?t={t}", game.id) }));
                a.name.clone()
            }
            _ => BLANK_GIF.to_string(),
        };
        g["assets"]["images"][&key]["src"] = json!(new_src);
    }
    if g["assets"].is_object() {
        g["assets"]["sounds"] = json!({});
    }
    if let Some(scripts) = g["scripts"].as_object_mut() {
        scripts.retain(|name, _| name.ends_with(".rhai"));
    }
    let (w, h) = match g["size"].as_array() {
        Some(s) => (js_num(&s[0]), js_num(&s[1])),
        None => (480.0, 270.0),
    };
    // At least ~960 px wide for link previews, at most 3x.
    let scale = (960.0 / w).ceil().clamp(1.0, 3.0);
    let mut files = vec![json!({ "name": "index.html", "content": THUMB_PAGE }), json!({ "name": "game.json", "content": g.to_string() })];
    files.extend(downloads);
    let spec = json!({ "files": files, "viewports": [{ "width": w.round(), "height": h.round(), "scale": scale }], "name": "thumbnail" });
    let (_, outputs) = crate::studio::system_render("html-still", &spec, json!({ "maxRenderSeconds": 120, "maxVideoSeconds": 0, "maxWidth": 4000, "maxHeight": 4000, "maxOutputMb": 20 })).await?;
    let Some((_, png)) = outputs.into_iter().find(|(f, _)| f.name.ends_with(".png")) else { return Ok(None) };
    sqlx::query("update games set thumbnail = $2, thumbnail_version = $3 where id = $1").bind(&game.id).bind(&png).bind(v.version).execute(db()).await?;
    Ok(Some(png))
}

pub async fn thumbnail_of(game_id: &str) -> Result<Option<(Vec<u8>, Option<i32>)>> {
    let row: Option<(Option<Vec<u8>>, Option<i32>)> = sqlx::query_as("select thumbnail, thumbnail_version from games where id = $1").bind(game_id).fetch_optional(db()).await?;
    Ok(row.and_then(|(t, v)| t.map(|t| (t, v))))
}

pub async fn set_listed(game: &Game, listed: bool) -> Result<Game> {
    if game.version == 0 {
        return Err(game_error("Publish it first."));
    }
    Ok(sqlx::query_as("update games set status = $2, updated_at = now() where id = $1 returning *").bind(&game.id).bind(if listed { "published" } else { "unlisted" }).fetch_one(db()).await?)
}

pub async fn remove_game(game: &Game) -> Result<()> {
    sqlx::query("update games set status = 'removed', updated_at = now() where id = $1").bind(&game.id).execute(db()).await?;
    Ok(())
}

/// The published version players load.
pub async fn published_version(game: &Game, version: i32) -> Result<Option<GameVersion>> {
    Ok(sqlx::query_as("select * from game_versions where game_id = $1 and version = $2").bind(&game.id).bind(version).fetch_optional(db()).await?)
}

// ---------------------------------------------------------------------------------------------
// Assets
// ---------------------------------------------------------------------------------------------

pub async fn upload_asset(game: &Game, name: &str, bytes: &[u8]) -> Result<Value> {
    if !valid_asset_name(name) {
        return Err(game_error("Asset names are letters, digits, . _ - with an image (png, jpg, webp, gif) or sound (mp3, wav, ogg, m4a) extension."));
    }
    if bytes.is_empty() || bytes.len() > ASSET_BYTES {
        return Err(game_error("Each file is at most 5 MB."));
    }
    let existing: Vec<(String, i32)> = sqlx::query_as("select name, size from game_assets where game_id = $1").bind(&game.id).fetch_all(db()).await?;
    let others: Vec<&(String, i32)> = existing.iter().filter(|e| e.0 != name).collect();
    if others.len() >= MAX_ASSETS {
        return Err(game_error(format!("At most {MAX_ASSETS} files per game.")));
    }
    if others.iter().map(|e| e.1 as i64).sum::<i64>() + bytes.len() as i64 > TOTAL_ASSET_BYTES {
        return Err(game_error("A game's files total at most 25 MB."));
    }
    let mime = asset_type(name).unwrap();
    let sha = hex::encode(Sha256::digest(bytes));
    sqlx::query(
        "insert into game_assets (game_id, name, mime, bytes, size, sha256) values ($1, $2, $3, $4, $5, $6)
         on conflict (game_id, name) do update set mime = excluded.mime, bytes = excluded.bytes, size = excluded.size, sha256 = excluded.sha256, created_at = now()",
    )
    .bind(&game.id)
    .bind(name)
    .bind(mime)
    .bind(bytes)
    .bind(bytes.len() as i32)
    .bind(&sha)
    .execute(db())
    .await?;
    Ok(json!({ "name": name, "size": bytes.len(), "mime": mime, "sha256": sha }))
}

pub async fn get_asset(game_id: &str, name: &str) -> Result<Option<GameAsset>> {
    Ok(sqlx::query_as("select * from game_assets where game_id = $1 and name = $2").bind(game_id).bind(name).fetch_optional(db()).await?)
}

pub async fn list_assets(game_id: &str) -> Result<Vec<Value>> {
    let rows: Vec<(String, String, i32, String)> = sqlx::query_as("select name, mime, size, sha256 from game_assets where game_id = $1").bind(game_id).fetch_all(db()).await?;
    Ok(rows.into_iter().map(|(name, mime, size, sha256)| json!({ "name": name, "mime": mime, "size": size, "sha256": sha256 })).collect())
}

pub async fn delete_asset(game: &Game, name: &str) -> Result<()> {
    sqlx::query("delete from game_assets where game_id = $1 and name = $2").bind(&game.id).bind(name).execute(db()).await?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Access, payments and play tokens
// ---------------------------------------------------------------------------------------------

/// Who gets paid: the agent's wallet when it publishes as the agent, otherwise the creator's.
pub async fn pay_to(game: &Game) -> Result<Address> {
    if game.publish_as_agent {
        if let Some(agent_id) = &game.agent_id {
            let addr: Option<String> = sqlx::query_scalar("select evm_address from agents where id = $1").bind(agent_id).fetch_optional(db()).await?;
            if let Some(a) = addr.filter(|a| !a.is_empty()) {
                return evm::parse_address(&a);
            }
        }
    }
    evm::parse_address(&game.creator_account_id)
}

#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct Access {
    pub allowed: bool,
    /// What's missing: "sign-in", "purchase" or "tokens".
    pub need: Option<&'static str>,
    pub reason: Option<String>,
}

fn allowed() -> Access {
    Access { allowed: true, need: None, reason: None }
}

pub async fn access_for(game: &Game, account_id: Option<&str>) -> Result<Access> {
    let a = &game.access;
    let kind = a["kind"].as_str().unwrap_or("free");
    if kind == "free" {
        return Ok(allowed());
    }
    let Some(account) = account_id else {
        return Ok(Access { allowed: false, need: Some("sign-in"), reason: Some(if kind == "paid" { "Sign in to buy this game." } else { "Sign in with a wallet that holds the token." }.into()) });
    };
    if can_edit(game, Some(account)) {
        return Ok(allowed());
    }
    if kind == "paid" {
        let bought: Option<String> = sqlx::query_scalar("select tx_hash from game_payments where game_id = $1 and account_id = $2 and kind = 'purchase' limit 1").bind(&game.id).bind(account.to_lowercase()).fetch_optional(db()).await?;
        return Ok(match bought {
            Some(_) => allowed(),
            None => Access { allowed: false, need: Some("purchase"), reason: Some(format!("${} to play, once.", crate::auth::js_number(a["priceUsd"].as_f64().unwrap_or(0.0)))) },
        });
    }
    let agent: Option<(Option<String>, String, String)> = sqlx::query_as("select token_address, launchpad, symbol from agents where id = $1").bind(a["tokenAgentId"].as_str().unwrap_or("")).fetch_optional(db()).await?;
    let Some((Some(token), launchpad, symbol)) = agent else {
        return Ok(Access { allowed: false, need: Some("tokens"), reason: Some("The token that unlocks this game isn't available.".into()) });
    };
    let min = a["minTokens"].as_f64().unwrap_or(1.0);
    let h = crate::auth::holder_status(Some(&token), &launchpad, min, account).await?;
    Ok(if h.allowed {
        allowed()
    } else {
        Access { allowed: false, need: Some("tokens"), reason: Some(format!("Hold {} ${symbol} to play (you hold {}).", crate::auth::js_number(min), crate::auth::js_number(h.balance))) }
    })
}

/// Unsigned transaction paying the creator: a purchase (the price, USDT/USDC) or a tip.
pub async fn payment_call(game: &Game, kind: &str, token: &str, amount: Option<f64>) -> Result<Value> {
    let to = pay_to(game).await?;
    let amount = if kind == "purchase" {
        let price = game.access["priceUsd"].as_f64().filter(|p| *p > 0.0);
        if game.access["kind"] != "paid" || price.is_none() {
            return Err(game_error("This game isn't sold."));
        }
        if token == "bnb" {
            return Err(game_error("Buy with USDT or USDC."));
        }
        price.unwrap()
    } else {
        if game.access["tips"] != true {
            return Err(game_error("This creator doesn't take tips."));
        }
        match amount.filter(|a| *a > 0.0) {
            Some(a) => a,
            None => return Err(game_error("Tip an amount above zero.")),
        }
    };
    // 18 decimals on BSC.
    let units = U256::from((amount * 1e6).round() as u128) * U256::from(10u64).pow(U256::from(12));
    let shown = crate::auth::js_number(amount);
    if token == "bnb" {
        let calls = Calls { chain_id: BSC.chain_id, amount: None, calls: vec![Call { to: to.to_string(), data: "0x".into(), value: units.to_string() }] };
        let mut v = serde_json::to_value(calls)?;
        v["amount"] = json!(shown);
        v["token"] = json!(NATIVE);
        return Ok(v);
    }
    let s = stable(token).ok_or_else(|| game_error("token must be usdt, usdc or bnb"))?;
    let data = evm::IERC20::transferCall { to, amount: units }.abi_encode();
    let calls = Calls { chain_id: BSC.chain_id, amount: None, calls: vec![Call { to: s.into(), data: format!("0x{}", hex::encode(data)), value: "0".into() }] };
    let mut v = serde_json::to_value(calls)?;
    v["amount"] = json!(shown);
    v["token"] = json!(s);
    Ok(v)
}

static TX_HASH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^0x[0-9a-fA-F]{64}$").unwrap());

/// Verify an on-chain payment from `account_id` to the creator, and record it (each tx once).
pub async fn record_payment(game: &Game, account_id: &str, tx_hash: &str, kind: &str) -> Result<GamePayment> {
    if !TX_HASH.is_match(tx_hash) {
        return Err(game_error("That isn't a transaction hash."));
    }
    if !evm::is_address(account_id) {
        return Err(game_error("Sign in with a wallet."));
    }
    let hash_lc = tx_hash.to_lowercase();
    let from = account_id.to_lowercase();
    let seen: Option<GamePayment> = sqlx::query_as("select * from game_payments where tx_hash = $1").bind(&hash_lc).fetch_optional(db()).await?;
    if let Some(s) = seen {
        if s.game_id == game.id && s.account_id == from {
            return Ok(s);
        }
        return Err(game_error("That transaction was already used."));
    }
    let h: alloy::primitives::B256 = tx_hash.parse().map_err(|_| game_error("That isn't a transaction hash."))?;
    let receipt = crate::wallets::receipt_of(h).await.ok().flatten().ok_or_else(|| Error::conflict("The transaction isn't on chain yet. Try again in a few seconds."))?;
    if !receipt.status() {
        return Err(game_error("The transaction failed."));
    }
    let to = pay_to(game).await?;
    let from_addr = evm::parse_address(&from)?;
    let mut token: Option<String> = None;
    let mut amount = U256::ZERO;
    for log in receipt.inner.logs() {
        let Some(s) = [BSC.usdt, BSC.usdc].into_iter().find(|s| s.eq_ignore_ascii_case(&format!("{:#x}", log.address()))) else { continue };
        if let Ok(ev) = evm::IERC20::Transfer::decode_log_data(log.data()) {
            if ev.from == from_addr && ev.to == to {
                token = Some(s.to_string());
                amount += ev.value;
            }
        }
    }
    if token.is_none() {
        if let Ok(Some(tx)) = evm::bsc().get_transaction_by_hash(h).await {
            use alloy::consensus::Transaction as _;
            use alloy::network::TransactionResponse as _;
            if tx.from() == from_addr && tx.to() == Some(to) && tx.value() > U256::ZERO {
                token = Some(NATIVE.into());
                amount = tx.value();
            }
        }
    }
    let Some(token) = token.filter(|_| amount > U256::ZERO) else {
        return Err(game_error("That transaction doesn't pay this game's creator from your wallet."));
    };
    let usd = (token != NATIVE).then(|| (amount / U256::from(10u64).pow(U256::from(12))).to::<u128>() as f64 / 1e6);
    if kind == "purchase" {
        if game.access["kind"] != "paid" || token == NATIVE {
            return Err(game_error("Purchases are paid in USDT or USDC."));
        }
        let price = game.access["priceUsd"].as_f64().unwrap_or(0.0);
        if usd.unwrap_or(0.0) + 1e-9 < price {
            return Err(game_error(format!("That paid ${}; the game is ${}.", crate::auth::js_number(usd.unwrap_or(0.0)), crate::auth::js_number(price))));
        }
    }
    Ok(sqlx::query_as("insert into game_payments (tx_hash, game_id, account_id, kind, token, amount, usd) values ($1, $2, $3, $4, $5, $6, $7) returning *")
        .bind(&hash_lc)
        .bind(&game.id)
        .bind(&from)
        .bind(kind)
        .bind(&token)
        .bind(amount.to_string())
        .bind(usd)
        .fetch_one(db())
        .await?)
}

fn play_mac(claim: &str) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(env().session_secret.as_bytes()).unwrap();
    mac.update(format!("play:{claim}").as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// A short-lived token that lets the sandboxed play page load this game (it can't send cookies):
/// the published version, or (`draft`, owners only) the working copy for the editor's preview.
pub fn play_token(game: &Game, account_id: Option<&str>, draft: bool) -> String {
    play_token_at(&game.id, account_id, draft, chrono::Utc::now().timestamp())
}

pub fn play_token_at(game_id: &str, account_id: Option<&str>, draft: bool, now_s: i64) -> String {
    // The TypeScript core's key order: g, a, d, exp.
    let a = match account_id {
        Some(a) => serde_json::to_string(&a.to_lowercase()).unwrap(),
        None => "null".into(),
    };
    let json = format!("{{\"g\":{},\"a\":{a},\"d\":{draft},\"exp\":{}}}", serde_json::to_string(game_id).unwrap(), now_s + PLAY_TOKEN_SECONDS);
    let claim = B64URL.encode(json);
    format!("{claim}.{}", B64URL.encode(play_mac(&claim)))
}

/// The token's claim (account, draft) if it's valid for this game, else None.
pub fn verify_play_token(token: Option<&str>, game_id: &str) -> Option<(Option<String>, bool)> {
    let (claim, sig) = token?.split_once('.')?;
    let sig = sig.split('.').next()?;
    if claim.is_empty() || sig.is_empty() {
        return None;
    }
    let want = play_mac(claim);
    let got = B64URL.decode(sig.trim_end_matches('=')).ok()?;
    if got.len() != want.len() || got.iter().zip(&want).fold(0u8, |a, (x, y)| a | (x ^ y)) != 0 {
        return None;
    }
    let c: Value = serde_json::from_slice(&B64URL.decode(claim.trim_end_matches('=')).ok()?).ok()?;
    let exp = c["exp"].as_f64()?;
    (c["g"] == game_id && exp * 1000.0 > chrono::Utc::now().timestamp_millis() as f64).then(|| (c["a"].as_str().map(String::from), c["d"].as_bool().unwrap_or(false)))
}

pub async fn count_play(game: &Game) -> Result<()> {
    sqlx::query("update games set plays = plays + 1 where id = $1").bind(&game.id).execute(db()).await?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Views and lists
// ---------------------------------------------------------------------------------------------

pub async fn game_view(game: &Game, viewer: Option<&str>) -> Result<Value> {
    let agent: Option<Value> = match &game.agent_id {
        Some(id) => {
            let r: Option<(String, Option<String>, String, Option<String>)> = sqlx::query_as("select id, handle, name, icon from agents where id = $1").bind(id).fetch_optional(db()).await?;
            r.map(|(id, handle, name, icon)| json!({ "id": id, "handle": handle, "name": name, "icon": icon }))
        }
        None => None,
    };
    let (n, tips): (i64, f64) = sqlx::query_as("select count(*), coalesce(sum(case when kind = 'tip' then usd else 0 end), 0)::float8 from game_payments where game_id = $1").bind(&game.id).fetch_one(db()).await?;
    let mine = can_edit(game, viewer);
    let d = &game.draft;
    let world = is_world(d);
    let size = if d["size"].is_array() { d["size"].clone() } else if world { json!([640, 360]) } else { json!([480, 270]) };
    let mut v = json!({
        "id": game.id,
        "slug": game.slug,
        "title": game.title,
        "description": game.description,
        "status": game.status,
        "version": game.version,
        "access": game.access,
        "plays": game.plays,
        "thumbnail": game.thumbnail_version.map(|tv| format!("/api/games/{}/thumbnail.png?v={tv}", game.id)),
        "size": size,
        "touch": !d["touch"].is_null() && d["touch"] != false && d["touch"] != 0 && d["touch"] != "",
        "kind": if world { "world" } else { "game" },
        "worldModel": if world { json!(d["model"].as_str().unwrap_or("dream")) } else { Value::Null },
        "creator": game.creator_account_id,
        "agent": agent,
        "madeByAgent": game.agent_id.is_some(),
        "publishAsAgent": game.publish_as_agent,
        "payTo": if game.version > 0 || mine { json!(pay_to(game).await?.to_string()) } else { Value::Null },
        "payments": n,
        "tipsUsd": tips,
        "publishedAt": game.published_at.map(|t| t.timestamp_millis()),
        "updatedAt": game.updated_at.timestamp_millis(),
    });
    if mine {
        let o = v.as_object_mut().unwrap();
        o.insert("draft".into(), game.draft.clone());
        o.insert("problems".into(), game.problems.clone());
        o.insert("hireId".into(), json!(game.hire_id));
    }
    Ok(v)
}

#[derive(Default)]
pub struct ListGames<'a> {
    pub limit: Option<i64>,
    pub agent_id: Option<&'a str>,
    pub creator: Option<&'a str>,
    pub include_drafts: bool,
}

/// Games, newest first, without their thumbnails' bytes.
pub async fn list_games(opts: ListGames<'_>) -> Result<Vec<Game>> {
    let status = if opts.include_drafts { "status <> 'removed'" } else { "status = 'published'" };
    let order = if opts.include_drafts { "updated_at" } else { "published_at" };
    let sql = format!(
        "select id, slug, title, description, creator_account_id, agent_id, publish_as_agent, hire_id, status, access, draft, problems, version, plays, null::bytea as thumbnail, thumbnail_version, published_at, created_at, updated_at
         from games where {status} and ($1::text is null or agent_id = $1) and ($2::text is null or creator_account_id = $2) order by {order} desc nulls last limit $3"
    );
    Ok(sqlx::query_as(&sql).bind(opts.agent_id).bind(opts.creator.map(str::to_lowercase)).bind(opts.limit.unwrap_or(60).min(200)).fetch_all(db()).await?)
}

/// Games an agent made for a marketplace job's client (drafts they can publish).
pub async fn games_for_hire(hire_id: &str) -> Result<Vec<Game>> {
    Ok(sqlx::query_as("select * from games where hire_id = $1").bind(hire_id).fetch_all(db()).await?)
}

/// For callers that only need the draft as an object.
pub fn draft_object(v: &Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}
