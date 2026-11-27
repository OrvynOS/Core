//! Bring-your-own-key AI providers. Each agent's creator (and each Vyn user) picks a provider and
//! model and supplies their own key. Keys are validated by listing the provider's models (a free
//! call), and the listed models are what the creator picks from.

use serde::Serialize;
use serde_json::Value;

use crate::error::{Error, Kind, Result};
use crate::http::client;

/// How a provider is spoken to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    Anthropic,
    /// OpenAI-style chat completions at this base URL.
    OpenAi(&'static str),
    Google,
    /// Orvyn's own gateway (OpenAI-style, paid from the key owner's Orvyn credits).
    Orvyn,
}

pub struct ProviderDef {
    pub id: &'static str,
    pub label: &'static str,
    pub key_url: &'static str,
    pub preferred_model: Option<&'static str>,
    pub wire: Wire,
}

const NEAR_AI_URL: &str = "https://cloud-api.near.ai/v1";

pub static PROVIDERS: &[ProviderDef] = &[
    ProviderDef { id: "orvyn", label: "Orvyn AI (pay with Orvyn credits)", key_url: "https://app.orvyn.cash/credits", preferred_model: Some(crate::inference::DEFAULT_MODEL), wire: Wire::Orvyn },
    ProviderDef { id: "anthropic", label: "Anthropic (Claude)", key_url: "https://console.anthropic.com/settings/keys", preferred_model: Some("claude-opus-5"), wire: Wire::Anthropic },
    ProviderDef { id: "openai", label: "OpenAI", key_url: "https://platform.openai.com/api-keys", preferred_model: None, wire: Wire::OpenAi("https://api.openai.com/v1") },
    ProviderDef { id: "google", label: "Google Gemini", key_url: "https://aistudio.google.com/apikey", preferred_model: None, wire: Wire::Google },
    ProviderDef { id: "xai", label: "xAI (Grok)", key_url: "https://console.x.ai", preferred_model: None, wire: Wire::OpenAi("https://api.x.ai/v1") },
    ProviderDef { id: "mistral", label: "Mistral", key_url: "https://console.mistral.ai/api-keys", preferred_model: None, wire: Wire::OpenAi("https://api.mistral.ai/v1") },
    ProviderDef { id: "deepseek", label: "DeepSeek", key_url: "https://platform.deepseek.com/api_keys", preferred_model: None, wire: Wire::OpenAi("https://api.deepseek.com/v1") },
    ProviderDef { id: "groq", label: "Groq", key_url: "https://console.groq.com/keys", preferred_model: None, wire: Wire::OpenAi("https://api.groq.com/openai/v1") },
    ProviderDef { id: "cerebras", label: "Cerebras", key_url: "https://cloud.cerebras.ai", preferred_model: None, wire: Wire::OpenAi("https://api.cerebras.ai/v1") },
    ProviderDef { id: "togetherai", label: "Together AI", key_url: "https://api.together.ai/settings/api-keys", preferred_model: None, wire: Wire::OpenAi("https://api.together.xyz/v1") },
    ProviderDef { id: "fireworks", label: "Fireworks", key_url: "https://fireworks.ai/account/api-keys", preferred_model: None, wire: Wire::OpenAi("https://api.fireworks.ai/inference/v1") },
    ProviderDef { id: "nearai", label: "NEAR AI (private inference)", key_url: "https://cloud.near.ai/dashboard/organizations", preferred_model: Some("anthropic/claude-opus-5"), wire: Wire::OpenAi(NEAR_AI_URL) },
    ProviderDef { id: "openrouter", label: "OpenRouter (300+ models)", key_url: "https://openrouter.ai/keys", preferred_model: None, wire: Wire::OpenAi("https://openrouter.ai/api/v1") },
];

pub fn is_provider_id(id: &str) -> bool {
    PROVIDERS.iter().any(|p| p.id == id)
}

pub fn provider(id: &str) -> Result<&'static ProviderDef> {
    PROVIDERS.iter().find(|p| p.id == id).ok_or_else(|| Error::invalid(format!("unsupported AI provider: {id}")))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogEntry {
    pub id: &'static str,
    pub label: &'static str,
    pub key_url: &'static str,
    pub preferred_model: Option<&'static str>,
}

/// Providers kept working for existing agents but no longer offered.
const RETIRED: &[&str] = &["openrouter"];

/// Public provider catalog for the UI.
pub fn provider_catalog() -> Vec<CatalogEntry> {
    PROVIDERS.iter().filter(|p| !RETIRED.contains(&p.id)).map(|p| CatalogEntry { id: p.id, label: p.label, key_url: p.key_url, preferred_model: p.preferred_model }).collect()
}

/// A bad key, as the creator should see it.
pub fn key_error(m: &str) -> Error {
    Error::new(Kind::Invalid, m)
}

async fn get_json(url: &str, headers: &[(&str, &str)]) -> Result<Value> {
    let mut req = client().get(url).timeout(std::time::Duration::from_secs(15));
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let res = req.send().await.map_err(|e| Error::internal(format!("provider: {e}")))?;
    let status = res.status().as_u16();
    if status == 400 || status == 401 || status == 403 {
        return Err(key_error("The provider rejected this API key."));
    }
    if !res.status().is_success() {
        return Err(Error::internal(format!("provider returned {status}")));
    }
    res.json().await.map_err(|e| Error::internal(format!("provider: {e}")))
}

