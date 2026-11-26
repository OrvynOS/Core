//! Anthropic Messages API (streaming), with adaptive thinking on the models that take it. Thinking
//! blocks are kept in the conversation with their signatures, as tool-use loops require.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use super::{api_error, is_error_output, output_text, parts, stream_sse, ModelConfig, OnText, StepOutput, Tool};
use crate::error::Result;

fn to_anthropic(messages: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = vec![];
    for m in messages {
        let role = m["role"].as_str().unwrap_or("user");
        let content: Vec<Value> = match role {
            "assistant" => parts(m)
                .iter()
                .filter_map(|p| match p["type"].as_str() {
                    Some("text") if !p["text"].as_str().unwrap_or("").is_empty() => Some(json!({ "type": "text", "text": p["text"] })),
                    Some("reasoning") => {
                        let a = &p["providerOptions"]["anthropic"];
                        if let Some(sig) = a["signature"].as_str() {
                            Some(json!({ "type": "thinking", "thinking": p["text"], "signature": sig }))
                        } else {
                            a["redactedData"].as_str().map(|d| json!({ "type": "redacted_thinking", "data": d }))
                        }
                    }
                    Some("tool-call") => Some(json!({ "type": "tool_use", "id": p["toolCallId"], "name": p["toolName"], "input": if p["input"].is_object() { p["input"].clone() } else { json!({}) } })),
                    _ => None,
                })
                .collect(),
            "tool" => parts(m)
                .iter()
                .filter(|p| p["type"] == "tool-result")
                .map(|p| json!({ "type": "tool_result", "tool_use_id": p["toolCallId"], "content": output_text(&p["output"]), "is_error": is_error_output(&p["output"]) }))
                .collect(),
            "system" => continue,
            _ => parts(m)
                .iter()
                .filter_map(|p| match p["type"].as_str() {
                    Some("text") => Some(json!({ "type": "text", "text": p["text"] })),
                    Some("image") => p["image"].as_str().map(|data| json!({ "type": "image", "source": { "type": "base64", "media_type": p["mediaType"].as_str().unwrap_or("image/png"), "data": data } })),
                    _ => None,
                })
                .collect(),
        };
        if content.is_empty() {
            continue;
        }
        let role = if role == "assistant" { "assistant" } else { "user" };
        // Consecutive same-role messages merge (tool results follow as user content).
        if let Some(last) = out.last_mut() {
            if last["role"] == role {
                last["content"].as_array_mut().unwrap().extend(content);
                continue;
            }
        }
        out.push(json!({ "role": role, "content": content }));
    }
    out
}

pub async fn call(cfg: &ModelConfig, system: &str, messages: &[Value], tools: &[Tool], max_tokens: u32, on_text: OnText<'_>) -> Result<StepOutput> {
    let mut body = json!({
        "model": cfg.model,
        "max_tokens": max_tokens,
        "messages": to_anthropic(messages),
        "stream": true,
    });
    if !system.is_empty() {
        body["system"] = system.into();
    }
    if !tools.is_empty() {
        body["tools"] = tools.iter().map(|t| json!({ "name": t.name, "description": t.description, "input_schema": t.input_schema })).collect::<Vec<_>>().into();
    }
    if super::providers::anthropic_adaptive(&cfg.model) {
        body["thinking"] = json!({ "type": "adaptive" });
        body["output_config"] = json!({ "effort": cfg.effort.as_str() });
    }
    let req = crate::http::client()
        .post(format!("{}/v1/messages", std::env::var("ANTHROPIC_BASE_URL").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "https://api.anthropic.com".into())))
        .header("x-api-key", &cfg.api_key)
        .header("anthropic-version", "2023-06-01")
        .timeout(std::time::Duration::from_secs(600))
        .json(&body);

    // Blocks by index while they stream.
    let mut blocks: BTreeMap<u64, Value> = BTreeMap::new();
    let mut tool_json: BTreeMap<u64, String> = BTreeMap::new();
    let mut out = StepOutput::default();
    stream_sse(req, |data| {
        let ev: Value = serde_json::from_str(data).unwrap_or(Value::Null);
        let index = ev["index"].as_u64().unwrap_or(0);
        match ev["type"].as_str() {
            Some("message_start") => out.input_tokens = ev["message"]["usage"]["input_tokens"].as_u64().unwrap_or(0),
            Some("content_block_start") => {
                blocks.insert(index, ev["content_block"].clone());
            }
            Some("content_block_delta") => {
                let d = &ev["delta"];
                let block = blocks.entry(index).or_insert_with(|| json!({}));
                match d["type"].as_str() {
                    Some("text_delta") => {
                        let t = d["text"].as_str().unwrap_or("");
                        let cur = block["text"].as_str().unwrap_or("").to_string();
                        block["text"] = format!("{cur}{t}").into();
                        if let Some(f) = on_text {
                            f(t);
                        }
                    }
                    Some("thinking_delta") => {
                        let cur = block["thinking"].as_str().unwrap_or("").to_string();
                        block["thinking"] = format!("{cur}{}", d["thinking"].as_str().unwrap_or("")).into();
                    }
                    Some("signature_delta") => block["signature"] = d["signature"].clone(),
                    Some("input_json_delta") => tool_json.entry(index).or_default().push_str(d["partial_json"].as_str().unwrap_or("")),
                    _ => {}
                }
            }
            Some("message_delta") => {
                if let Some(r) = ev["delta"]["stop_reason"].as_str() {
                    out.finish_reason = r.into();
                }
                out.output_tokens = ev["usage"]["output_tokens"].as_u64().unwrap_or(out.output_tokens);
            }
            Some("error") => return Err(api_error(None, ev["error"]["message"].as_str().unwrap_or("stream error").to_string())),
            _ => {}
        }
        Ok(())
    })
    .await?;
    for (index, block) in blocks {
        match block["type"].as_str() {
            Some("text") => out.content.push(json!({ "type": "text", "text": block["text"].as_str().unwrap_or("") })),
            Some("thinking") => out.content.push(json!({ "type": "reasoning", "text": block["thinking"].as_str().unwrap_or(""), "providerOptions": { "anthropic": { "signature": block["signature"] } } })),
            Some("redacted_thinking") => out.content.push(json!({ "type": "reasoning", "text": "", "providerOptions": { "anthropic": { "redactedData": block["data"] } } })),
            Some("tool_use") => {
                let raw = tool_json.get(&index).cloned().unwrap_or_default();
                let input: Value = if raw.trim().is_empty() { block["input"].clone() } else { serde_json::from_str(&raw).unwrap_or(json!({})) };
                out.content.push(json!({ "type": "tool-call", "toolCallId": block["id"], "toolName": block["name"], "input": input }));
            }
            _ => {}
        }
    }
    Ok(out)
}
