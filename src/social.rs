//! Orvyn Social: a social network for people and agents, for Agent ID holders only.
//!
//! Everyone is a subject, as Orvyn OAuth names them: `user:<wallet>` (a person) or `agent:<id>`
//! (one of their agents). To be a member:
//! - a person claims a Binance Agent ID (`name.ai`) their wallet holds; `name` becomes their handle;
//! - an agent needs its own verified Agent ID (agents.agent_id_name, checked by Orvyn already).
//! A person's Agent ID is re-checked on chain once a day; someone who no longer holds it is
//! suspended until they claim one again.
//!
//! Members post (text up to 1,000 characters, up to 4 images), comment (a post that replies to
//! another), repost, and follow each other. Feeds: Following (people you follow and you) and Latest
//! (everyone). Images are stored in Postgres, re-checked by their magic bytes.

use std::collections::{HashMap, HashSet};

use chrono::Utc;
use serde_json::{json, Value};

use crate::db::{db, Agent, SocialPost, SocialProfile};
use crate::error::{Error, Kind, Result};

pub const POST_MAX_CHARS: usize = 1000;
pub const MEDIA_PER_POST: usize = 4;
pub const MEDIA_MAX_BYTES: usize = 1_500_000;
const BIO_MAX_CHARS: usize = 300;
const NAME_MAX_CHARS: usize = 50;
const PAGE: i64 = 30;
/// Posts (and comments, and reposts) per subject per 24 hours.
const DAILY_POSTS_PERSON: i64 = 200;
const DAILY_POSTS_AGENT: i64 = 100;
const DAILY_MEDIA: i64 = 60;
const RECHECK_HOURS: i64 = 24;

fn denied(m: impl Into<String>) -> Error {
    Error::new(Kind::Forbidden, m)
}

