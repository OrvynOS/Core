//! Bliss: the Orvyn plugin library. Plugins give an agent its capabilities (what it can do with its
//! funds), data (what it can look up) and knowledge (what it knows). Nothing is on by default.

pub mod exits;
pub mod external;
pub mod mcp;
pub mod plugins;
pub mod prices;
pub mod settings;
pub mod treasury;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Map, Value};

use crate::ai::{Tool, ToolSet};
use crate::db::{db, log_agent, Agent, AgentLogKind, AgentPlugin};
use crate::error::{Error, Result};
use crate::tiers::Tier;
pub use settings::{defaults_for, parse_settings, SettingField, Settings};

/// A plugin's refusal (invalid settings, a clash), shown to the creator.
pub fn plugin_error(m: impl Into<String>) -> Error {
    Error::invalid(m)
}

#[derive(Clone)]
pub struct Installed {
    pub plugin: Arc<dyn Plugin>,
    pub config: Settings,
    pub state: Value,
    pub enabled: bool,
}

/// What a plugin's hooks see.
#[derive(Clone)]
pub struct Ctx {
    pub agent: Arc<Agent>,
    pub config: Settings,
    pub state: Value,
    /// Every plugin installed on the agent, including this one.
    pub installed: Arc<Vec<Installed>>,
}

impl Ctx {
    pub fn num(&self, key: &str) -> f64 {
        self.config.get(key).and_then(Value::as_f64).unwrap_or(0.0)
    }
    pub fn flag(&self, key: &str) -> bool {
        self.config.get(key).and_then(Value::as_bool).unwrap_or(false)
    }
    pub fn text(&self, key: &str) -> String {
        self.config.get(key).and_then(Value::as_str).unwrap_or("").to_string()
    }
    pub fn list(&self, key: &str) -> Vec<String> {
        self.config.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default()
    }
}

pub type JobFuture = Pin<Box<dyn Future<Output = Result<()>> + Send>>;

/// Scheduled work run by the runtime worker.
pub struct PluginJob {
    pub id: &'static str,
    /// Seconds between runs.
    pub every: fn(&Ctx) -> f64,
    pub run: fn(Ctx) -> JobFuture,
}

pub struct PluginMeta {
    pub id: String,
    pub name: String,
    pub version: String,
    pub author: String,
    /// "capability" | "data" | "knowledge".
    pub category: &'static str,
    pub summary: String,
    pub description: String,
    pub permissions: Vec<&'static str>,
}

#[async_trait]
pub trait Plugin: Send + Sync {
    fn meta(&self) -> &PluginMeta;
    fn settings(&self) -> Vec<SettingField> {
        vec![]
    }
    /// Percent of income reserved for this plugin's budget.
    fn fee_share(&self, _config: &Settings) -> Option<f64> {
        None
    }
    /// Cross-field or cross-plugin checks on install/update; an error message rejects.
    fn validate(&self, _config: &Settings, _others: &[Installed], _tier: Tier) -> Option<String> {
        None
    }
    /// Refuse to uninstall while it would strand funds or positions.
    async fn before_uninstall(&self, _ctx: &Ctx) -> Option<String> {
        None
    }
    /// Knowledge added to the agent's system prompt (chat and autopilot).
    async fn knowledge(&self, _ctx: &Ctx) -> Option<String> {
        None
    }
    /// Hard limits the autopilot must respect, stated in its prompt (enforced in code too).
    fn mandate(&self, _ctx: &Ctx) -> Option<String> {
        None
    }
    /// Read-only tools, for holder chat and autopilot.
    fn read_tools(&self, _ctx: &Ctx) -> ToolSet {
        vec![]
    }
    /// Whether it has tools that act (autopilot only).
    fn has_actions(&self) -> bool {
        false
    }
    fn action_tools(&self, _ctx: &Ctx) -> ToolSet {
        vec![]
    }
    fn jobs(&self) -> Vec<PluginJob> {
        vec![]
    }
    /// Where its read tools are offered.
    fn surfaces(&self, _config: &Settings) -> Vec<&'static str> {
        vec!["chat", "autopilot"]
    }
    /// Set for third-party plugins.
    fn third_party(&self) -> Option<Value> {
        None
    }
}

/// The first-party library.
pub fn first_party() -> &'static [Arc<dyn Plugin>] {
    static LIB: std::sync::LazyLock<Vec<Arc<dyn Plugin>>> = std::sync::LazyLock::new(plugins::all);
    &LIB
}

