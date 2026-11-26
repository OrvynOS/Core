//! Dream worlds end to end, on a throwaway database with the real Orvyn Worlds server (crates/worlds):
//! - world files are checked; GPU worlds can't be free (publishing and access settings refuse it);
//! - licences by country (Tencent models: not the EU/UK/KR, not unknown), sign-in for GPU worlds;
//! - a session signed here is accepted by the Rust server, frames stream as keys are held, the
//!   player ends it, and the server's signed usage report is recorded (time capped);
//! - one open session per player, the daily GPU allowance; forged reports are refused;
//! - a dream world's thumbnail is a still of the world.
//!
//! Needs the server built: `cargo build -p orvyn-worlds --release`, then
//! `cargo test -p orvyn-core --test worlds -- --ignored`.
mod support;

use axum::http::HeaderMap;
use axum::routing::post;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use orvyn_core::db::Game;
use orvyn_core::games::{self, worlds, NewGame};
use serde_json::{json, Value};
use sha2::Sha256;
use tokio_tungstenite::tungstenite::Message;

const KEY: &str = "worlds-check-key-0123456789abcdef0123456789";
const CREATOR: &str = "0xc0ffee0000000000000000000000000000000001";
const PLAYER: &str = "0xa11ce00000000000000000000000000000000002";

fn server_binary() -> std::path::PathBuf {
    let target = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
    let exe = if cfg!(windows) { "orvyn-worlds.exe" } else { "orvyn-worlds" };
    ["release", "debug"].iter().map(|p| target.join(p).join(exe)).find(|p| p.exists()).expect("build the Worlds server first: cargo build -p orvyn-worlds --release")
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

async fn refused<T: std::fmt::Debug>(what: &str, r: orvyn_core::Result<T>, needle: &str) {
    match r {
        Ok(v) => panic!("{what}: allowed ({v:?})"),
        Err(e) => assert!(e.message.contains(needle), "{what}: {}", e.message),
    }
}

fn session<'a>(game: &'a Game, account: Option<&'a str>, world: &'a Value, country: Option<&'a str>, image: Option<&'a str>) -> worlds::OpenSession<'a> {
    worlds::OpenSession { game_id: &game.id, account_id: account, world, country, image_url: image, preview: false }
}

