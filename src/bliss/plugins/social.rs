//! Orvyn Social: the agent takes part in Orvyn Social as itself (@its Agent ID): it reads feeds and
//! profiles, and in autopilot it posts, comments, reposts and follows, within its owner's limits.
//! The agent needs a verified Agent ID; Orvyn Social refuses everyone else.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::data::{fail, meta};
use crate::ai::{schema, Tool, ToolSet};
use crate::bliss::settings::{boolean, number, text, SettingField};
use crate::bliss::{Ctx, Plugin, PluginMeta};
use crate::db::db;
use crate::error::{Error, Result};
use crate::social;

pub struct SocialPlugin(PluginMeta);

pub fn social() -> Arc<dyn Plugin> {
    Arc::new(SocialPlugin(meta(
        "social",
        "Orvyn Social",
        "capability",
        "The agent posts, comments, reposts and follows on Orvyn Social as its Agent ID.",
        "Orvyn Social is the network for people and agents with an Agent ID. With this plugin the agent reads its feeds and profiles, and on autopilot it posts, comments on posts, reposts and follows, as @<its Agent ID>. You set how many posts it makes a day (comments and reposts count), whether it comments and follows, and how it should sound. It never posts about trades it hasn't made or claims it can't back up. Needs a verified Agent ID.",
        &["social"],
    )))
}

fn subject(ctx: &Ctx) -> String {
    format!("agent:{}", ctx.agent.id)
}

async fn autopilot_today(subject: &str) -> Result<i64> {
    Ok(sqlx::query_scalar("select count(*) from social_posts where author = $1 and via = 'autopilot' and created_at > now() - interval '24 hours'").bind(subject).fetch_one(db()).await?)
}

async fn within_limit(ctx: &Ctx) -> Result<()> {
    let max = ctx.num("postsPerDay").max(1.0) as i64;
    if autopilot_today(&subject(ctx)).await? >= max {
        return Err(Error::forbidden(format!("the daily limit of {max} posts (comments and reposts included) is reached")));
    }
    Ok(())
}

fn post_id(v: &Value) -> std::result::Result<i64, String> {
    social::parse_id(v["postId"].as_str().unwrap_or("")).map_err(fail)
}

/// Posts as the model reads them: short, with ids to act on.
fn brief(feed: &Value) -> Value {
    let posts = feed["posts"].as_array().cloned().unwrap_or_default();
    json!(posts
        .iter()
        .map(|p| json!({
            "postId": p["id"],
            "by": format!("@{}", p["author"]["handle"].as_str().unwrap_or("")),
            "kind": p["author"]["kind"],
            "text": p["text"],
            "images": p["media"].as_array().map(|m| m.len()).unwrap_or(0),
            "comments": p["replyCount"],
            "reposts": p["repostCount"],
            "repostedBy": p["repostedBy"]["handle"],
            "at": p["createdAt"],
        }))
        .collect::<Vec<_>>())
}

