//! Leaderboards and saves for Orvyn Engine games.
//!
//! - A game declares them in its file: `leaderboard: { var, order, label }` and `save: [vars]`.
//! - Saves are per player and per game. Each save comes back with a signed token; a run carries the
//!   token of the save it started from, so its replay starts from exactly that state.
//! - A score counts only if replaying the run's recorded input in the engine gives it. Each player
//!   keeps their best.

use std::collections::BTreeMap;

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use serde::Serialize;
use serde_json::{json, Map, Value};
use sha2::Sha256;

use crate::db::{db, GameScore};
use crate::env::env;
use crate::error::{Error, Result};

const SAVE_BYTES: usize = 16_000;
const LOG_BYTES: usize = 600_000;

fn score_error(m: impl Into<String>) -> Error {
    Error::invalid(m)
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Board {
    pub var: String,
    /// "high" | "low".
    pub order: &'static str,
    pub label: String,
}

/// The leaderboard a published game declares, or None.
pub fn board_of(game: &Value) -> Option<Board> {
    let lb = &game["leaderboard"];
    let var = lb["var"].as_str().filter(|v| !v.is_empty())?.to_string();
    let order = if lb["order"] == "low" { "low" } else { "high" };
    let label = lb["label"].as_str().map(str::trim).filter(|l| !l.is_empty()).map(String::from).unwrap_or_else(|| {
        if var == "$time" {
            "Time".into()
        } else {
            let mut c = var.chars();
            c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
        }
    });
    Some(Board { var, order, label })
}

fn save_list_of(game: &Value) -> Vec<String> {
    game["save"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// Saves
// ---------------------------------------------------------------------------------------------

/// JSON.stringify of the object with its top-level keys sorted (values as they are).
fn canon(data: &Map<String, Value>) -> String {
    let sorted: BTreeMap<&String, &Value> = data.iter().collect();
    let parts: Vec<String> = sorted.iter().map(|(k, v)| format!("{}:{}", serde_json::to_string(k).unwrap(), crate::environment::canonical_json(v))).collect();
    format!("{{{}}}", parts.join(","))
}

fn sign_save(game_id: &str, account_id: &str, data: &Map<String, Value>) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(env().session_secret.as_bytes()).unwrap();
    mac.update(format!("save:{game_id}:{}:{}", account_id.to_lowercase(), canon(data)).as_bytes());
    B64URL.encode(mac.finalize().into_bytes())
}

fn save_token_ok(game_id: &str, account_id: &str, data: &Map<String, Value>, token: Option<&str>) -> bool {
    let Some(token) = token else { return false };
    let (Ok(want), Ok(got)) = (B64URL.decode(sign_save(game_id, account_id, data)), B64URL.decode(token.trim_end_matches('='))) else { return false };
    got.len() == want.len() && got.iter().zip(&want).fold(0u8, |a, (x, y)| a | (x ^ y)) == 0
}

/// The player's save (only the game's save list), with its token.
pub async fn get_save(game_id: &str, account_id: &str, game: &Value) -> Result<Value> {
    let keys = save_list_of(game);
    let stored: Option<Value> = sqlx::query_scalar("select data from game_saves where game_id = $1 and account_id = $2").bind(game_id).bind(account_id.to_lowercase()).fetch_optional(db()).await?;
    let data: Map<String, Value> = stored.and_then(|d| d.as_object().cloned()).unwrap_or_default().into_iter().filter(|(k, _)| keys.contains(k)).collect();
    Ok(json!({ "data": data, "token": sign_save(game_id, account_id, &data), "keys": keys }))
}

fn plain(v: &Value) -> bool {
    match v {
        Value::Array(a) => a.iter().all(plain),
        Value::Object(o) => o.values().all(plain),
        _ => true,
    }
}

pub async fn put_save(game_id: &str, account_id: &str, game: &Value, data: &Value) -> Result<Value> {
    let keys = save_list_of(game);
    if keys.is_empty() {
        return Err(score_error("This game doesn't save progress."));
    }
    let Some(obj) = data.as_object() else { return Err(score_error("A save is an object of the game's saved variables.")) };
    let clean: Map<String, Value> = obj.iter().filter(|(k, _)| keys.contains(k)).map(|(k, v)| (k.clone(), v.clone())).collect();
    if !plain(&Value::Object(clean.clone())) {
        return Err(score_error("Saved values must be plain JSON."));
    }
    if Value::Object(clean.clone()).to_string().len() > SAVE_BYTES {
        return Err(score_error("A save is at most 16 KB."));
    }
    sqlx::query("insert into game_saves (game_id, account_id, data) values ($1, $2, $3) on conflict (game_id, account_id) do update set data = excluded.data, updated_at = now()")
        .bind(game_id)
        .bind(account_id.to_lowercase())
        .bind(Value::Object(clean.clone()))
        .execute(db())
        .await?;
    Ok(json!({ "data": clean, "token": sign_save(game_id, account_id, &clean) }))
}

// ---------------------------------------------------------------------------------------------
// Scores
// ---------------------------------------------------------------------------------------------

fn better(order: &str, a: f64, b: f64) -> bool {
    if order == "high" {
        a > b
    } else {
        a < b
    }
}

pub struct Run<'a> {
    pub game_id: &'a str,
    pub account_id: &'a str,
    pub version: i32,
    pub game: &'a Value,
    pub log: &'a Value,
    pub save: Option<&'a str>,
}

/// Check a run by replaying it, and keep it if it's the player's best.
pub async fn submit_run(input: Run<'_>) -> Result<Value> {
    let board = board_of(input.game).ok_or_else(|| score_error("This game has no leaderboard."))?;
    if input.log.to_string().len() > LOG_BYTES {
        return Err(score_error("That run's recording is too long to check."));
    }
    if !input.log["frames"].is_number() {
        return Err(score_error("Send the run's input log."));
    }
    // The run must start from the player's stored save (signed), or from nothing.
    let start = input.log["start"].as_object().cloned().unwrap_or_default();
    if !start.is_empty() && !save_token_ok(input.game_id, input.account_id, &start, input.save) {
        return Err(Error::conflict("The run started from a save Orvyn didn't give out; play again."));
    }
    let r = super::engine::replay(input.game, input.log).await?;
    if let Some(p) = r["problems"].as_array().filter(|p| !p.is_empty()) {
        let list: Vec<&str> = p.iter().filter_map(Value::as_str).collect();
        return Err(score_error(format!("The run couldn't be replayed: {}", list.join("; "))));
    }
    if r["verifiable"] != true {
        return Err(score_error(r["reason"].as_str().unwrap_or("This game's scores can't be checked.").to_string()));
    }
    if r["status"] == "running" {
        return Err(score_error("The run hadn't ended when it was sent."));
    }
    let value = if board.var == "$time" {
        if r["status"] != "won" {
            return Ok(json!({ "counted": false, "reason": "Only won runs count for time.", "replay": { "status": r["status"], "time": r["time"] } }));
        }
        (r["time"].as_f64().unwrap_or(0.0) * 1000.0).round() / 1000.0
    } else {
        let v = &r["vars"][&board.var];
        let n = v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())).or_else(|| v.as_bool().map(|b| b as i32 as f64)).filter(|n| n.is_finite());
        n.ok_or_else(|| score_error(format!("\"{}\" isn't a number at the end of the run.", board.var)))?
    };
    let account = input.account_id.to_lowercase();
    let frames = r["frames"].as_i64().unwrap_or(0) as i32;
    let prev: Option<GameScore> = sqlx::query_as("select * from game_scores where game_id = $1 and account_id = $2").bind(input.game_id).bind(&account).fetch_optional(db()).await?;
    let best = prev.as_ref().is_none_or(|p| better(board.order, value, p.value));
    match &prev {
        None => {
            sqlx::query("insert into game_scores (game_id, account_id, value, version, frames) values ($1, $2, $3, $4, $5)").bind(input.game_id).bind(&account).bind(value).bind(input.version).bind(frames).execute(db()).await?;
        }
        Some(_) if best => {
            sqlx::query("update game_scores set value = $3, version = $4, frames = $5, runs = runs + 1, updated_at = now() where game_id = $1 and account_id = $2")
                .bind(input.game_id)
                .bind(&account)
                .bind(value)
                .bind(input.version)
                .bind(frames)
                .execute(db())
                .await?;
        }
        Some(_) => {
            sqlx::query("update game_scores set runs = runs + 1 where game_id = $1 and account_id = $2").bind(input.game_id).bind(&account).execute(db()).await?;
        }
    }
    let mine = if best { value } else { prev.unwrap().value };
    Ok(json!({ "counted": true, "value": value, "best": mine, "newBest": best, "rank": rank_of(input.game_id, &board, mine).await?, "label": board.label }))
}