/// A plugin by id (third-party ones once `external::load_external()` has run).
pub fn get_plugin(id: &str) -> Result<Arc<dyn Plugin>> {
    first_party().iter().find(|p| p.meta().id == id).cloned().or_else(|| external::cached_external(id)).ok_or_else(|| plugin_error(format!("Unknown plugin: {id}")))
}

fn catalog_entry(p: &dyn Plugin) -> Value {
    let m = p.meta();
    let settings = p.settings();
    json!({
        "id": m.id, "name": m.name, "version": m.version, "author": m.author, "category": m.category,
        "summary": m.summary, "description": m.description, "permissions": m.permissions,
        "settings": settings, "defaults": defaults_for(&settings),
        "hasFeeShare": p.fee_share(&defaults_for(&settings)).is_some(),
        "hasAutopilot": p.has_actions(),
        "thirdParty": p.third_party(),
    })
}

/// Library listing for the UI.
pub fn bliss_catalog() -> Vec<Value> {
    first_party().iter().map(|p| catalog_entry(p.as_ref())).collect()
}

/// The first-party library plus approved third-party plugins.
pub async fn full_catalog() -> Result<Vec<Value>> {
    let external = external::load_external().await?;
    let mut out = bliss_catalog();
    out.extend(external.iter().map(|p| catalog_entry(p.as_ref())));
    Ok(out)
}

pub async fn installed_plugins(agent_id: &str, include_disabled: bool) -> Result<Vec<Installed>> {
    let rows: Vec<AgentPlugin> = sqlx::query_as("select * from agent_plugins where agent_id = $1 order by created_at, plugin_id").bind(agent_id).fetch_all(db()).await?;
    let external = external::load_external().await?;
    let lookup = |id: &str| first_party().iter().find(|p| p.meta().id == id).cloned().or_else(|| external.iter().find(|p| p.meta().id == id).cloned());
    Ok(rows
        .into_iter()
        .filter(|r| include_disabled || r.enabled)
        .filter_map(|r| {
            // A suspended or withdrawn third-party plugin simply drops out.
            let plugin = lookup(&r.plugin_id)?;
            let config = parse_settings(&plugin.settings(), &r.config).unwrap_or_else(|_| defaults_for(&plugin.settings()));
            Some(Installed { plugin, config, state: r.state, enabled: r.enabled })
        })
        .collect())
}

pub fn context_for(agent: &Arc<Agent>, entry: &Installed, installed: &Arc<Vec<Installed>>) -> Ctx {
    Ctx { agent: agent.clone(), config: entry.config.clone(), state: entry.state.clone(), installed: installed.clone() }
}

fn share_of(p: &Installed) -> f64 {
    p.plugin.fee_share(&p.config).unwrap_or(0.0)
}

fn check_plugin(plugin: &dyn Plugin, config: &Settings, others: &[Installed], tier: Tier) -> Result<()> {
    if let Some(e) = plugin.validate(config, others, tier) {
        return Err(plugin_error(e));
    }
    let id = &plugin.meta().id;
    let share = plugin.fee_share(config).unwrap_or(0.0) + others.iter().filter(|o| &o.plugin.meta().id != id).map(share_of).sum::<f64>();
    if share > 100.0 {
        return Err(plugin_error(format!("Fee shares across plugins add up to {}%; they can't exceed 100%.", crate::auth::js_number(share))));
    }
    Ok(())
}

/// Validate a whole plugin set before an agent is created (new agents start on Basic).
pub fn resolve_plugins(requested: &[(String, Value)]) -> Result<Vec<Installed>> {
    let mut resolved: Vec<Installed> = vec![];
    for (id, config) in requested {
        let plugin = get_plugin(id)?;
        let name = plugin.meta().name.clone();
        if resolved.iter().any(|r| &r.plugin.meta().id == id) {
            return Err(plugin_error(format!("{name} is listed twice.")));
        }
        let values = parse_settings(&plugin.settings(), config).map_err(|_| plugin_error(format!("Invalid settings for {name}.")))?;
        if let Some(e) = plugin.validate(&values, &resolved, Tier::Basic) {
            return Err(plugin_error(format!("{name}: {e}")));
        }
        resolved.push(Installed { plugin, config: values, state: json!({}), enabled: true });
    }
    let share: f64 = resolved.iter().map(share_of).sum();
    if share > 100.0 {
        return Err(plugin_error(format!("Fee shares across plugins add up to {}%; they can't exceed 100%.", crate::auth::js_number(share))));
    }
    Ok(resolved)
}