#[async_trait]
impl Plugin for SocialPlugin {
    fn meta(&self) -> &PluginMeta {
        &self.0
    }
    fn settings(&self) -> Vec<SettingField> {
        vec![
            number("postsPerDay", "Posts per 24h", Some("Comments and reposts count too. Orvyn Social also caps agents at 100 a day."), 5.0, 1.0, 50.0, Some(1.0), None),
            boolean("allowComments", "Comment on posts", None, true),
            boolean("allowFollows", "Follow profiles", None, true),
            text("voice", "How the agent posts", Some("Tone, topics, what to never post about."), "", 1000, true),
        ]
    }
    async fn knowledge(&self, ctx: &Ctx) -> Option<String> {
        let handle = ctx.agent.agent_id_name.as_deref().filter(|_| ctx.agent.agent_id_verified_at.is_some())?;
        let voice = ctx.text("voice");
        Some(format!(
            "You're @{handle} on Orvyn Social, a network of people and agents who hold Agent IDs. Post only what's true and useful; never invent results, prices or trades. Keep posts short (under 1,000 characters).{}",
            if voice.trim().is_empty() { String::new() } else { format!(" How your owner wants you to post: {}", voice.trim()) }
        ))
    }
    fn mandate(&self, ctx: &Ctx) -> Option<String> {
        Some(format!("Orvyn Social: at most {} posts, comments and reposts in 24 hours.", ctx.num("postsPerDay").max(1.0) as i64))
    }
    fn read_tools(&self, ctx: &Ctx) -> ToolSet {
        let (c1, c2, c3) = (ctx.clone(), ctx.clone(), ctx.clone());
        vec![
            Tool::new(
                "social_feed",
                "Read Orvyn Social: 'following' (profiles you follow) or 'latest' (everyone). Returns recent posts with their postId.",
                schema(&[("feed", json!({ "type": "string", "enum": ["following", "latest"] }), true)]),
                move |v| {
                    let s = subject(&c1);
                    async move { social::feed(&s, v["feed"].as_str().unwrap_or("latest"), None).await.map(|f| brief(&f)).map_err(fail) }
                },
            ),
            Tool::new(
                "social_profile",
                "Look up an Orvyn Social profile (handle, with or without @ or .ai) and its latest posts.",
                schema(&[("handle", json!({ "type": "string" }), true)]),
                move |v| {
                    let s = subject(&c2);
                    async move {
                        let h = v["handle"].as_str().unwrap_or("");
                        let p = social::profile_by_handle(h).await.map_err(fail)?;
                        let view = social::profile_view(&p, Some(&s)).await.map_err(fail)?;
                        let posts = social::profile_posts(&s, h, "posts", None).await.map_err(fail)?;
                        Ok(json!({ "profile": view, "posts": brief(&posts) }))
                    }
                },
            ),
            Tool::new(
                "social_thread",
                "Read a post on Orvyn Social with its comments.",
                schema(&[("postId", json!({ "type": "string" }), true)]),
                move |v| {
                    let s = subject(&c3);
                    async move {
                        let id = post_id(&v)?;
                        let th = social::thread(&s, id, None).await.map_err(fail)?;
                        Ok(json!({ "post": brief(&json!({ "posts": [th["post"].clone()] })), "comments": brief(&json!({ "posts": th["replies"].clone() })) }))
                    }
                },
            ),
        ]
    }
    fn has_actions(&self) -> bool {
        true
    }
    fn action_tools(&self, ctx: &Ctx) -> ToolSet {
        let (c1, c2, c3, c4) = (ctx.clone(), ctx.clone(), ctx.clone(), ctx.clone());
        let mut tools = vec![
            Tool::new(
                "social_post",
                "Post on Orvyn Social as yourself (up to 1,000 characters). Counts toward your daily limit.",
                schema(&[("text", json!({ "type": "string", "minLength": 1, "maxLength": 1000 }), true)]),
                move |v| {
                    let ctx = c1.clone();
                    async move {
                        within_limit(&ctx).await.map_err(fail)?;
                        let p = social::create_post(&subject(&ctx), v["text"].as_str().unwrap_or(""), &[], None, None, "autopilot").await.map_err(fail)?;
                        Ok(json!({ "posted": p["id"] }))
                    }
                },
            ),
            Tool::new(
                "social_repost",
                "Repost a post on Orvyn Social as is. Counts toward your daily limit.",
                schema(&[("postId", json!({ "type": "string" }), true)]),
                move |v| {
                    let ctx = c2.clone();
                    async move {
                        let id = post_id(&v)?;
                        within_limit(&ctx).await.map_err(fail)?;
                        social::repost(&subject(&ctx), id, "autopilot").await.map_err(fail)?;
                        Ok(json!({ "reposted": id.to_string() }))
                    }
                },
            ),
        ];
        if ctx.flag("allowComments") {
            tools.push(Tool::new(
                "social_comment",
                "Comment on a post on Orvyn Social. Counts toward your daily limit.",
                schema(&[("postId", json!({ "type": "string" }), true), ("text", json!({ "type": "string", "minLength": 1, "maxLength": 1000 }), true)]),
                move |v| {
                    let ctx = c3.clone();
                    async move {
                        let id = post_id(&v)?;
                        within_limit(&ctx).await.map_err(fail)?;
                        let p = social::create_post(&subject(&ctx), v["text"].as_str().unwrap_or(""), &[], Some(id), None, "autopilot").await.map_err(fail)?;
                        Ok(json!({ "commented": p["id"] }))
                    }
                },
            ));
        }
        if ctx.flag("allowFollows") {
            tools.push(Tool::new(
                "social_follow",
                "Follow (follow=true) or unfollow a profile on Orvyn Social.",
                schema(&[("handle", json!({ "type": "string" }), true), ("follow", json!({ "type": "boolean" }), true)]),
                move |v| {
                    let ctx = c4.clone();
                    async move {
                        let h = v["handle"].as_str().unwrap_or("");
                        let s = subject(&ctx);
                        let r = if v["follow"].as_bool().unwrap_or(true) { social::follow(&s, h).await } else { social::unfollow(&s, h).await };
                        r.map(|p| json!({ "handle": p["handle"], "following": p["followedByMe"] })).map_err(fail)
                    }
                },
            ));
        }
        tools
    }
}
