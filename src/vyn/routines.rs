//! Reminders and recurring tasks. At the due time the orchestrator runs the instruction as a turn,
//! puts the result in the person's "From Vyn" conversation and notifies every device and chat app.
//! The daily check-in from settings is a routine too (kind "checkin").

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use super::time::{is_hhmm, next_daily, valid_time_zone};
use crate::db::{db, VynRoutine};
use crate::error::{Error, Result};

const MAX_ROUTINES: i64 = 50;

/// `{ kind: "once", at }` or `{ kind: "daily", time, days? }`.
pub fn next_run(schedule: &Value, tz: &str, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
    if schedule["kind"] == "once" {
        let at = DateTime::parse_from_rfc3339(schedule["at"].as_str()?).ok()?.with_timezone(&Utc);
        return (at > after).then_some(at);
    }
    let days: Vec<u32> = schedule["days"].as_array().map(|a| a.iter().filter_map(|d| d.as_u64().map(|d| d as u32)).collect()).unwrap_or_default();
    next_daily(schedule["time"].as_str()?, tz, after, &days)
}

pub async fn timezone_of(account_id: &str) -> Result<String> {
    let tz: Option<String> = sqlx::query_scalar("select timezone from vyn_settings where account_id = $1").bind(account_id.to_lowercase()).fetch_optional(db()).await?;
    Ok(tz.filter(|t| valid_time_zone(t)).unwrap_or_else(|| "UTC".into()))
}

pub fn check_schedule(schedule: &Value) -> Result<()> {
    if schedule["kind"] == "once" {
        if schedule["at"].as_str().and_then(|a| DateTime::parse_from_rfc3339(a).ok()).is_none() {
            return Err(Error::invalid("`at` must be a date and time"));
        }
    } else {
        if !schedule["time"].as_str().is_some_and(is_hhmm) {
            return Err(Error::invalid("time must be HH:MM (24-hour)"));
        }
        if schedule["days"].as_array().is_some_and(|d| d.iter().any(|x| !x.as_u64().is_some_and(|n| n <= 6))) {
            return Err(Error::invalid("days are 0 (Sunday) to 6 (Saturday)"));
        }
    }
    Ok(())
}

pub async fn create_routine(account_id: &str, title: &str, instruction: &str, schedule: &Value, kind: &str) -> Result<VynRoutine> {
    let account = account_id.to_lowercase();
    check_schedule(schedule)?;
    let mine: i64 = sqlx::query_scalar("select count(*) from vyn_routines where account_id = $1 and active").bind(&account).fetch_one(db()).await?;
    if mine >= MAX_ROUTINES {
        return Err(Error::invalid(format!("there are already {MAX_ROUTINES} reminders; cancel some first")));
    }
    let tz = timezone_of(&account).await?;
    let next = next_run(schedule, &tz, Utc::now()).ok_or_else(|| Error::invalid("that time has already passed"))?;
    Ok(sqlx::query_as("insert into vyn_routines (id, account_id, title, instruction, schedule, kind, next_run_at) values ($1, $2, $3, $4, $5, $6, $7) returning *")
        .bind(crate::lifecycle::uuid_v4())
        .bind(&account)
        .bind(title.chars().take(120).collect::<String>())
        .bind(instruction.chars().take(2000).collect::<String>())
        .bind(schedule)
        .bind(kind)
        .bind(next)
        .fetch_one(db())
        .await?)
}

pub async fn list_routines(account_id: &str) -> Result<Vec<VynRoutine>> {
    Ok(sqlx::query_as("select * from vyn_routines where account_id = $1 and active order by next_run_at asc nulls last").bind(account_id.to_lowercase()).fetch_all(db()).await?)
}

pub async fn cancel_routine(account_id: &str, id: &str) -> Result<bool> {
    let n = sqlx::query("update vyn_routines set active = false, next_run_at = null where id = $1 and account_id = $2").bind(id).bind(account_id.to_lowercase()).execute(db()).await?.rows_affected();
    Ok(n > 0)
}

/// Keep the daily check-in routine in line with settings (one per person, or none).
pub async fn sync_check_in(account_id: &str, time: Option<&str>) -> Result<()> {
    let account = account_id.to_lowercase();
    sqlx::query("update vyn_routines set active = false, next_run_at = null where account_id = $1 and kind = 'checkin'").bind(&account).execute(db()).await?;
    let Some(time) = time else { return Ok(()) };
    create_routine(
        &account,
        "Daily check-in",
        "Daily check-in. Greet them briefly by name. Mention today's reminders (list_schedule) and anything about their Orvyn agents that needs them (my_agents): money that arrived, an agent that stopped, a payment waiting. If there's nothing worth saying, keep it to one warm line.",
        &json!({ "kind": "daily", "time": time }),
        "checkin",
    )
    .await?;
    Ok(())
}

/// Claim routines that are due, moving each to its next run first (so two orchestrators never run
/// the same one). One-off reminders deactivate when claimed.
pub async fn claim_due(limit: i64) -> Result<Vec<VynRoutine>> {
    let now = Utc::now();
    let due: Vec<VynRoutine> = sqlx::query_as("select * from vyn_routines where active and next_run_at <= $1 order by next_run_at asc limit $2").bind(now).bind(limit).fetch_all(db()).await?;
    let mut claimed = vec![];
    for r in due {
        let tz = timezone_of(&r.account_id).await?;
        let next = if r.schedule["kind"] == "once" { None } else { next_run(&r.schedule, &tz, now) };
        let won = sqlx::query("update vyn_routines set next_run_at = $3, last_run_at = $4, active = $5 where id = $1 and next_run_at = $2")
            .bind(&r.id)
            .bind(r.next_run_at)
            .bind(next)
            .bind(now)
            .bind(next.is_some())
            .execute(db())
            .await?
            .rows_affected();
        if won > 0 {
            claimed.push(r);
        }
    }
    Ok(claimed)
}
