//! Creative jobs end to end, on a throwaway database with a real Orvyn Studio worker (crates/studio):
//! - the agent's tier and the job's category decide which renderers it may use;
//! - a render is recorded with its files; the per-job quota holds;
//! - a file that failed its checks can't be delivered, nor another job's; passing files can, and
//!   the result's on-chain hash covers their SHA-256s;
//! - the client downloads delivered files through a signed link (bytes match); a tampered or
//!   expired link is refused;
//! - a game's thumbnail is rendered by the real player in Studio; every renderer has a guide.
//!
//! Needs the worker built (`cargo build -p orvyn-studio`) and Chromium and ffmpeg on the machine
//! (CHROME_PATH, FFMPEG_PATH, FFPROBE_PATH): `cargo test -p orvyn-core --test studio_jobs -- --ignored`.
mod support;

use orvyn_core::db::{Agent, JobFile};
use orvyn_core::games::{self, NewGame};
use orvyn_core::studio::{self, JobRef};
use orvyn_core::tiers::Tier;
use orvyn_core::{hires, secrets};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const KEY: &str = "studio-jobs-check-key-0123456789abcdef0123";

struct Running(std::process::Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn worker_binary() -> std::path::PathBuf {
    let target = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
    let exe = if cfg!(windows) { "orvyn-studio.exe" } else { "orvyn-studio" };
    ["release", "debug"].iter().map(|p| target.join(p).join(exe)).find(|p| p.exists()).expect("build the Studio worker first: cargo build -p orvyn-studio")
}

fn sha256(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

#[tokio::test]
#[ignore = "needs the Studio worker built, Chromium and ffmpeg"]
async fn creative_jobs() {
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let worker = format!("http://127.0.0.1:{port}");
    support::test_env(&[("ORVYN_STUDIO_URL", &worker), ("ORVYN_STUDIO_KEY", KEY)]);
    let db = support::shared_db().await;
    let data_dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("studio-jobs");
    let _worker = Running(
        std::process::Command::new(worker_binary())
            .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
            .envs([("STUDIO_KEY", KEY), ("PORT", &port.to_string()), ("DATA_DIR", &data_dir.display().to_string())])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let http = reqwest::Client::new();
    for _ in 0..100 {
        if http.get(format!("{worker}/health")).send().await.is_ok_and(|r| r.status().is_success()) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    let caps: Value = http.get(format!("{worker}/capabilities")).bearer_auth(KEY).send().await.unwrap().json().await.unwrap();
    assert!(caps["available"].as_array().is_some_and(|a| a.contains(&json!("html-video"))) && caps["hardware"]["cpus"].as_u64().unwrap_or(0) > 0, "worker up with renderers and hardware: {caps}");
    assert_eq!(http.get(format!("{worker}/capabilities")).send().await.unwrap().status(), 401, "capabilities need the key");

    for (id, tier) in [("pro-agent", "pro"), ("basic-agent", "basic")] {
        sqlx::query(
            "insert into agents (id, name, symbol, owner_account_id, ai_provider, ai_model, ai_key_encrypted, ai_key_hint, evm_address, evm_wallet, launchpad, status, hosting_status, tier)
             values ($1, $1, 'ART', '0x2222222222222222222222222222222222222222', 'anthropic', 'claude-opus-5', $2, 'x', '0x1111111111111111111111111111111111111111', '{}', 'none', 'live', 'running', $3)",
        )
        .bind(id)
        .bind(secrets::encrypt_secret(&"x".repeat(20), id, secrets::SecretKind::AiKey).unwrap())
        .bind(tier)
        .execute(db)
        .await
        .unwrap();
        sqlx::query(
            "insert into hires (id, agent_id, client_account_id, offer_title, brief, token, amount, fee_bps, status, deadline_at)
             values ($1, $2, '0x3333333333333333333333333333333333333333', 'Logo animation', 'Animate my logo for five seconds.', '0x55d398326f99059fF775485246999027B3197955', '50000000000000000000', 1000, 'funded', now() + interval '1 day')",
        )
        .bind(if tier == "pro" { "h-pro" } else { "h-basic" })
        .bind(id)
        .execute(db)
        .await
        .unwrap();
    }
    let load = |id: &'static str| async move { sqlx::query_as::<_, Agent>("select * from agents where id = $1").bind(id).fetch_one(db).await.unwrap() };
    let (pro, basic) = (load("pro-agent").await, load("basic-agent").await);

    // ---- Who may render what ----
    let blocked = |tier, renderer, category| studio::render_blocked(tier, renderer, category).unwrap_or_default();
    assert!(blocked(Tier::Basic, "html-video", None).contains("needs Pro"), "Basic can't render video");
    assert!(blocked(Tier::Pro, "blender", None).contains("needs Studio"), "Pro can't render 3D");
    assert!(blocked(Tier::Pro, "manim", Some("video-edit")).contains("isn't used for"), "the renderer must suit the job");
    assert!(studio::render_blocked(Tier::Pro, "html-video", Some("motion")).is_none(), "a Pro motion job may use html-video");
    let job = |hire_id: &'static str| Some(JobRef { hire_id, category: "motion", country: None });
    let e = studio::studio_render(&basic, job("h-basic"), "html-video", &json!({})).await.err().unwrap();
    assert!(e.message.contains("needs Pro"), "a Basic agent's video render is refused before reaching Studio");

    // ---- Render for the job ----
    let page = r##"<!doctype html><html><head><meta charset="utf-8"><style>body{margin:0;background:#101010}#l{position:absolute;top:120px;left:40px;font:700 64px system-ui;color:#f0c46c;opacity:0}</style><script src="/lib/gsap/gsap.min.js"></script></head><body><div id="l">ORVYN</div><script>gsap.timeline().to("#l",{opacity:1,x:200,duration:1.2}).to("#l",{scale:1.2,duration:0.8});</script></body></html>"##;
    let good = studio::studio_render(&pro, job("h-pro"), "html-video", &json!({ "files": [{ "name": "index.html", "content": page }], "width": 640, "height": 360, "fps": 24, "seconds": 2, "name": "logo.mp4" })).await.unwrap();
    assert!(good.result.ok && good.files.len() == 1 && good.files[0].check_ok && good.files[0].hire_id.as_deref() == Some("h-pro"), "a motion render is recorded with its file: {:?}", good.files.first().map(|f| &f.check_notes));
    let blank = studio::studio_render(&pro, job("h-pro"), "html-video", &json!({ "files": [{ "name": "index.html", "content": "<!doctype html><body style='margin:0;background:#000'></body>" }], "width": 320, "height": 180, "fps": 10, "seconds": 1, "name": "blank.mp4" })).await.unwrap();
    assert!(!blank.result.ok && !blank.files[0].check_ok, "a blank video is recorded as failing its check: {:?}", blank.files[0].check_notes);
    assert_eq!((good.renders_left, blank.renders_left), (Some(29), Some(28)), "renders count against the job's quota");

    // ---- Deliver ----
    let hire = hires::get_hire("h-pro").await.unwrap().unwrap();
    let text = "Here is your five-second logo animation.";
    let e = hires::deliver_result(&hire, text, &[blank.files[0].id.clone()]).await.err().unwrap();
    assert!(e.message.contains("failed their checks"), "a file that failed its checks can't be delivered: {}", e.message);
    let e = hires::deliver_result(&hire, text, &["f-someone-elses".into()]).await.err().unwrap();
    assert!(e.message.contains("Not files of this job"), "another job's files can't be delivered: {}", e.message);
    let delivered = hires::deliver_result(&hire, "Here is your five-second logo animation: logo.mp4, 640x360, 24 fps.", &[good.files[0].id.clone()]).await.unwrap();
    let result = delivered.result.clone().unwrap();
    assert!(result.contains("logo.mp4") && result.contains(&good.files[0].sha256), "delivered with the file listed and hashed");
    assert_eq!(delivered.result_hash.as_deref().map(|h| h.trim_start_matches("0x").to_string()), Some(sha256(result.as_bytes())), "the on-chain hash covers the result");
    let client_files: Vec<JobFile> = studio::files_for_hire("h-pro", true).await.unwrap();
    assert!(client_files.len() == 1 && client_files[0].name == "logo.mp4", "the client sees only the delivered file");
    assert_eq!(studio::files_for_hire("h-pro", false).await.unwrap().len(), 2, "the creator sees every render");

    // ---- Download ----
    let f = &client_files[0];
    let link = studio::file_link(&f.render_id, &f.name, &f.worker_url, 600).unwrap();
    let res = http.get(&link).send().await.unwrap();
    let mime = res.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let bytes = res.bytes().await.unwrap();
    assert!(sha256(&bytes) == f.sha256 && mime == "video/mp4", "the signed link downloads the exact file ({} bytes)", bytes.len());
    let sig_at = link.find("sig=").unwrap() + 4;
    let tampered = format!("{}0000{}", &link[..sig_at], &link[sig_at + 4..]);
    assert_eq!(http.get(&tampered).send().await.unwrap().status(), 403, "a tampered link is refused");
    let expired = studio::file_link(&f.render_id, &f.name, &f.worker_url, -10).unwrap();
    assert_eq!(http.get(&expired).send().await.unwrap().status(), 403, "an expired link is refused");

    // ---- Game thumbnails: the real player in Studio ----
    let vectors: Value = serde_json::from_str(include_str!("vectors/games.json")).unwrap();
    let (game, _) = games::create_game(NewGame { creator_account_id: "0x2222222222222222222222222222222222222222", draft: &vectors["example"], agent_id: None, hire_id: None, publish_as_agent: false }).await.unwrap();
    let published = games::publish_game(&game).await.unwrap();
    let png = games::render_thumbnail(&published).await.unwrap().expect("a thumbnail");
    let size = (u32::from_be_bytes(png[16..20].try_into().unwrap()), u32::from_be_bytes(png[20..24].try_into().unwrap()));
    assert_eq!(size, (960, 540), "the game thumbnail is rendered");
    let view = games::game_view(&games::get_game(&published.id).await.unwrap().unwrap(), None).await.unwrap();
    assert!(!view["thumbnail"].is_null(), "its view links it");

    // ---- The agent's instructions ----
    assert!(studio::RENDERERS.iter().all(|r| studio::RENDERER_GUIDE.get(*r).is_some_and(|g| g.len() > 40)), "every renderer has a guide");
}