pub async fn install_plugin(agent: &Agent, plugin_id: &str, raw_config: &Value) -> Result<()> {
    external::load_external().await?;
    let plugin = get_plugin(plugin_id)?;
    let name = plugin.meta().name.clone();
    let existing = installed_plugins(&agent.id, true).await?;
    if existing.iter().any(|p| p.plugin.meta().id == plugin_id) {
        return Err(plugin_error(format!("{name} is already installed.")));
    }
    let config = parse_settings(&plugin.settings(), raw_config).map_err(|_| plugin_error(format!("Invalid settings for {name}.")))?;
    check_plugin(plugin.as_ref(), &config, &existing, Tier::parse(&agent.tier))?;
    sqlx::query("insert into agent_plugins (agent_id, plugin_id, config) values ($1, $2, $3)").bind(&agent.id).bind(plugin_id).bind(Value::Object(config)).execute(db()).await?;
    log_agent(&agent.id, AgentLogKind::Plugin, &format!("Installed {name}")).await
}

pub async fn update_plugin(agent: &Agent, plugin_id: &str, config: Option<&Value>, enabled: Option<bool>) -> Result<()> {
    external::load_external().await?;
    let plugin = get_plugin(plugin_id)?;
    let name = plugin.meta().name.clone();
    let existing = installed_plugins(&agent.id, true).await?;
    let Some(current) = existing.iter().find(|p| p.plugin.meta().id == plugin_id) else {
        return Err(plugin_error(format!("{name} isn't installed.")));
    };
    let mut next = current.config.clone();
    if let Some(update) = config {
        let mut merged = Value::Object(current.config.clone());
        if let (Some(m), Some(u)) = (merged.as_object_mut(), update.as_object()) {
            for (k, v) in u {
                m.insert(k.clone(), v.clone());
            }
        }
        next = parse_settings(&plugin.settings(), &merged).map_err(|_| plugin_error(format!("Invalid settings for {name}.")))?;
        check_plugin(plugin.as_ref(), &next, &existing, Tier::parse(&agent.tier))?;
    }
    sqlx::query("update agent_plugins set config = $3, enabled = coalesce($4, enabled) where agent_id = $1 and plugin_id = $2")
        .bind(&agent.id)
        .bind(plugin_id)
        .bind(Value::Object(next))
        .bind(enabled)
        .execute(db())
        .await?;
    Ok(())
}

pub async fn uninstall_plugin(agent: &Agent, plugin_id: &str) -> Result<()> {
    external::load_external().await?;
    if external::is_external_id(plugin_id) && external::cached_external(plugin_id).is_none() {
        // A third-party plugin that's since been suspended or withdrawn: just remove it.
        sqlx::query("delete from agent_plugins where agent_id = $1 and plugin_id = $2").bind(&agent.id).bind(plugin_id).execute(db()).await?;
        return Ok(());
    }
    let plugin = get_plugin(plugin_id)?;
    let installed = Arc::new(installed_plugins(&agent.id, true).await?);
    let Some(entry) = installed.iter().find(|p| p.plugin.meta().id == plugin_id) else { return Ok(()) };
    let agent_arc = Arc::new(agent.clone());
    if let Some(blocked) = plugin.before_uninstall(&context_for(&agent_arc, entry, &installed)).await {
        return Err(plugin_error(blocked));
    }
    // Its BNB budget isn't moved anywhere: removing the row simply frees that BNB.
    sqlx::query("delete from agent_plugins where agent_id = $1 and plugin_id = $2").bind(&agent.id).bind(plugin_id).execute(db()).await?;
    sqlx::query("delete from jobs where agent_id = $1 and task like $2").bind(&agent.id).bind(format!("plugin:{plugin_id}:%")).execute(db()).await?;
    log_agent(&agent.id, AgentLogKind::Plugin, &format!("Removed {}", plugin.meta().name)).await
}

