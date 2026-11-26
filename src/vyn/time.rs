//! Local-time helpers for reminders and check-ins: "09:00 every weekday in Europe/Lisbon" becomes
//! the next UTC instant, correct across daylight-saving changes. Same algorithm as the TypeScript
//! core (a guess corrected twice by the zone's offset), so gaps and overlaps resolve the same way.

use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Timelike, Utc};
use chrono_tz::Tz;

pub fn valid_time_zone(tz: &str) -> bool {
    tz.parse::<Tz>().is_ok()
}

fn zone(tz: &str) -> Tz {
    tz.parse().unwrap_or(chrono_tz::UTC)
}

/// HH:MM, 24-hour.
pub fn is_hhmm(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 5 && b[2] == b':' && b[..2].iter().chain(&b[3..]).all(u8::is_ascii_digit) && s[..2].parse::<u32>().is_ok_and(|h| h < 24) && s[3..].parse::<u32>().is_ok_and(|m| m < 60)
}

/// Milliseconds `tz` is ahead of UTC at `ms` (whole seconds, as Intl reports them).
fn offset(ms: i64, tz: Tz) -> i64 {
    let t = Utc.timestamp_millis_opt(ms).single().unwrap_or_default().with_timezone(&tz);
    let local = Utc.with_ymd_and_hms(t.year(), t.month(), t.day(), t.hour(), t.minute(), t.second()).single().unwrap_or_default().timestamp_millis();
    local - ms.div_euclid(1000) * 1000
}

/// The UTC instant of a wall-clock time in `tz`.
pub fn zoned(y: i32, m: u32, d: u32, h: u32, mi: u32, tz: &str) -> DateTime<Utc> {
    let z = zone(tz);
    let guess = Utc.with_ymd_and_hms(y, m, d, h, mi, 0).single().unwrap_or_default().timestamp_millis();
    let mut t = guess - offset(guess, z);
    // Re-check at the result: the offset may differ there (a DST change in between).
    t = guess - offset(t, z);
    Utc.timestamp_millis_opt(t).single().unwrap_or_default()
}

/// The next time after `after` that the clock in `tz` reads `time` on one of `days` (0 = Sunday).
pub fn next_daily(time: &str, tz: &str, after: DateTime<Utc>, days: &[u32]) -> Option<DateTime<Utc>> {
    if !is_hhmm(time) {
        return None;
    }
    let (h, mi): (u32, u32) = (time[..2].parse().ok()?, time[3..].parse().ok()?);
    let today = after.with_timezone(&zone(tz)).date_naive();
    for i in 0..8 {
        let day: NaiveDate = today + chrono::Duration::days(i);
        let weekday = day.weekday().num_days_from_sunday();
        if !days.is_empty() && !days.contains(&weekday) {
            continue;
        }
        let at = zoned(day.year(), day.month(), day.day(), h, mi, tz);
        if at > after {
            return Some(at);
        }
    }
    None
}

/// "Tue 24 Nov, 09:00" in the person's zone, for messages.
pub fn describe(date: DateTime<Utc>, tz: &str) -> String {
    date.with_timezone(&zone(tz)).format("%a %-d %b, %H:%M").to_string()
}

/// "Tuesday, 24 November 2026 at 14:05 (Europe/Lisbon)" for the model's sense of now.
pub fn now_line(tz: &str, now: DateTime<Utc>) -> String {
    format!("{} ({tz})", now.with_timezone(&zone(tz)).format("%A, %-d %B %Y at %H:%M"))
}

/// "2026-11-28T09:00" in the person's zone → an instant. Full ISO with an offset is taken as-is.
pub fn local_to_utc(at: &str, tz: &str) -> Result<DateTime<Utc>, String> {
    if let Ok(t) = DateTime::parse_from_rfc3339(at) {
        return Ok(t.with_timezone(&Utc));
    }
    let re = regex::Regex::new(r"^(\d{4})-(\d\d)-(\d\d)[T ](\d\d):(\d\d)").unwrap();
    let c = re.captures(at).ok_or("at must look like 2026-11-28T09:00")?;
    let n = |i: usize| c[i].parse::<u32>().unwrap();
    Ok(zoned(n(1) as i32, n(2), n(3), n(4), n(5), tz))
}
