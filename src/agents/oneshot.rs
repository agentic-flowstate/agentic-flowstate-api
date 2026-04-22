//! Shared one-shot agent execution: create client, send message, collect text output.
//! Used by handlers that need a simple "prompt in, text out" pattern.

use cc_sdk::{ClaudeSDKClient, ClaudeCodeOptions, Message, ContentBlock, ToolsConfig, PermissionMode};
use futures::StreamExt;
use std::path::Path;

use super::codex_exec::{normalize_reasoning_effort, resolve_codex_model, run_codex_text};
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

    let use_codex_exec = agent_type
        .as_ref()
        .map(|at| at.allowed_tools().is_empty())
        .unwrap_or(true);

    if use_codex_exec {
        let model = agent_type
            .as_ref()
            .map(|at| resolve_codex_model(at.model()))
            .unwrap_or(resolve_codex_model("haiku"));
        let reasoning_effort = agent_type
            .as_ref()
            .map(|at| normalize_reasoning_effort(at.effort()))
            .unwrap_or("low");
        let text = run_codex_text(model, reasoning_effort, system_prompt, working_dir, &prompt)
            .await?;

        return Ok(OneshotResult {
            text,
            tool_call_count: 0,
        });
    }

    let mut builder = ClaudeCodeOptions::builder()
        .system_prompt(system_prompt)
        .disallowed_tools(crate::safety::disallowed_tools())
        .permission_mode(PermissionMode::BypassPermissions)
        .cwd(working_dir);

    if let Some(ref at) = agent_type {
        builder = builder.model(at.model());

        let tools_list: Vec<String> = at.allowed_tools().iter().map(|s| s.to_string()).collect();
        builder = builder
            .tools(ToolsConfig::list(tools_list.clone()))
            .allowed_tools(tools_list);

        if let Some(turns) = at.max_turns() {
            builder = builder.max_turns(turns);
        }
    } else {
        builder = builder.tools(ToolsConfig::none()).max_turns(1);
    }

    let options = builder.build();
    let mut sdk_client = ClaudeSDKClient::new(options);

    sdk_client.connect(None).await
        .map_err(|e| format!("Failed to connect agent: {}", e))?;

    sdk_client.send_user_message(prompt).await
        .map_err(|e| format!("Failed to send message: {}", e))?;

    let mut response_stream = sdk_client.receive_messages().await;
    let mut output_parts = Vec::new();
    let mut tool_call_count: i32 = 0;

    while let Some(msg_result) = response_stream.next().await {
        match msg_result {
            Ok(msg) => {
                if let Message::Assistant { message: assistant_msg } = &msg {
                    for block in &assistant_msg.content {
                        match block {
                            ContentBlock::Text(text_content) => {
                                output_parts.push(text_content.text.clone());
                            }
                            ContentBlock::ToolUse(tool_use) => {
                                tool_call_count += 1;
                                tracing::info!("Oneshot tool use: {} [#{}]", tool_use.name, tool_call_count);
                            }
                            _ => {}
                        }
                    }
                }
                if let Message::Result { .. } = &msg {
                    break;
                }
            }
            Err(e) => {
                tracing::error!("Stream error in oneshot agent: {}", e);
                return Err(format!("Agent stream error: {}", e));
            }
        }
    }

    if output_parts.is_empty() {
        return Err("No output from agent".to_string());
    }

    tracing::info!("[ONESHOT] tool_call_count={}", tool_call_count);

    Ok(OneshotResult {
        text: output_parts.join(""),
        tool_call_count,
    })
}
