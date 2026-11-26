//! The AI layer: one tool-calling loop over every provider (Anthropic, OpenAI-compatible APIs,
//! Google Gemini), with streaming text.
//!
//! Messages are kept in the AI SDK's model-message JSON (what the TypeScript core stored in
//! vyn_messages): `{ role: "user" | "assistant" | "tool" | "system", content }` where assistant
//! content parts are `text`, `reasoning` (Anthropic thinking: `providerOptions.anthropic.signature`)
//! and `tool-call` (`toolCallId`, `toolName`, `input`), and tool content parts are `tool-result`
//! (`toolCallId`, `toolName`, `output: { type: "json" | "text" | "error-text", value }`).

pub mod anthropic;
pub mod google;
pub mod openai;
pub mod providers;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::db::Agent;
use crate::error::{Error, Kind, Result};

pub type ToolFuture = Pin<Box<dyn Future<Output = std::result::Result<Value, String>> + Send>>;
pub type ToolFn = Arc<dyn Fn(Value) -> ToolFuture + Send + Sync>;

/// A tool the model can call. Without `execute`, calling it ends the turn with the call pending
/// (device tools that run on the person's computer or phone).
#[derive(Clone)]
pub struct Tool {
    pub name: String,
    pub description: String,
    /// JSON Schema of the input (an object).
    pub input_schema: Value,
    pub execute: Option<ToolFn>,
}

impl Tool {
    pub fn new<F, Fut>(name: &str, description: &str, input_schema: Value, f: F) -> Self
    where
        F: Fn(Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = std::result::Result<Value, String>> + Send + 'static,
    {
        Self { name: name.into(), description: description.into(), input_schema, execute: Some(Arc::new(move |v| Box::pin(f(v)))) }
    }
    /// A tool the model may call but that runs elsewhere (the turn stops at the call).
    pub fn remote(name: &str, description: &str, input_schema: Value) -> Self {
        Self { name: name.into(), description: description.into(), input_schema, execute: None }
    }
}

/// Build a JSON Schema object: `schema(&[("query", json!({"type":"string"}), true)])`.
pub fn schema(props: &[(&str, Value, bool)]) -> Value {
    let properties: serde_json::Map<String, Value> = props.iter().map(|(k, v, _)| (k.to_string(), v.clone())).collect();
    let required: Vec<&str> = props.iter().filter(|(_, _, r)| *r).map(|(k, _, _)| *k).collect();
    json!({ "type": "object", "properties": properties, "required": required, "additionalProperties": false })
}

pub type ToolSet = Vec<Tool>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effort {
    Low,
    Medium,
    High,
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
        }
    }
}

/// Which model, with whose key.
#[derive(Clone)]
pub struct ModelConfig {
    pub provider: String,
    pub model: String,
    pub api_key: String,
    pub effort: Effort,
}

/// A request for one turn.
pub struct Request<'a> {
    pub system: String,
    pub messages: Vec<Value>,
    pub tools: &'a [Tool],
    /// Model calls in the loop (each one may call tools).
    pub max_steps: usize,
    pub max_output_tokens: u32,
}

