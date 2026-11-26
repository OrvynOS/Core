//! Orvyn Studio from the agent's side: render files for a job on a Studio worker (crates/studio),
//! within what the agent's tier and the job's category allow, and keep a record of every file so
//! the client can download what was delivered.
//!
//! - The renderer must be in the tier's set (tiers.rs) and useful for the job's category.
//! - Each job gets the tier's number of renders (failed ones count: retries aren't free).
//! - The worker validates each spec (crates/studio/src/spec.rs) and checks every output; only
//!   files that passed can be delivered. A refused spec records nothing and costs no render.
//! - Files stay on the worker; clients get short-lived signed links (`file_link`).

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;

use crate::db::{db, Agent, JobFile};
use crate::env::env;
use crate::error::{Error, Result};
use crate::tiers::{job_category, Tier, TIER_ORDER};

pub const RENDERERS: &[&str] = &["diagram", "html-still", "design-review", "ffmpeg", "html-video", "reel", "manim", "blender", "ai-video", "world-3d", "world-video", "long-video"];
const GPU_RENDERERS: &[&str] = &["blender", "ai-video", "world-3d", "world-video", "long-video"];
const MODEL_RENDERERS: &[&str] = &["world-3d", "world-video", "long-video"];

/// How to write each renderer's spec, told to the agent in its job session.
pub static RENDERER_GUIDE: LazyLock<HashMap<String, String>> = LazyLock::new(|| serde_json::from_str(include_str!("studio_guide.json")).expect("studio_guide.json"));

fn studio_error(m: impl Into<String>) -> Error {
    Error::invalid(m)
}

pub fn studio_enabled() -> bool {
    let e = env();
    e.orvyn_studio_url.is_some() && e.orvyn_studio_key.is_some()
}

fn worker_for(renderer: &str) -> Result<String> {
    let e = env();
    if e.orvyn_studio_key.is_none() {
        return Err(Error::unavailable("Orvyn Studio isn't set up on this deployment."));
    }
    let gpu = GPU_RENDERERS.contains(&renderer);
    let url = if gpu { e.orvyn_studio_gpu_url.clone() } else { e.orvyn_studio_url.clone() };
    url.map(|u| u.trim_end_matches('/').to_string()).ok_or_else(|| {
        Error::unavailable(if gpu { "No GPU Studio worker is running yet, so 3D, AI video and worlds can't be rendered." } else { "Orvyn Studio isn't set up on this deployment." })
    })
}

/// Why `tier` can't run `renderer` for a job in `category_id`, or None if it can.
pub fn render_blocked(tier: Tier, renderer: &str, category_id: Option<&str>) -> Option<String> {
    let spec = tier.spec();
    if !spec.studio.renderers.contains(&renderer) {
        let needs = TIER_ORDER.iter().filter(|t| matches!(t, Tier::Pro | Tier::Studio)).find(|t| t.spec().studio.renderers.contains(&renderer));
        return Some(format!("{renderer} needs {} hosting (this agent is on {}).", needs.map(|t| t.spec().name).unwrap_or("a higher"), spec.name));
    }
    let category = category_id.and_then(job_category)?;
    if !category.renderers.contains(&renderer) {
        let use_ = if category.renderers.is_empty() { "text only".to_string() } else { category.renderers.join(", ") };
        return Some(format!("{renderer} isn't used for {} jobs (use {use_}).", category.name));
    }
    None
}

// ---- Models (crates/studio/src/renderers/models.rs) and their licences ----

/// The Tencent Hunyuan community licences' territory: worldwide except the EU, the UK and Korea.
pub const EU: &[&str] = &["AT", "BE", "BG", "HR", "CY", "CZ", "DK", "EE", "FI", "FR", "DE", "GR", "HU", "IE", "IT", "LV", "LT", "LU", "MT", "NL", "PL", "PT", "RO", "SK", "SI", "ES", "SE"];

