use cc_sdk::{ClaudeSDKClient, ClaudeCodeOptions, Message, ContentBlock, ToolsConfig, PermissionMode, McpServerConfig};
use futures::StreamExt;
use tokio::sync::mpsc;
use anyhow::{Result, Context};
use std::collections::HashMap;
use std::path::PathBuf;

use super::{AgentType, AgentRun, AgentRunStatus, TicketContext, StreamEvent, EmailOutput};
use super::prompts::load_prompt;

/// Build ClaudeCodeOptions for an agent type, optionally resuming from a previous session.
pub fn build_agent_options(
    agent_type: &AgentType,
    system_prompt: &str,
    working_dir: &std::path::Path,
    cc_session_id: Option<&str>,
) -> ClaudeCodeOptions {
    let tools_list: Vec<String> = agent_type
        .allowed_tools()
        .iter()
        .map(|s| s.to_string())
        .collect();

    let has_mcp_tools = tools_list.iter().any(|t| t.starts_with("mcp__"));

    let mut builder = ClaudeCodeOptions::builder()
        .system_prompt(system_prompt)
        .model(agent_type.model())
        .tools(ToolsConfig::list(tools_list.clone()))
        .allowed_tools(tools_list)
        .permission_mode(PermissionMode::BypassPermissions)
        .cwd(working_dir);

    if has_mcp_tools {
        let mcp_binary = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../agentic-flowstate-mcp/target/release/agentic_mcp");
        let mcp_binary = mcp_binary.canonicalize().unwrap_or(mcp_binary);
        builder = builder.add_mcp_server(
            "agentic-mcp",
            McpServerConfig::Stdio {
                command: mcp_binary.to_string_lossy().to_string(),
                args: None,
                env: None,
            },
        );
    }

    if let Some(turns) = agent_type.max_turns() {
        builder = builder.max_turns(turns);
    }

    // Enable adaptive thinking via --effort flag (Opus 4.6 recommended approach)
    builder = builder.add_extra_arg("effort", Some(agent_type.effort().to_string()));

    if let Some(sid) = cc_session_id {
        builder = builder.resume(sid.to_string());
    }

    builder.build()
}

/// Executes agents using the Claude Code CLI via cc-sdk ClaudeSDKClient.
pub struct AgentExecutor {
    working_dir: PathBuf,
}

impl AgentExecutor {
    pub fn new(working_dir: PathBuf) -> Self {
        Self { working_dir }
    }

