//! Orvyn's trading MCP servers for BNB Chain: PancakeSwap, Uniswap, four.meme and Flap.
//!
//! Every tool reads the chain and returns unsigned transactions; this service holds no keys and
//! signs nothing. Agents (or any wallet) sign and send the calls themselves; Orvyn-hosted agents
//! sign through Orvyn's wallet policy, which checks each call again.
//!
//!   POST /pancakeswap/mcp   POST /uniswap/mcp   POST /fourmeme/mcp   POST /flap/mcp
//!   GET  /health
//!
//! Orvyn serves every tool from its one MCP endpoint, https://api.orvyn.cash/mcp (crates/api uses
//! `all_tools` and `call_tool`), and Orvyn's agents call them in process (`call_json`). This
//! binary serves them per venue for anyone who self-hosts:
//!
//! Streamable HTTP, stateless, JSON responses. Open source and self-hostable: `PORT`,
//! `BSC_RPC_URL`, optional `MCP_API_KEYS` (comma-separated bearer keys; unset = open) and
//! `MCP_RATE_LIMIT_PER_MIN` (per key or IP, default 120).

pub mod chain;
pub mod tools;
pub mod v3;
pub mod venues;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use subtle::ConstantTimeEq;

use tools::{fail, result, Args, Invalid};
use venues::{venue, VENUES};

const SUPPORTED: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05", "2024-10-07"];

fn reply(status: u16, body: Value) -> Response {
    (StatusCode::from_u16(status).unwrap(), Json(body)).into_response()
}

fn keys() -> &'static [String] {
    static K: LazyLock<Vec<String>> = LazyLock::new(|| std::env::var("MCP_API_KEYS").unwrap_or_default().split(',').map(|k| k.trim().to_string()).filter(|k| k.len() >= 16).collect());
    &K
}

fn authorized(headers: &HeaderMap) -> bool {
    if keys().is_empty() {
        return true;
    }
    let h = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("");
    let given = if h.len() > 7 && h[..7].eq_ignore_ascii_case("bearer ") { h[7..].trim_start() } else { h };
    keys().iter().any(|k| k.len() == given.len() && bool::from(k.as_bytes().ct_eq(given.as_bytes())))
}

/// A fixed one-minute window per caller (API key, else the client's IP).
fn limited(id: &str) -> bool {
    static WINDOWS: LazyLock<Mutex<HashMap<String, (Instant, u32)>>> = LazyLock::new(Default::default);
    let per_minute: u32 = std::env::var("MCP_RATE_LIMIT_PER_MIN").ok().and_then(|v| v.parse().ok()).unwrap_or(120);
    let now = Instant::now();
    let mut w = WINDOWS.lock().unwrap();
    match w.get_mut(id) {
        Some((start, count)) if now.duration_since(*start) <= Duration::from_secs(60) => {
            *count += 1;
            *count > per_minute
        }
        _ => {
            w.insert(id.to_string(), (now, 1));
            if w.len() > 50_000 {
                w.clear();
            }
            false
        }
    }
}

/// What the trading tools are for, for an MCP server's instructions.
pub const INSTRUCTIONS: &str = "Trading on BNB Chain (PancakeSwap v2/v3, Uniswap v3, four.meme and Flap bonding curves): the *_quote and *_token_info tools price and describe; the *_build_* tools return unsigned transactions (an exact approval if needed, then the trade, dry-run with eth_simulateV1) for the account to sign and send. Nothing is signed or sent. Launchpad tokens trade on their curve until they graduate, then on PancakeSwap.";

/// Every venue's tools, as tools/list gives them, with the venue each belongs to.
pub fn all_tools() -> Vec<(&'static str, Value)> {
    VENUES.iter().flat_map(|v| venue(v).map(|ven| ven.tools.iter().map(|t| (*v, t.json())).collect::<Vec<_>>()).unwrap_or_default()).collect()
}

/// The venue a tool name belongs to (names are unique across venues).
pub fn venue_of(tool: &str) -> Option<&'static str> {
    VENUES.iter().copied().find(|v| venue(v).is_some_and(|ven| ven.tools.iter().any(|t| t.name == tool)))
}

/// Run a trading tool: its MCP result (content, isError), or None when there's no such tool.
pub async fn call_tool(name: &str, arguments: &Value) -> Option<Value> {
    let v = venue_of(name)?;
    let ven = venue(v)?;
    let tool = ven.tools.iter().find(|t| t.name == name)?;
    let empty = json!({});
    let raw = if arguments.is_object() { arguments } else { &empty };
    let args = Args { tool, v: raw };
    if let Err(Invalid(why)) = args.check() {
        return Some(fail(&format!("MCP error -32602: Input validation error: Invalid arguments for tool {name}: {why}")));
    }
    Some(match tokio::time::timeout(Duration::from_secs(60), venues::call(v, name, &args)).await {
        Ok(r) => result(r),
        Err(_) => fail("the chain didn't answer in time; try again"),
    })
}

