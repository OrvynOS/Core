//! Orvyn Mail: every agent with a handle has the address <handle>@orvyn.cash (ORVYN_MAIL_DOMAIN).
//!
//! - Inbound: Orvyn Mail (orvyn-mail, hosted by the API: providers post to /api/mail/webhook) checks
//!   each recipient with `resolve_recipient`, verifies DKIM / DMARC, parses the message and hands it
//!   to `receive_message`.
//! - Outbound: `queue_message` validates and queues; Orvyn Mail claims due messages
//!   (`claim_outbound`), DKIM-signs them, delivers them through the relay and reports per recipient
//!   (`report_delivery`). Temporary failures retry with backoff for up to 3 days.
//! - Bounces: outbound mail uses the envelope sender bounces+<id>-<tag>@domain (VERP).
//!
//! Email is untrusted input. The agent reads it only in its mail session, which has read-only
//! tools plus replying, never trading or spending tools.

use std::collections::{BTreeMap, HashSet};
use std::sync::LazyLock;

use hmac::{Hmac, Mac};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::Sha256;

use crate::db::{db, log_agent, Agent, AgentLogKind, MailMessage};
use crate::env::env;
use crate::error::{Error, Result};

pub const RECIPIENTS: usize = 10;
pub const SUBJECT_CHARS: usize = 300;
pub const TEXT_CHARS: usize = 100_000;
pub const KEEP_PER_AGENT: i64 = 5000;
/// Agent replies to one correspondent per 24h (stops reply loops with other bots).
pub const REPLIES_PER_CORRESPONDENT: i64 = 5;
pub const MAX_QUEUE_HOURS: i64 = 72;
pub const BLOCKED_MAX: usize = 200;
pub const DISPLAY_NAME_CHARS: usize = 64;

/// Retry delays (seconds) for temporary delivery failures, then the last one repeats.
const BACKOFF: &[i64] = &[60, 300, 900, 1800, 3600, 7200, 14400, 28800, 43200];

/// Local parts Orvyn Mail handles itself (forwarded to Orvyn's operators).
pub const SYSTEM_LOCAL_PARTS: &[&str] = &["postmaster", "abuse", "dmarc", "hostmaster", "webmaster", "security"];
const BOUNCE_LOCAL: &str = "bounces";

fn mail_error(m: impl Into<String>) -> Error {
    Error::invalid(m)
}

pub fn mail_domain() -> String {
    env().orvyn_mail_domain.to_lowercase()
}

/// The agent's email address, or None if it has no handle yet.
pub fn agent_address(handle: Option<&str>) -> Option<String> {
    handle.map(|h| format!("{h}@{}", mail_domain()))
}

static EMAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[A-Za-z0-9.!#$%&'*+/=?^_`{|}~-]{1,64}@[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?(?:\.[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?)+$").unwrap()
});

pub fn normalize_email(address: &str) -> Option<String> {
    let t = address.trim();
    let t = t.strip_prefix('<').unwrap_or(t);
    let t = t.strip_suffix('>').unwrap_or(t);
    let a = t.to_lowercase();
    (a.chars().count() <= 254 && EMAIL.is_match(&a)).then_some(a)
}

#[derive(Debug, Clone, PartialEq)]
pub struct AddressParts {
    pub local: String,
    pub tag: Option<String>,
    pub domain: String,
}

/// Split an address into local part (without +tag), tag and domain.
pub fn split_address(address: &str) -> Option<AddressParts> {
    let a = normalize_email(address)?;
    let at = a.rfind('@')?;
    let mut it = a[..at].splitn(2, '+');
    let local = it.next().unwrap_or("").to_string();
    let tag = it.next().map(String::from);
    Some(AddressParts { local, tag, domain: a[at + 1..].to_string() })
}

pub fn new_message_id() -> String {
    format!("<{}@{}>", crate::lifecycle::uuid_v4(), mail_domain())
}

/// Tag binding a bounce address to a message, so bounces can't be forged by guessing ids.
fn bounce_tag(message_id: i64) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(env().orvyn_mail_api_key.clone().unwrap_or_default().as_bytes()).unwrap();
    mac.update(format!("bounce:{message_id}").as_bytes());
    hex::encode(mac.finalize().into_bytes())[..12].to_string()
}

/// Envelope sender for an outbound message (VERP): bounces come back to it.
pub fn bounce_address(message_id: i64) -> String {
    format!("{BOUNCE_LOCAL}+{message_id}-{}@{}", bounce_tag(message_id), mail_domain())
}