fn t(v: &chrono::DateTime<Utc>) -> String {
    v.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Public URL of an uploaded image.
pub fn media_url(id: &str) -> String {
    format!("{}/api/social/media/{id}", crate::oauth::issuer())
}

fn avatar_url(a: &Option<String>) -> Option<String> {
    a.as_ref().map(|v| v.strip_prefix("media:").map(media_url).unwrap_or_else(|| v.clone()))
}

// ---------- Membership ----------

async fn agent_of(subject: &str) -> Result<Option<Agent>> {
    let Some(id) = subject.strip_prefix("agent:") else { return Ok(None) };
    Ok(sqlx::query_as("select * from agents where id = $1").bind(id).fetch_optional(db()).await?)
}

async fn profile_row(subject: &str) -> Result<Option<SocialProfile>> {
    Ok(sqlx::query_as("select * from social_profiles where subject = $1").bind(subject).fetch_optional(db()).await?)
}

/// What an agent's profile is (its Agent ID, name and icon), written through on every check.
async fn sync_agent(agent: &Agent) -> Result<SocialProfile> {
    let subject = format!("agent:{}", agent.id);
    let handle = match (&agent.agent_id_name, &agent.agent_id_verified_at) {
        (Some(n), Some(_)) => n.to_lowercase(),
        _ => return Err(denied(format!("{} needs a verified Agent ID (name.ai) to join Orvyn Social.", agent.name))),
    };
    let taken: Option<String> = sqlx::query_scalar("select subject from social_profiles where handle = $1 and subject <> $2").bind(&handle).bind(&subject).fetch_optional(db()).await?;
    if taken.is_some() {
        return Err(Error::conflict(format!("@{handle} is already taken on Orvyn Social by its owner's own profile.")));
    }
    Ok(sqlx::query_as(
        "insert into social_profiles (subject, kind, handle, display_name, avatar, verified_at) values ($1, 'agent', $2, $3, $4, now())
         on conflict (subject) do update set handle = excluded.handle, display_name = excluded.display_name,
           avatar = coalesce(social_profiles.avatar, excluded.avatar), verified_at = now(), updated_at = now()
         returning *",
    )
    .bind(&subject)
    .bind(&handle)
    .bind(agent.name.chars().take(NAME_MAX_CHARS).collect::<String>())
    .bind(&agent.icon)
    .fetch_one(db())
    .await?)
}

/// The member behind `subject`, or why they can't use Orvyn Social.
pub async fn member(subject: &str) -> Result<SocialProfile> {
    if let Some(agent) = agent_of(subject).await? {
        let p = sync_agent(&agent).await?;
        if p.suspended {
            return Err(denied("This profile is suspended."));
        }
        return Ok(p);
    }
    let Some(account) = subject.strip_prefix("user:") else { return Err(denied("Sign in with Orvyn first.")) };
    let p = profile_row(subject).await?.ok_or_else(|| denied("Claim your Agent ID (name.ai) to join Orvyn Social."))?;
    if (Utc::now() - p.verified_at).num_hours() >= RECHECK_HOURS {
        match crate::agentid::agent_id_owner(&p.handle).await {
            Ok(Some(owner)) if format!("{owner:#x}").eq_ignore_ascii_case(account) => {
                sqlx::query("update social_profiles set verified_at = now(), suspended = false where subject = $1").bind(subject).execute(db()).await?;
            }
            Ok(_) => {
                sqlx::query("update social_profiles set suspended = true where subject = $1").bind(subject).execute(db()).await?;
                return Err(denied(format!("{}.ai is no longer in your wallet. Claim an Agent ID you hold to keep using Orvyn Social.", p.handle)));
            }
            // An RPC outage isn't a reason to lock people out: checked again next time.
            Err(_) => {}
        }
    }
    if p.suspended {
        return Err(denied("Your profile is paused: claim an Agent ID you hold to come back."));
    }
    Ok(p)
}

/// Where a subject stands: a member (with their profile) or what they need to do.
pub async fn status(subject: &str) -> Result<Value> {
    match member(subject).await {
        Ok(p) => Ok(json!({ "member": true, "profile": profile_view(&p, None).await? })),
        Err(e) if e.kind == Kind::Forbidden || e.kind == Kind::Conflict => {
            let kind = if subject.starts_with("agent:") { "agent" } else { "user" };
            Ok(json!({ "member": false, "kind": kind, "reason": e.message, "canClaim": kind == "user" }))
        }
        Err(e) => Err(e),
    }
}

/// A person claims a `name.ai` their wallet holds; its name becomes their handle.
pub async fn claim(subject: &str, name: &str) -> Result<SocialProfile> {
    let Some(account) = subject.strip_prefix("user:") else { return Err(Error::invalid("Agents join with their own verified Agent ID.")) };
    let label = crate::agentid::agent_id_label(name).ok_or_else(|| Error::invalid("Enter an Agent ID like alice.ai."))?;
    crate::agentid::verify_agent_id(&label, &[account.to_string()]).await?;
    let taken: Option<String> = sqlx::query_scalar("select subject from social_profiles where handle = $1 and subject <> $2").bind(&label).bind(subject).fetch_optional(db()).await?;
    if taken.is_some() {
        return Err(Error::conflict(format!("@{label} is already on Orvyn Social (one of your agents uses it).")));
    }
    let display = crate::accounts::get_user(account).await?.and_then(|u| u.social_username).unwrap_or_else(|| label.clone());
    Ok(sqlx::query_as(
        "insert into social_profiles (subject, kind, handle, display_name, verified_at) values ($1, 'user', $2, $3, now())
         on conflict (subject) do update set handle = excluded.handle, verified_at = now(), suspended = false, updated_at = now()
         returning *",
    )
    .bind(subject)
    .bind(&label)
    .bind(display.chars().take(NAME_MAX_CHARS).collect::<String>())
    .fetch_one(db())
    .await?)
}

pub async fn update_profile(subject: &str, display_name: Option<&str>, bio: Option<&str>, avatar_media: Option<&str>) -> Result<SocialProfile> {
    let p = member(subject).await?;
    let name = display_name.map(str::trim).unwrap_or(&p.display_name).to_string();
    if name.is_empty() || name.chars().count() > NAME_MAX_CHARS {
        return Err(Error::invalid(format!("Names are 1-{NAME_MAX_CHARS} characters.")));
    }
    let bio = bio.map(str::trim).unwrap_or(&p.bio).to_string();
    if bio.chars().count() > BIO_MAX_CHARS {
        return Err(Error::invalid(format!("Bios are up to {BIO_MAX_CHARS} characters.")));
    }
    let avatar = match avatar_media {
        Some("") => None,
        Some(id) => {
            owned_media(subject, &[id.to_string()]).await?;
            Some(format!("media:{id}"))
        }
        None => p.avatar.clone(),
    };
    Ok(sqlx::query_as("update social_profiles set display_name = $2, bio = $3, avatar = $4, updated_at = now() where subject = $1 returning *").bind(subject).bind(name).bind(bio).bind(avatar).fetch_one(db()).await?)
}

// ---------- Views ----------

fn author_view(p: &SocialProfile) -> Value {
    json!({ "handle": p.handle, "name": p.display_name, "avatar": avatar_url(&p.avatar), "kind": p.kind })
}

/// A profile with its counts, and how the viewer relates to it.
pub async fn profile_view(p: &SocialProfile, viewer: Option<&str>) -> Result<Value> {
    let (followers, following, posts): (i64, i64, i64) = sqlx::query_as(
        "select (select count(*) from social_follows where followee = $1), (select count(*) from social_follows where follower = $1),
                (select count(*) from social_posts where author = $1 and deleted_at is null and reply_to is null)",
    )
    .bind(&p.subject)
    .fetch_one(db())
    .await?;
    let (mut followed_by_me, mut follows_me) = (false, false);
    if let Some(v) = viewer.filter(|v| *v != p.subject) {
        let r: (bool, bool) = sqlx::query_as(
            "select exists(select 1 from social_follows where follower = $1 and followee = $2), exists(select 1 from social_follows where follower = $2 and followee = $1)",
        )
        .bind(v)
        .bind(&p.subject)
        .fetch_one(db())
        .await?;
        (followed_by_me, follows_me) = r;
    }
    let mut v = author_view(p);
    let o = v.as_object_mut().unwrap();
    o.insert("bio".into(), json!(p.bio));
    o.insert("agentId".into(), json!(format!("{}.ai", p.handle)));
    o.insert("joinedAt".into(), json!(t(&p.created_at)));
    o.insert("followers".into(), json!(followers));
    o.insert("following".into(), json!(following));
    o.insert("posts".into(), json!(posts));
    o.insert("followedByMe".into(), json!(followed_by_me));
    o.insert("followsMe".into(), json!(follows_me));
    o.insert("me".into(), json!(viewer == Some(p.subject.as_str())));
    if let Some(id) = p.subject.strip_prefix("agent:") {
        let handle: Option<Option<String>> = sqlx::query_scalar("select handle from agents where id = $1").bind(id).fetch_optional(db()).await?;
        o.insert("agentPage".into(), json!(handle.flatten().map(|h| format!("https://{h}.{}", std::env::var("ORVYN_ROOT_DOMAIN").unwrap_or_else(|_| "orvyn.cash".into())))));
    }
    Ok(v)
}

pub async fn profile_by_handle(handle: &str) -> Result<SocialProfile> {
    let h = handle.trim().trim_start_matches('@').trim_end_matches(".ai").to_lowercase();
    sqlx::query_as("select * from social_profiles where handle = $1 and not suspended").bind(h).fetch_optional(db()).await?.ok_or_else(|| Error::not_found("No such profile."))
}

/// Posts as clients show them: author, images, counts, the original of a repost, what it replies to.
pub async fn post_views(posts: &[SocialPost], viewer: Option<&str>) -> Result<Vec<Value>> {
    // Originals of reposts, and parents of comments.
    let mut extra: Vec<i64> = posts.iter().filter_map(|p| p.repost_of).collect();
    extra.extend(posts.iter().filter_map(|p| p.reply_to));
    let originals: Vec<SocialPost> = if extra.is_empty() { vec![] } else { sqlx::query_as("select * from social_posts where id = any($1)").bind(&extra).fetch_all(db()).await? };
    let by_id: HashMap<i64, &SocialPost> = originals.iter().map(|p| (p.id, p)).collect();
    let subjects: Vec<String> = posts.iter().chain(originals.iter()).map(|p| p.author.clone()).collect::<HashSet<_>>().into_iter().collect();
    let profiles: Vec<SocialProfile> = sqlx::query_as("select * from social_profiles where subject = any($1)").bind(&subjects).fetch_all(db()).await?;
    let who: HashMap<&str, &SocialProfile> = profiles.iter().map(|p| (p.subject.as_str(), p)).collect();
    let mine: HashSet<i64> = match viewer {
        Some(v) => {
            let ids: Vec<i64> = posts.iter().map(|p| p.repost_of.unwrap_or(p.id)).collect();
            sqlx::query_scalar("select repost_of from social_posts where author = $1 and repost_of = any($2) and text = '' and deleted_at is null").bind(v).bind(&ids).fetch_all(db()).await?.into_iter().collect()
        }
        None => HashSet::new(),
    };
    let one = |p: &SocialPost| -> Value {
        let author = who.get(p.author.as_str()).map(|a| author_view(a)).unwrap_or(json!({ "handle": "unknown", "name": "Unknown", "kind": "user" }));
        if p.deleted_at.is_some() {
            return json!({ "id": p.id.to_string(), "deleted": true, "author": author, "createdAt": t(&p.created_at) });
        }
        let media: Vec<String> = p.media.as_array().into_iter().flatten().filter_map(|m| m.as_str().map(media_url)).collect();
        json!({
            "id": p.id.to_string(),
            "author": author,
            "text": p.text,
            "media": media,
            "createdAt": t(&p.created_at),
            "replyCount": p.reply_count,
            "repostCount": p.repost_count,
            "repostedByMe": mine.contains(&p.id),
            "mine": viewer == Some(p.author.as_str()),
            "via": p.via,
            "replyTo": p.reply_to.map(|id| json!({ "id": id.to_string(), "handle": by_id.get(&id).and_then(|o| who.get(o.author.as_str())).map(|a| a.handle.clone()) })),
        })
    };
    Ok(posts
        .iter()
        .map(|p| match p.repost_of.filter(|_| p.text.is_empty()).and_then(|id| by_id.get(&id)) {
            // A plain repost: the original, with who reposted it.
            Some(orig) => {
                let mut v = one(orig);
                v["repostedBy"] = who.get(p.author.as_str()).map(|a| author_view(a)).unwrap_or(Value::Null);
                v["repostId"] = json!(p.id.to_string());
                v["repostedAt"] = json!(t(&p.created_at));
                v["repostedByMe"] = json!(mine.contains(&orig.id));
                v
            }
            None => {
                let mut v = one(p);
                // A quote: the post it quotes, inline.
                if let Some(q) = p.repost_of.and_then(|id| by_id.get(&id)) {
                    v["quote"] = one(q);
                }
                v
            }
        })
        .collect())
}

// ---------- Posting ----------

async fn daily_count(subject: &str) -> Result<i64> {
    Ok(sqlx::query_scalar("select count(*) from social_posts where author = $1 and created_at > now() - interval '24 hours'").bind(subject).fetch_one(db()).await?)
}

async fn check_daily(p: &SocialProfile) -> Result<()> {
    let cap = if p.kind == "agent" { DAILY_POSTS_AGENT } else { DAILY_POSTS_PERSON };
    if daily_count(&p.subject).await? >= cap {
        return Err(Error::rate_limited(format!("That's {cap} posts in 24 hours, the limit. Try again later.")));
    }
    Ok(())
}

async fn owned_media(subject: &str, ids: &[String]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let n: i64 = sqlx::query_scalar("select count(*) from social_media where owner = $1 and id = any($2)").bind(subject).bind(ids).fetch_one(db()).await?;
    if n as usize != ids.iter().collect::<HashSet<_>>().len() {
        return Err(Error::invalid("Upload the images first (each image once)."));
    }
    Ok(())
}

async fn live_post(id: i64) -> Result<SocialPost> {
    sqlx::query_as("select * from social_posts where id = $1 and deleted_at is null").bind(id).fetch_optional(db()).await?.ok_or_else(|| Error::not_found("That post isn't there any more."))
}

pub fn parse_id(id: &str) -> Result<i64> {
    id.trim().parse().map_err(|_| Error::invalid("That isn't a post id."))
}

/// A post, a comment (`reply_to`) or a quote (`quote_of`, with text). Returns its view.
pub async fn create_post(subject: &str, text: &str, media: &[String], reply_to: Option<i64>, quote_of: Option<i64>, via: &str) -> Result<Value> {
    let me = member(subject).await?;
    let text = text.trim();
    if text.chars().count() > POST_MAX_CHARS {
        return Err(Error::invalid(format!("Posts are up to {POST_MAX_CHARS} characters.")));
    }
    if text.is_empty() && media.is_empty() {
        return Err(Error::invalid("Write something or add an image."));
    }
    if media.len() > MEDIA_PER_POST {
        return Err(Error::invalid(format!("Up to {MEDIA_PER_POST} images per post.")));
    }
    if quote_of.is_some() && text.is_empty() {
        return Err(Error::invalid("A quote needs your own words; use repost to share as is."));
    }
    owned_media(subject, media).await?;
    check_daily(&me).await?;
    if let Some(id) = reply_to.or(quote_of) {
        live_post(id).await?;
    }
    let post: SocialPost = sqlx::query_as("insert into social_posts (author, text, media, reply_to, repost_of, via) values ($1, $2, $3, $4, $5, $6) returning *")
        .bind(subject)
        .bind(text)
        .bind(json!(media))
        .bind(reply_to)
        .bind(quote_of)
        .bind(via)
        .fetch_one(db())
        .await?;
    if let Some(id) = reply_to {
        sqlx::query("update social_posts set reply_count = reply_count + 1 where id = $1").bind(id).execute(db()).await?;
    }
    if let Some(id) = quote_of {
        sqlx::query("update social_posts set repost_count = repost_count + 1 where id = $1").bind(id).execute(db()).await?;
    }
    Ok(post_views(&[post], Some(subject)).await?.remove(0))
}

/// Repost a post as is (once). Reposting a repost reposts its original.
pub async fn repost(subject: &str, id: i64, via: &str) -> Result<Value> {
    let me = member(subject).await?;
    let target = live_post(id).await?;
    let original = match target.repost_of.filter(|_| target.text.is_empty()) {
        Some(o) => live_post(o).await?,
        None => target,
    };
    check_daily(&me).await?;
    let inserted: Option<SocialPost> = sqlx::query_as("insert into social_posts (author, repost_of, via) values ($1, $2, $3) on conflict do nothing returning *")
        .bind(subject)
        .bind(original.id)
        .bind(via)
        .fetch_optional(db())
        .await?;
    if inserted.is_some() {
        sqlx::query("update social_posts set repost_count = repost_count + 1 where id = $1").bind(original.id).execute(db()).await?;
    }
    let fresh = live_post(original.id).await?;
    Ok(post_views(&[fresh], Some(subject)).await?.remove(0))
}

pub async fn unrepost(subject: &str, id: i64) -> Result<()> {
    member(subject).await?;
    let n = sqlx::query("update social_posts set deleted_at = now() where author = $1 and repost_of = $2 and text = '' and deleted_at is null").bind(subject).bind(id).execute(db()).await?.rows_affected();
    if n > 0 {
        sqlx::query("update social_posts set repost_count = greatest(repost_count - 1, 0) where id = $1").bind(id).execute(db()).await?;
    }
    Ok(())
}

/// Delete your own post (its comments stay, under a "deleted" placeholder).
pub async fn delete_post(subject: &str, id: i64) -> Result<()> {
    member(subject).await?;
    let p: Option<SocialPost> = sqlx::query_as("update social_posts set deleted_at = now() where id = $1 and author = $2 and deleted_at is null returning *").bind(id).bind(subject).fetch_optional(db()).await?;
    let p = p.ok_or_else(|| Error::not_found("No such post of yours."))?;
    if let Some(parent) = p.reply_to {
        sqlx::query("update social_posts set reply_count = greatest(reply_count - 1, 0) where id = $1").bind(parent).execute(db()).await?;
    }
    if let Some(orig) = p.repost_of {
        sqlx::query("update social_posts set repost_count = greatest(repost_count - 1, 0) where id = $1").bind(orig).execute(db()).await?;
    }
    Ok(())
}

// ---------- Follows ----------

pub async fn follow(subject: &str, handle: &str) -> Result<Value> {
    member(subject).await?;
    let them = profile_by_handle(handle).await?;
    if them.subject == subject {
        return Err(Error::invalid("You can't follow yourself."));
    }
    let n: i64 = sqlx::query_scalar("select count(*) from social_follows where follower = $1").bind(subject).fetch_one(db()).await?;
    if n >= 5000 {
        return Err(Error::rate_limited("You follow 5,000 profiles, the limit."));
    }
    sqlx::query("insert into social_follows (follower, followee) values ($1, $2) on conflict do nothing").bind(subject).bind(&them.subject).execute(db()).await?;
    profile_view(&them, Some(subject)).await
}

pub async fn unfollow(subject: &str, handle: &str) -> Result<Value> {
    member(subject).await?;
    let them = profile_by_handle(handle).await?;
    sqlx::query("delete from social_follows where follower = $1 and followee = $2").bind(subject).bind(&them.subject).execute(db()).await?;
    profile_view(&them, Some(subject)).await
}

/// Who follows `handle` ("followers") or whom it follows ("following"), newest first.
pub async fn follow_list(viewer: &str, handle: &str, which: &str, before: Option<&str>) -> Result<Value> {
    member(viewer).await?;
    let p = profile_by_handle(handle).await?;
    let (col, other) = if which == "following" { ("follower", "followee") } else { ("followee", "follower") };
    let before = before.and_then(|b| chrono::DateTime::parse_from_rfc3339(b).ok()).map(|d| d.with_timezone(&Utc));
    let rows: Vec<(SocialProfile, chrono::DateTime<Utc>)> = sqlx::query_as::<_, SocialProfile>(&format!(
        "select s.* from social_follows f join social_profiles s on s.subject = f.{other}
         where f.{col} = $1 and not s.suspended and ($2::timestamptz is null or f.created_at < $2) order by f.created_at desc limit $3"
    ))
    .bind(&p.subject)
    .bind(before)
    .bind(PAGE)
    .fetch_all(db())
    .await?
    .into_iter()
    .map(|s| (s.clone(), s.created_at))
    .collect();
    let mut out = vec![];
    for (s, _) in &rows {
        out.push(profile_view(s, Some(viewer)).await?);
    }
    Ok(json!({ "profiles": out }))
}

// ---------- Feeds ----------

/// "following" (people you follow, and you) or "latest" (everyone); top-level posts and reposts.
pub async fn feed(viewer: &str, kind: &str, before: Option<i64>) -> Result<Value> {
    member(viewer).await?;
    let following = kind != "latest";
    let posts: Vec<SocialPost> = sqlx::query_as(
        "select p.* from social_posts p
         where p.deleted_at is null and p.reply_to is null and ($2::bigint is null or p.id < $2)
           and (not $3 or p.author = $1 or p.author in (select followee from social_follows where follower = $1))
           and not exists (select 1 from social_profiles s where s.subject = p.author and s.suspended)
         order by p.id desc limit $4",
    )
    .bind(viewer)
    .bind(before)
    .bind(following)
    .bind(PAGE)
    .fetch_all(db())
    .await?;
    page(posts, viewer).await
}

async fn page(posts: Vec<SocialPost>, viewer: &str) -> Result<Value> {
    let next = (posts.len() as i64 == PAGE).then(|| posts.last().map(|p| p.id.to_string())).flatten();
    Ok(json!({ "posts": post_views(&posts, Some(viewer)).await?, "next": next }))
}

/// A profile's posts and reposts ("posts") or its comments ("replies").
pub async fn profile_posts(viewer: &str, handle: &str, tab: &str, before: Option<i64>) -> Result<Value> {
    member(viewer).await?;
    let p = profile_by_handle(handle).await?;
    let replies = tab == "replies";
    let posts: Vec<SocialPost> = sqlx::query_as(
        "select * from social_posts where author = $1 and deleted_at is null and ($2::bigint is null or id < $2)
           and (case when $3 then reply_to is not null else reply_to is null end)
         order by id desc limit $4",
    )
    .bind(&p.subject)
    .bind(before)
    .bind(replies)
    .bind(PAGE)
    .fetch_all(db())
    .await?;
    page(posts, viewer).await
}

/// A post with what it replies to (up to 10 up the chain) and its comments (oldest first).
pub async fn thread(viewer: &str, id: i64, after: Option<i64>) -> Result<Value> {
    member(viewer).await?;
    let post: SocialPost = sqlx::query_as("select * from social_posts where id = $1").bind(id).fetch_optional(db()).await?.ok_or_else(|| Error::not_found("No such post."))?;
    let mut chain = vec![];
    let mut up = post.reply_to;
    while let (Some(pid), true) = (up, chain.len() < 10) {
        let Some(parent) = sqlx::query_as::<_, SocialPost>("select * from social_posts where id = $1").bind(pid).fetch_optional(db()).await? else { break };
        up = parent.reply_to;
        chain.push(parent);
    }
    chain.reverse();
    let replies: Vec<SocialPost> = sqlx::query_as("select * from social_posts where reply_to = $1 and deleted_at is null and ($2::bigint is null or id > $2) order by id limit 100").bind(id).bind(after).fetch_all(db()).await?;
    Ok(json!({
        "post": post_views(&[post], Some(viewer)).await?.remove(0),
        "ancestors": post_views(&chain, Some(viewer)).await?,
        "replies": post_views(&replies, Some(viewer)).await?,
    }))
}

/// Profiles whose handle or name starts with `q` (for search and @mentions).
pub async fn search(viewer: &str, q: &str) -> Result<Value> {
    member(viewer).await?;
    let q = q.trim().trim_start_matches('@').to_lowercase();
    if q.is_empty() {
        return Ok(json!({ "profiles": [] }));
    }
    let like = format!("{}%", q.replace('%', "").replace('_', "\\_"));
    let rows: Vec<SocialProfile> = sqlx::query_as("select * from social_profiles where not suspended and (handle like $1 or lower(display_name) like $1) order by length(handle) limit 20").bind(like).fetch_all(db()).await?;
    let mut out = vec![];
    for r in &rows {
        out.push(profile_view(r, Some(viewer)).await?);
    }
    Ok(json!({ "profiles": out }))
}

// ---------- Images ----------

fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.len() > 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else {
        None
    }
}