    /// Execute an agent for a specific ticket.
    ///
    /// Returns the completed AgentRun with session_id and output summary.
    /// If event_tx is provided, structured events are sent for real-time UI updates.
    /// `selected_context` is used by the email agent to inject outputs from multiple selected agent runs.
    /// `sender_info` is used by the email agent to populate the signature with user contact details.
    /// Returns the completed AgentRun and the still-connected ClaudeSDKClient
    /// so it can be stored in ChatClientManager for follow-up messages.
    pub async fn execute(
        &self,
        agent_type: AgentType,
        ticket_context: TicketContext,
        previous_output: Option<String>,
        selected_context: Option<String>,
        sender_info: Option<String>,
        event_tx: Option<mpsc::Sender<StreamEvent>>,
    ) -> Result<(AgentRun, ClaudeSDKClient)> {
        let started_at = chrono::Utc::now().to_rfc3339();
        let session_id = uuid::Uuid::new_v4().to_string();

        // Build prompt variables — only small context goes in system prompt
        let mut vars = HashMap::new();
        vars.insert("epic_id".to_string(), ticket_context.epic_id.clone());
        vars.insert("slice_id".to_string(), ticket_context.slice_id.clone());
        vars.insert("ticket_id".to_string(), ticket_context.ticket_id.clone());
        vars.insert("ticket_title".to_string(), ticket_context.title.clone());
        vars.insert("ticket_intent".to_string(), ticket_context.intent.clone());

        // Load system prompt — role + rules only, no large data
        let system_prompt = load_prompt(agent_type.as_str(), vars)
            .context("Failed to load agent prompt")?;

        // Log what we're about to do
        tracing::info!(
            "Starting agent execution: type={}, ticket={}, model={}",
            agent_type.as_str(),
            ticket_context.ticket_id,
            agent_type.model()
        );
        tracing::info!("System prompt length: {} chars", system_prompt.len());
        tracing::info!("Working dir: {:?}", self.working_dir);
        tracing::info!("Tools config: {:?}", agent_type.allowed_tools());
        tracing::info!("Max turns: {:?}", agent_type.max_turns());

        let options = build_agent_options(&agent_type, &system_prompt, &self.working_dir, None);

        // Build user message: large data at top (XML-tagged), task at bottom
        // Per Anthropic best practices: data first, instructions last
        let mut context_sections = Vec::new();

        if let Some(prev) = &previous_output {
            let tag = match agent_type {
                AgentType::Planning => "research_output",
                AgentType::Execution => "implementation_plan",
                AgentType::Evaluation => "prior_outputs",
                AgentType::ResearchSynthesis => "research_findings",
                AgentType::TicketPlanner => "research_synthesis",
                AgentType::TicketCreator => "ticket_plan",
                AgentType::DocDrafter => "research_output",
                _ => "prior_context",
            };
            context_sections.push(format!("<{}>\n{}\n</{}>", tag, prev, tag));
        }

        if let Some(ctx) = &selected_context {
            context_sections.push(format!("<selected_context>\n{}\n</selected_context>", ctx));
        }

        if let Some(info) = &sender_info {
            context_sections.push(format!("<sender_info>\n{}\n</sender_info>", info));
        }

        let prompt = if context_sections.is_empty() {
            format!(
                "Work on this ticket:\n\nTitle: {}\nIntent: {}",
                ticket_context.title, ticket_context.intent
            )
        } else {
            format!(
                "{}\n\nWork on this ticket:\n\nTitle: {}\nIntent: {}",
                context_sections.join("\n\n"),
                ticket_context.title,
                ticket_context.intent
            )
        };

        let mut output_parts = Vec::new();
        let mut status = AgentRunStatus::Running;
        let actual_session_id = session_id.clone();
        let mut tool_call_count: i32 = 0;
        let mut cc_session_id: Option<String> = None;

        tracing::info!("Creating ClaudeSDKClient...");
        let query_start = std::time::Instant::now();

        let mut sdk_client = ClaudeSDKClient::new(options);

        match sdk_client.connect(None).await {
            Ok(_) => {
                tracing::info!("Client connected in {:?}", query_start.elapsed());

                if let Err(e) = sdk_client.send_user_message(prompt).await {
                    tracing::error!("Failed to send message: {}", e);
                    status = AgentRunStatus::Failed;
                } else {
                    let mut response_stream = sdk_client.receive_messages().await;
                    let mut message_count = 0u32;

                    while let Some(message_result) = response_stream.next().await {
                        message_count += 1;
                        match message_result {
                            Ok(message) => {
                                let msg_type = match &message {
                                    Message::System { .. } => "System",
                                    Message::Assistant { .. } => "Assistant",
                                    Message::User { .. } => "User",
                                    Message::Result { .. } => "Result",
                                };
                                tracing::info!("Received message #{}: type={}", message_count, msg_type);

                                if let Message::Assistant { message: assistant_msg } = &message {
                                    for block in &assistant_msg.content {
                                        match block {
                                            ContentBlock::Text(text_content) => {
                                                tracing::debug!("Assistant text: {} chars", text_content.text.len());
                                                output_parts.push(text_content.text.clone());

                                                if let Some(ref tx) = event_tx {
                                                    let event = StreamEvent::Text { content: text_content.text.clone() };
                                                    if let Err(e) = tx.send(event).await {
                                                        tracing::warn!("Failed to send text event: {}", e);
                                                    }
                                                }
                                            }
                                            ContentBlock::ToolUse(tool_use) => {
                                                tool_call_count += 1;
                                                tracing::info!("Tool use: {} ({}) [#{}]", tool_use.name, tool_use.id, tool_call_count);

                                                if let Some(ref tx) = event_tx {
                                                    let event = StreamEvent::ToolUse {
                                                        id: tool_use.id.clone(),
                                                        name: tool_use.name.clone(),
                                                        input: tool_use.input.clone(),
                                                    };
                                                    if let Err(e) = tx.send(event).await {
                                                        tracing::warn!("Failed to send tool_use event: {}", e);
                                                    }
                                                }
                                            }
                                            ContentBlock::ToolResult(tool_result) => {
                                                tracing::debug!(
                                                    "ToolResult block from stream: {}",
                                                    tool_result.tool_use_id
                                                );

                                                if let Some(ref tx) = event_tx {
                                                    let content = tool_result.content.as_ref()
                                                        .map(|c| serde_json::to_string(c).unwrap_or_default())
                                                        .unwrap_or_default();
                                                    let event = StreamEvent::ToolResult {
                                                        tool_use_id: tool_result.tool_use_id.clone(),
                                                        content,
                                                        is_error: tool_result.is_error.unwrap_or(false),
                                                    };
                                                    if let Err(e) = tx.send(event).await {
                                                        tracing::warn!("Failed to send tool_result event: {}", e);
                                                    }
                                                }
                                            }
                                            ContentBlock::Thinking(thinking) => {
                                                tracing::debug!("Thinking: {} chars", thinking.thinking.len());

                                                if let Some(ref tx) = event_tx {
                                                    let event = StreamEvent::Thinking { content: thinking.thinking.clone() };
                                                    if let Err(e) = tx.send(event).await {
                                                        tracing::warn!("Failed to send thinking event: {}", e);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }

                                if let Message::Result {
                                    subtype,
                                    session_id: sess_id,
                                    is_error,
                                    result,
                                    ..
                                } = &message {
                                    tracing::info!(
                                        "Result message: subtype={}, is_error={}, session_id={}",
                                        subtype, is_error, sess_id
                                    );
                                    if let Some(result_text) = result {
                                        tracing::info!("Result text: {} chars", result_text.len());
                                    }
                                    // Capture SDK session_id for future resume (separate from our DB session_id)
                                    cc_session_id = Some(sess_id.clone());
                                    if *is_error {
                                        tracing::error!("Agent returned error result");
                                        status = AgentRunStatus::Failed;
                                    } else if subtype == "success" {
                                        tracing::info!("Agent completed successfully");
                                        status = AgentRunStatus::Completed;
                                    }

                                    if let Some(ref tx) = event_tx {
                                        let event = StreamEvent::Result {
                                            session_id: sess_id.clone(),
                                            status: subtype.clone(),
                                            is_error: *is_error,
                                        };
                                        if let Err(e) = tx.send(event).await {
                                            tracing::warn!("Failed to send result event: {}", e);
                                        }
                                    }

                                    tracing::info!("Breaking out of stream loop after Result message");
                                    break;
                                }
                            }
                            Err(e) => {
                                tracing::error!("Error receiving message #{}: {}", message_count, e);
                                status = AgentRunStatus::Failed;
                                break;
                            }
                        }
                    }

                    tracing::info!(
                        "Stream ended after {} messages, total time: {:?}",
                        message_count,
                        query_start.elapsed()
                    );
                }
            }
            Err(e) => {
                tracing::error!("Client connect failed after {:?}: {}", query_start.elapsed(), e);
                status = AgentRunStatus::Failed;
            }
        }

        // If we never got a result message, assume completed if we got output
        if status == AgentRunStatus::Running {
            tracing::warn!(
                "No Result message received, inferring status from output (parts={})",
                output_parts.len()
            );
            status = if output_parts.is_empty() {
                tracing::error!("No output received, marking as failed");
                AgentRunStatus::Failed
            } else {
                tracing::info!("Got {} output parts, marking as completed", output_parts.len());
                AgentRunStatus::Completed
            };
        }

        let completed_at = chrono::Utc::now().to_rfc3339();
        let output_summary = if output_parts.is_empty() {
            None
        } else {
            let full_output = output_parts.join("\n\n");
            if full_output.len() > 100000 {
                let end = (0..=100000).rev().find(|&i| full_output.is_char_boundary(i)).unwrap_or(0);
                Some(format!("{}...\n\n[Output truncated]", &full_output[..end]))
            } else {
                Some(full_output)
            }
        };

        tracing::info!(
            "Agent run complete: status={:?}, output_len={}, session={}",
            status,
            output_summary.as_ref().map(|s| s.len()).unwrap_or(0),
            actual_session_id
        );

        // Parse email output if this is an email agent
        let email_output = if agent_type == AgentType::Email {
            output_summary.as_ref().and_then(|s| EmailOutput::parse(s))
        } else {
            None
        };

        tracing::info!("[AGENT-RUN] tool_call_count={}", tool_call_count);

        let agent_run = AgentRun {
            session_id: actual_session_id,
            organization: None,
            ticket_id: Some(ticket_context.ticket_id),
            epic_id: Some(ticket_context.epic_id),
            slice_id: Some(ticket_context.slice_id),
            agent_type: agent_type.as_str().to_string(),
            status,
            started_at,
            completed_at: Some(completed_at),
            input_message: ticket_context.intent,
            output_summary,
            email_output,
            tool_call_count,
            cc_session_id,
        };

        Ok((agent_run, sdk_client))
    }
}