// Drop embedding, audio, image and moderation models; agents need chat models with tool use.
fn is_chat(id: &str) -> bool {
    let l = id.to_lowercase();
    !["embed", "tts", "whisper", "transcri", "dall-e", "image", "imagen", "audio", "realtime", "moderation", "rerank", "veo", "guard", "speech", "search-preview"].iter().any(|w| l.contains(w))
}

fn chat_models(ids: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut v: Vec<String> = ids.into_iter().filter(|i| is_chat(i)).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    v.sort();
    v
}

fn ids_of(rows: &Value) -> Vec<String> {
    let rows = rows.get("data").unwrap_or(rows);
    rows.as_array().map(|a| a.iter().filter_map(|m| m["id"].as_str().map(String::from)).collect()).unwrap_or_default()
}

async fn check_near_ai_key(api_key: &str) -> Result<()> {
    let res = client()
        .post(format!("{NEAR_AI_URL}/chat/completions"))
        .bearer_auth(api_key)
        .json(&serde_json::json!({ "model": "orvyn-key-check", "messages": [{ "role": "user", "content": "." }], "max_tokens": 1 }))
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| Error::internal(format!("provider: {e}")))?;
    let s = res.status().as_u16();
    if s == 401 || s == 403 {
        return Err(key_error("The provider rejected this API key."));
    }
    if s >= 500 {
        return Err(Error::internal(format!("provider returned {s}")));
    }
    Ok(())
}

/// The chat models this key can use.
pub async fn list_models(provider_id: &str, api_key: &str) -> Result<Vec<String>> {
    let bearer = format!("Bearer {api_key}");
    let auth: &[(&str, &str)] = &[("authorization", &bearer)];
    Ok(match provider_id {
        "anthropic" => {
            let body = get_json("https://api.anthropic.com/v1/models?limit=1000", &[("x-api-key", api_key), ("anthropic-version", "2023-06-01")]).await?;
            chat_models(ids_of(&body))
        }
        "google" => {
            let body = get_json("https://generativelanguage.googleapis.com/v1beta/models?pageSize=1000", &[("x-goog-api-key", api_key)]).await?;
            let models = body["models"].as_array().cloned().unwrap_or_default();
            chat_models(
                models
                    .iter()
                    .filter(|m| m["supportedGenerationMethods"].as_array().is_some_and(|a| a.iter().any(|x| x == "generateContent")))
                    .filter_map(|m| m["name"].as_str().map(|n| n.trim_start_matches("models/").to_string())),
            )
        }
        "togetherai" => {
            let rows = get_json("https://api.together.xyz/v1/models", auth).await?;
            let rows = rows.as_array().cloned().unwrap_or_default();
            chat_models(rows.iter().filter(|m| m["type"].as_str().is_none_or(|t| t == "chat")).filter_map(|m| m["id"].as_str().map(String::from)))
        }
        "nearai" => {
            check_near_ai_key(api_key).await?;
            let body = get_json(&format!("{NEAR_AI_URL}/models"), &[]).await?;
            let rows = body["data"].as_array().cloned().unwrap_or_default();
            chat_models(
                rows.iter()
                    .filter(|m| m["supported_features"].as_array().is_some_and(|f| f.iter().any(|x| x == "tools")))
                    .filter(|m| m["output_modalities"].as_array().is_none_or(|o| o.iter().any(|x| x == "text")))
                    .filter_map(|m| m["id"].as_str().map(String::from)),
            )
        }
        "openrouter" => {
            // The model list is public, so check the key separately.
            get_json("https://openrouter.ai/api/v1/key", auth).await?;
            let body = get_json("https://openrouter.ai/api/v1/models", &[]).await?;
            let rows = body["data"].as_array().cloned().unwrap_or_default();
            chat_models(rows.iter().filter(|m| m["supported_parameters"].as_array().is_some_and(|p| p.iter().any(|x| x == "tools"))).filter_map(|m| m["id"].as_str().map(String::from)))
        }
        // An Orvyn API key (orv_sk_…): the gateway lists the models it serves.
        "orvyn" => ids_of(&get_json(&format!("{}/models", crate::inference::gateway_base()), auth).await?),
        "deepseek" => chat_models(ids_of(&get_json("https://api.deepseek.com/models", auth).await?)),
        other => match provider(other)?.wire {
            Wire::OpenAi(base) => chat_models(ids_of(&get_json(&format!("{base}/models"), auth).await?)),
            _ => unreachable!(),
        },
    })
}

/// Validate a key and return the chat models it can use.
pub async fn verify_provider_key(provider_id: &str, api_key: &str) -> Result<Vec<String>> {
    provider(provider_id)?;
    let models = list_models(provider_id, api_key.trim()).await?;
    if models.is_empty() {
        return Err(key_error("This key has no chat models available."));
    }
    Ok(models)
}

/// Whether a Claude model takes adaptive thinking and effort.
pub fn anthropic_adaptive(model: &str) -> bool {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"claude-(opus|sonnet|fable|mythos)-(4-[678]|5)").unwrap());
    RE.is_match(model)
}