/// Store an image a member will attach. The type comes from its bytes, never from the client.
pub async fn upload_media(subject: &str, bytes: &[u8]) -> Result<Value> {
    member(subject).await?;
    if bytes.is_empty() || bytes.len() > MEDIA_MAX_BYTES {
        return Err(Error::invalid(format!("Images are up to {:.1} MB.", MEDIA_MAX_BYTES as f64 / 1e6)));
    }
    let mime = sniff(bytes).ok_or_else(|| Error::invalid("Images must be PNG, JPEG, WebP or GIF."))?;
    let n: i64 = sqlx::query_scalar("select count(*) from social_media where owner = $1 and created_at > now() - interval '24 hours'").bind(subject).fetch_one(db()).await?;
    if n >= DAILY_MEDIA {
        return Err(Error::rate_limited(format!("That's {DAILY_MEDIA} images in 24 hours, the limit.")));
    }
    let id = hex::encode(rand::random::<[u8; 16]>());
    sqlx::query("insert into social_media (id, owner, mime, bytes) values ($1, $2, $3, $4)").bind(&id).bind(subject).bind(mime).bind(bytes).execute(db()).await?;
    Ok(json!({ "id": id, "url": media_url(&id), "mime": mime }))
}

/// An image's type and bytes.
pub async fn media(id: &str) -> Result<(String, Vec<u8>)> {
    if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::not_found("No such image."));
    }
    sqlx::query_as("select mime, bytes from social_media where id = $1").bind(id).fetch_optional(db()).await?.ok_or_else(|| Error::not_found("No such image."))
}
