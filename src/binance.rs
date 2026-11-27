//! Binance MCP Server (agentic): a person's Binance "Agentic" sub-account (market data, balances,
//! spot, margin, convert and futures trading, transfers between that sub-account's wallets; never
//! withdrawals), through Binance's hosted MCP server.
//!
//! - Connecting: standard MCP OAuth. Orvyn is a public client identified by its client metadata
//!   document (`client_id` is that document's URL), with PKCE (S256). The person signs in on
//!   Binance's consent screen and picks the scopes. Binance issues no refresh tokens: when access
//!   expires, the person connects again. Tokens are stored encrypted, bound to their owner.
//! - Two kinds of owner: the person's Vyn (`agent_id` null), and one of their agents (the
//!   `binance` Bliss plugin).
//! - Tools come from the server (`tools/list`) and are kept with the connection. A tool is a read
//!   only when the server marks it read-only, or, without annotations, when its name starts with a
//!   read verb; everything else is a write.
//! - Writes never go through unchecked: Vyn's need the person's approval on their device (the
//!   server runs the exact call they approved, see vyn::mod); an agent's must pass its owner's caps
//!   (`check_agent_write`).
//!
//! Env: BINANCE_MCP_URL (default https://agent.binance.com/mcp/agentic), BINANCE_MCP_ISSUER (default
//! https://agent.binance.com), BINANCE_MCP_CLIENT_ID (default: Orvyn's metadata document URL).

use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine as _;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::bliss::mcp::McpClient;
use crate::db::{db, log_agent, Agent, AgentLogKind, BinanceConnection, BinanceOauthState};
use crate::error::{Error, Result};
use crate::secrets::{decrypt_secret, encrypt_secret, SecretKind};

pub const PLUGIN_ID: &str = "binance";
const STATE_TTL_SECS: i64 = 600;
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

