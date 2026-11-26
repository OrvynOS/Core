//! Push notifications to Orvyn Mobile through Expo's push service. Each signed-in device registers
//! its Expo push token; we notify an account's devices about what needs them: a new job for their
//! agent, a client's message, a delivered result, new email, an agent error.
//!
//! Sending never fails the caller: a notification is best-effort.

use std::collections::{BTreeSet, HashMap};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::json;

use crate::db::{db, PushDevice};
use crate::error::{Error, Result};
use crate::http::{client, clip};

const EXPO_PUSH_URL: &str = "https://exp.host/--/api/v2/push/send";

/// `ExpoPushToken[…]` / `ExponentPushToken[…]`.
pub fn is_push_token(token: &str) -> bool {
    let inner = token
        .strip_prefix("ExpoPushToken[")
        .or_else(|| token.strip_prefix("ExponentPushToken["))
        .and_then(|t| t.strip_suffix(']'));
    inner.is_some_and(|t| t.len() >= 10 && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'))
}

#[derive(Clone, Debug)]
pub struct Notice {
    pub title: String,
    pub body: String,
    /// In-app path to open on tap, e.g. "/agent/<id>/inbox".
    pub path: Option<String>,
}

pub async fn register_device(account_id: &str, token: &str, platform: &str) -> Result<()> {
    if !is_push_token(token) {
        return Err(Error::invalid("invalid push token"));
    }
    if platform != "ios" && platform != "android" {
        return Err(Error::invalid("platform must be ios or android"));
    }
    // A device moves to whoever signed in on it last.
    sqlx::query(
        "insert into push_devices (token, account_id, platform, last_seen_at) values ($1, $2, $3, now())
         on conflict (token) do update set account_id = excluded.account_id, platform = excluded.platform, last_seen_at = now()",
    )
    .bind(token)
    .bind(account_id)
    .bind(platform)
    .execute(db())
    .await?;
    Ok(())
}

pub async fn remove_device(token: &str, account_id: &str) -> Result<()> {
    sqlx::query("delete from push_devices where token = $1 and account_id = $2").bind(token).bind(account_id).execute(db()).await?;
    Ok(())
}

#[derive(Deserialize)]
struct Ticket {
    status: String,
    details: Option<TicketDetails>,
}
#[derive(Deserialize)]
struct TicketDetails {
    error: Option<String>,
}
#[derive(Deserialize)]
struct Tickets {
    data: Option<Vec<Ticket>>,
}

async fn send(account_ids: &[String], notice: &Notice) -> Result<()> {
    let ids: Vec<String> = account_ids.iter().filter(|a| !a.is_empty()).cloned().collect::<BTreeSet<_>>().into_iter().collect();
    if ids.is_empty() {
        return Ok(());
    }
    let devices: Vec<PushDevice> = sqlx::query_as("select * from push_devices where account_id = any($1)").bind(&ids).fetch_all(db()).await?;
    let token = std::env::var("EXPO_ACCESS_TOKEN").ok().filter(|t| !t.is_empty());
    for batch in devices.chunks(100) {
        let body: Vec<_> = batch
            .iter()
            .map(|d| {
                json!({
                    "to": d.token,
                    "title": clip(&notice.title, 100),
                    "body": clip(&notice.body, 300),
                    "data": notice.path.as_ref().map(|p| json!({ "path": p })).unwrap_or(json!({})),
                    "sound": "default",
                })
            })
            .collect();
        // EXPO_PUSH_URL: a stand-in push service (tests).
        let url = std::env::var("EXPO_PUSH_URL").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| EXPO_PUSH_URL.into());
        let mut req = client().post(&url).header("accept", "application/json").json(&body).timeout(Duration::from_secs(10));
        if let Some(t) = &token {
            req = req.bearer_auth(t);
        }
        let Ok(res) = req.send().await else { continue };
        let tickets: Tickets = res.json().await.unwrap_or(Tickets { data: None });
        // Uninstalled apps: forget their tokens.
        let gone: Vec<String> = tickets
            .data
            .unwrap_or_default()
            .iter()
            .zip(batch)
            .filter(|(t, _)| t.status == "error" && t.details.as_ref().and_then(|d| d.error.as_deref()) == Some("DeviceNotRegistered"))
            .map(|(_, d)| d.token.clone())
            .collect();
        if !gone.is_empty() {
            sqlx::query("delete from push_devices where token = any($1)").bind(&gone).execute(db()).await?;
        }
    }
    Ok(())
}

/// Notify these accounts' devices. Fire and forget.
pub fn notify(account_ids: Vec<String>, notice: Notice) {
    tokio::spawn(async move {
        if let Err(e) = send(&account_ids, &notice).await {
            eprintln!("[push] {e}");
        }
    });
}

/// Notify an agent's creator. Fire and forget.
pub fn notify_owner(agent_id: &str, notice: Notice) {
    let agent_id = agent_id.to_string();
    tokio::spawn(async move {
        let owner: Option<String> = sqlx::query_scalar("select owner_account_id from agents where id = $1")
            .bind(&agent_id)
            .fetch_optional(db())
            .await
            .ok()
            .flatten();
        if let Some(owner) = owner {
            if let Err(e) = send(&[owner], &notice).await {
                eprintln!("[push] {e}");
            }
        }
    });
}

// Errors can repeat every minute (a revoked AI key): at most one per agent per 30 minutes.
static LAST_ERROR: LazyLock<Mutex<HashMap<String, Instant>>> = LazyLock::new(Default::default);

pub async fn notify_agent_error(agent_id: &str, message: &str) -> Result<()> {
    {
        let mut last = LAST_ERROR.lock().unwrap();
        if last.get(agent_id).is_some_and(|t| t.elapsed() < Duration::from_secs(30 * 60)) {
            return Ok(());
        }
        last.insert(agent_id.to_string(), Instant::now());
    }
    let row: Option<(String, String)> = sqlx::query_as("select owner_account_id, name from agents where id = $1").bind(agent_id).fetch_optional(db()).await?;
    if let Some((owner, name)) = row {
        let notice = Notice { title: format!("{name} needs attention"), body: message.to_string(), path: Some(format!("/agent/{agent_id}/activity")) };
        send(&[owner], &notice).await?;
    }
    Ok(())
}