pub fn tencent_excluded(country: &str) -> bool {
    EU.contains(&country) || country == "GB" || country == "KR"
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelInfo {
    pub id: &'static str,
    pub renderer: &'static str,
    pub name: &'static str,
    pub what: &'static str,
    pub license: &'static str,
    pub url: &'static str,
    /// Whether the licence excludes the EU, the UK and South Korea (Tencent's).
    pub tencent: bool,
    /// Said with every output (licence notices, provider disclosure, AI labelling).
    pub notice: String,
}

fn tencent_notice(model: &str) -> String {
    format!("Made with {model} (Powered by Tencent Hunyuan), under the Tencent Hunyuan Community License Agreement. This service is provided by Orvyn; Tencent isn't affiliated with it and doesn't endorse it. AI-generated content.")
}

pub static MODELS: LazyLock<Vec<ModelInfo>> = LazyLock::new(|| {
    let m = |id, renderer, name, what, license, url, tencent: bool, notice: String| ModelInfo { id, renderer, name, what, license, url, tencent, notice };
    vec![
        m("lyra", "world-3d", "Lyra 1.0 (NVIDIA)", "One image → an explorable 3D scene as Gaussian splats (.ply).", "Apache-2.0 (code), NVIDIA Open Model License (weights)", "https://github.com/nv-tlabs/lyra", false, "AI-generated 3D scene (Lyra by NVIDIA).".into()),
        m("hunyuanworld", "world-3d", "HunyuanWorld 1.0 (Tencent)", "Text or an image → a 360° panorama and a layered 3D mesh world.", "Tencent HunyuanWorld-1.0 Community License", "https://github.com/Tencent-Hunyuan/HunyuanWorld-1.0", true, tencent_notice("HunyuanWorld 1.0")),
        m("yume", "world-video", "YUME 1.5", "An image + a path of keys → a video walking through that world.", "Apache-2.0", "https://github.com/stdstu12/YUME", false, "AI-generated video (YUME).".into()),
        m("worldplay", "world-video", "HY-World 1.5 WorldPlay (Tencent)", "An image + keyboard/mouse path → a long, geometrically consistent world video.", "Tencent HY-WorldPlay Community License", "https://github.com/Tencent-Hunyuan/HY-WorldPlay", true, tencent_notice("HY-World 1.5 (WorldPlay)")),
        m("gamecraft", "world-video", "Hunyuan-GameCraft 1.0 (Tencent)", "An image + WASD actions → game-style 720p video.", "Tencent Hunyuan Community License (GameCraft)", "https://github.com/Tencent-Hunyuan/Hunyuan-GameCraft-1.0", true, tencent_notice("Hunyuan-GameCraft 1.0")),
        m("helios", "long-video", "Helios (PKU-YuanGroup)", "Text (or an image) → minute-long video in real time; prompts can change along the way.", "Apache-2.0", "https://github.com/PKU-YuanGroup/Helios", false, "AI-generated video (Helios).".into()),
        m("longlive", "long-video", "LongLive (NVIDIA)", "A timeline of prompts → one long video that follows them.", "Apache-2.0", "https://github.com/NVlabs/LongLive", false, "AI-generated video (LongLive by NVIDIA).".into()),
    ]
});

pub fn model(id: &str) -> Option<&'static ModelInfo> {
    MODELS.iter().find(|m| m.id == id)
}

pub fn models_for(renderer: &str) -> Vec<&'static ModelInfo> {
    MODELS.iter().filter(|m| m.renderer == renderer).collect()
}

/// Why `model_id` can't be used for someone in `country`, or None if it can.
pub fn model_blocked(model_id: &str, renderer: &str, country: Option<&str>) -> Option<String> {
    let ids = |list: Vec<&ModelInfo>| list.iter().map(|m| m.id).collect::<Vec<_>>();
    let Some(m) = model(model_id) else {
        let known = ids(models_for(renderer)).join(", ");
        return Some(format!("Unknown model {model_id} ({} for {renderer}).", if known.is_empty() { "none".into() } else { known }));
    };
    if m.renderer != renderer {
        return Some(format!("{} is a {} model, not {renderer}.", m.name, m.renderer));
    }
    if m.tencent {
        let open = ids(models_for(m.renderer).into_iter().filter(|x| !x.tencent).collect()).join(" or ");
        match country {
            None => return Some(format!("{}'s licence doesn't cover every country, and the client's country isn't known. Use {open} instead.", m.name)),
            Some(c) if tencent_excluded(c) => return Some(format!("{}'s licence doesn't cover the client's country ({c}). Use {open} instead.", m.name)),
            _ => {}
        }
    }
    None
}

// ---- Renders ----

