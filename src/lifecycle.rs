//! An agent's life: creation (wallet, handle, AI key, plugins), its token launch on four.meme or
//! Flap from its own wallet once funded, graduation, and splitting its income into plugin budgets.

use alloy::primitives::U256;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::ai::providers::{key_error, verify_provider_key};
use crate::bliss::{installed_plugins, resolve_plugins, treasury::allocate_fee_income};
use crate::db::{db, log_agent, Agent, AgentLogKind};
use crate::env::env;
use crate::error::{Error, Result};
use crate::evm;
use crate::launchpad::{self, validate_tax, TokenTax};
use crate::public::{handle_available, suggest_handle, validate_handle};
use crate::secrets::{decrypt_secret, encrypt_secret, secret_hint, SecretKind};

#[derive(Debug, Clone, Deserialize)]
pub struct AiChoice {
    pub provider: String,
    pub model: String,
    #[serde(rename = "apiKey")]
    pub api_key: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CreateAgentInput {
    #[serde(default)]
    pub owner_account_id: String,
    pub name: String,
    pub symbol: String,
    pub description: Option<String>,
    pub icon: Option<String>,
    pub links: Option<Value>,
    pub persona: Option<String>,
    pub strategy: Option<String>,
    pub min_holding_tokens: Option<f64>,
    pub autopilot_enabled: Option<bool>,
    pub autopilot_interval_min: Option<i32>,
    pub billing_mode: Option<String>,
    pub handle: Option<String>,
    pub rate_limit_per_minute: Option<i32>,
    #[serde(default)]
    pub plugins: Vec<PluginChoice>,
    pub chat_messages_per_hour: Option<i32>,
    pub tax: Option<TokenTax>,
    pub launchpad: Option<String>,
    pub ai: Option<AiChoice>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginChoice {
    pub id: String,
    #[serde(default)]
    pub config: Value,
}

/// Check that the key works and can use the chosen model.
async fn check_ai_config(provider: &str, model: &str, api_key: &str) -> Result<()> {
    let models = verify_provider_key(provider, api_key).await?;
    if !models.iter().any(|m| m == model) {
        return Err(key_error(&format!("This key can't use model \"{model}\".")));
    }
    Ok(())
}

pub async fn pick_handle(requested: Option<&str>, symbol: &str, wallets: &[String]) -> Result<String> {
    if let Some(r) = requested {
        let handle = r.trim().to_lowercase();
        if let Some(e) = validate_handle(&handle) {
            return Err(Error::invalid(e));
        }
        if !handle_available(&handle, wallets).await? {
            return Err(Error::conflict(format!("{handle}.orvyn.cash is taken (or is someone else's Binance Agent ID).")));
        }
        return Ok(handle);
    }
    for _ in 0..10 {
        let handle = suggest_handle(symbol);
        if validate_handle(&handle).is_none() && handle_available(&handle, wallets).await? {
            return Ok(handle);
        }
    }
    Err(Error::conflict("Couldn't generate a free handle; pick one."))
}

pub async fn create_agent(input: CreateAgentInput) -> Result<Agent> {
    let requested: Vec<(String, Value)> = input.plugins.iter().map(|p| (p.id.clone(), p.config.clone())).collect();
    let plugins = resolve_plugins(&requested)?;
    let tax = validate_tax(input.tax.as_ref())?;
    let owner = input.owner_account_id.to_lowercase();
    let handle = pick_handle(input.handle.as_deref(), &input.symbol, std::slice::from_ref(&owner)).await?;
    let ai = input.ai.clone().ok_or_else(|| Error::invalid("Choose an AI provider, model and key."))?;
    let api_key = ai.api_key.trim().to_string();
    check_ai_config(&ai.provider, &ai.model, &api_key).await?;

    let id = uuid_v4();
    let wallets = crate::wallets::create_agent_wallets(&id).await?;
    let launchpad = match input.launchpad.as_deref() {
        Some("flap") => "flap",
        Some("none") => "none",
        _ => "fourmeme",
    };
    let agent: Agent = sqlx::query_as(
        "insert into agents (id, handle, owner_account_id, name, symbol, description, icon, links, persona, strategy, autopilot_enabled, autopilot_interval_min,
           billing_mode, rate_limit_per_minute, min_holding_tokens, chat_messages_per_hour, tax, launchpad, status, ai_provider, ai_model, ai_key_encrypted, ai_key_hint,
           evm_address, evm_wallet)
         values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,$24,$25) returning *",
    )
    .bind(&id)
    .bind(&handle)
    .bind(&owner)
    .bind(&input.name)
    .bind(input.symbol.to_uppercase())
    .bind(input.description.clone().unwrap_or_default())
    .bind(&input.icon)
    .bind(&input.links)
    .bind(input.persona.clone().unwrap_or_default())
    .bind(input.strategy.clone().unwrap_or_default())
    .bind(input.autopilot_enabled.unwrap_or(false))
    .bind(input.autopilot_interval_min.unwrap_or(60))
    .bind(input.billing_mode.clone().unwrap_or_else(|| "manual".into()))
    .bind(input.rate_limit_per_minute.unwrap_or(60))
    .bind(input.min_holding_tokens.unwrap_or(1.0))
    .bind(input.chat_messages_per_hour.unwrap_or(20))
    .bind(serde_json::to_value(tax)?)
    .bind(launchpad)
    // No token to launch: the agent is live right away.
    .bind(if launchpad == "none" { "live" } else { "awaiting_funds" })
    .bind(&ai.provider)
    .bind(&ai.model)
    .bind(encrypt_secret(&api_key, &id, SecretKind::AiKey)?)
    .bind(secret_hint(&api_key))
    .bind(&wallets.evm_address)
    .bind(serde_json::to_value(&wallets.evm_wallet)?)
    .fetch_one(db())
    .await?;
    for p in &plugins {
        sqlx::query("insert into agent_plugins (agent_id, plugin_id, config) values ($1, $2, $3)").bind(&id).bind(&p.plugin.meta().id).bind(Value::Object(p.config.clone())).execute(db()).await?;
    }
    let names: Vec<String> = plugins.iter().map(|p| p.plugin.meta().name.clone()).collect();
    log_agent(&id, AgentLogKind::Launch, &format!("Agent created with wallet {} on BNB Chain. Plugins: {}.", wallets.evm_address, if names.is_empty() { "none".into() } else { names.join(", ") })).await?;
    Ok(agent)
}