fn var(k: &str) -> Option<String> {
    std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

pub fn mcp_url() -> String {
    var("BINANCE_MCP_URL").unwrap_or_else(|| "https://agent.binance.com/mcp/agentic".into())
}

fn issuer() -> String {
    var("BINANCE_MCP_ISSUER").unwrap_or_else(|| "https://agent.binance.com".into()).trim_end_matches('/').to_string()
}

/// Orvyn's API, where Binance sends the person back and finds the client metadata document.
pub fn api_base() -> String {
    var("NEXT_PUBLIC_API_URL").unwrap_or_else(|| format!("https://api.{}", crate::erc8004::root_domain())).trim_end_matches('/').to_string()
}

fn app_base() -> String {
    var("ORVYN_APP_URL").unwrap_or_else(|| format!("https://app.{}", crate::erc8004::root_domain())).trim_end_matches('/').to_string()
}

pub fn client_id() -> String {
    var("BINANCE_MCP_CLIENT_ID").unwrap_or_else(|| format!("{}/api/binance/client.json", api_base()))
}

pub fn redirect_uri() -> String {
    format!("{}/api/binance/callback", api_base())
}

/// The client metadata document served at `client_id()` (OAuth Client ID Metadata Document).
pub fn client_metadata() -> Value {
    json!({
        "client_id": client_id(),
        "client_name": "Orvyn",
        "client_uri": app_base(),
        "logo_uri": format!("{}/icon.png", app_base()),
        "redirect_uris": [redirect_uri()],
        "grant_types": ["authorization_code"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    })
}

fn binance_error(m: impl Into<String>) -> Error {
    Error::invalid(m)
}

fn aad(account_id: &str, agent_id: Option<&str>) -> String {
    format!("binance:{}:{}", account_id.to_lowercase(), agent_id.unwrap_or("vyn"))
}

// ---------- OAuth ----------

#[derive(Clone)]
struct AuthServer {
    authorization_endpoint: String,
    token_endpoint: String,
}

static AUTH_SERVER: LazyLock<Mutex<Option<(Instant, String, AuthServer)>>> = LazyLock::new(Default::default);

async fn auth_server() -> Result<AuthServer> {
    let iss = issuer();
    if let Some((at, i, s)) = AUTH_SERVER.lock().unwrap().clone() {
        if i == iss && at.elapsed() < Duration::from_secs(3600) {
            return Ok(s);
        }
    }
    let meta: Value = crate::http::client()
        .get(format!("{iss}/.well-known/oauth-authorization-server"))
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| Error::unavailable(format!("Binance sign-in isn't reachable ({e})")))?
        .json()
        .await
        .map_err(|_| Error::unavailable("Binance sign-in answered something unexpected"))?;
    let s = AuthServer {
        authorization_endpoint: meta["authorization_endpoint"].as_str().ok_or_else(|| Error::unavailable("Binance sign-in has no authorization endpoint"))?.into(),
        token_endpoint: meta["token_endpoint"].as_str().ok_or_else(|| Error::unavailable("Binance sign-in has no token endpoint"))?.into(),
    };
    *AUTH_SERVER.lock().unwrap() = Some((Instant::now(), iss, s.clone()));
    Ok(s)
}

fn challenge_of(verifier: &str) -> String {
    B64URL.encode(Sha256::digest(verifier.as_bytes()))
}

/// Start connecting Binance for the person's Vyn (`agent` None) or one of their agents (which must
/// have the Binance plugin installed). Returns the URL to open: Binance's consent screen.
pub async fn start_connect(account_id: &str, agent: Option<&Agent>, return_to: Option<&str>) -> Result<String> {
    let account = account_id.to_lowercase();
    if let Some(a) = agent {
        if a.owner_account_id.to_lowercase() != account {
            return Err(Error::forbidden("only the agent's owner can connect Binance"));
        }
        let installed: Option<String> = sqlx::query_scalar("select plugin_id from agent_plugins where agent_id = $1 and plugin_id = $2").bind(&a.id).bind(PLUGIN_ID).fetch_optional(db()).await?;
        if installed.is_none() {
            return Err(binance_error("install the Binance plugin on this agent first"));
        }
    }
    // Only back to Orvyn's own pages.
    let return_to = return_to.filter(|r| r.starts_with(&app_base()) || r.starts_with('/')).map(String::from);
    let server = auth_server().await?;
    let state = B64URL.encode(rand::random::<[u8; 24]>());
    let verifier = B64URL.encode(rand::random::<[u8; 32]>());
    let agent_id = agent.map(|a| a.id.clone());
    sqlx::query("delete from binance_oauth_states where expires_at < now()").execute(db()).await?;
    sqlx::query("insert into binance_oauth_states (state, account_id, agent_id, verifier_encrypted, return_to, expires_at) values ($1, $2, $3, $4, $5, now() + make_interval(secs => $6))")
        .bind(&state)
        .bind(&account)
        .bind(&agent_id)
        .bind(encrypt_secret(&verifier, &format!("binance-state:{state}"), SecretKind::AiKey)?)
        .bind(&return_to)
        .bind(STATE_TTL_SECS as f64)
        .execute(db())
        .await?;
    let mut url = url::Url::parse(&server.authorization_endpoint).map_err(|_| Error::unavailable("Binance's authorization endpoint isn't a URL"))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &client_id())
        .append_pair("redirect_uri", &redirect_uri())
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge_of(&verifier))
        .append_pair("code_challenge_method", "S256")
        .append_pair("resource", &mcp_url());
    Ok(url.to_string())
}

