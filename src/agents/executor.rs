use cc_sdk::{query, ClaudeCodeOptions, Message, ContentBlock};
use futures::StreamExt;
use tokio::sync::mpsc;
use anyhow::{Result, Context};
use std::collections::HashMap;
use std::path::PathBuf;

use super::{AgentType, AgentRun, AgentRunStatus, TicketContext, StreamEvent};
use super::prompts::load_prompt;

/// Executes agents using the Claude Code CLI via cc-sdk.
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
    pub async fn execute(
        &self,
        agent_type: AgentType,
        ticket_context: TicketContext,
        previous_output: Option<String>,
        event_tx: Option<mpsc::Sender<StreamEvent>>,
    ) -> Result<AgentRun> {
        let started_at = chrono::Utc::now().to_rfc3339();
        let session_id = uuid::Uuid::new_v4().to_string();

        // Build prompt variables
        let mut vars = HashMap::new();
        vars.insert("epic_id".to_string(), ticket_context.epic_id.clone());
        vars.insert("slice_id".to_string(), ticket_context.slice_id.clone());
        vars.insert("ticket_id".to_string(), ticket_context.ticket_id.clone());
        vars.insert("ticket_title".to_string(), ticket_context.title.clone());
        vars.insert("ticket_intent".to_string(), ticket_context.intent.clone());

        // Add previous output for chaining
        if let Some(prev) = &previous_output {
            vars.insert("previous_output".to_string(), prev.clone());
            // Also set specific variables based on agent type
            match agent_type {
                AgentType::Planning => {
                    vars.insert("research_output".to_string(), prev.clone());
                }
                AgentType::Execution => {
                    vars.insert("plan_output".to_string(), prev.clone());
                }
                AgentType::Evaluation => {
                    vars.insert("execution_output".to_string(), prev.clone());
                }
                _ => {}
            }
        }

        // Load system prompt for this agent type
        let system_prompt = load_prompt(agent_type.as_str(), vars)
            .context("Failed to load agent prompt")?;

        // Build cc-sdk options using builder pattern
        let allowed_tools: Vec<String> = agent_type
            .allowed_tools()
            .iter()
            .map(|s| s.to_string())
            .collect();

        // Log what we're about to do
        tracing::info!(
            "Starting agent execution: type={}, ticket={}, model={}",
            agent_type.as_str(),
            ticket_context.ticket_id,
            agent_type.model()
        );
        tracing::info!("System prompt length: {} chars", system_prompt.len());
        tracing::info!("Working dir: {:?}", self.working_dir);
        tracing::info!("Allowed tools: {:?}", allowed_tools);
        tracing::info!("Max turns: {:?}", agent_type.max_turns());

        // Build options
        let mut builder = ClaudeCodeOptions::builder()
            .system_prompt(&system_prompt)
            .model(agent_type.model())
            .allowed_tools(allowed_tools)
            .cwd(&self.working_dir);

        // Only set max_turns if configured (otherwise unlimited)
        if let Some(turns) = agent_type.max_turns() {
            builder = builder.max_turns(turns);
        }

        let options = builder.build();

        // The initial prompt is the ticket intent
        let prompt = format!(
            "Work on this ticket:\n\nTitle: {}\nIntent: {}",
            ticket_context.title,
            ticket_context.intent
        );

        // Execute the query and collect output
        let mut output_parts = Vec::new();
        let mut status = AgentRunStatus::Running;
        let mut actual_session_id = session_id.clone();

        tracing::info!("Calling cc-sdk query...");
        let query_start = std::time::Instant::now();

        match query(prompt.as_str(), Some(options)).await {
            Ok(stream) => {
                tracing::info!("cc-sdk query returned stream in {:?}", query_start.elapsed());
                let mut stream = Box::pin(stream);
                let mut message_count = 0u32;

                while let Some(message_result) = stream.next().await {
                    message_count += 1;
                    match message_result {
                        Ok(message) => {
                            // Log message type for debugging
                            let msg_type = match &message {
                                Message::System { .. } => "System",
                                Message::Assistant { .. } => "Assistant",
                                Message::User { .. } => "User",
                                Message::Result { .. } => "Result",
                            };
                            tracing::info!("Received message #{}: type={}", message_count, msg_type);

                            // Track pending tool for synthetic result generation
                            // The CLI doesn't emit tool results directly - we infer completion
                            // when we see text output after a tool use

                            // Extract content from assistant messages
                            if let Message::Assistant { message: assistant_msg } = &message {
                                for block in &assistant_msg.content {
                                    match block {
                                        ContentBlock::Text(text_content) => {
                                            tracing::debug!("Assistant text: {} chars", text_content.text.len());
                                            output_parts.push(text_content.text.clone());

                                            // Forward structured event if provided
                                            if let Some(ref tx) = event_tx {
                                                let event = StreamEvent::Text { content: text_content.text.clone() };
                                                if let Err(e) = tx.send(event).await {
                                                    tracing::warn!("Failed to send text event: {}", e);
                                                }
                                            }
                                        }
                                        ContentBlock::ToolUse(tool_use) => {
                                            tracing::info!("Tool use: {} ({})", tool_use.name, tool_use.id);

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
                                            // Note: cc-sdk/CLI rarely emits this directly
                                            // Tool results are usually handled internally by the CLI
                                            tracing::info!("Tool result for: {} (content: {})",
                                                tool_result.tool_use_id,
                                                tool_result.content.is_some());

                                            if let Some(ref tx) = event_tx {
                                                let content = match &tool_result.content {
                                                    Some(cc_sdk::ContentValue::Text(s)) => s.clone(),
                                                    Some(cc_sdk::ContentValue::Structured(vals)) => {
                                                        serde_json::to_string(vals).unwrap_or_default()
                                                    }
                                                    None => String::new(),
                                                };

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

                            // Check for result message to capture session info and status
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
                                actual_session_id = sess_id.clone();
                                if *is_error {
                                    tracing::error!("Agent returned error result");
                                    status = AgentRunStatus::Failed;
                                } else if subtype == "success" {
                                    tracing::info!("Agent completed successfully");
                                    status = AgentRunStatus::Completed;
                                }

                                // Send result event
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

                                // Result message means we're done - break out of the loop
                                // The cc-sdk stream may not close automatically after Result
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
            Err(e) => {
                tracing::error!("Failed to start agent query after {:?}: {}", query_start.elapsed(), e);
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
            // Truncate if too long
            let full_output = output_parts.join("\n\n");
            if full_output.len() > 10000 {
                Some(format!("{}...\n\n[Output truncated]", &full_output[..10000]))
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

        Ok(AgentRun {
            session_id: actual_session_id,
            ticket_id: ticket_context.ticket_id,
            epic_id: ticket_context.epic_id,
            slice_id: ticket_context.slice_id,
            agent_type,
            status,
            started_at,
            completed_at: Some(completed_at),
            input_message: ticket_context.intent,
            output_summary,
        })
    }

    /// Resume an existing session with a new message.
    pub async fn resume(
        &self,
        session_id: &str,
        message: &str,
        message_tx: Option<mpsc::Sender<String>>,
    ) -> Result<Vec<String>> {
        let options = ClaudeCodeOptions::builder()
            .resume(session_id.to_string())
            .cwd(&self.working_dir)
            .build();

        let mut output_parts = Vec::new();

        match query(message, Some(options)).await {
            Ok(stream) => {
                let mut stream = Box::pin(stream);

                while let Some(message_result) = stream.next().await {
                    match message_result {
                        Ok(message) => {
                            if let Message::Assistant { message: assistant_msg } = &message {
                                for block in &assistant_msg.content {
                                    if let ContentBlock::Text(text_content) = block {
                                        output_parts.push(text_content.text.clone());

                                        if let Some(ref tx) = message_tx {
                                            let _ = tx.send(text_content.text.clone()).await;
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!("Error receiving message: {}", e);
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                return Err(anyhow::anyhow!("Failed to resume session: {}", e));
            }
        }

        Ok(output_parts)
    }
}
