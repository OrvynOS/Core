//! Holder chat on the agent's own model. Read-only: the model gets core tools plus its plugins'
//! read tools, never action tools.

use std::sync::Arc;

use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;

use super::{chat_system_prompt, core_tools, reporting};
use crate::ai::{agent_model, describe_ai_error, generate, Effort, Request};
use crate::bliss::{compose_knowledge, compose_tools, installed_plugins};
use crate::db::{db, Agent, ChatMessage};
use crate::error::Result;

const HISTORY_LIMIT: usize = 30;

pub async fn chat_history(agent_id: &str, account_id: &str) -> Result<Vec<ChatMessage>> {
    let rows: Vec<ChatMessage> = sqlx::query_as("select * from chat_messages where agent_id = $1 and account_id = $2 order by id asc").bind(agent_id).bind(account_id).fetch_all(db()).await?;
    let skip = rows.len().saturating_sub(HISTORY_LIMIT);
    Ok(rows.into_iter().skip(skip).collect())
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Quota {
    pub used: i64,
    pub limit: i32,
    pub remaining: i64,
}

/// Holder messages in the last hour. Chat runs on the creator's API key, so it is capped.
pub async fn chat_quota(agent: &Agent, account_id: &str) -> Result<Quota> {
    let used: i64 = sqlx::query_scalar("select count(*) from chat_messages where agent_id = $1 and account_id = $2 and role = 'user' and created_at >= now() - interval '1 hour'")
        .bind(&agent.id)
        .bind(account_id)
        .fetch_one(db())
        .await?;
    Ok(Quota { used, limit: agent.chat_messages_per_hour, remaining: (agent.chat_messages_per_hour as i64 - used).max(0) })
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ChatEvent {
    Text { text: String },
    Tool { name: String },
    Done { text: String },
    Error { message: String },
}

/// Stream a holder-chat reply into `events`. The exchange is stored only once it completed, so
/// history always alternates user/assistant.
pub fn stream_chat<'a>(agent: Arc<Agent>, account_id: &'a str, user_message: &'a str, events: UnboundedSender<ChatEvent>) -> futures_util::future::BoxFuture<'a, Result<()>> {
    Box::pin(stream_chat_inner(agent, account_id, user_message, events))
}

async fn stream_chat_inner(agent: Arc<Agent>, account_id: &str, user_message: &str, events: UnboundedSender<ChatEvent>) -> Result<()> {
    let history = chat_history(&agent.id, account_id).await?;
    let installed = Arc::new(installed_plugins(&agent.id, false).await?);
    let mut messages: Vec<Value> = history.iter().map(|m| json!({ "role": m.role, "content": m.content })).collect();
    messages.push(json!({ "role": "user", "content": user_message }));

    let sent = events.clone();
    let reply = Arc::new(std::sync::Mutex::new(String::new()));
    let r: Result<String> = async {
        let cfg = agent_model(&agent, Effort::Medium)?;
        let knowledge = compose_knowledge(&agent, &installed).await;
        let tool_events = events.clone();
        let mut tools = core_tools(&agent, &installed);
        // Read-only: holder chat never gets a plugin's action tools.
        tools.extend(compose_tools(&agent, &installed, "chat")?);
        let tools = reporting(tools, Arc::new(move |name: &str| {
            let _ = tool_events.send(ChatEvent::Tool { name: name.to_string() });
        }));
        let acc = reply.clone();
        let on_text = move |t: &str| {
            acc.lock().unwrap().push_str(t);
            let _ = sent.send(ChatEvent::Text { text: t.to_string() });
        };
        let out = generate(&cfg, Request { system: chat_system_prompt(&agent, &installed, &knowledge), messages, tools: &tools, max_steps: 10, max_output_tokens: 8000 }, Some(&on_text)).await?;
        if out.finish_reason == "content-filter" {
            return Err(crate::error::Error::forbidden("The agent's model declined to answer that."));
        }
        Ok(reply.lock().unwrap().clone())
    }
    .await;
    let reply = match r {
        Ok(text) => text.trim().to_string(),
        Err(e) => {
            let message = if e.kind == crate::error::Kind::Forbidden { e.message } else { describe_ai_error(&e) };
            let _ = events.send(ChatEvent::Error { message });
            return Ok(());
        }
    };
    if !reply.is_empty() {
        sqlx::query("insert into chat_messages (agent_id, account_id, role, content) values ($1, $2, 'user', $3), ($1, $2, 'assistant', $4)")
            .bind(&agent.id)
            .bind(account_id)
            .bind(user_message)
            .bind(&reply)
            .execute(db())
            .await?;
    }
    let _ = events.send(ChatEvent::Done { text: reply });
    Ok(())
}