/// Binance sent the person back: exchange the code, list the tools, keep the connection. Returns
/// where to send them next (an Orvyn page).
pub async fn finish_connect(state: &str, code: &str) -> Result<String> {
    let row: Option<BinanceOauthState> = sqlx::query_as("delete from binance_oauth_states where state = $1 and expires_at > now() returning *").bind(state).fetch_optional(db()).await?;
    let row = row.ok_or_else(|| binance_error("this Binance sign-in expired; start again"))?;
    let verifier = decrypt_secret(&row.verifier_encrypted, &format!("binance-state:{state}"), SecretKind::AiKey)?;
    let server = auth_server().await?;
    let res = crate::http::client()
        .post(&server.token_endpoint)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &redirect_uri()),
            ("client_id", &client_id()),
            ("code_verifier", &verifier),
            ("resource", &mcp_url()),
        ])
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| Error::unavailable(format!("Binance sign-in isn't reachable ({e})")))?;
    let status = res.status();
    let token: Value = res.json().await.unwrap_or(Value::Null);
    let Some(access) = token["access_token"].as_str().filter(|_| status.is_success()) else {
        let why = token["error_description"].as_str().or(token["error"].as_str()).unwrap_or("no access token");
        return Err(binance_error(format!("Binance didn't grant access ({why})")));
    };
    let tools = list_tools(access).await?;
    let (account, agent_id) = (row.account_id.clone(), row.agent_id.clone());
    let key = aad(&account, agent_id.as_deref());
    let refresh = token["refresh_token"].as_str().map(|r| encrypt_secret(r, &key, SecretKind::AiKey)).transpose()?;
    let expires_at = token["expires_in"].as_f64().map(|s| chrono::Utc::now() + chrono::Duration::milliseconds((s * 1000.0) as i64));
    sqlx::query("delete from binance_connections where account_id = $1 and coalesce(agent_id, '') = coalesce($2, '')").bind(&account).bind(&agent_id).execute(db()).await?;
    sqlx::query("insert into binance_connections (account_id, agent_id, access_token_encrypted, refresh_token_encrypted, scope, expires_at, tools) values ($1, $2, $3, $4, $5, $6, $7)")
        .bind(&account)
        .bind(&agent_id)
        .bind(encrypt_secret(access, &key, SecretKind::AiKey)?)
        .bind(refresh)
        .bind(token["scope"].as_str())
        .bind(expires_at)
        .bind(Value::Array(tools.clone()))
        .execute(db())
        .await?;
    if let Some(id) = &agent_id {
        crate::bliss::merge_state(id, PLUGIN_ID, json!({ "connected": true, "tools": tools })).await?;
        log_agent(id, AgentLogKind::Plugin, &format!("Owner connected a Binance Agentic sub-account ({} tools)", tools.len())).await?;
    }
    Ok(row.return_to.map(|r| if r.starts_with('/') { format!("{}{r}", app_base()) } else { r }).unwrap_or_else(|| match &agent_id {
        Some(id) => format!("{}/agents/{id}?binance=connected", app_base()),
        None => format!("{}/vyn?binance=connected", app_base()),
    }))
}

pub async fn connection(account_id: &str, agent_id: Option<&str>) -> Result<Option<BinanceConnection>> {
    Ok(sqlx::query_as("select * from binance_connections where account_id = $1 and coalesce(agent_id, '') = coalesce($2, '')").bind(account_id.to_lowercase()).bind(agent_id).fetch_optional(db()).await?)
}

pub fn connection_view(c: Option<&BinanceConnection>) -> Value {
    match c {
        None => json!({ "connected": false }),
        Some(c) => json!({
            "connected": true,
            "scope": c.scope,
            "expiresAt": c.expires_at.map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
            "expired": c.expires_at.is_some_and(|t| t <= chrono::Utc::now()),
            "tools": c.tools.as_array().map(|t| t.iter().map(|x| json!({ "name": x["name"], "read": is_read_tool(x) })).collect::<Vec<_>>()).unwrap_or_default(),
            "connectedAt": c.created_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        }),
    }
}

/// Forget the connection (the person can also disconnect Orvyn in Binance's sub-account settings).
pub async fn disconnect(account_id: &str, agent_id: Option<&str>) -> Result<bool> {
    let n = sqlx::query("delete from binance_connections where account_id = $1 and coalesce(agent_id, '') = coalesce($2, '')").bind(account_id.to_lowercase()).bind(agent_id).execute(db()).await?.rows_affected();
    if let Some(id) = agent_id {
        crate::bliss::merge_state(id, PLUGIN_ID, json!({ "connected": false, "tools": [] })).await?;
        if n > 0 {
            log_agent(id, AgentLogKind::Plugin, "Owner disconnected Binance").await?;
        }
    }
    Ok(n > 0)
}