pub fn uuid_v4() -> String {
    let mut b: [u8; 16] = rand::random();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex::encode(b);
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// Change the agent's provider, model or key. Without a new key, the stored one is reused.
pub async fn update_agent_ai(agent: &Agent, provider: &str, model: &str, api_key: Option<&str>) -> Result<Agent> {
    let key = match api_key.map(str::trim).filter(|k| !k.is_empty()) {
        Some(k) => k.to_string(),
        None if provider == agent.ai_provider => decrypt_secret(&agent.ai_key_encrypted, &agent.id, SecretKind::AiKey)?,
        None => return Err(key_error("Switching provider needs an API key for the new provider.")),
    };
    check_ai_config(provider, model, &key).await?;
    Ok(sqlx::query_as("update agents set ai_provider = $2, ai_model = $3, ai_key_encrypted = $4, ai_key_hint = $5 where id = $1 returning *")
        .bind(&agent.id)
        .bind(provider)
        .bind(model)
        .bind(encrypt_secret(&key, &agent.id, SecretKind::AiKey)?)
        .bind(secret_hint(&key))
        .fetch_one(db())
        .await?)
}

pub struct Funding {
    pub required: U256,
    pub balance: U256,
    pub launch_cost: U256,
    pub ready: bool,
}

/// BNB the owner must send to the agent wallet before launch.
pub async fn launch_funding(agent: &Agent) -> Result<Funding> {
    // Flap charges nothing to create a token (only gas); four.meme takes a launch fee.
    let launch_cost = if agent.launchpad == "flap" { U256::ZERO } else { launchpad::launch_fee_wei() };
    let required = launch_cost + evm::to_wei(0.01) + evm::to_wei(agent.gas_reserve_bnb);
    let balance = evm::bnb_balance(&agent.evm_address).await?;
    Ok(Funding { required, balance, launch_cost, ready: balance >= required })
}

async fn launch_now(agent: &Agent) -> Result<Agent> {
    let wallet = crate::wallets::agent_wallet(&agent.id, &agent.evm_wallet)?;
    let tax = validate_tax(launchpad::tax_from_value(agent.tax.as_ref()).as_ref())?;
    let (launched, where_) = if agent.launchpad == "flap" {
        (launchpad::launch_on_flap(agent, &wallet, &launchpad::token_name(&agent.name)).await?, "Flap")
    } else {
        let input = launchpad::LaunchInput { name: launchpad::token_name(&agent.name), symbol: agent.symbol.clone(), description: agent.description.clone(), icon: agent.icon.clone(), links: agent.links.as_ref(), tax };
        (launchpad::launch_on_fourmeme(agent, &wallet, input).await?, "four.meme")
    };
    let updated: Agent = sqlx::query_as("update agents set status = 'live', status_detail = null, token_address = $2, launch_tx = $3, launch_request_id = $4 where id = $1 returning *")
        .bind(&agent.id)
        .bind(launched.token.to_lowercase())
        .bind(&launched.tx_hash)
        .bind(&launched.request_id)
        .fetch_one(db())
        .await?;
    log_agent(&agent.id, AgentLogKind::Launch, &format!("Launched ${} on {where_} as {} (tax {}%, tx {})", agent.symbol, launched.token, tax.fee_rate, launched.tx_hash)).await?;
    Ok(updated)
}

/// Launch the agent's token once funded: for agents created with a token, and live agents whose
/// owner chose to launch one later. Agents without a token are left alone.
pub async fn try_launch(agent: Agent) -> Result<Agent> {
    if agent.launchpad == "none" || agent.token_address.is_some() {
        return Ok(agent);
    }
    let later = agent.status == "live";
    if !later && agent.status != "awaiting_funds" && agent.status != "failed" {
        return Ok(agent);
    }
    if !launch_funding(&agent).await?.ready {
        return Ok(agent);
    }
    // Claim the launch so a concurrent caller can't double-launch.
    let claimed = if later {
        sqlx::query("update agents set launch_tx = 'pending' where id = $1 and status = 'live' and token_address is null and launch_tx is null").bind(&agent.id).execute(db()).await?
    } else {
        sqlx::query("update agents set status = 'launching', status_detail = null where id = $1 and status in ('awaiting_funds', 'failed')").bind(&agent.id).execute(db()).await?
    };
    if claimed.rows_affected() == 0 {
        return Ok(agent);
    }
    match launch_now(&agent).await {
        Ok(a) => Ok(a),
        Err(e) => {
            if later {
                sqlx::query("update agents set launch_tx = null, status_detail = $2 where id = $1").bind(&agent.id).bind(format!("token launch failed: {}", e.message)).execute(db()).await?;
            } else {
                sqlx::query("update agents set status = 'failed', status_detail = $2 where id = $1").bind(&agent.id).bind(&e.message).execute(db()).await?;
            }
            log_agent(&agent.id, AgentLogKind::Error, &format!("Launch failed: {}", e.message)).await?;
            Err(e)
        }
    }
}

/// The owner of a live agent without a token chooses to launch one (launchpad and tax, fixed once
/// launched). The worker launches it once the wallet holds enough BNB.
pub async fn launch_token_later(agent: &Agent, launchpad_choice: &str, tax: Option<&TokenTax>) -> Result<Agent> {
    if agent.token_address.is_some() {
        return Err(Error::conflict("This agent already has a token."));
    }
    if agent.status != "live" {
        return Err(Error::conflict("The agent must be live to launch a token."));
    }
    if agent.icon.is_none() {
        return Err(Error::invalid("Add an image for the agent first; launchpads need a token image."));
    }
    let tax = validate_tax(tax)?;
    let lp = if launchpad_choice == "flap" { "flap" } else { "fourmeme" };
    let updated: Option<Agent> = sqlx::query_as("update agents set launchpad = $2, tax = $3, status_detail = null where id = $1 and token_address is null and launch_tx is null returning *")
        .bind(&agent.id)
        .bind(lp)
        .bind(serde_json::to_value(tax)?)
        .fetch_optional(db())
        .await?;
    let Some(updated) = updated else { return Err(Error::conflict("A launch is already in progress.")) };
    log_agent(&agent.id, AgentLogKind::Launch, &format!("Token launch on {} requested (tax {}%); it launches once the wallet is funded.", if lp == "flap" { "Flap" } else { "four.meme" }, tax.fee_rate)).await?;
    Ok(try_launch(updated.clone()).await.unwrap_or(updated))
}

/// Mirror the token's graduation to PancakeSwap.
pub async fn sync_launch(agent: Agent) -> Result<Agent> {
    let Some(token) = agent.token_address.clone().filter(|_| !agent.graduated) else { return Ok(agent) };
    let graduated = if agent.launchpad == "flap" { launchpad::flap_curve_info(&token).await.ok().map(|c| c.graduated) } else { launchpad::curve_info(&token).await.ok().map(|c| c.graduated) };
    if graduated != Some(true) {
        return Ok(agent);
    }
    let updated: Agent = sqlx::query_as("update agents set graduated = true where id = $1 returning *").bind(&agent.id).fetch_one(db()).await?;
    log_agent(&agent.id, AgentLogKind::Launch, &format!("${} graduated to PancakeSwap.", agent.symbol)).await?;
    Ok(updated)
}

pub async fn process_income(agent: &Agent) -> Result<()> {
    if agent.status != "live" || agent.token_address.is_none() {
        return Ok(());
    }
    allocate_earnings(agent).await
}

/// Split new tax income into plugin budgets (all of it is the agent's). Measured from a cumulative
/// total; a compare-and-set on the allocated total stops two workers allocating the same BNB.
pub async fn allocate_earnings(agent: &Agent) -> Result<()> {
    let Some(total) = launchpad::tax_income_total(agent).await? else { return Ok(()) };
    let allocated = U256::from_str_radix(&agent.tax_allocated_wei, 10).unwrap_or_default();
    let income = total.saturating_sub(allocated);
    if income < evm::to_wei(env().tax_allocate_min_bnb) {
        return Ok(());
    }
    let won = sqlx::query("update agents set tax_allocated_wei = $2 where id = $1 and tax_allocated_wei = $3").bind(&agent.id).bind(total.to_string()).bind(&agent.tax_allocated_wei).execute(db()).await?;
    if won.rows_affected() == 0 {
        return Ok(());
    }
    sqlx::query("insert into fee_claims (agent_id, kind, amount, tx) values ($1, 'tax', $2, null)").bind(&agent.id).bind(income.to_string()).execute(db()).await?;
    allocate_fee_income(&agent.id, &installed_plugins(&agent.id, false).await?, income).await?;
    log_agent(&agent.id, AgentLogKind::Fees, &format!("Received {} BNB from ${}'s trade tax", crate::auth::js_number(evm::to_bnb(income)), agent.symbol)).await
}

pub fn launch_json(f: &Funding) -> Value {
    json!({ "required": f.required.to_string(), "balance": f.balance.to_string(), "launchCost": f.launch_cost.to_string(), "ready": f.ready })
}
