//! Google Gemini (streamGenerateContent over SSE). Function calls carry thought signatures on newer
//! models; they're kept on the tool-call part (`providerOptions.google.thoughtSignature`) and sent
//! back with it.

use serde_json::{json, Value};

use super::{parts, stream_sse, ModelConfig, OnText, StepOutput, Tool};
use crate::error::Result;

/// Gemini takes an OpenAPI subset of JSON Schema: drop what it refuses.
fn gemini_schema(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut out = serde_json::Map::new();
            for (k, val) in m {
                if ["additionalProperties", "$schema", "$ref", "$defs", "definitions", "default", "examples", "const"].contains(&k.as_str()) {
                    continue;
                }
                out.insert(k.clone(), gemini_schema(val));
            }
            // An empty object schema isn't accepted: say nothing about its properties.
            if out.get("type") == Some(&json!("object")) && out.get("properties").is_some_and(|p| p.as_object().is_some_and(|o| o.is_empty())) {
                out.remove("properties");
                out.remove("required");
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(gemini_schema).collect()),
        other => other.clone(),
    }
}

fn to_gemini(messages: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = vec![];
    for m in messages {
        let role = m["role"].as_str().unwrap_or("user");
        let ps: Vec<Value> = match role {
            "assistant" => parts(m)
                .iter()
                .filter_map(|p| match p["type"].as_str() {
                    Some("text") if !p["text"].as_str().unwrap_or("").is_empty() => Some(json!({ "text": p["text"] })),
                    Some("tool-call") => {
                        let mut part = json!({ "functionCall": { "name": p["toolName"], "args": p["input"] } });
                        if let Some(sig) = p["providerOptions"]["google"]["thoughtSignature"].as_str() {
                            part["thoughtSignature"] = sig.into();
                        }
                        Some(part)
                    }
                    _ => None,
                })
                .collect(),
            "tool" => parts(m)
                .iter()
                .filter(|p| p["type"] == "tool-result")
                .map(|p| {
                    let o = &p["output"];
                    let response = match o["type"].as_str() {
                        Some("json") => json!({ "name": p["toolName"], "content": o["value"] }),
                        _ => json!({ "name": p["toolName"], "content": o["value"].as_str().unwrap_or("") }),
                    };
                    json!({ "functionResponse": { "name": p["toolName"], "response": response } })
                })
                .collect(),
            "system" => continue,
            _ => parts(m)
                .iter()
                .filter_map(|p| match p["type"].as_str() {
                    Some("text") => Some(json!({ "text": p["text"] })),
                    Some("image") => p["image"].as_str().map(|d| json!({ "inlineData": { "mimeType": p["mediaType"].as_str().unwrap_or("image/png"), "data": d } })),
                    _ => None,
                })
                .collect(),
        };
        if ps.is_empty() {
            continue;
        }
        let role = if role == "assistant" { "model" } else { "user" };
        if let Some(last) = out.last_mut() {
            if last["role"] == role {
                last["parts"].as_array_mut().unwrap().extend(ps);
                continue;
            }
        }
        out.push(json!({ "role": role, "parts": ps }));
    }
    out
}

pub async fn call(cfg: &ModelConfig, system: &str, messages: &[Value], tools: &[Tool], max_tokens: u32, on_text: OnText<'_>) -> Result<StepOutput> {
    let mut body = json!({ "contents": to_gemini(messages), "generationConfig": { "maxOutputTokens": max_tokens } });
    if !system.is_empty() {
        body["systemInstruction"] = json!({ "parts": [{ "text": system }] });
    }
    if !tools.is_empty() {
        body["tools"] = json!([{ "functionDeclarations": tools.iter().map(|t| json!({ "name": t.name, "description": t.description, "parameters": gemini_schema(&t.input_schema) })).collect::<Vec<_>>() }]);
    }
    let url = format!("https://generativelanguage.googleapis.com/v1beta/models/{}:streamGenerateContent?alt=sse", cfg.model);
    let req = crate::http::client().post(url).header("x-goog-api-key", &cfg.api_key).timeout(std::time::Duration::from_secs(600)).json(&body);
    let mut text = String::new();
    let mut calls = vec![];
    let mut out = StepOutput::default();
    stream_sse(req, |data| {
        let ev: Value = serde_json::from_str(data).unwrap_or(Value::Null);
        if let Some(u) = ev.get("usageMetadata") {
            out.input_tokens = u["promptTokenCount"].as_u64().unwrap_or(out.input_tokens);
            out.output_tokens = u["candidatesTokenCount"].as_u64().unwrap_or(out.output_tokens);
        }
        let cand = &ev["candidates"][0];
        for p in cand["content"]["parts"].as_array().cloned().unwrap_or_default() {
            if p["thought"] == true {
                continue;
            }
            if let Some(t) = p["text"].as_str() {
                text.push_str(t);
                if let Some(f) = on_text {
                    f(t);
                }
            }
            if p["functionCall"].is_object() {
                let n = calls.len();
                let mut call = json!({ "type": "tool-call", "toolCallId": format!("call_{n}_{}", hex::encode(rand::random::<[u8; 4]>())), "toolName": p["functionCall"]["name"], "input": if p["functionCall"]["args"].is_object() { p["functionCall"]["args"].clone() } else { json!({}) } });
                if let Some(sig) = p["thoughtSignature"].as_str() {
                    call["providerOptions"] = json!({ "google": { "thoughtSignature": sig } });
                }
                calls.push(call);
            }
        }
        if let Some(r) = cand["finishReason"].as_str() {
            out.finish_reason = r.to_lowercase();
        }
        Ok(())
    })
    .await?;
    if !text.is_empty() {
        out.content.push(json!({ "type": "text", "text": text }));
    }
    out.content.extend(calls);
    Ok(out)
}
