//! Shared one-shot agent execution: create client, send message, collect text output.
//! Used by handlers that need a simple "prompt in, text out" pattern.

use std::path::Path;

use super::codex_exec::{resolve_codex_model, run_codex_text};
use super::executor::run_codex_agent_turn;
use super::AgentType;

/// Result of a one-shot agent execution.
pub struct OneshotResult {
    pub text: String,
    pub tool_call_count: i32,
}

/// Run a one-shot agent: create client, send prompt, collect all text output, return.
///
/// For agents that need tool access, pass the AgentType to configure tools.
/// For tool-less agents (e.g., meeting notes extraction), pass None.
pub async fn run_oneshot(
    agent_type: Option<AgentType>,
    system_prompt: &str,
    working_dir: &Path,
    prompt: impl Into<String>,
) -> Result<OneshotResult, String> {
    let prompt = prompt.into();

    if let Some(agent_type) = agent_type {
        let session_id = uuid::Uuid::new_v4().to_string();
        let result = run_codex_agent_turn(
            &agent_type,
            working_dir,
            system_prompt,
            &prompt,
            None,
            false,
            None,
            &session_id,
        )
        .await
        .map_err(|e| e.to_string())?;

        return Ok(OneshotResult {
            text: result.output_summary,
            tool_call_count: result.tool_call_count,
        });
    }

    let text = run_codex_text(
        resolve_codex_model("haiku"),
        "low",
        system_prompt,
        working_dir,
        &prompt,
    )
    .await?;

    Ok(OneshotResult {
        text,
        tool_call_count: 0,
    })
}