/// What one model call returned, normalized.
#[derive(Debug, Clone, Default)]
pub struct StepOutput {
    /// Assistant content parts (text, reasoning, tool-call).
    pub content: Vec<Value>,
    pub finish_reason: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl StepOutput {
    pub fn text(&self) -> String {
        self.content.iter().filter(|p| p["type"] == "text").filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("")
    }
    pub fn tool_calls(&self) -> Vec<&Value> {
        self.content.iter().filter(|p| p["type"] == "tool-call").collect()
    }
}

#[derive(Debug, Clone, Default)]
pub struct Step {
    pub text: String,
    pub tool_calls: Vec<Value>,
    pub tool_results: Vec<Value>,
}

#[derive(Debug, Clone, Default)]
pub struct Output {
    /// The last step's text.
    pub text: String,
    pub steps: Vec<Step>,
    /// The messages this turn added (assistant and tool), to append to the conversation.
    pub response_messages: Vec<Value>,
    pub finish_reason: String,
    /// Calls to tools without `execute` (device tools): the turn stopped at them.
    pub pending_tool_calls: Vec<Value>,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Streamed text, as it arrives.
pub type OnText<'a> = Option<&'a (dyn Fn(&str) + Send + Sync)>;

/// A provider's HTTP error, kept so it can be described to the person without leaking the key.
#[derive(Debug, Clone)]
pub struct ApiCallError {
    pub status: Option<u16>,
    pub message: String,
}

pub fn api_error(status: Option<u16>, message: impl Into<String>) -> Error {
    let message = message.into();
    Error::new(Kind::Internal, format!("AI_API_ERROR:{}:{message}", status.map(|s| s.to_string()).unwrap_or_default()))
}

/// Turn provider errors into messages a creator can act on, without leaking the key.
pub fn describe_ai_error(e: &Error) -> String {
    if let Some(rest) = e.message.strip_prefix("AI_API_ERROR:") {
        let status: Option<u16> = rest.split(':').next().and_then(|s| s.parse().ok());
        return match status {
            Some(401) | Some(403) => "The AI provider rejected the agent's API key. The creator needs to update it.".into(),
            Some(429) => "The AI provider is rate limiting the agent's key. Try again shortly.".into(),
            Some(402) => "The agent's AI account is out of credits. Add credits to keep it running.".into(),
            Some(404) => "The agent's AI model was not found. The creator needs to pick another model.".into(),
            Some(s) => format!("AI provider error ({s})."),
            None => "AI provider error (network).".into(),
        };
    }
    e.message.clone()
}

async fn call_model(cfg: &ModelConfig, req: &Request<'_>, messages: &[Value], on_text: OnText<'_>) -> Result<StepOutput> {
    match providers::provider(&cfg.provider)?.wire {
        providers::Wire::Anthropic => anthropic::call(cfg, &req.system, messages, req.tools, req.max_output_tokens, on_text).await,
        providers::Wire::OpenAi(base) => {
            // ORVYN_AI_BASE_<PROVIDER> points a provider elsewhere (a proxy, or tests).
            let base = std::env::var(format!("ORVYN_AI_BASE_{}", cfg.provider.to_uppercase())).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| base.to_string());
            openai::call(&base, cfg, &req.system, messages, req.tools, req.max_output_tokens, on_text).await
        }
        providers::Wire::Orvyn => openai::call(&crate::inference::gateway_base(), cfg, &req.system, messages, req.tools, req.max_output_tokens, on_text).await,
        providers::Wire::Google => google::call(cfg, &req.system, messages, req.tools, req.max_output_tokens, on_text).await,
    }
}

/// Run the tool loop: call the model, run the tools it called, feed the results back, until it
/// answers without tools, calls a tool that runs elsewhere, or `max_steps` is reached.
/// The tool loop, boxed: callers nest it inside tools and sessions, and an opaque boxed future keeps
/// the compiler from re-proving `Send` through every layer (which made builds take many minutes).
pub fn generate<'a>(cfg: &'a ModelConfig, req: Request<'a>, on_text: OnText<'a>) -> futures_util::future::BoxFuture<'a, Result<Output>> {
    Box::pin(generate_loop(cfg, req, on_text))
}