pub async fn renders_for_hire(hire_id: &str) -> Result<i64> {
    Ok(sqlx::query_scalar("select count(*) from studio_renders where hire_id = $1").bind(hire_id).fetch_one(db()).await?)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputCheck {
    pub ok: bool,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderedFile {
    pub name: String,
    pub mime: String,
    pub bytes: i64,
    pub sha256: String,
    pub check: OutputCheck,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderResult {
    pub id: String,
    pub ok: bool,
    pub renderer: String,
    pub files: Vec<RenderedFile>,
    #[serde(default)]
    pub log: String,
    pub error: Option<String>,
    pub duration_ms: i64,
}

pub struct JobRef<'a> {
    pub hire_id: &'a str,
    pub category: &'a str,
    pub country: Option<&'a str>,
}

pub struct Rendered {
    pub result: RenderResult,
    pub files: Vec<JobFile>,
    pub renders_left: Option<i64>,
    pub notice: Option<String>,
}

fn limits_of(tier: Tier) -> Value {
    let s = &tier.spec().studio;
    json!({ "maxRenderSeconds": s.max_render_seconds, "maxVideoSeconds": s.max_video_seconds, "maxWidth": s.max_width, "maxHeight": s.max_height, "maxOutputMb": s.max_output_mb })
}

fn random_id(prefix: &str) -> String {
    format!("{prefix}{}", hex::encode(rand::random::<[u8; 9]>()))
}

/// Render on a Studio worker for a job (or none), record it and its files.
pub async fn studio_render(agent: &Agent, job: Option<JobRef<'_>>, renderer: &str, spec: &Value) -> Result<Rendered> {
    let tier = Tier::parse(&agent.tier);
    if !RENDERERS.contains(&renderer) {
        return Err(studio_error(format!("unknown renderer {renderer}")));
    }
    if let Some(why) = render_blocked(tier, renderer, job.as_ref().map(|j| j.category)) {
        return Err(studio_error(why));
    }
    let mut notice = None;
    if MODEL_RENDERERS.contains(&renderer) {
        let id = spec["model"].as_str().unwrap_or("");
        if let Some(why) = model_blocked(id, renderer, job.as_ref().and_then(|j| j.country)) {
            return Err(studio_error(why));
        }
        notice = model(id).map(|m| m.notice.clone());
    }
    let s = &tier.spec().studio;
    if let Some(j) = &job {
        if renders_for_hire(j.hire_id).await? >= s.renders_per_job as i64 {
            return Err(studio_error(format!("This job has used all {} renders {} allows. Deliver what you have, or decline.", s.renders_per_job, tier.spec().name)));
        }
    }
    let base = worker_for(renderer)?;
    let res = crate::http::client()
        .post(format!("{base}/renders"))
        .bearer_auth(env().orvyn_studio_key.clone().unwrap_or_default())
        .json(&json!({ "renderer": renderer, "spec": spec, "limits": limits_of(tier) }))
        // The worker answers when the render is done: its time limit plus the wait in line.
        .timeout(Duration::from_secs(s.max_render_seconds as u64 + 600))
        .send()
        .await
        .map_err(|e| Error::unavailable(format!("Orvyn Studio didn't answer ({}). Try again shortly.", if e.is_timeout() { "timed out" } else { "unreachable" })))?;
    let status = res.status();
    let body: Value = res.json().await.unwrap_or(Value::Null);
    let result: Option<RenderResult> = serde_json::from_value(body.clone()).ok().filter(|r: &RenderResult| !r.id.is_empty());
    let Some(result) = result.filter(|_| status.is_success()) else {
        return Err(studio_error(body["error"].as_str().map(String::from).unwrap_or_else(|| format!("Orvyn Studio returned HTTP {}.", status.as_u16()))));
    };
    let hire_id = job.as_ref().map(|j| j.hire_id.to_string());
    sqlx::query("insert into studio_renders (id, agent_id, hire_id, renderer, worker_url, ok, error, duration_ms) values ($1, $2, $3, $4, $5, $6, $7, $8)")
        .bind(&result.id)
        .bind(&agent.id)
        .bind(&hire_id)
        .bind(renderer)
        .bind(&base)
        .bind(result.ok)
        .bind(&result.error)
        .bind(result.duration_ms as i32)
        .execute(db())
        .await?;
    let mut files = vec![];
    for f in &result.files {
        let row: JobFile = sqlx::query_as(
            "insert into job_files (id, render_id, agent_id, hire_id, name, mime, bytes, sha256, worker_url, check_ok, check_notes) values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) returning *",
        )
        .bind(random_id("f"))
        .bind(&result.id)
        .bind(&agent.id)
        .bind(&hire_id)
        .bind(&f.name)
        .bind(&f.mime)
        .bind(f.bytes as i32)
        .bind(&f.sha256)
        .bind(&base)
        .bind(f.check.ok)
        .bind(json!(f.check.notes))
        .fetch_one(db())
        .await?;
        files.push(row);
    }
    let renders_left = match &job {
        Some(j) => Some(s.renders_per_job as i64 - renders_for_hire(j.hire_id).await?),
        None => None,
    };
    Ok(Rendered { result, files, renders_left, notice })
}

/// A render Orvyn itself asks for (game thumbnails): no agent, job or quota. Returns the result and
/// each output's bytes (fetched from the worker with a signed link).
pub async fn system_render(renderer: &str, spec: &Value, limits: Value) -> Result<(RenderResult, Vec<(RenderedFile, Vec<u8>)>)> {
    let base = worker_for(renderer)?;
    let max = limits["maxRenderSeconds"].as_u64().unwrap_or(120);
    let res = crate::http::client()
        .post(format!("{base}/renders"))
        .bearer_auth(env().orvyn_studio_key.clone().unwrap_or_default())
        .json(&json!({ "renderer": renderer, "spec": spec, "limits": limits }))
        .timeout(Duration::from_secs(max + 300))
        .send()
        .await
        .map_err(|e| Error::unavailable(format!("Orvyn Studio: {e}")))?;
    let status = res.status();
    let result: RenderResult = res.json().await.ok().filter(|r: &RenderResult| status.is_success() && !r.id.is_empty()).ok_or_else(|| Error::unavailable(format!("Orvyn Studio returned HTTP {}", status.as_u16())))?;
    let mut out = vec![];
    for f in &result.files {
        let url = file_link(&result.id, &f.name, &base, 600)?.replacen(&public_base(&base), &base, 1);
        let bytes = crate::http::client().get(url).timeout(Duration::from_secs(60)).send().await.map_err(|e| Error::unavailable(e.to_string()))?.bytes().await.map_err(|e| Error::unavailable(e.to_string()))?;
        out.push((f.clone(), bytes.to_vec()));
    }
    Ok((result, out))
}

/// Where clients reach a worker's /files (the API talks to it on a private address).
fn public_base(worker_url: &str) -> String {
    let e = env();
    let strip = |u: &Option<String>| u.as_deref().map(|s| s.trim_end_matches('/').to_string());
    if strip(&e.orvyn_studio_url).as_deref() == Some(worker_url) {
        if let Some(p) = strip(&e.orvyn_studio_public_url) {
            return p;
        }
    }
    if strip(&e.orvyn_studio_gpu_url).as_deref() == Some(worker_url) {
        if let Some(p) = strip(&e.orvyn_studio_gpu_public_url) {
            return p;
        }
    }
    worker_url.to_string()
}

/// HMAC-SHA256 payload for a file URL: `${renderId}/${name}:${expiresAt}`.
pub fn file_signature_payload(render_id: &str, name: &str, expires_at: i64) -> String {
    format!("{render_id}/{name}:{expires_at}")
}

/// A short-lived download link for a file (the worker checks the signature and expiry).
pub fn file_link(render_id: &str, name: &str, worker_url: &str, ttl_seconds: i64) -> Result<String> {
    let key = env().orvyn_studio_key.clone().ok_or_else(|| Error::unavailable("Orvyn Studio isn't set up on this deployment."))?;
    let exp = chrono::Utc::now().timestamp() + ttl_seconds;
    let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes()).unwrap();
    mac.update(file_signature_payload(render_id, name, exp).as_bytes());
    let sig = hex::encode(mac.finalize().into_bytes());
    let encoded: String = url::form_urlencoded::byte_serialize(name.as_bytes()).collect::<String>().replace('+', "%20");
    Ok(format!("{}/files/{render_id}/{encoded}?exp={exp}&sig={sig}", public_base(worker_url)))
}

pub async fn files_for_hire(hire_id: &str, delivered_only: bool) -> Result<Vec<JobFile>> {
    Ok(sqlx::query_as("select * from job_files where hire_id = $1 and (not $2 or delivered) order by created_at").bind(hire_id).bind(delivered_only).fetch_all(db()).await?)
}

/// Files the agent may deliver for this hire: these ids, rendered for this job, all checks passed.
pub async fn deliverable_files(hire_id: &str, ids: &[String]) -> Result<Vec<JobFile>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let mut unique: Vec<String> = vec![];
    for id in ids {
        if !unique.contains(id) {
            unique.push(id.clone());
        }
    }
    let rows: Vec<JobFile> = sqlx::query_as("select * from job_files where hire_id = $1 and id = any($2)").bind(hire_id).bind(&unique).fetch_all(db()).await?;
    let missing: Vec<&String> = unique.iter().filter(|id| !rows.iter().any(|r| &r.id == *id)).collect();
    if !missing.is_empty() {
        return Err(studio_error(format!("Not files of this job: {}", missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "))));
    }
    let failed: Vec<String> = rows
        .iter()
        .filter(|r| !r.check_ok)
        .map(|f| format!("{} ({})", f.name, f.check_notes.as_array().map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("; ")).unwrap_or_default()))
        .collect();
    if !failed.is_empty() {
        return Err(studio_error(format!("These files failed their checks and can't be delivered: {}", failed.join(", "))));
    }
    Ok(rows)
}

pub async fn mark_delivered(ids: &[String]) -> Result<()> {
    if !ids.is_empty() {
        sqlx::query("update job_files set delivered = true where id = any($1)").bind(ids).execute(db()).await?;
    }
    Ok(())
}

/// The delivery's file list, appended to the result text so its hash (on chain) covers the files.
pub fn files_manifest(files: &[JobFile]) -> String {
    if files.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = files.iter().map(|f| format!("- {} ({} KB) {}", f.name, crate::auth::js_number((f.bytes as f64 / 1024.0).round()), f.sha256)).collect();
    format!("\n\n---\nFiles (SHA-256):\n{}", lines.join("\n"))
}
