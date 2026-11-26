//! One autopilot session on the creator's chosen model, with every installed plugin's tools.
//! Limits are enforced inside the action tools, so the model can't exceed the mandate.

use std::sync::Arc;

use super::{autopilot_system_prompt, core_tools};
use crate::ai::{agent_model, describe_ai_error, generate, user_message, Effort, Request};
use crate::bliss::{compose_knowledge, compose_mandates, compose_tools, has_autopilot_capability, installed_plugins};
use crate::db::{log_agent, Agent, AgentLogKind};
use crate::error::Result;

pub async fn run_autopilot_session(agent: Arc<Agent>) -> Result<Option<String>> {
    let installed = Arc::new(installed_plugins(&agent.id, false).await?);
    if !has_autopilot_capability(&installed) {
        return Ok(None);
    }
    let r: Result<_> = async {
        let cfg = agent_model(&agent, Effort::High)?;
        let knowledge = compose_knowledge(&agent, &installed).await;
        let mut tools = core_tools(&agent, &installed);
        tools.extend(compose_tools(&agent, &installed, "autopilot")?);
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        generate(
            &cfg,
            Request {
                system: autopilot_system_prompt(&agent, &installed, &knowledge, &compose_mandates(&agent, &installed)),
                messages: vec![user_message(&format!("Autopilot session at {now}. Review your treasury and markets, act if warranted, then summarize."))],
                tools: &tools,
                max_steps: 20,
                max_output_tokens: 16000,
            },
            None,
        )
        .await
    }
    .await;
    match r {
        Ok(out) if out.finish_reason == "content-filter" => {
            log_agent(&agent.id, AgentLogKind::Error, "Autopilot session declined by the model.").await?;
            Ok(None)
        }
        Ok(out) => {
            let summary = out.text.trim().to_string();
            if !summary.is_empty() {
                log_agent(&agent.id, AgentLogKind::Think, &summary).await?;
            }
            Ok(Some(summary))
        }
        Err(e) => {
            // Surfaced in the activity log so the creator sees e.g. a revoked key.
            log_agent(&agent.id, AgentLogKind::Error, &format!("Autopilot session failed: {}", describe_ai_error(&e))).await?;
            Ok(None)
        }
    }
}