/// Run a trading tool in process and read its data (for Orvyn's agents): Err with the message
/// the tool gave when it failed.
pub async fn call_json(name: &str, arguments: &Value) -> Result<Value, String> {
    let r = call_tool(name, arguments).await.ok_or_else(|| format!("no trading tool {name}"))?;
    let text = r["content"][0]["text"].as_str().unwrap_or("").to_string();
    if r["isError"] == true {
        return Err(if text.is_empty() { format!("{name} failed") } else { text });
    }
    serde_json::from_str(&text).map_err(|e| format!("{name}: {e}"))
}

pub fn router() -> Router {
    Router::new()
        .route("/health", get(|| async { reply(200, json!({ "ok": true, "venues": VENUES, "chainId": chain::CHAIN_ID })) }))
        .route("/{venue}/mcp", post(handle).get(not_allowed).delete(not_allowed))
        .route("/{venue}/mcp/", post(handle).get(not_allowed).delete(not_allowed))
        .fallback(|| async { not_found() })
        .layer(DefaultBodyLimit::max(1_000_000))
}

fn not_found() -> Response {
    reply(404, json!({ "error": "not found", "endpoints": VENUES.iter().map(|v| format!("/{v}/mcp")).collect::<Vec<_>>() }))
}

async fn not_allowed(Path(v): Path<String>) -> Response {
    if venue(&v).is_none() {
        return not_found();
    }
    let mut r = reply(405, json!({ "jsonrpc": "2.0", "error": { "code": -32000, "message": "Method not allowed." }, "id": null }));
    r.headers_mut().insert("allow", "POST".parse().unwrap());
    r
}

async fn handle(Path(v): Path<String>, ConnectInfo(addr): ConnectInfo<SocketAddr>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(venue) = venue(&v) else { return not_found() };
    if !authorized(&headers) {
        return reply(401, json!({ "jsonrpc": "2.0", "error": { "code": -32001, "message": "API key required" }, "id": null }));
    }
    let header = |k: &str| headers.get(k).and_then(|h| h.to_str().ok());
    let id = header("authorization")
        .map(String::from)
        .or_else(|| header("x-forwarded-for").and_then(|f| f.split(',').next()).map(|s| s.trim().to_string()))
        .unwrap_or_else(|| addr.ip().to_string());
    if limited(&id) {
        return reply(429, json!({ "jsonrpc": "2.0", "error": { "code": -32000, "message": "Rate limit exceeded" }, "id": null }));
    }
    let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
        return reply(400, json!({ "jsonrpc": "2.0", "error": { "code": -32700, "message": "Parse error: Invalid JSON" }, "id": null }));
    };
    let (batch, messages) = match msg {
        Value::Array(a) => (true, a),
        m => (false, vec![m]),
    };
    let mut replies = vec![];
    for m in messages {
        if m["jsonrpc"] != "2.0" || !m["method"].is_string() {
            if m.get("id").is_some() && (m.get("result").is_some() || m.get("error").is_some()) {
                continue;
            }
            replies.push(json!({ "jsonrpc": "2.0", "error": { "code": -32600, "message": "Invalid Request" }, "id": m.get("id").cloned().unwrap_or(Value::Null) }));
            continue;
        }
        let Some(id) = m.get("id").cloned() else { continue };
        replies.push(match dispatch(&v, &venue, m["method"].as_str().unwrap(), &m["params"]).await {
            Ok(r) => json!({ "jsonrpc": "2.0", "result": r, "id": id }),
            Err((code, message)) => json!({ "jsonrpc": "2.0", "error": { "code": code, "message": message }, "id": id }),
        });
    }
    if replies.is_empty() {
        return StatusCode::ACCEPTED.into_response();
    }
    reply(200, if batch { Value::Array(replies) } else { replies.remove(0) })
}

async fn dispatch(id: &str, venue: &venues::Venue, method: &str, params: &Value) -> Result<Value, (i64, String)> {
    match method {
        "initialize" => {
            let asked = params["protocolVersion"].as_str().unwrap_or("");
            Ok(json!({
                "protocolVersion": if SUPPORTED.contains(&asked) { asked } else { SUPPORTED[0] },
                "capabilities": { "tools": { "listChanged": true } },
                "serverInfo": { "name": venue.name, "title": venue.title, "version": "1.0.0" },
                "instructions": venue.instructions,
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": venue.tools.iter().map(|t| t.json()).collect::<Vec<_>>() })),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or("");
            let Some(tool) = venue.tools.iter().find(|t| t.name == name) else { return Err((-32602, format!("Tool {name} not found"))) };
            let empty = json!({});
            let raw = if params["arguments"].is_object() { &params["arguments"] } else { &empty };
            let args = Args { tool, v: raw };
            if let Err(Invalid(why)) = args.check() {
                return Ok(fail(&format!("MCP error -32602: Input validation error: Invalid arguments for tool {name}: {why}")));
            }
            Ok(match tokio::time::timeout(Duration::from_secs(60), venues::call(id, name, &args)).await {
                Ok(r) => result(r),
                Err(_) => fail("the chain didn't answer in time; try again"),
            })
        }
        _ => Err((-32601, "Method not found".into())),
    }
}