// ---------- The MCP server ----------

fn client(access: &str) -> McpClient {
    McpClient::new(&mcp_url(), vec![("authorization".into(), format!("Bearer {access}"))], CALL_TIMEOUT)
}

fn mcp_error(e: Error) -> Error {
    if e.message.contains("answered 401") || e.message.contains("answered 403") {
        return binance_error("Binance access expired or was revoked; reconnect Binance");
    }
    Error::unavailable(format!("Binance: {}", e.message))
}

async fn list_tools(access: &str) -> Result<Vec<Value>> {
    let mut c = client(access);
    c.connect().await.map_err(mcp_error)?;
    let tools = c.list_tools().await.map_err(mcp_error);
    c.close().await;
    Ok(tools?
        .into_iter()
        .filter(|t| t["name"].as_str().is_some_and(|n| !n.is_empty() && n.len() <= 64 && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')))
        .map(|t| json!({ "name": t["name"], "description": crate::http::clip(t["description"].as_str().unwrap_or(""), 1000), "inputSchema": if t["inputSchema"]["type"] == "object" { t["inputSchema"].clone() } else { json!({ "type": "object", "properties": {} }) }, "annotations": t["annotations"] }))
        .collect())
}

const READ_VERBS: &[&str] = &["get", "list", "query", "fetch", "search", "check", "show", "view", "read", "describe", "lookup"];

/// A read: marked read-only by the server, or (without annotations) named with a read verb.
pub fn is_read_tool(tool: &Value) -> bool {
    let a = &tool["annotations"];
    if let Some(ro) = a["readOnlyHint"].as_bool() {
        return ro && a["destructiveHint"] != true;
    }
    let name = tool["name"].as_str().unwrap_or("").to_lowercase();
    let first = name.split(['_', '-']).next().unwrap_or("");
    READ_VERBS.contains(&first)
}

/// Call one of the connection's tools with its stored token.
pub async fn call_tool(conn: &BinanceConnection, tool: &str, args: Value) -> Result<Value> {
    if !conn.tools.as_array().is_some_and(|t| t.iter().any(|x| x["name"] == tool)) {
        return Err(binance_error(format!("Binance has no tool {tool} on this connection")));
    }
    if conn.expires_at.is_some_and(|t| t <= chrono::Utc::now()) {
        return Err(binance_error("Binance access expired; reconnect Binance"));
    }
    let access = decrypt_secret(&conn.access_token_encrypted, &aad(&conn.account_id, conn.agent_id.as_deref()), SecretKind::AiKey)?;
    let mut c = client(&access);
    c.connect().await.map_err(mcp_error)?;
    let out = c.call_tool(tool, if args.is_object() { args } else { json!({}) }).await.map_err(mcp_error);
    c.close().await;
    out
}

/// A tool result as text for the model (Binance's data; capped).
pub fn result_text(result: &Value) -> String {
    let parts = result["content"].as_array().cloned().unwrap_or_default();
    let mut text: String = parts.iter().filter(|p| p["type"] == "text").filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n");
    if text.is_empty() && !result["structuredContent"].is_null() {
        text = result["structuredContent"].to_string();
    }
    let text = crate::http::clip(&text, 16_000);
    if result["isError"] == true { format!("Binance returned an error: {text}") } else { text }
}

/// The name Orvyn gives a Binance tool (namespaced next to the agent's other tools).
pub fn tool_name(binance_tool: &str) -> String {
    format!("binance_{binance_tool}")
}

pub fn binance_tool_of(name: &str) -> Option<&str> {
    name.strip_prefix("binance_")
}

/// A one-line description of a write call, for the person's approval card.
pub fn describe_call(tool: &str, args: &Value) -> String {
    let fields: Vec<String> = args.as_object().map(|o| o.iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| format!("{k}: {}", v.as_str().map(String::from).unwrap_or_else(|| v.to_string()))).collect()).unwrap_or_default();
    format!("{}{}", tool.replace('_', " "), if fields.is_empty() { String::new() } else { format!(" ({})", fields.join(", ")) })
}

// ---------- Agents: the owner's caps on writes ----------

#[derive(Debug, Clone, PartialEq)]
pub enum WriteKind {
    /// Cancelling orders only lowers risk: always allowed.
    Cancel,
    Transfer,
    Leverage,
    Order { product: &'static str },
    Other,
}

pub fn write_kind(tool: &str) -> WriteKind {
    let n = tool.to_lowercase();
    if n.contains("cancel") {
        return WriteKind::Cancel;
    }
    if n.contains("transfer") {
        return WriteKind::Transfer;
    }
    if n.contains("leverage") {
        return WriteKind::Leverage;
    }
    let product = if n.contains("convert") {
        "convert"
    } else if n.contains("margin") {
        "margin"
    } else if n.contains("futures") || n.contains("perp") || n.contains("_um") || n.contains("_cm") || n.starts_with("um_") || n.starts_with("cm_") {
        "futures"
    } else {
        "spot"
    };
    if ["order", "buy", "sell", "convert", "trade", "place", "close", "open"].iter().any(|w| n.contains(w)) {
        return WriteKind::Order { product };
    }
    WriteKind::Other
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

const STABLES: &[&str] = &["USDT", "USDC", "FDUSD", "USD", "TUSD", "DAI", "USD1"];

fn quote_of(symbol: &str) -> Option<&'static str> {
    STABLES.iter().copied().find(|q| symbol.ends_with(q) && symbol.len() > q.len())
}

/// Binance's public price for a symbol (spot, then USDⓈ-M futures).
pub(crate) async fn market_price(symbol: &str) -> Option<f64> {
    let spot = var("BINANCE_SPOT_API").unwrap_or_else(|| "https://api.binance.com".into());
    let futures = var("BINANCE_FUTURES_API").unwrap_or_else(|| "https://fapi.binance.com".into());
    for url in [format!("{spot}/api/v3/ticker/price?symbol={symbol}"), format!("{futures}/fapi/v1/ticker/price?symbol={symbol}")] {
        if let Ok(r) = crate::http::client().get(&url).timeout(Duration::from_secs(10)).send().await {
            if let Ok(v) = r.json::<Value>().await {
                if let Some(p) = num(&v["price"]).filter(|p| *p > 0.0) {
                    return Some(p);
                }
            }
        }
    }
    None
}

/// The order's size in USD, from its arguments (and the market price when it's given in the base
/// asset). None when Orvyn can't tell, and then the order is refused.
pub async fn order_notional(args: &Value) -> Option<f64> {
    let symbol = args["symbol"].as_str().map(str::to_uppercase);
    let stable_quote = symbol.as_deref().and_then(quote_of).is_some();
    if let Some(q) = num(&args["quoteOrderQty"]).or_else(|| num(&args["quote_order_qty"])).or_else(|| num(&args["quoteQty"])) {
        if stable_quote {
            return Some(q);
        }
    }
    // Convert: the side being sold.
    if let (Some(from), Some(amount)) = (args["fromAsset"].as_str(), num(&args["fromAmount"])) {
        if STABLES.contains(&from.to_uppercase().as_str()) {
            return Some(amount);
        }
        return market_price(&format!("{}USDT", from.to_uppercase())).await.map(|p| p * amount);
    }
    let qty = num(&args["quantity"]).or_else(|| num(&args["qty"])).or_else(|| num(&args["amount"]))?;
    let symbol = symbol?;
    if !stable_quote {
        return None;
    }
    let price = match num(&args["price"]).or_else(|| num(&args["stopPrice"])) {
        Some(p) => p,
        None => market_price(&symbol).await?,
    };
    Some(qty * price)
}

/// The owner's caps (the Binance plugin's settings).
#[derive(Debug, Clone)]
pub struct Caps {
    pub max_order_usd: f64,
    pub max_daily_usd: f64,
    pub products: Vec<String>,
    pub max_leverage: f64,
    pub symbols: Vec<String>,
    pub allow_transfers: bool,
}

impl Caps {
    pub fn from_settings(c: &crate::bliss::Settings) -> Caps {
        let list = |k: &str| -> Vec<String> { c.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.trim().to_uppercase())).filter(|s| !s.is_empty()).collect()).unwrap_or_default() };
        Caps {
            max_order_usd: c.get("maxOrderUsd").and_then(Value::as_f64).unwrap_or(50.0),
            max_daily_usd: c.get("maxDailyUsd").and_then(Value::as_f64).unwrap_or(200.0),
            products: list("products").into_iter().map(|s: String| s.to_lowercase()).collect::<Vec<_>>(),
            max_leverage: c.get("maxLeverage").and_then(Value::as_f64).unwrap_or(1.0),
            symbols: list("symbols"),
            allow_transfers: c.get("allowTransfers").and_then(Value::as_bool).unwrap_or(false),
        }
    }
}