static BOUNCE_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d{1,12})-([a-f0-9]{12})$").unwrap());

fn bounce_id(tag: Option<&str>) -> Option<i64> {
    let c = BOUNCE_TAG.captures(tag?)?;
    let id: i64 = c[1].parse().ok()?;
    let expected = bounce_tag(id);
    // Constant-time comparison.
    let ok = expected.len() == c[2].len() && expected.bytes().zip(c[2].bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0;
    ok.then_some(id)
}

// ---------------------------------------------------------------------------------------------
// The creator's mailbox settings
// ---------------------------------------------------------------------------------------------

/// "alice@x.com" matches itself; "x.com" or "@x.com" matches the whole domain.
pub fn matches_list(address: &str, list: &[String]) -> bool {
    let a = address.to_lowercase();
    let domain = &a[a.rfind('@').map(|i| i + 1).unwrap_or(0)..];
    list.iter().any(|entry| {
        let e = entry.trim().to_lowercase();
        if e.is_empty() {
            return false;
        }
        if e.contains('@') && !e.starts_with('@') {
            return e == a;
        }
        e.trim_start_matches('@') == domain
    })
}

static DOMAIN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^[a-z0-9-]+(\.[a-z0-9-]+)+$").unwrap());

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub agent_id: String,
    pub enabled: bool,
    pub display_name: Option<String>,
    pub blocked: Vec<String>,
    pub forward_to_owner: bool,
}

pub async fn get_mail_settings(agent_id: &str) -> Result<Settings> {
    let row: Option<crate::db::MailSettings> = sqlx::query_as("select * from mail_settings where agent_id = $1").bind(agent_id).fetch_optional(db()).await?;
    Ok(match row {
        Some(r) => Settings {
            agent_id: r.agent_id,
            enabled: r.enabled,
            display_name: r.display_name,
            blocked: serde_json::from_value(r.blocked).unwrap_or_default(),
            forward_to_owner: r.forward_to_owner,
        },
        None => Settings { agent_id: agent_id.into(), enabled: true, display_name: None, blocked: vec![], forward_to_owner: false },
    })
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsPatch {
    pub enabled: Option<bool>,
    #[serde(default, deserialize_with = "double_option")]
    pub display_name: Option<Option<String>>,
    pub blocked: Option<Vec<String>>,
    pub forward_to_owner: Option<bool>,
}

/// `null` (clear) vs absent (keep).
fn double_option<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Option<Option<String>>, D::Error> {
    Ok(Some(Option::<String>::deserialize(d)?))
}

static NAME_JUNK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"[\r\n<>"]+"#).unwrap());

pub async fn update_mail_settings(agent_id: &str, patch: SettingsPatch) -> Result<Settings> {
    let mut next = get_mail_settings(agent_id).await?;
    if let Some(e) = patch.enabled {
        next.enabled = e;
    }
    if let Some(f) = patch.forward_to_owner {
        next.forward_to_owner = f;
    }
    if let Some(name) = patch.display_name {
        let name = NAME_JUNK.replace_all(&name.unwrap_or_default(), " ").trim().to_string();
        if name.chars().count() > DISPLAY_NAME_CHARS {
            return Err(mail_error(format!("The sender name is limited to {DISPLAY_NAME_CHARS} characters.")));
        }
        next.display_name = (!name.is_empty()).then_some(name);
    }
    if let Some(blocked) = patch.blocked {
        let mut list: Vec<String> = vec![];
        for e in blocked.iter().map(|e| e.trim().to_lowercase()).filter(|e| !e.is_empty()) {
            if !list.contains(&e) {
                list.push(e);
            }
        }
        if list.len() > BLOCKED_MAX {
            return Err(mail_error(format!("You can block at most {BLOCKED_MAX} senders.")));
        }
        if let Some(bad) = list.iter().find(|e| if e.contains('@') && !e.starts_with('@') { normalize_email(e).is_none() } else { !DOMAIN.is_match(e.trim_start_matches('@')) }) {
            return Err(mail_error(format!("\"{bad}\" isn't an email address or a domain.")));
        }
        next.blocked = list;
    }
    sqlx::query(
        "insert into mail_settings (agent_id, enabled, display_name, blocked, forward_to_owner, updated_at) values ($1, $2, $3, $4, $5, now())
         on conflict (agent_id) do update set enabled = excluded.enabled, display_name = excluded.display_name, blocked = excluded.blocked, forward_to_owner = excluded.forward_to_owner, updated_at = now()",
    )
    .bind(agent_id)
    .bind(next.enabled)
    .bind(&next.display_name)
    .bind(json!(next.blocked))
    .bind(next.forward_to_owner)
    .execute(db())
    .await?;
    Ok(next)
}

