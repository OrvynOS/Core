//! A minimal MCP client over Streamable HTTP (JSON-RPC 2.0): initialize, tools/list, tools/call.
//! Answers come back as JSON or as an SSE stream; both are read. Used for third-party plugins
//! (behind the safety checks in external.rs) and Orvyn's trading MCP servers.

use serde_json::{json, Value};

use crate::error::{Error, Result};

pub const PROTOCOL_VERSION: &str = "2025-06-18";

pub struct McpClient {
    url: String,
    headers: Vec<(String, String)>,
    session: Option<String>,
    next_id: u64,
    timeout: std::time::Duration,
    client: reqwest::Client,
}

impl McpClient {
    pub fn new(url: &str, headers: Vec<(String, String)>, timeout: std::time::Duration) -> Self {
        // Redirects are refused: a reviewed host can't bounce calls elsewhere.
        let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).user_agent("Orvyn-Bliss/1.0").build().expect("client");
        Self { url: url.into(), headers, session: None, next_id: 1, timeout, client }
    }

    async fn post(&mut self, body: Value, expect_reply: bool) -> Result<Value> {
        let mut req = self.client.post(&self.url).header("accept", "application/json, text/event-stream").header("content-type", "application/json").timeout(self.timeout).json(&body);
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        if let Some(s) = &self.session {
            req = req.header("mcp-session-id", s).header("mcp-protocol-version", PROTOCOL_VERSION);
        }
        let res = req.send().await.map_err(|e| Error::internal(format!("MCP: {e}")))?;
        if let Some(s) = res.headers().get("mcp-session-id").and_then(|h| h.to_str().ok()) {
            self.session = Some(s.to_string());
        }
        let status = res.status();
        if !status.is_success() {
            return Err(Error::internal(format!("MCP server answered {}", status.as_u16())));
        }
        if !expect_reply {
            return Ok(Value::Null);
        }
        let is_sse = res.headers().get("content-type").and_then(|h| h.to_str().ok()).is_some_and(|c| c.starts_with("text/event-stream"));
        let text = res.text().await.map_err(|e| Error::internal(format!("MCP: {e}")))?;
        let id = body["id"].clone();
        let reply = if is_sse {
            let mut reader = crate::ai::SseReader::new();
            let mut events = reader.push(text.as_bytes());
            events.extend(reader.push(b"\n\n"));
            events.into_iter().filter_map(|d| serde_json::from_str::<Value>(&d).ok()).find(|m| m["id"] == id).ok_or_else(|| Error::internal("MCP: no reply in the stream"))?
        } else {
            serde_json::from_str(&text).map_err(|e| Error::internal(format!("MCP: {e}")))?
        };
        if let Some(err) = reply.get("error").filter(|e| !e.is_null()) {
            return Err(Error::internal(format!("MCP error: {}", err["message"].as_str().unwrap_or("unknown"))));
        }
        Ok(reply["result"].clone())
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.post(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }), true).await
    }

    pub async fn connect(&mut self) -> Result<()> {
        self.request("initialize", json!({ "protocolVersion": PROTOCOL_VERSION, "capabilities": {}, "clientInfo": { "name": "orvyn-bliss", "version": "1.0.0" } })).await?;
        self.post(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }), false).await?;
        Ok(())
    }

    pub async fn list_tools(&mut self) -> Result<Vec<Value>> {
        Ok(self.request("tools/list", json!({})).await?["tools"].as_array().cloned().unwrap_or_default())
    }

    pub async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        self.request("tools/call", json!({ "name": name, "arguments": arguments })).await
    }

    pub async fn close(&mut self) {
        if let Some(s) = self.session.take() {
            let mut req = self.client.delete(&self.url).header("mcp-session-id", s).timeout(std::time::Duration::from_secs(5));
            for (k, v) in &self.headers {
                req = req.header(k, v);
            }
            let _ = req.send().await;
        }
    }
}