async fn generate_loop(cfg: &ModelConfig, req: Request<'_>, on_text: OnText<'_>) -> Result<Output> {
    let mut messages = req.messages.clone();
    let mut out = Output::default();
    for step_index in 0..req.max_steps.max(1) {
        if step_index > 0 {
            if let Some(f) = on_text {
                // Steps of one turn read as one reply.
                if !out.text.is_empty() {
                    f("\n\n");
                }
            }
        }
        let step = call_model(cfg, &req, &messages, on_text).await?;
        out.input_tokens += step.input_tokens;
        out.output_tokens += step.output_tokens;
        out.finish_reason = step.finish_reason.clone();
        let assistant = json!({ "role": "assistant", "content": step.content });
        messages.push(assistant.clone());
        out.response_messages.push(assistant);
        let calls: Vec<Value> = step.tool_calls().into_iter().cloned().collect();
        let mut record = Step { text: step.text(), tool_calls: calls.clone(), tool_results: vec![] };
        out.text = record.text.clone();
        if calls.is_empty() {
            out.steps.push(record);
            break;
        }
        let mut results = vec![];
        let mut pending = vec![];
        for call in &calls {
            let name = call["toolName"].as_str().unwrap_or("");
            let tool = req.tools.iter().find(|t| t.name == name);
            match tool.and_then(|t| t.execute.clone()) {
                Some(exec) => {
                    let output = match exec(call["input"].clone()).await {
                        Ok(Value::String(s)) => json!({ "type": "text", "value": s }),
                        Ok(v) => json!({ "type": "json", "value": v }),
                        Err(e) => json!({ "type": "error-text", "value": e }),
                    };
                    results.push(json!({ "type": "tool-result", "toolCallId": call["toolCallId"], "toolName": name, "output": output }));
                }
                None if tool.is_some() => pending.push(call.clone()),
                None => results.push(json!({ "type": "tool-result", "toolCallId": call["toolCallId"], "toolName": name, "output": { "type": "error-text", "value": format!("Unknown tool: {name}") } })),
            }
        }
        record.tool_results = results.clone();
        out.steps.push(record);
        if !results.is_empty() {
            let tool_message = json!({ "role": "tool", "content": results });
            messages.push(tool_message.clone());
            out.response_messages.push(tool_message);
        }
        if !pending.is_empty() {
            out.pending_tool_calls = pending;
            break;
        }
    }
    Ok(out)
}

/// The creator's configured model for an agent, with their decrypted key.
pub fn agent_model(agent: &Agent, effort: Effort) -> Result<ModelConfig> {
    let api_key = crate::secrets::decrypt_secret(&agent.ai_key_encrypted, &agent.id, crate::secrets::SecretKind::AiKey)?;
    Ok(ModelConfig { provider: agent.ai_provider.clone(), model: agent.ai_model.clone(), api_key, effort })
}

// ---- Message helpers shared by the wire formats ----

/// A message's content as parts (a plain string becomes one text part).
pub fn parts(message: &Value) -> Vec<Value> {
    match &message["content"] {
        Value::String(s) => vec![json!({ "type": "text", "text": s })],
        Value::Array(a) => a.clone(),
        _ => vec![],
    }
}

/// A tool result's output as the text the model reads.
pub fn output_text(output: &Value) -> String {
    match output["type"].as_str() {
        Some("text") | Some("error-text") => output["value"].as_str().unwrap_or("").to_string(),
        _ => serde_json::to_string(&output["value"]).unwrap_or_default(),
    }
}

pub fn is_error_output(output: &Value) -> bool {
    matches!(output["type"].as_str(), Some("error-text") | Some("error-json"))
}

pub fn user_message(text: &str) -> Value {
    json!({ "role": "user", "content": text })
}

/// Server-sent events: yields each `data:` payload as it completes.
pub struct SseReader {
    buffer: String,
}

impl SseReader {
    pub fn new() -> Self {
        Self { buffer: String::new() }
    }
    /// Feed bytes; returns complete data payloads.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buffer.push_str(&String::from_utf8_lossy(chunk));
        let mut out = vec![];
        loop {
            let Some(end) = self.buffer.find("\n\n").or_else(|| self.buffer.find("\r\n\r\n")) else { break };
            let sep = if self.buffer[end..].starts_with("\r\n\r\n") { 4 } else { 2 };
            let event: String = self.buffer.drain(..end + sep).collect();
            let data: Vec<&str> = event.lines().filter_map(|l| l.strip_prefix("data:")).map(|d| d.strip_prefix(' ').unwrap_or(d)).collect();
            if !data.is_empty() {
                out.push(data.join("\n"));
            }
        }
        out
    }
}

/// Send a streaming request and hand each SSE payload to `each`.
pub(crate) async fn stream_sse(req: reqwest::RequestBuilder, mut each: impl FnMut(&str) -> Result<()>) -> Result<()> {
    use futures_util::StreamExt;
    let res = req.send().await.map_err(|e| api_error(None, e.to_string()))?;
    let status = res.status();
    if !status.is_success() {
        let body = res.text().await.unwrap_or_default();
        return Err(api_error(Some(status.as_u16()), crate::http::clip(&body, 500)));
    }
    let mut reader = SseReader::new();
    let mut stream = res.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| api_error(None, e.to_string()))?;
        for data in reader.push(&chunk) {
            each(&data)?;
        }
    }
    Ok(())
}