/// The creator's verified email (from signing in with email), where forwarded mail goes.
pub async fn owner_email(owner_account_id: &str) -> Result<Option<String>> {
    let e: Option<Option<String>> = sqlx::query_scalar("select email from users where account_id = $1").bind(owner_account_id).fetch_optional(db()).await?;
    Ok(e.flatten().and_then(|e| normalize_email(&e)))
}

async fn agent(agent_id: &str) -> Result<Option<Agent>> {
    Ok(sqlx::query_as("select * from agents where id = $1").bind(agent_id).fetch_optional(db()).await?)
}

/// Copy a new message to the creator's own email. Never fails.
async fn forward_to_owner(agent_id: &str, from_address: &str, from_name: &str, subject: &str, text: &str, to: &str) {
    let r: Result<()> = async {
        let Some(a) = agent(agent_id).await? else { return Ok(()) };
        let Some(owner) = owner_email(&a.owner_account_id).await? else { return Ok(()) };
        // The owner wrote to their own agent: nothing to forward.
        if owner == from_address {
            return Ok(());
        }
        let from = if from_name.is_empty() { from_address.to_string() } else { format!("{from_name} <{from_address}>") };
        queue_message(Outgoing {
            agent: &a,
            author: "owner",
            to: vec![owner],
            cc: vec![],
            subject: clip(&format!("Fwd: {}", if subject.is_empty() { "(no subject)" } else { subject }), SUBJECT_CHARS),
            text: clip(&format!("Forwarded from {to}\nFrom: {from}\nSubject: {subject}\n\n{text}"), TEXT_CHARS),
            reply_to: None,
            auto_submitted: Some(true),
        })
        .await?;
        Ok(())
    }
    .await;
    if let Err(e) = r {
        eprintln!("[mail] forwarding for {agent_id}: {}", e.message);
    }
}

/// JavaScript's `slice(0, n)` on UTF-16 units, approximated by chars.
fn clip(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        s.chars().take(n).collect()
    } else {
        s.to_string()
    }
}

// ---------------------------------------------------------------------------------------------
// Inbound
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Recipient {
    #[serde(rename_all = "camelCase")]
    Agent { agent_id: String, address: String, over_quota: bool },
    #[serde(rename_all = "camelCase")]
    Bounce { message_id: i64 },
    System { local: String },
    Unknown,
}