/// Kills the server when the test ends, pass or fail.
struct Running(std::process::Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
#[ignore = "needs the Worlds server built (cargo build -p orvyn-worlds --release)"]
async fn dream_worlds() {
    let port = free_port();
    let worlds_url = format!("http://127.0.0.1:{port}");
    support::test_env(&[("ORVYN_WORLDS_KEY", KEY), ("ORVYN_WORLDS_URL", &worlds_url)]);
    let db = support::shared_db().await;

    // ---- World files ----
    let dream = json!({ "format": "orvyn-world/1", "title": "Dune Dusk", "description": "Fly over golden dunes.", "model": "dream", "prompt": "A golden dusk over endless dunes and still water", "seconds": 60, "size": [320, 180] });
    let with = |patch: Value| {
        let mut w = dream.clone();
        for (k, v) in patch.as_object().unwrap() {
            w[k] = v.clone();
        }
        w
    };
    let c1 = games::check_game(&dream, &[]).await.unwrap();
    assert!(c1.ok, "a dream world checks clean: {:?}", c1.problems);
    let c2 = games::check_game(&with(json!({ "model": "yume" })), &[]).await.unwrap();
    assert!(c2.problems.iter().any(|p| p.contains("starts from a picture")), "a picture model needs its picture: {:?}", c2.problems);
    let c3 = games::check_game(&with(json!({ "model": "yume", "image": "start.png" })), &[]).await.unwrap();
    assert!(c3.missing_assets.contains(&"start.png".to_string()), "the picture must be uploaded");
    let c4 = games::check_game(&with(json!({ "seconds": 5000, "prompt": "x", "model": "sora" })), &[]).await.unwrap();
    assert!(c4.problems.len() >= 3, "time, prompt and model are checked: {:?}", c4.problems);

    let (g0, _) = games::create_game(NewGame { creator_account_id: CREATOR, draft: &dream, agent_id: None, hire_id: None, publish_as_agent: false }).await.unwrap();
    let g = games::publish_game(&g0).await.unwrap();
    assert_eq!(g.version, 1, "a dream world publishes (free)");
    let pine = with(json!({ "title": "Pine Walk", "model": "yume", "image": "start.png" }));
    let (gpu, _) = games::create_game(NewGame { creator_account_id: CREATOR, draft: &pine, agent_id: None, hire_id: None, publish_as_agent: false }).await.unwrap();
    let png = hex::decode("89504e470d0a1a0a0000000d4948445200000001000000010802000000907753de0000000c4944415408d763f8cfc000000301010018dd8db00000000049454e44ae426082").unwrap();
    games::upload_asset(&gpu, "start.png", &png).await.unwrap();
    refused("a free GPU world can't be published", games::publish_game(&gpu).await, "costs").await;
    refused("and can't be made free", games::set_access(&gpu, &json!({ "kind": "free", "tips": true }), None).await, "can't be free").await;

    // ---- Session rules ----
    let tencent = with(json!({ "model": "worldplay" }));
    let yume = with(json!({ "model": "yume" }));
    refused("Tencent worlds aren't opened in the EU", worlds::open_session(session(&g, Some(PLAYER), &tencent, Some("FR"), None)).await, "doesn't cover your country").await;
    refused("or where the country is unknown", worlds::open_session(session(&g, Some(PLAYER), &tencent, None, None)).await, "couldn't tell where you are").await;
    refused("GPU worlds need a signed-in player", worlds::open_session(session(&g, None, &yume, Some("US"), None)).await, "Sign in").await;
    let anon = worlds::open_session(session(&g, None, &dream, None, None)).await.unwrap();
    assert!(anon["url"].as_str().unwrap().contains("/play?token="), "anyone can open a dream world, anywhere");

    // ---- The real Worlds server ----
    // A stand-in Orvyn API on this test's runtime, recording usage reports with the real handler.
    let reports = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let r2 = reports.clone();
    let api = Router::new().route(
        "/api/worlds/usage",
        post(move |headers: HeaderMap, body: String| {
            let reports = r2.clone();
            async move {
                let sig = headers.get("x-orvyn-signature").and_then(|v| v.to_str().ok()).map(String::from);
                let r = worlds::record_usage(&body, sig.as_deref()).await;
                reports.lock().unwrap().push(r.as_ref().map(Clone::clone).unwrap_or_else(|e| json!({ "error": e.message })));
                if r.is_ok() { "{}" } else { "refused" }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, api).await.unwrap() });
    let data_dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("worlds-check");
    let _server = Running(
        std::process::Command::new(server_binary())
            .envs([("WORLDS_KEY", KEY), ("ORVYN_API_URL", &api_url), ("PORT", &port.to_string()), ("WORLDS_SLOTS", "2"), ("DATA_DIR", &data_dir.display().to_string())])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let http = reqwest::Client::new();
    for _ in 0..100 {
        if http.get(format!("{worlds_url}/health")).send().await.is_ok_and(|r| r.status().is_success()) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    let s = worlds::open_session(session(&g, Some(PLAYER), &dream, Some("DE"), None)).await.unwrap();
    let url = s["url"].as_str().unwrap().to_string();
    assert!(url.starts_with(&format!("ws://127.0.0.1:{port}/play?token=")), "the session URL points at the Worlds server over WebSocket");
    refused("one open session per player", worlds::open_session(session(&g, Some(PLAYER), &dream, Some("DE"), None)).await, "already in a world").await;

    // Play it with a WebSocket, as the play page does: hold keys, then end it.
    let (mut ws, _) = tokio_tungstenite::connect_async(url.as_str()).await.unwrap();
    let (mut ready, mut ended, mut frames, mut first) = (Value::Null, Value::Null, 0, vec![]);
    let mut end_at = None;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let next = tokio::time::timeout_at(end_at.unwrap_or(deadline).min(deadline), ws.next()).await;
        let msg = match next {
            Err(_) if end_at.is_some() && tokio::time::Instant::now() < deadline => {
                ws.send(Message::text(json!({ "type": "end" }).to_string())).await.unwrap();
                end_at = None;
                continue;
            }
            Err(_) | Ok(None) => break,
            Ok(Some(m)) => m.unwrap(),
        };
        match msg {
            Message::Text(t) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                if v["type"] == "ready" {
                    ready = v;
                    ws.send(Message::text(json!({ "type": "keys", "keys": ["w", "left"] }).to_string())).await.unwrap();
                    end_at = Some(tokio::time::Instant::now() + std::time::Duration::from_millis(2500));
                } else if v["type"] == "ended" {
                    ended = v;
                }
            }
            Message::Binary(b) => {
                frames += 1;
                if first.is_empty() {
                    first = b[..3].to_vec();
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    assert!(ready["type"] == "ready" && ready["width"] == 320, "the Rust server accepts the session Orvyn signed: {ready}");
    assert!(frames > 30 && first == [0xff, 0xd8, 0xff], "frames stream (JPEG): {frames} frames");
    assert_eq!(ended["reason"], "the player ended it");

    for _ in 0..50 {
        if !reports.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    let sid = s["sid"].as_str().unwrap();
    let (seconds, country, ended_at): (Option<f64>, Option<String>, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("select seconds::float8, country, ended_at from world_sessions where id = $1").bind(sid).fetch_one(db).await.unwrap();
    assert!(ended_at.is_some() && seconds.is_some_and(|s| s > 1.5 && s < 6.0), "the server's signed usage report is recorded: {seconds:?} {:?}", reports.lock().unwrap());
    assert_eq!(country.as_deref(), Some("DE"), "the country is kept with the session");

    refused("a forged usage report", worlds::record_usage(&json!({ "sid": sid, "seconds": 1 }).to_string(), Some(&"0".repeat(64))).await, "signature").await;
    let big = json!({ "sid": sid, "seconds": 99999 }).to_string();
    let mut mac = Hmac::<Sha256>::new_from_slice(KEY.as_bytes()).unwrap();
    mac.update(format!("usage:{big}").as_bytes());
    let capped = worlds::record_usage(&big, Some(&hex::encode(mac.finalize().into_bytes()))).await.unwrap();
    assert!(capped["seconds"].as_f64().unwrap() <= 65.0, "reported time is capped at what was allowed: {capped}");

    // The daily GPU allowance.
    sqlx::query("insert into world_sessions (id, game_id, account_id, model, gpu, allowed_seconds, seconds, ended_at, expires_at) values ('wsold', $1, $2, 'yume', true, 1800, 1790, now(), now())")
        .bind(&g.id)
        .bind(PLAYER)
        .execute(db)
        .await
        .unwrap();
    refused("the daily GPU allowance holds", worlds::open_session(session(&g, Some(PLAYER), &yume, Some("US"), Some("https://example.com/a.png"))).await, "today's time").await;

    // A dream world with no picture gets a still of itself as its thumbnail.
    games::render_thumbnail(&g).await.unwrap();
    let stored = games::thumbnail_of(&g.id).await.unwrap();
    assert!(stored.as_ref().is_some_and(|(b, _)| b.starts_with(&[0xff, 0xd8])), "a dream world's thumbnail is a still of the world ({} bytes)", stored.map(|(b, _)| b.len()).unwrap_or(0));
}
