//! Plugins that give the agent work of its own: Orvyn Mail (it answers email), "Hire me" (it takes
//! paid marketplace jobs) and Game studio (it makes and publishes games).

use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use regex::Regex;
use serde_json::{json, Value};

use super::data::meta;
use crate::agent::hire_session::run_hire_session;
use crate::agent::mail_session::{run_mail_session, MailSettings};
use crate::ai::{schema, Tool, ToolSet};
use crate::bliss::settings::{boolean, list, number, select, text, SettingField, Settings};
use crate::bliss::{Ctx, Installed, JobFuture, Plugin, PluginJob, PluginMeta};
use crate::db::{log_agent, AgentLogKind};
use crate::error::Kind;
use crate::games;
use crate::hires::{self, offer_of, HIRE_PLUGIN_ID};
use crate::tiers::{category_blocked, job_category, Tier, JOB_CATEGORIES};

fn app_link(path: &str) -> String {
    let base = std::env::var("ORVYN_APP_URL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "https://app.orvyn.cash".into());
    format!("{}{path}", base.trim_end_matches('/'))
}

fn js(n: f64) -> String {
    crate::auth::js_number(n)
}

// ---------- Orvyn Mail ----------

pub struct MailPlugin(PluginMeta);

pub fn mail() -> Arc<dyn Plugin> {
    Arc::new(MailPlugin(meta(
        "mail",
        "Orvyn Mail",
        "capability",
        "The agent answers email at its own @orvyn.cash address.",
        "Every agent has an address, <handle>@orvyn.cash. With this plugin the agent reads new email every minute and replies on your model, using the same read-only tools as holder chat. It never gets trading or spending tools from email. By default it only replies to people who wrote first; you can add contacts it may email on its own, cap how much it sends, and restrict which senders it answers.",
        &["email"],
    )))
}

static DOMAIN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^[a-z0-9-]+(\.[a-z0-9-]+)+$").unwrap());

fn mail_settings(config: &Settings) -> Option<MailSettings> {
    serde_json::from_value(Value::Object(config.clone())).ok()
}

fn inbox_job(ctx: Ctx) -> JobFuture {
    Box::pin(async move {
        let Some(settings) = mail_settings(&ctx.config) else { return Ok(()) };
        run_mail_session(ctx.agent.clone(), ctx.installed.clone(), &settings).await.map(|_| ())
    })
}

#[async_trait]
impl Plugin for MailPlugin {
    fn meta(&self) -> &PluginMeta {
        &self.0
    }
    fn settings(&self) -> Vec<SettingField> {
        vec![
            boolean("autoReply", "Answer new email automatically", Some("Off: mail still arrives and you can read and answer it yourself."), true),
            select("sendMode", "Who the agent may email", None, "reply", &[("reply", "Only people who wrote first (replies)"), ("contacts", "Replies, plus the contacts below")]),
            list("contacts", "Contacts", Some("Addresses (alice@example.com) or whole domains (example.com) the agent may start emails to."), vec![], 50, false),
            list("allowedSenders", "Only answer these senders", Some("Addresses or domains. Empty: answer anyone. Other mail stays in the inbox for you."), vec![], 50, false),
            number("dailySendLimit", "Emails per 24h", Some("Orvyn also applies a platform cap per agent."), 20.0, 1.0, 100.0, Some(1.0), None),
            text("instructions", "How to handle email", Some("e.g. who to help, what to never discuss, when to stay silent."), "", 2000, true),
            text("signature", "Signature", None, "", 300, true),
        ]
    }
    fn validate(&self, config: &Settings, _others: &[Installed], _tier: Tier) -> Option<String> {
        let entries = |k: &str| config.get(k).and_then(Value::as_array).cloned().unwrap_or_default();
        entries("contacts").into_iter().chain(entries("allowedSenders")).filter_map(|e| e.as_str().map(String::from)).find_map(|e| {
            let v = e.trim().trim_start_matches('@');
            let bad = if v.contains('@') { crate::mail::normalize_email(v).is_none() } else { !DOMAIN.is_match(v) };
            bad.then(|| format!("\"{e}\" isn't an email address or a domain."))
        })
    }
    async fn knowledge(&self, ctx: &Ctx) -> Option<String> {
        crate::mail::agent_address(ctx.agent.handle.as_deref()).map(|a| format!("Your email address is {a}. People can write to you there; you answer email in separate sessions."))
    }
    fn jobs(&self) -> Vec<PluginJob> {
        vec![PluginJob { id: "inbox", every: |_| 60.0, run: inbox_job }]
    }
}

// ---------- Hire me ----------

pub struct HirePlugin(PluginMeta);

pub fn hire() -> Arc<dyn Plugin> {
    let mut m = meta(
        HIRE_PLUGIN_ID,
        "Hire me",
        "capability",
        "List the agent on the marketplace: anyone can hire it for a paid one-off job.",
        "Offer one service, e.g. a token due-diligence report, a diagram set or a motion graphic, at a fixed price in USDC or USDT. Clients pay into Orvyn's escrow contract. The agent works with read-only tools (no trading or spending) and, for creative work, renders files in Orvyn Studio, which checks them before they're delivered. Video, motion and UI work needs Pro hosting; 3D and AI video need Studio. The client approves the result, or the payment releases automatically after the review window. Late, declined or failed jobs are refunded in full. Orvyn takes 10% of each paid job.",
        &["jobs"],
    );
    m.version = "1.1.0".into();
    Arc::new(HirePlugin(m))
}

/// Category options, labelled with the tier each needs (made once, kept for the process).
static CATEGORY_OPTIONS: LazyLock<Vec<(&'static str, &'static str)>> = LazyLock::new(|| {
    JOB_CATEGORIES
        .iter()
        .map(|c| {
            let label: &'static str = if c.min_tier == Tier::Basic { c.name } else { Box::leak(format!("{} ({})", c.name, c.min_tier.spec().name).into_boxed_str()) };
            (c.id, label)
        })
        .collect()
});

fn jobs_job(ctx: Ctx) -> JobFuture {
    Box::pin(async move {
        let offer = offer_of(&Value::Object(ctx.config.clone()));
        for hire in hires::hires_needing_agent(&ctx.agent.id, 3).await? {
            run_hire_session(ctx.agent.clone(), ctx.installed.clone(), &hire, &offer).await?;
        }
        Ok(())
    })
}

#[async_trait]
impl Plugin for HirePlugin {
    fn meta(&self) -> &PluginMeta {
        &self.0
    }
    fn settings(&self) -> Vec<SettingField> {
        vec![
            select("category", "Kind of work", Some("Video, motion and UI builds need Pro hosting; 3D and AI video need Studio."), "research", &CATEGORY_OPTIONS),
            text("offerTitle", "Service", None, "Token due-diligence report", 80, false),
            text(
                "offerDescription",
                "What the client gets",
                None,
                "Send me a token or project. I'll check its liquidity, holders, contract risks and price action, and deliver a written report with the data behind it.",
                1500,
                true,
            ),
            number("priceUsd", "Price", None, 25.0, 1.0, 10_000.0, Some(1.0), Some("USD")),
            number("deliveryHours", "Delivery time", Some("Unpaid if not delivered in time: the client is refunded."), 24.0, 1.0, 336.0, Some(1.0), Some("hours")),
            number("maxOpenJobs", "Jobs at once", Some("The agent shows as fully booked beyond this."), 3.0, 1.0, 20.0, Some(1.0), None),
            text("instructions", "How to do the job", Some("Private: seen only by the agent."), "", 2000, true),
        ]
    }
    fn validate(&self, config: &Settings, others: &[Installed], tier: Tier) -> Option<String> {
        let offer = offer_of(&Value::Object(config.clone()));
        if let Some(craft) = crate::tiers::required_craft(&offer.category) {
            if !others.iter().any(|p| p.enabled && p.plugin.meta().id == craft) {
                let name = super::crafts::craft(craft).map(|c| c.name).unwrap_or(craft);
                return Some(format!("Install the {name} craft pack first: this kind of work needs it."));
            }
        }
        if offer.title.trim().chars().count() < 3 {
            return Some("Give the service a name.".into());
        }
        if offer.description.trim().chars().count() < 20 {
            return Some("Describe what the client gets.".into());
        }
        category_blocked(tier, &offer.category)
    }
    async fn before_uninstall(&self, ctx: &Ctx) -> Option<String> {
        match hires::open_hire_count(&ctx.agent.id).await {
            Ok(0) => None,
            Ok(_) => Some("The agent has open jobs. Let them finish or decline them first.".into()),
            Err(e) => Some(e.message),
        }
    }
    async fn knowledge(&self, ctx: &Ctx) -> Option<String> {
        let offer = offer_of(&Value::Object(ctx.config.clone()));
        let c = job_category(&offer.category);
        let blocked = category_blocked(Tier::parse(&ctx.agent.tier), &offer.category).map(|b| format!(" It's paused: {b}")).unwrap_or_default();
        let packs = super::crafts::crafts_for_category(&offer.category);
        let have: Vec<&str> = packs.iter().filter(|p| ctx.installed.iter().any(|i| i.enabled && i.plugin.meta().id == p.id)).map(|p| p.name).collect();
        let craft = if have.is_empty() {
            if packs.is_empty() { String::new() } else { format!(" Craft packs that would help with this work aren't installed ({}); suggest them to your owner.", packs.iter().map(|p| p.name).collect::<Vec<_>>().join(", ")) }
        } else {
            format!(" Use your craft packs ({}) for this work: read their guides before you start a job.", have.join(", "))
        };
        Some(format!(
            "You're listed on the Orvyn marketplace offering \"{}\" ({}) for ${} (delivered within {}h).{blocked}{craft} If people ask about hiring you, point them to your marketplace page. Jobs are handled in separate sessions.",
            offer.title,
            c.map(|c| c.name).unwrap_or(&offer.category),
            js(offer.price_usd),
            js(offer.delivery_hours)
        ))
    }
    fn jobs(&self) -> Vec<PluginJob> {
        vec![PluginJob { id: "jobs", every: |_| 60.0, run: jobs_job }]
    }
}

// ---------- Game studio ----------

pub struct GameStudio(PluginMeta);

pub fn game_studio() -> Arc<dyn Plugin> {
    Arc::new(GameStudio(meta(
        "game-studio",
        "Game studio",
        "capability",
        "The agent makes its own games with Orvyn Engine and publishes them under its name.",
        "The agent designs, builds and checks 2D games (Orvyn Engine: rules, behaviours, Rhai scripts) in copilot. Games belong to you and appear under the agent's name; sales and tips go to the agent's wallet. By default they wait as drafts in Games → My games for you to publish; turn on \"Publish without asking\" to let the agent publish (and update) them itself.",
        &["publish"],
    )))
}

/// The agent's own games (it made them, it publishes as itself).
async fn agent_games(agent_id: &str) -> crate::Result<Vec<crate::db::Game>> {
    games::list_games(games::ListGames { agent_id: Some(agent_id), include_drafts: true, limit: Some(200), creator: None }).await
}

fn game_summary(c: &games::GameCheck) -> Value {
    json!({ "ok": c.ok, "problems": c.problems, "missingFiles": c.missing_assets, "headless": c.smoke.as_ref().map(|s| json!({ "frames": s["frames"], "status": s["status"] })), "playtest": c.playtest })
}

#[async_trait]
impl Plugin for GameStudio {
    fn meta(&self) -> &PluginMeta {
        &self.0
    }
    fn settings(&self) -> Vec<SettingField> {
        vec![
            boolean("autoPublish", "Publish without asking", Some("Off: games wait as drafts for you to publish."), false),
            select("access", "How its games are played", None, "free", &[("free", "Free"), ("paid", "Paid (price below)"), ("token", "Holders of the agent's token")]),
            number("priceUsd", "Price of paid games", None, 2.0, 0.5, 100.0, Some(0.5), Some("USD")),
            number("minTokens", "Tokens to hold (token-gated)", None, 1000.0, 1.0, 1_000_000_000.0, Some(1.0), None),
            boolean("tips", "Accept tips", None, true),
            number("maxGames", "Games at most", None, 10.0, 1.0, 100.0, Some(1.0), None),
            text("brief", "What kind of games", Some("Themes, genres, audience. Seen only by the agent."), "", 1500, true),
        ]
    }
    async fn knowledge(&self, ctx: &Ctx) -> Option<String> {
        let list = agent_games(&ctx.agent.id).await.unwrap_or_default();
        let lines: Vec<String> = list
            .iter()
            .map(|g| format!("- {} ({}{}, {} plays): {}", g.title, g.status, if g.version > 0 { format!(", v{}", g.version) } else { String::new() }, g.plays, app_link(&format!("/games/{}", g.slug))))
            .collect();
        let auto = ctx.flag("autoPublish");
        let brief = ctx.text("brief");
        Some(format!(
            "You make games with Orvyn Engine (tools: game_check, game_save{}). {} At most {} games.{}\n{}",
            if auto { ", game_publish" } else { "" },
            if auto { "You may publish them yourself." } else { "They wait as drafts for your creator to publish." },
            js(ctx.num("maxGames")),
            if brief.is_empty() { String::new() } else { format!("\nYour creator wants: {brief}") },
            if lines.is_empty() { "You haven't made a game yet.".to_string() } else { format!("Your games:\n{}", lines.join("\n")) }
        ))
    }
    fn read_tools(&self, ctx: &Ctx) -> ToolSet {
        let id = ctx.agent.id.clone();
        vec![Tool::new("my_games", "The games this agent has made, with links, status and plays.", schema(&[]), move |_| {
            let id = id.clone();
            async move {
                let list = agent_games(&id).await.map_err(|e| e.message)?;
                Ok(json!(list.iter().map(|g| json!({ "title": g.title, "slug": g.slug, "status": g.status, "version": g.version, "plays": g.plays, "access": g.access, "link": app_link(&format!("/games/{}", g.slug)) })).collect::<Vec<_>>()))
            }
        })]
    }
    fn has_actions(&self) -> bool {
        true
    }
    fn action_tools(&self, ctx: &Ctx) -> ToolSet {
        let (c1, c2) = (ctx.clone(), ctx.clone());
        let mut tools = vec![
            Tool::new(
                "game_check",
                &format!("Check an Orvyn Engine game: the engine validates the file, plays 10 seconds headless, then bots (idle, explorer, random) play 30 seconds each and report what a player would notice. Fix every problem before saving; treat playtest findings as advice, but fix players falling out of the level, getting stuck, scores that never change and games that never end.\n\n{}", games::GAME_GUIDE),
                schema(&[("game", json!({ "type": "object", "additionalProperties": {} }), true)]),
                |v| async move {
                    Ok(match games::check_game(&v["game"], &[]).await {
                        Ok(c) => game_summary(&c),
                        Err(e) => json!({ "ok": false, "error": if e.kind == Kind::Unavailable { e.message } else { "The engine couldn't check it.".into() } }),
                    })
                },
            ),
            Tool::new(
                "game_save",
                "Save a game: a new one, or an update to one of yours (give its slug). It's owned by your creator and shown under your name.",
                schema(&[("game", json!({ "type": "object", "additionalProperties": {} }), true), ("slug", json!({ "type": "string", "maxLength": 60 }), false)]),
                move |v| {
                    let ctx = c1.clone();
                    async move {
                        let agent = ctx.agent.clone();
                        let r: crate::Result<Value> = async {
                            let (game, check) = match v["slug"].as_str() {
                                Some(slug) => {
                                    let Some(existing) = games::get_game(slug).await?.filter(|g| g.agent_id.as_deref() == Some(agent.id.as_str())) else {
                                        return Ok(json!({ "saved": false, "error": "Not one of your games." }));
                                    };
                                    games::save_draft(&existing, &v["game"]).await?
                                }
                                None => {
                                    let max = ctx.num("maxGames");
                                    if agent_games(&agent.id).await?.len() as f64 >= max {
                                        return Ok(json!({ "saved": false, "error": format!("You've made {} games, the most your creator allows. Update one instead.", js(max)) }));
                                    }
                                    let (game, check) = games::create_game(games::NewGame { creator_account_id: &agent.owner_account_id, draft: &v["game"], agent_id: Some(&agent.id), hire_id: None, publish_as_agent: true }).await?;
                                    let tips = ctx.flag("tips");
                                    // Token-gating needs the agent's token; without one its games are free.
                                    let access = match ctx.text("access").as_str() {
                                        "paid" => json!({ "kind": "paid", "priceUsd": ctx.num("priceUsd"), "tips": tips }),
                                        "token" if agent.token_address.is_some() => json!({ "kind": "token", "tokenAgentId": agent.id, "minTokens": ctx.num("minTokens"), "tips": tips }),
                                        _ => json!({ "kind": "free", "tips": tips }),
                                    };
                                    (games::set_access(&game, &access, Some(true)).await?, check)
                                }
                            };
                            let state = if check.ok { "passes its checks".to_string() } else { format!("{} problems", check.problems.len()) };
                            log_agent(&agent.id, AgentLogKind::Plugin, &format!("Saved game \"{}\" ({state})", game.title)).await?;
                            let mut out = json!({ "saved": true, "slug": game.slug, "edit": app_link(&format!("/games/{}/edit", game.slug)) });
                            if let (Some(o), Value::Object(s)) = (out.as_object_mut(), game_summary(&check)) {
                                o.extend(s);
                            }
                            Ok(out)
                        }
                        .await;
                        Ok(match r {
                            Ok(v) => v,
                            Err(e) if e.kind == Kind::Invalid => json!({ "saved": false, "error": e.message }),
                            Err(_) => json!({ "saved": false, "error": "Couldn't save the game." }),
                        })
                    }
                },
            ),
        ];
        if ctx.flag("autoPublish") {
            tools.push(Tool::new(
                "game_publish",
                "Publish one of your saved games (a new version if it was published before). It must pass every check.",
                schema(&[("slug", json!({ "type": "string", "maxLength": 60 }), true)]),
                move |v| {
                    let agent = c2.agent.clone();
                    async move {
                        let g = games::get_game(v["slug"].as_str().unwrap_or("")).await.map_err(|e| e.message)?;
                        let Some(g) = g.filter(|g| g.agent_id.as_deref() == Some(agent.id.as_str())) else { return Ok(json!({ "published": false, "error": "Not one of your games." })) };
                        Ok(match games::publish_game(&g).await {
                            Ok(p) => {
                                let link = app_link(&format!("/games/{}", p.slug));
                                let _ = log_agent(&agent.id, AgentLogKind::Plugin, &format!("Published game \"{}\" v{}: {link}", p.title, p.version)).await;
                                json!({ "published": true, "version": p.version, "link": link })
                            }
                            Err(e) if e.kind == Kind::Invalid => json!({ "published": false, "error": e.message }),
                            Err(_) => json!({ "published": false, "error": "Couldn't publish." }),
                        })
                    }
                },
            ));
        }
        tools
    }
}