/// Whether an agent may make this write call under its owner's caps. Ok(Some(usd)) for an order
/// of that size (counted toward the day), Ok(None) for a write with no size, Err(why) refused.
pub async fn check_agent_write(agent_id: &str, caps: &Caps, tool: &str, args: &Value) -> std::result::Result<Option<f64>, String> {
    let leverage = num(&args["leverage"]);
    if leverage.is_some_and(|l| l > caps.max_leverage) {
        return Err(format!("leverage {} is over the owner's limit of {}x", leverage.unwrap_or(0.0), caps.max_leverage));
    }
    match write_kind(tool) {
        WriteKind::Cancel => Ok(None),
        WriteKind::Leverage if leverage.is_some() => Ok(None),
        WriteKind::Leverage => Err("give the leverage to set".into()),
        WriteKind::Transfer if caps.allow_transfers => Ok(None),
        WriteKind::Transfer => Err("the owner hasn't allowed transfers between wallets".into()),
        WriteKind::Other => Err(format!("Orvyn doesn't let agents use {tool}; the owner can do it on Binance")),
        WriteKind::Order { product } => {
            if !caps.products.iter().any(|p| p == product) {
                return Err(format!("{product} trading isn't allowed by the owner (allowed: {})", if caps.products.is_empty() { "none".into() } else { caps.products.join(", ") }));
            }
            let symbol = args["symbol"].as_str().map(str::to_uppercase).or_else(|| match (args["fromAsset"].as_str(), args["toAsset"].as_str()) {
                (Some(f), Some(t)) => Some(format!("{}{}", f.to_uppercase(), t.to_uppercase())),
                _ => None,
            });
            if !caps.symbols.is_empty() && !symbol.as_ref().is_some_and(|s| caps.symbols.contains(s)) {
                return Err(format!("{} isn't on the owner's list of symbols ({})", symbol.unwrap_or_else(|| "this market".into()), caps.symbols.join(", ")));
            }
            let Some(usd) = order_notional(args).await else {
                return Err("Orvyn couldn't size this order in USD; give quoteOrderQty, or quantity with a USD-quoted symbol".into());
            };
            if usd > caps.max_order_usd {
                return Err(format!("this order is about ${usd:.2}, over the owner's limit of ${} per order", caps.max_order_usd));
            }
            let today: f64 = sqlx::query_scalar("select coalesce(sum(notional_usd), 0)::float8 from binance_agent_orders where agent_id = $1 and created_at > now() - interval '1 day'")
                .bind(agent_id)
                .fetch_one(db())
                .await
                .map_err(|e| e.to_string())?;
            if today + usd > caps.max_daily_usd {
                return Err(format!("this order (${usd:.2}) would pass the owner's daily limit of ${} (${today:.2} used in the last 24h)", caps.max_daily_usd));
            }
            Ok(Some(usd))
        }
    }
}

/// Record an agent's order toward its daily limit.
pub async fn record_agent_order(agent_id: &str, tool: &str, args: &Value, usd: f64) -> Result<()> {
    sqlx::query("insert into binance_agent_orders (agent_id, tool, symbol, notional_usd) values ($1, $2, $3, $4)").bind(agent_id).bind(tool).bind(args["symbol"].as_str()).bind(usd).execute(db()).await?;
    Ok(())
}