/// What an RCPT TO address is. Orvyn Mail refuses "unknown" and over-quota agents.
pub async fn resolve_recipient(address: &str) -> Result<Recipient> {
    let Some(parts) = split_address(address).filter(|p| p.domain == mail_domain()) else { return Ok(Recipient::Unknown) };
    if parts.local == BOUNCE_LOCAL {
        return Ok(bounce_id(parts.tag.as_deref()).map(|id| Recipient::Bounce { message_id: id }).unwrap_or(Recipient::Unknown));
    }
    if SYSTEM_LOCAL_PARTS.contains(&parts.local.as_str()) {
        return Ok(Recipient::System { local: parts.local });
    }
    if crate::public::RESERVED_HANDLES.contains(&parts.local.as_str()) {
        return Ok(Recipient::Unknown);
    }
    let row: Option<(String, Option<String>)> = sqlx::query_as("select id, held_reason from agents where handle = $1").bind(&parts.local).fetch_optional(db()).await?;
    let Some((agent_id, None)) = row else { return Ok(Recipient::Unknown) };
    // The creator turned the mailbox off: refused like a mailbox that doesn't exist.
    if !get_mail_settings(&agent_id).await?.enabled {
        return Ok(Recipient::Unknown);
    }
    let n: i64 = sqlx::query_scalar("select count(*) from mail_messages where agent_id = $1 and direction = 'in' and created_at > now() - interval '24 hours'").bind(&agent_id).fetch_one(db()).await?;
    Ok(Recipient::Agent { agent_id, address: format!("{}@{}", parts.local, parts.domain), over_quota: n >= env().orvyn_mail_daily_receive_cap as i64 })
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Auth {
    pub spf: String,
    pub dkim: String,
    pub dmarc: String,
    #[serde(rename = "dmarcPolicy")]
    pub dmarc_policy: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NameAddress {
    pub address: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InboundMessage {
    /// Envelope recipients (RCPT TO) on our domain.
    pub recipients: Vec<String>,
    /// Envelope sender (MAIL FROM); empty for bounces.
    pub mail_from: String,
    pub remote_ip: Option<String>,
    pub auth: Auth,
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    #[serde(default)]
    pub references: Vec<String>,
    pub from: Option<NameAddress>,
    #[serde(default)]
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub attachments: Vec<Value>,
    pub auto_submitted: Option<String>,
    pub precedence: Option<String>,
    pub list_id: Option<String>,
}

/// Spam: the sender's domain says to reject or quarantine mail failing DMARC, or no
/// authentication passes (SPF and DKIM both fail).
pub fn is_spam(auth: &Auth) -> bool {
    if auth.dmarc == "fail" && matches!(auth.dmarc_policy.as_deref(), Some("reject" | "quarantine")) {
        return true;
    }
    auth.spf == "fail" && auth.dkim != "pass"
}

static BULK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(bulk|list|junk|auto_reply)$").unwrap());
static ROBOT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(mailer-daemon|postmaster|no-?reply|do-?not-?reply|bounces?)([+@])").unwrap());

/// Mail no one should auto-answer: auto-replies, bounces, mailing lists and bulk mail.
pub fn is_automated(auto_submitted: Option<&str>, precedence: Option<&str>, list_id: Option<&str>, mail_from: &str, from_address: &str) -> bool {
    if auto_submitted.is_some_and(|a| !a.is_empty() && a.trim().to_lowercase() != "no") {
        return true;
    }
    if precedence.is_some_and(|p| BULK.is_match(p.trim())) {
        return true;
    }
    if list_id.is_some_and(|l| !l.is_empty()) {
        return true;
    }
    if mail_from.is_empty() {
        return true; // null sender: a bounce or DSN
    }
    ROBOT.is_match(from_address)
}

fn non_empty(s: &Option<String>) -> Option<String> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

/// Store a message for each agent it was addressed to. Returns how many were stored.
pub async fn receive_message(input: &InboundMessage) -> Result<usize> {
    let from_address = input.from.as_ref().and_then(|f| normalize_email(&f.address)).or_else(|| normalize_email(&input.mail_from)).unwrap_or_else(|| "unknown@invalid".into());
    let from_name = input.from.as_ref().map(|f| f.name.clone()).unwrap_or_default();
    let spam = is_spam(&input.auth);
    let automated = is_automated(input.auto_submitted.as_deref(), input.precedence.as_deref(), input.list_id.as_deref(), &input.mail_from, &from_address);
    let message_id = non_empty(&input.message_id).unwrap_or_else(new_message_id);
    let in_reply_to = non_empty(&input.in_reply_to);
    let refs: Vec<String> = input.references.iter().map(|r| r.trim().to_string()).filter(|r| !r.is_empty()).collect();
    let references: Vec<String> = refs[refs.len().saturating_sub(20)..].to_vec();
    let addresses = |list: &[String]| -> Vec<String> { list.iter().filter_map(|a| normalize_email(a)).take(50).collect() };
    let text = clip(&input.text, TEXT_CHARS);

    let mut stored = 0;
    let mut seen = HashSet::new();
    for rcpt in &input.recipients {
        let Recipient::Agent { agent_id, address, .. } = resolve_recipient(rcpt).await? else { continue };
        if !seen.insert(agent_id.clone()) {
            continue;
        }
        let settings = get_mail_settings(&agent_id).await?;
        // Blocked senders land in spam: kept for the creator, never shown to the agent.
        let junk = spam || matches_list(&from_address, &settings.blocked);
        // Same conversation as an earlier message of this agent's, if it references one.
        let mut lookup = references.clone();
        lookup.extend(in_reply_to.clone());
        let thread_id = thread_for(&agent_id, &lookup, &message_id).await?;
        // A client's reply in a marketplace job's thread goes to the job, not to the mail session.
        let for_job = !junk && !automated && thread_id != message_id && crate::hires::on_inbound_email(&agent_id, &thread_id, &from_address, &text).await.unwrap_or(false);
        sqlx::query(
            "insert into mail_messages (agent_id, direction, status, message_id, in_reply_to, \"references\", thread_id, from_address, from_name, to_addresses, cc_addresses, subject, text, attachments, auth, auto_submitted, handled)
             values ($1, 'in', $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)",
        )
        .bind(&agent_id)
        .bind(if junk { "spam" } else { "received" })
        .bind(&message_id)
        .bind(&in_reply_to)
        .bind(json!(references))
        .bind(&thread_id)
        .bind(&from_address)
        .bind(clip(&from_name, 200))
        .bind(json!(addresses(&input.to)))
        .bind(json!(addresses(&input.cc)))
        .bind(clip(&input.subject, SUBJECT_CHARS))
        .bind(&text)
        .bind(json!(input.attachments.iter().take(50).collect::<Vec<_>>()))
        .bind(json!({ "spf": input.auth.spf, "dkim": input.auth.dkim, "dmarc": input.auth.dmarc, "ip": input.remote_ip }))
        .bind(automated)
        // Spam, blocked and automated mail and job replies never reach the agent's mail session.
        .bind(junk || automated || for_job)
        .execute(db())
        .await?;
        prune(&agent_id).await?;
        let to_agent = !junk && !automated && !for_job;
        if to_agent && settings.forward_to_owner {
            forward_to_owner(&agent_id, &from_address, &from_name, &input.subject, &input.text, &address).await;
        }
        if to_agent {
            let who = if from_name.is_empty() { from_address.clone() } else { from_name.clone() };
            crate::push::notify_owner(
                &agent_id,
                crate::push::Notice { title: format!("New email to {address}"), body: format!("{who}: {}", if input.subject.is_empty() { "(no subject)" } else { &input.subject }), path: Some(format!("/agent/{agent_id}/inbox")) },
            );
        }
        stored += 1;
    }
    Ok(stored)
}

async fn thread_for(agent_id: &str, refs: &[String], fallback: &str) -> Result<String> {
    if !refs.is_empty() {
        let hit: Option<String> = sqlx::query_scalar("select thread_id from mail_messages where agent_id = $1 and message_id = any($2) limit 1").bind(agent_id).bind(refs).fetch_optional(db()).await?;
        if let Some(t) = hit {
            return Ok(t);
        }
    }
    Ok(fallback.to_string())
}

async fn prune(agent_id: &str) -> Result<()> {
    sqlx::query("delete from mail_messages where agent_id = $1 and id < (select id from mail_messages where agent_id = $1 order by id desc offset $2 limit 1)")
        .bind(agent_id)
        .bind(KEEP_PER_AGENT)
        .execute(db())
        .await?;
    Ok(())
}

/// A bounce came back for outbound message `id` (VERP).
pub async fn mark_bounced(id: i64, detail: &str) -> Result<bool> {
    let msg: Option<MailMessage> = sqlx::query_as("select * from mail_messages where id = $1 and direction = 'out'").bind(id as i32).fetch_optional(db()).await?;
    let Some(msg) = msg else { return Ok(false) };
    sqlx::query("update mail_messages set status = 'bounced', last_error = $2 where id = $1").bind(id as i32).bind(clip(detail, 2000)).execute(db()).await?;
    log_agent(&msg.agent_id, AgentLogKind::Mail, &format!("Email to {} bounced: {}", strings(&msg.to_addresses).join(", "), clip(detail, 300))).await?;
    Ok(true)
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// Outbound
// ---------------------------------------------------------------------------------------------

pub async fn sent_last_24h(agent_id: &str, author: Option<&str>) -> Result<i64> {
    Ok(sqlx::query_scalar("select count(*) from mail_messages where agent_id = $1 and direction = 'out' and created_at > now() - interval '24 hours' and ($2::text is null or author = $2)")
        .bind(agent_id)
        .bind(author)
        .fetch_one(db())
        .await?)
}

pub async fn replies_to(agent_id: &str, address: &str) -> Result<i64> {
    Ok(sqlx::query_scalar("select count(*) from mail_messages where agent_id = $1 and direction = 'out' and author = 'agent' and created_at > now() - interval '24 hours' and to_addresses @> $2")
        .bind(agent_id)
        .bind(json!([address]))
        .fetch_one(db())
        .await?)
}

pub struct Outgoing<'a> {
    pub agent: &'a Agent,
    pub author: &'static str,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub text: String,
    /// The message this replies to (threads it and sets In-Reply-To / References).
    pub reply_to: Option<&'a MailMessage>,
    /// Agent sends are marked Auto-Submitted, so other bots don't answer them.
    pub auto_submitted: Option<bool>,
}

static NEWLINES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\r\n]+").unwrap());

fn dedupe(list: &[String]) -> Vec<Option<String>> {
    let mut out: Vec<Option<String>> = vec![];
    for a in list.iter().map(|a| normalize_email(a)) {
        if !out.contains(&a) {
            out.push(a);
        }
    }
    out
}

/// Thousands separators, as toLocaleString("en-US").
fn grouped(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Validate and queue a message from the agent's address.
pub async fn queue_message(m: Outgoing<'_>) -> Result<MailMessage> {
    let from = agent_address(m.agent.handle.as_deref()).ok_or_else(|| mail_error("This agent has no handle, so it has no email address yet."))?;
    let (to, cc) = (dedupe(&m.to), dedupe(&m.cc));
    if to.is_empty() || to.contains(&None) || cc.contains(&None) {
        return Err(mail_error("Every recipient must be a valid email address."));
    }
    let (to, cc): (Vec<String>, Vec<String>) = (to.into_iter().flatten().collect(), cc.into_iter().flatten().collect());
    let recipients: Vec<String> = to.iter().chain(&cc).cloned().collect();
    if recipients.len() > RECIPIENTS {
        return Err(mail_error(format!("At most {RECIPIENTS} recipients per email.")));
    }
    if recipients.contains(&from) {
        return Err(mail_error("An agent can't email itself."));
    }
    let subject = NEWLINES.replace_all(&m.subject, " ").trim().to_string();
    if subject.chars().count() > SUBJECT_CHARS {
        return Err(mail_error(format!("Subject is limited to {SUBJECT_CHARS} characters.")));
    }
    if m.text.trim().is_empty() {
        return Err(mail_error("The email is empty."));
    }
    if m.text.chars().count() > TEXT_CHARS {
        return Err(mail_error(format!("Body is limited to {} characters.", grouped(TEXT_CHARS))));
    }
    let cap = env().orvyn_mail_daily_send_cap;
    if sent_last_24h(&m.agent.id, None).await? >= cap as i64 {
        return Err(mail_error(format!("This agent has reached its limit of {cap} emails per 24 hours.")));
    }
    let parent = m.reply_to;
    let message_id = new_message_id();
    let delivery: Map<String, Value> = recipients.iter().map(|r| (r.clone(), json!({ "status": "pending" }))).collect();
    let references: Vec<String> = match parent {
        Some(p) => {
            let mut r = strings(&p.references);
            r.push(p.message_id.clone());
            r[r.len().saturating_sub(20)..].to_vec()
        }
        None => vec![],
    };
    let subject = if !subject.is_empty() {
        subject
    } else {
        parent.map(|p| reply_subject(&p.subject)).unwrap_or_else(|| "(no subject)".into())
    };
    let from_name = get_mail_settings(&m.agent.id).await?.display_name.unwrap_or_else(|| m.agent.name.clone());
    let row: MailMessage = sqlx::query_as(
        "insert into mail_messages (agent_id, direction, status, author, message_id, in_reply_to, \"references\", thread_id, from_address, from_name, to_addresses, cc_addresses, subject, text, auto_submitted, read, handled, delivery, next_attempt_at)
         values ($1, 'out', 'queued', $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, true, true, $14, now()) returning *",
    )
    .bind(&m.agent.id)
    .bind(m.author)
    .bind(&message_id)
    .bind(parent.map(|p| p.message_id.clone()))
    .bind(json!(references))
    .bind(parent.map(|p| p.thread_id.clone()).unwrap_or_else(|| message_id.clone()))
    .bind(&from)
    .bind(from_name)
    .bind(json!(to))
    .bind(json!(cc))
    .bind(subject)
    .bind(&m.text)
    .bind(m.auto_submitted.unwrap_or(m.author == "agent"))
    .bind(Value::Object(delivery))
    .fetch_one(db())
    .await?;
    Ok(row)
}

static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^re:").unwrap());

pub fn reply_subject(subject: &str) -> String {
    let s = subject.trim();
    if RE.is_match(s) {
        s.to_string()
    } else {
        format!("Re: {s}")
    }
}

/// A message as Orvyn Mail needs it to compose and deliver.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutboundJob {
    pub id: i32,
    pub envelope_from: String,
    pub from: NameAddress,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    /// Recipients still to deliver to.
    pub pending: Vec<String>,
    pub subject: String,
    pub text: String,
    pub message_id: String,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub auto_submitted: bool,
    pub date: String,
}

/// Lease due outbound messages to Orvyn Mail (5-minute lease; an expired lease is retried).
pub async fn claim_outbound(limit: i64) -> Result<Vec<OutboundJob>> {
    let ids: Vec<i32> = sqlx::query_scalar(
        "update mail_messages set status = 'sending', locked_until = now() + interval '5 minutes'
         where id in (
           select id from mail_messages
           where direction = 'out' and ((status = 'queued' and next_attempt_at <= now()) or (status = 'sending' and locked_until < now()))
           order by next_attempt_at limit $1 for update skip locked
         ) returning id",
    )
    .bind(limit)
    .fetch_all(db())
    .await?;
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let msgs: Vec<MailMessage> = sqlx::query_as("select * from mail_messages where id = any($1)").bind(&ids).fetch_all(db()).await?;
    Ok(msgs
        .into_iter()
        .map(|m| OutboundJob {
            id: m.id,
            envelope_from: bounce_address(m.id as i64),
            from: NameAddress { name: m.from_name.clone(), address: m.from_address.clone() },
            to: strings(&m.to_addresses),
            cc: strings(&m.cc_addresses),
            pending: m.delivery.as_object().map(|d| d.iter().filter(|(_, v)| v["status"] == "pending").map(|(k, _)| k.clone()).collect()).unwrap_or_default(),
            subject: m.subject,
            text: m.text,
            message_id: m.message_id,
            in_reply_to: m.in_reply_to,
            references: strings(&m.references),
            auto_submitted: m.auto_submitted,
            // Date.toUTCString(): "Tue, 24 Nov 2026 10:00:00 GMT".
            date: m.created_at.format("%a, %d %b %Y %H:%M:%S GMT").to_string(),
        })
        .collect())
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeliveryResult {
    /// "sent" | "temp" | "perm".
    pub status: String,
    pub error: Option<String>,
}

/// Record what happened to each recipient, and schedule a retry or settle the message.
/// Returns "retry", "sent" or "failed", or None for an unknown message.
pub async fn report_delivery(id: i64, results: &BTreeMap<String, DeliveryResult>) -> Result<Option<&'static str>> {
    let msg: Option<MailMessage> = sqlx::query_as("select * from mail_messages where id = $1 and direction = 'out'").bind(id as i32).fetch_optional(db()).await?;
    let Some(msg) = msg else { return Ok(None) };
    let now = chrono::Utc::now();
    let now_ms = now.timestamp_millis();
    let expired = now_ms - msg.created_at.timestamp_millis() > MAX_QUEUE_HOURS * 3_600_000;
    let mut delivery = msg.delivery.as_object().cloned().unwrap_or_default();
    let mut last_error: Option<String> = None;
    for (rcpt, r) in results {
        if delivery.get(rcpt).is_none_or(|d| d["status"] != "pending") {
            continue;
        }
        if r.status == "sent" {
            delivery.insert(rcpt.clone(), json!({ "status": "sent", "at": now_ms }));
            continue;
        }
        // Permanent refusals fail now; temporary ones stay pending until the queue time runs out.
        if r.status == "perm" || expired {
            delivery.insert(rcpt.clone(), json!({ "status": "failed", "error": clip(r.error.as_deref().unwrap_or("rejected"), 500), "at": now_ms }));
        }
        let why = r.error.clone().unwrap_or_else(|| if r.status == "perm" { "rejected".into() } else { "temporary failure".into() });
        last_error = Some(clip(&format!("{rcpt}: {why}"), 500));
    }
    if expired {
        for d in delivery.values_mut() {
            if d["status"] == "pending" {
                *d = json!({ "status": "failed", "error": "gave up after 3 days", "at": now_ms });
            }
        }
    }
    let attempts = msg.attempts + 1;
    if delivery.values().any(|d| d["status"] == "pending") {
        let delay = BACKOFF[(attempts as usize - 1).min(BACKOFF.len() - 1)];
        sqlx::query("update mail_messages set status = 'queued', delivery = $2, attempts = $3, last_error = $4, locked_until = null, next_attempt_at = now() + make_interval(secs => $5) where id = $1")
            .bind(id as i32)
            .bind(Value::Object(delivery))
            .bind(attempts)
            .bind(&last_error)
            .bind(delay as f64)
            .execute(db())
            .await?;
        return Ok(Some("retry"));
    }
    let any_sent = delivery.values().any(|d| d["status"] == "sent");
    let failed: Vec<String> = delivery.iter().filter(|(_, d)| d["status"] == "failed").map(|(r, d)| format!("{r} ({})", d["error"].as_str().unwrap_or(""))).collect();
    sqlx::query("update mail_messages set status = $2, delivery = $3, attempts = $4, last_error = $5, locked_until = null, next_attempt_at = null, sent_at = $6 where id = $1")
        .bind(id as i32)
        .bind(if any_sent { "sent" } else { "failed" })
        .bind(Value::Object(delivery))
        .bind(attempts)
        .bind(&last_error)
        .bind(any_sent.then_some(now))
        .execute(db())
        .await?;
    if !failed.is_empty() {
        log_agent(&msg.agent_id, AgentLogKind::Mail, &format!("Email \"{}\" couldn't be delivered to {}", clip(&msg.subject, 80), failed.join(", "))).await?;
    }
    Ok(Some(if any_sent { "sent" } else { "failed" }))
}

// ---------------------------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------------------------

fn folder_filter(folder: &str) -> Result<&'static str> {
    Ok(match folder {
        "inbox" => "direction = 'in' and status <> 'spam'",
        "spam" => "direction = 'in' and status = 'spam'",
        "sent" => "direction = 'out'",
        _ => return Err(mail_error("folder must be inbox, sent or spam")),
    })
}

/// Newest first, paged by id.
pub async fn list_messages(agent_id: &str, folder: &str, before: Option<i64>, limit: Option<i64>) -> Result<Vec<MailMessage>> {
    Ok(sqlx::query_as(&format!("select * from mail_messages where agent_id = $1 and {} and ($2::int is null or id < $2) order by id desc limit $3", folder_filter(folder)?))
        .bind(agent_id)
        .bind(before.map(|b| b as i32))
        .bind(limit.unwrap_or(50).min(100))
        .fetch_all(db())
        .await?)
}

pub async fn get_message(agent_id: &str, id: i64) -> Result<Option<MailMessage>> {
    Ok(sqlx::query_as("select * from mail_messages where agent_id = $1 and id = $2").bind(agent_id).bind(id as i32).fetch_optional(db()).await?)
}

pub async fn thread_messages(agent_id: &str, thread_id: &str, limit: i64) -> Result<Vec<MailMessage>> {
    let mut rows: Vec<MailMessage> = sqlx::query_as("select * from mail_messages where agent_id = $1 and thread_id = $2 and status <> 'spam' order by id desc limit $3").bind(agent_id).bind(thread_id).bind(limit).fetch_all(db()).await?;
    rows.reverse();
    Ok(rows)
}

pub async fn unread_count(agent_id: &str) -> Result<i64> {
    Ok(sqlx::query_scalar("select count(*) from mail_messages where agent_id = $1 and direction = 'in' and status = 'received' and read = false").bind(agent_id).fetch_one(db()).await?)
}

/// Inbound mail the agent's mail session hasn't dealt with yet, oldest first.
pub async fn unhandled(agent_id: &str, limit: i64) -> Result<Vec<MailMessage>> {
    Ok(sqlx::query_as("select * from mail_messages where agent_id = $1 and direction = 'in' and status = 'received' and handled = false order by id limit $2").bind(agent_id).bind(limit).fetch_all(db()).await?)
}

pub async fn mark_handled(ids: &[i32]) -> Result<()> {
    if !ids.is_empty() {
        sqlx::query("update mail_messages set handled = true where id = any($1)").bind(ids).execute(db()).await?;
    }
    Ok(())
}

/// The creator moves a message out of (or into) spam. Rescued mail goes to the agent again.
pub async fn set_spam(agent_id: &str, id: i64, spam: bool) -> Result<()> {
    let (status, handled) = if spam { ("spam", true) } else { ("received", false) };
    sqlx::query("update mail_messages set status = $3, handled = $4 where agent_id = $1 and id = $2 and direction = 'in'").bind(agent_id).bind(id as i32).bind(status).bind(handled).execute(db()).await?;
    Ok(())
}

static SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());

/// What the owner's inbox shows: never auth internals beyond pass/fail, never other agents' mail.
pub fn message_view(m: &MailMessage, full: bool) -> Value {
    let mut v = json!({
        "id": m.id,
        "direction": m.direction,
        "status": m.status,
        "author": m.author,
        "threadId": m.thread_id,
        "from": { "address": m.from_address, "name": m.from_name },
        "to": m.to_addresses,
        "cc": m.cc_addresses,
        "subject": m.subject,
        "preview": clip(&SPACES.replace_all(&m.text, " "), 160),
        "read": m.read,
        "handled": m.handled,
        "automated": m.auto_submitted,
        "attachments": m.attachments,
        "createdAt": m.created_at.timestamp_millis(),
        "sentAt": m.sent_at.map(|t| t.timestamp_millis()),
    });
    if full {
        let o = v.as_object_mut().unwrap();
        o.insert("text".into(), json!(m.text));
        o.insert("auth".into(), m.auth.as_ref().map(|a| json!({ "spf": a["spf"], "dkim": a["dkim"], "dmarc": a["dmarc"] })).unwrap_or(Value::Null));
        o.insert("delivery".into(), m.delivery.clone());
        o.insert("lastError".into(), json!(m.last_error));
    }
    v
}
