//! The Orvyn Engine on the server: the same engine the player runs (crates/engine, linked natively
//! here instead of loaded as WebAssembly) to check games before they're saved or published:
//! `validate`, `smoke_test` (play it headless with no input), `playtest` (bots play it and say
//! what they found) and `replay` (re-run a player's recorded input: how scores are checked).
//! Each returns the JSON the WebAssembly exports return (crates/engine/src/wasm.rs). The engine is
//! CPU-bound, so calls run on blocking threads.

use orvyn_engine::world::{Input, Status};
use orvyn_engine::Engine;
use serde_json::{json, Value};

use crate::error::{Error, Result};

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f).await.map_err(|e| Error::internal(format!("engine: {e}")))
}

fn src_of(game: &Value) -> String {
    match game {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Problems in a game file (empty: it's valid).
pub fn validate_sync(game: &Value) -> Vec<String> {
    match serde_json::from_value::<orvyn_engine::format::Game>(game.clone()) {
        Ok(g) => orvyn_engine::validate::problems(&g),
        Err(e) => vec![format!("the game file isn't valid: {e}")],
    }
}

pub async fn validate(game: &Value) -> Result<Vec<String>> {
    let g = game.clone();
    blocking(move || validate_sync(&g)).await
}

/// Play `seconds` of the game headless with no input:
/// `{ problems, errors, status, frames, first: { draws, calls }, halted }`.
pub fn smoke_sync(game: &Value, seconds: f64) -> Value {
    let frames = (seconds * 60.0).round() as u32;
    let mut engine = match Engine::load(&src_of(game)) {
        Ok(e) => e,
        Err(problems) => return json!({ "problems": problems, "errors": [], "status": null, "frames": 0, "first": null, "halted": null }),
    };
    let mut errors: Vec<String> = vec![];
    let mut ran = 0;
    let mut first: Option<Value> = None;
    for _ in 0..frames.min(36_000) {
        let f = engine.step(Input::default());
        ran += 1;
        if first.is_none() {
            first = Some(json!({ "draws": f.draws.len(), "calls": f.calls.iter().map(|c| c.function.clone()).collect::<Vec<_>>() }));
        }
        for e in f.errors {
            if !errors.contains(&e) {
                errors.push(e);
            }
        }
        if engine.halted.is_some() || f.status != Status::Running {
            break;
        }
    }
    json!({ "problems": [], "errors": errors, "status": engine.status(), "frames": ran, "first": first, "halted": engine.halted })
}

pub async fn smoke_test(game: &Value, seconds: f64) -> Result<Value> {
    let g = game.clone();
    blocking(move || smoke_sync(&g, seconds)).await
}

/// Bots play the game (idle, explorer, random) for `seconds` each and report what they found.
/// `sheet` adds a base64 PNG contact sheet; `images` are asset name → base64 PNG for it.
pub async fn playtest(game: &Value, seconds: f64, sheet: bool, images: Value) -> Result<Value> {
    let src = src_of(game);
    blocking(move || {
        let opts: orvyn_engine::playtest::Options = serde_json::from_value(json!({ "seconds": seconds })).unwrap_or_default();
        match orvyn_engine::playtest::playtest(&src, &opts) {
            Ok(report) => {
                let mut v = serde_json::to_value(&report).unwrap_or(Value::Null);
                if sheet {
                    let imgs: orvyn_engine::render::Images = images
                        .as_object()
                        .map(|m| m.iter().filter_map(|(k, b)| Some((k.clone(), orvyn_engine::render::unbase64(b.as_str()?)?))).collect())
                        .unwrap_or_default();
                    let size = serde_json::from_str::<orvyn_engine::format::Game>(&src).map(|g| g.size).unwrap_or([480.0, 270.0]);
                    if let Some(png) = orvyn_engine::render::contact_sheet(&report, size, &imgs) {
                        v["sheet"] = json!(orvyn_engine::render::base64(&png));
                    }
                } else if let Some(runs) = v["runs"].as_array_mut() {
                    // Snapshots are for the sheet; callers that didn't ask for one don't need the frames.
                    for run in runs {
                        run["snapshots"] = json!([]);
                    }
                }
                v
            }
            Err(problems) => json!({ "problems": problems }),
        }
    })
    .await
}

/// Replay a recorded run of the game: what it really scored.
pub async fn replay(game: &Value, log: &Value) -> Result<Value> {
    let (src, log) = (src_of(game), log.clone());
    blocking(move || {
        let log: orvyn_engine::replay::InputLog = match serde_json::from_value(log) {
            Ok(l) => l,
            Err(e) => return json!({ "problems": [format!("the input log isn't valid: {e}")] }),
        };
        match orvyn_engine::replay::replay(&src, &log) {
            Ok(r) => serde_json::to_value(&r).unwrap_or(Value::Null),
            Err(problems) => json!({ "problems": problems }),
        }
    })
    .await
}

/// TypeScript to JavaScript (an ES module, types stripped), as esbuild's `loader: "ts"` did.
pub fn compile_ts(name: &str, source: &str) -> std::result::Result<String, String> {
    use oxc::allocator::Allocator;
    use oxc::codegen::Codegen;
    use oxc::parser::Parser;
    use oxc::semantic::SemanticBuilder;
    use oxc::span::SourceType;
    use oxc::transformer::{TransformOptions, Transformer};

    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, SourceType::ts()).parse();
    let first = |d: &[oxc::diagnostics::OxcDiagnostic]| d.first().map(|e| e.to_string());
    if let Some(e) = first(&parsed.diagnostics) {
        return Err(e);
    }
    let mut program = parsed.program;
    let scoping = SemanticBuilder::new().build(&program).semantic.into_scoping();
    let opts = TransformOptions::from_target("es2022").map_err(|e| e.to_string())?;
    let out = Transformer::new(&allocator, std::path::Path::new(name), &opts).build_with_scoping(scoping, &mut program);
    if let Some(e) = first(&out.diagnostics) {
        return Err(e);
    }
    Ok(Codegen::new().build(&program).code)
}