async fn rank_of(game_id: &str, board: &Board, value: f64) -> Result<i64> {
    let op = if board.order == "high" { ">" } else { "<" };
    let n: i64 = sqlx::query_scalar(&format!("select count(*) from game_scores where game_id = $1 and value {op} $2")).bind(game_id).bind(value).fetch_one(db()).await?;
    Ok(n + 1)
}

/// A public name for a player: their social handle, or a short address. Never an email.
fn name_of(account_id: &str, social: Option<&str>) -> String {
    match social.filter(|s| !s.is_empty()) {
        Some(s) => format!("@{}", s.trim_start_matches('@')),
        None => {
            let n = account_id.chars().count();
            format!("{}…{}", account_id.chars().take(6).collect::<String>(), account_id.chars().skip(n.saturating_sub(4)).collect::<String>())
        }
    }
}

/// The top of a game's leaderboard, and the viewer's own place on it.
pub async fn leaderboard(game_id: &str, game: &Value, limit: Option<i64>, account_id: Option<&str>) -> Result<Option<Value>> {
    let Some(board) = board_of(game) else { return Ok(None) };
    let limit = limit.unwrap_or(20).clamp(1, 100);
    let dir = if board.order == "high" { "desc" } else { "asc" };
    let rows: Vec<GameScore> = sqlx::query_as(&format!("select * from game_scores where game_id = $1 order by value {dir}, updated_at asc limit $2")).bind(game_id).bind(limit).fetch_all(db()).await?;
    let ids: Vec<String> = rows.iter().map(|r| r.account_id.clone()).collect();
    let people: Vec<(String, Option<String>)> = if ids.is_empty() { vec![] } else { sqlx::query_as("select account_id, social_username from users where account_id = any($1)").bind(&ids).fetch_all(db()).await? };
    let players: i64 = sqlx::query_scalar("select count(*) from game_scores where game_id = $1").bind(game_id).fetch_one(db()).await?;
    let me = match account_id {
        Some(a) => {
            let mine: Option<GameScore> = sqlx::query_as("select * from game_scores where game_id = $1 and account_id = $2").bind(game_id).bind(a.to_lowercase()).fetch_optional(db()).await?;
            match mine {
                Some(m) => Some(json!({ "rank": rank_of(game_id, &board, m.value).await?, "value": m.value })),
                None => None,
            }
        }
        None => None,
    };
    let viewer = account_id.map(str::to_lowercase);
    let top: Vec<Value> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let social = people.iter().find(|p| p.0 == r.account_id).and_then(|p| p.1.as_deref());
            json!({
                "rank": i + 1,
                "name": name_of(&r.account_id, social),
                "value": r.value,
                "runs": r.runs,
                "at": r.updated_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                "me": viewer.as_deref() == Some(r.account_id.as_str()),
            })
        })
        .collect();
    Ok(Some(json!({ "board": board, "players": players, "top": top, "me": me })))
}
