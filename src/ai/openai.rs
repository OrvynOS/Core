//! OpenAI-style chat completions (streaming): OpenAI, xAI, Mistral, DeepSeek, Groq, Cerebras,
//! Together, Fireworks, NEAR AI and OpenRouter.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use super::{output_text, parts, stream_sse, ModelConfig, OnText, StepOutput, Tool};
use crate::error::Result;

fn to_openai(system: &str, messages: &[Value]) -> Vec<Value> {
    let mut out = vec![];
    if !system.is_empty() {
        out.push(json!({ "role": "system", "content": system }));
    }
    for m in messages {
        match m["role"].as_str().unwrap_or("user") {
            "assistant" => {
                let ps = parts(m);
                let text: String = ps.iter().filter(|p| p["type"] == "text").filter_map(|p| p["text"].as_str()).collect();
                let calls: Vec<Value> = ps
                    .iter()
                    .filter(|p| p["type"] == "tool-call")
                    .map(|p| json!({ "id": p["toolCallId"], "type": "function", "function": { "name": p["toolName"], "arguments": serde_json::to_string(&p["input"]).unwrap_or_else(|_| "{}".into()) } }))
                    .collect();
                let mut msg = json!({ "role": "assistant", "content": if text.is_empty() && !calls.is_empty() { Value::Null } else { text.into() } });
                if !calls.is_empty() {
                    msg["tool_calls"] = calls.into();
                }
                out.push(msg);
            }
            "tool" => {
                for p in parts(m).iter().filter(|p| p["type"] == "tool-result") {
                    out.push(json!({ "role": "tool", "tool_call_id": p["toolCallId"], "content": output_text(&p["output"]) }));
                }
            }
            "system" => out.push(json!({ "role": "system", "content": m["content"] })),
            _ => {
                let ps = parts(m);
                let images = ps.iter().any(|p| p["type"] == "image");
                if images {
                    let content: Vec<Value> = ps
                        .iter()
                        .filter_map(|p| match p["type"].as_str() {
                            Some("text") => Some(json!({ "type": "text", "text": p["text"] })),
                            Some("image") => p["image"].as_str().map(|d| json!({ "type": "image_url", "image_url": { "url": format!("data:{};base64,{d}", p["mediaType"].as_str().unwrap_or("image/png")) } })),
                            _ => None,
                        })
                        .collect();
                    out.push(json!({ "role": "user", "content": content }));
                } else {
                    let text: String = ps.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n");
                    out.push(json!({ "role": "user", "content": text }));
                }
            }
        }
    }
    out
}

pub async fn call(base: &str, cfg: &ModelConfig, system: &str, messages: &[Value], tools: &[Tool], max_tokens: u32, on_text: OnText<'_>) -> Result<StepOutput> {
    let mut body = json!({
        "model": cfg.model,
        "messages": to_openai(system, messages),
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    // OpenAI's reasoning models take max_completion_tokens; the others max_tokens.
    if cfg.provider == "openai" {
        body["max_completion_tokens"] = max_tokens.into();
    } else {
        body["max_tokens"] = max_tokens.into();
    }
    if !tools.is_empty() {
        body["tools"] = tools.iter().map(|t| json!({ "type": "function", "function": { "name": t.name, "description": t.description, "parameters": t.input_schema } })).collect::<Vec<_>>().into();
    }
    let mut req = crate::http::client().post(format!("{base}/chat/completions")).bearer_auth(&cfg.api_key).timeout(std::time::Duration::from_secs(600)).json(&body);
    if cfg.provider == "orvyn" {
        // Shows as "Agent" usage on the owner's Credits page.
        req = req.header("x-orvyn-source", "agent");
    }
    let mut text = String::new();
    let mut calls: BTreeMap<u64, (String, String, String)> = BTreeMap::new();
    let mut out = StepOutput::default();
    stream_sse(req, |data| {
        if data.trim() == "[DONE]" {
            return Ok(());
        }
        let ev: Value = serde_json::from_str(data).unwrap_or(Value::Null);
        if let Some(u) = ev.get("usage").filter(|u| u.is_object()) {
            out.input_tokens = u["prompt_tokens"].as_u64().unwrap_or(out.input_tokens);
            out.output_tokens = u["completion_tokens"].as_u64().unwrap_or(out.output_tokens);
        }
        let choice = &ev["choices"][0];
        let d = &choice["delta"];
        if let Some(t) = d["content"].as_str() {
            text.push_str(t);
            if let Some(f) = on_text {
                f(t);
            }
        }
        if let Some(tcs) = d["tool_calls"].as_array() {
            for tc in tcs {
                let i = tc["index"].as_u64().unwrap_or(0);
                let e = calls.entry(i).or_default();
                if let Some(id) = tc["id"].as_str() {
                    e.0 = id.into();
                }
                if let Some(n) = tc["function"]["name"].as_str() {
                    e.1.push_str(n);
                }
                if let Some(a) = tc["function"]["arguments"].as_str() {
                    e.2.push_str(a);
                }
            }
        }
        if let Some(r) = choice["finish_reason"].as_str() {
            out.finish_reason = r.into();
        }
        Ok(())
    })
    .await?;
    if !text.is_empty() {
        out.content.push(json!({ "type": "text", "text": text }));
    }
    for (i, (id, name, args)) in calls {
        let input: Value = if args.trim().is_empty() { json!({}) } else { serde_json::from_str(&args).unwrap_or(json!({})) };
        let id = if id.is_empty() { format!("call_{i}") } else { id };
        out.content.push(json!({ "type": "tool-call", "toolCallId": id, "toolName": name, "input": input }));
    }
    Ok(out)
}