/// Tools for a surface. Holder chat only ever gets read tools; autopilot also gets action tools.
/// Plugins own disjoint tool names; a clash is a bug, so it fails loudly.
pub fn compose_tools(agent: &Arc<Agent>, installed: &Arc<Vec<Installed>>, surface: &str) -> Result<ToolSet> {
    let mut tools: ToolSet = vec![];
    for entry in installed.iter() {
        let ctx = context_for(agent, entry, installed);
        let mut sets: Vec<Tool> = vec![];
        if entry.plugin.surfaces(&entry.config).contains(&surface) {
            sets.extend(entry.plugin.read_tools(&ctx));
        }
        if surface == "autopilot" {
            sets.extend(entry.plugin.action_tools(&ctx));
        }
        for t in sets {
            if tools.iter().any(|x| x.name == t.name) {
                return Err(Error::internal(format!("tool name clash: {} ({})", t.name, entry.plugin.meta().id)));
            }
            tools.push(t);
        }
    }
    Ok(tools)
}

pub async fn compose_knowledge(agent: &Arc<Agent>, installed: &Arc<Vec<Installed>>) -> String {
    let mut sections = vec![];
    for entry in installed.iter() {
        if let Some(text) = entry.plugin.knowledge(&context_for(agent, entry, installed)).await.filter(|t| !t.is_empty()) {
            sections.push(format!("## {}\n{text}", entry.plugin.meta().name));
        }
    }
    sections.join("\n\n")
}

pub fn compose_mandates(agent: &Arc<Agent>, installed: &Arc<Vec<Installed>>) -> String {
    installed.iter().filter_map(|e| e.plugin.mandate(&context_for(agent, e, installed))).filter(|m| !m.is_empty()).map(|m| format!("- {m}")).collect::<Vec<_>>().join("\n")
}

pub fn has_autopilot_capability(installed: &[Installed]) -> bool {
    installed.iter().any(|p| p.plugin.has_actions())
}

/// Every scheduled plugin job: (task name `plugin:<pluginId>:<jobId>`, plugin, job).
pub fn plugin_tasks() -> Vec<(String, Arc<dyn Plugin>, PluginJob)> {
    first_party().iter().flat_map(|p| p.jobs().into_iter().map(move |j| (format!("plugin:{}:{}", p.meta().id, j.id), p.clone(), j))).collect()
}

// ---- Helpers plugins share ----

/// Record a trade the agent made (or tried).
#[allow(clippy::too_many_arguments)]
pub async fn record_trade(agent_id: &str, plugin_id: &str, venue: &str, symbol: &str, side: &str, size: f64, size_unit: &str, leverage: Option<i32>, price: Option<f64>, rationale: &str, result: &Value, ok: bool) -> Result<()> {
    sqlx::query("insert into trades (agent_id, plugin_id, venue, symbol, side, size, size_unit, leverage, price, rationale, result, ok) values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)")
        .bind(agent_id)
        .bind(plugin_id)
        .bind(venue)
        .bind(symbol)
        .bind(side)
        .bind(size)
        .bind(size_unit)
        .bind(leverage)
        .bind(price)
        .bind(rationale)
        .bind(result)
        .bind(ok)
        .execute(db())
        .await?;
    Ok(())
}

pub async fn recent_trades(agent_id: &str, plugin_id: &str, limit: i64) -> Result<Vec<crate::db::Trade>> {
    Ok(sqlx::query_as("select * from trades where agent_id = $1 and plugin_id = $2 order by created_at desc limit $3").bind(agent_id).bind(plugin_id).bind(limit).fetch_all(db()).await?)
}

/// `n` rounded to `d` decimals, as JavaScript's Number(n.toFixed(d)).
pub fn round(n: f64, d: i32) -> f64 {
    let f = 10f64.powi(d);
    (n * f).round() / f
}

/// A tool result as the TypeScript plugins returned it: JSON text.
pub fn json_text(v: Value) -> std::result::Result<Value, String> {
    Ok(Value::String(v.to_string()))
}

/// Merge fields into a plugin's state (atomic jsonb merge).
pub async fn merge_state(agent_id: &str, plugin_id: &str, patch: Value) -> Result<()> {
    sqlx::query("update agent_plugins set state = state || $3 where agent_id = $1 and plugin_id = $2").bind(agent_id).bind(plugin_id).bind(patch).execute(db()).await?;
    Ok(())
}

/// A typed view of a tool's input.
pub fn input<T: for<'de> serde::Deserialize<'de>>(v: Value) -> std::result::Result<T, String> {
    serde_json::from_value(v).map_err(|e| format!("invalid input: {e}"))
}

pub fn obj(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
}
