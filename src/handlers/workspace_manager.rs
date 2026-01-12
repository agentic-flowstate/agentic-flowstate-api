use axum::{
    extract::State,
    response::sse::{Event, KeepAlive, Sse},
    Json,
};
use futures::stream::Stream;
use std::convert::Infallible;
use std::sync::Arc;
use std::path::PathBuf;
use std::collections::HashMap;
use std::pin::Pin;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use async_stream::stream;
use sqlx::SqlitePool;
use serde::Deserialize;
use cc_sdk::{query, ClaudeCodeOptions, Message, ContentBlock, ToolsConfig};
use futures::StreamExt;
use ticketing_system::{conversations, AddMessageRequest, ToolUse, UpdateConversationRequest};

use crate::agents::{AgentType, StreamEvent};
use crate::agents::prompts::load_prompt;

/// Request to start or continue a workspace manager conversation
#[derive(Debug, Deserialize)]
pub struct WorkspaceManagerRequest {
    /// The user's message
    pub message: String,
    /// Organization to create tickets in (required)
    pub organization: String,
    /// Session ID if continuing an existing conversation
    pub session_id: Option<String>,
    /// Conversation ID for persisting messages (required for message persistence)
    pub conversation_id: Option<String>,
}

/// How often to flush accumulated content to the database (ms)
const DB_FLUSH_INTERVAL_MS: u64 = 2000;

type SseStream = Sse<Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>>;

/// POST /api/workspace-manager/chat
///
/// SSE endpoint for workspace manager conversation.
/// Creates or continues a conversation about ticket planning.
/// Messages are persisted to the database incrementally during streaming.
pub async fn workspace_manager_chat(
    State(db): State<Arc<SqlitePool>>,
    Json(req): Json<WorkspaceManagerRequest>,
) -> SseStream {
    tracing::info!("=== WORKSPACE_MANAGER_CHAT START ===");
    tracing::info!("Organization: {}, Message: {}...", req.organization, &req.message[..req.message.len().min(50)]);
    tracing::info!("Conversation ID: {:?}", req.conversation_id);

    let (tx, rx) = mpsc::channel::<StreamEvent>(100);

    // Generate or use provided session ID
    let session_id = req.session_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let session_id_clone = session_id.clone();
    let conversation_id = req.conversation_id.clone();

    tracing::info!("Session ID: {}", session_id);

    // Spawn the agent execution in background
    tokio::spawn(async move {
        tracing::info!("[SPAWN] Background task started for workspace manager session: {}", session_id_clone);

        // Build prompt variables
        let mut vars = HashMap::new();
        vars.insert("organization".to_string(), req.organization.clone());

        // Load system prompt
        let system_prompt = match load_prompt("workspace-manager", vars) {
            Ok(prompt) => prompt,
            Err(e) => {
                tracing::error!("[SPAWN] Failed to load workspace-manager prompt: {}", e);
                let _ = tx.send(StreamEvent::Status {
                    status: "failed".to_string(),
                    message: Some(format!("Failed to load prompt: {}", e)),
                }).await;
                return;
            }
        };

        // Get agent config for tools
        let agent_type = AgentType::WorkspaceManager;
        let tools_list: Vec<String> = agent_type
            .allowed_tools()
            .iter()
            .map(|s| s.to_string())
            .collect();

        let working_dir = PathBuf::from("/Users/jarvisgpt/projects");

        tracing::info!("[SPAWN] Tools config: {:?}", tools_list);

        // Build cc-sdk options
        // Use ToolsConfig to actually restrict which tools are available (not just auto-approval)
        let options = ClaudeCodeOptions::builder()
            .system_prompt(&system_prompt)
            .model(agent_type.model())
            .tools(ToolsConfig::list(tools_list.clone()))
            .allowed_tools(tools_list) // Also auto-approve these tools
            .cwd(&working_dir)
            .build();

        // Send running status
        let _ = tx.send(StreamEvent::Status {
            status: "running".to_string(),
            message: Some(format!("Session: {}", session_id_clone)),
        }).await;

        tracing::info!("[SPAWN] Calling cc-sdk query...");

        match query(req.message.as_str(), Some(options)).await {
            Ok(stream) => {
                tracing::info!("[SPAWN] cc-sdk query returned stream");
                let mut stream = Box::pin(stream);
                let mut message_count = 0u32;

                // Accumulate content for DB persistence
                let mut accumulated_text = String::new();
                let mut accumulated_tool_uses: Vec<ToolUse> = Vec::new();
                let mut assistant_message_id: Option<String> = None;
                let mut last_flush = Instant::now();
                let flush_interval = Duration::from_millis(DB_FLUSH_INTERVAL_MS);

                // Create initial assistant message in DB if we have a conversation_id
                if let Some(conv_id) = &conversation_id {
                    match conversations::add_message(
                        &db,
                        conv_id,
                        AddMessageRequest {
                            role: "assistant".to_string(),
                            content: String::new(),
                            tool_uses: None,
                        },
                    ).await {
                        Ok(msg) => {
                            assistant_message_id = Some(msg.id);
                            tracing::info!("[SPAWN] Created assistant message: {:?}", assistant_message_id);
                        }
                        Err(e) => {
                            tracing::error!("[SPAWN] Failed to create assistant message: {}", e);
                        }
                    }
                }

                while let Some(message_result) = stream.next().await {
                    message_count += 1;
                    match message_result {
                        Ok(message) => {
                            // Extract content from assistant messages
                            if let Message::Assistant { message: assistant_msg } = &message {
                                for block in &assistant_msg.content {
                                    match block {
                                        ContentBlock::Text(text_content) => {
                                            tracing::debug!("[SPAWN] Text: {} chars", text_content.text.len());
                                            accumulated_text.push_str(&text_content.text);
                                            let _ = tx.send(StreamEvent::Text {
                                                content: text_content.text.clone()
                                            }).await;
                                        }
                                        ContentBlock::ToolUse(tool_use) => {
                                            tracing::info!("[SPAWN] Tool use: {} ({})", tool_use.name, tool_use.id);
                                            accumulated_tool_uses.push(ToolUse {
                                                id: tool_use.id.clone(),
                                                name: tool_use.name.clone(),
                                                input: Some(tool_use.input.clone()),
                                                result: None,
                                                is_error: None,
                                            });
                                            let _ = tx.send(StreamEvent::ToolUse {
                                                id: tool_use.id.clone(),
                                                name: tool_use.name.clone(),
                                                input: tool_use.input.clone(),
                                            }).await;
                                        }
                                        ContentBlock::ToolResult(tool_result) => {
                                            let content = match &tool_result.content {
                                                Some(cc_sdk::ContentValue::Text(s)) => s.clone(),
                                                Some(cc_sdk::ContentValue::Structured(vals)) => {
                                                    serde_json::to_string(vals).unwrap_or_default()
                                                }
                                                None => String::new(),
                                            };
                                            // Update the corresponding tool use with result
                                            if let Some(tu) = accumulated_tool_uses.iter_mut().find(|t| t.id == tool_result.tool_use_id) {
                                                tu.result = Some(content.clone());
                                                tu.is_error = tool_result.is_error;
                                            }
                                            let _ = tx.send(StreamEvent::ToolResult {
                                                tool_use_id: tool_result.tool_use_id.clone(),
                                                content,
                                                is_error: tool_result.is_error.unwrap_or(false),
                                            }).await;
                                        }
                                        ContentBlock::Thinking(thinking) => {
                                            let _ = tx.send(StreamEvent::Thinking {
                                                content: thinking.thinking.clone()
                                            }).await;
                                        }
                                    }
                                }
                            }

                            // Periodically flush to DB
                            if last_flush.elapsed() >= flush_interval {
                                if let (Some(msg_id), Some(_conv_id)) = (&assistant_message_id, &conversation_id) {
                                    let tool_uses_opt = if accumulated_tool_uses.is_empty() {
                                        None
                                    } else {
                                        Some(accumulated_tool_uses.as_slice())
                                    };
                                    if let Err(e) = conversations::update_message(
                                        &db,
                                        msg_id,
                                        &accumulated_text,
                                        tool_uses_opt,
                                    ).await {
                                        tracing::error!("[SPAWN] Failed to flush message to DB: {}", e);
                                    } else {
                                        tracing::debug!("[SPAWN] Flushed message to DB ({} chars, {} tools)", accumulated_text.len(), accumulated_tool_uses.len());
                                    }
                                }
                                last_flush = Instant::now();
                            }

                            // Check for result message
                            if let Message::Result { session_id: sess_id, is_error, subtype, .. } = &message {
                                tracing::info!("[SPAWN] Result: subtype={}, is_error={}", subtype, is_error);
                                let _ = tx.send(StreamEvent::Result {
                                    session_id: sess_id.clone(),
                                    status: subtype.clone(),
                                    is_error: *is_error,
                                }).await;

                                // Update conversation with session_id
                                if let Some(conv_id) = &conversation_id {
                                    let _ = conversations::update_conversation(
                                        &db,
                                        conv_id,
                                        UpdateConversationRequest {
                                            title: None,
                                            session_id: Some(sess_id.clone()),
                                        },
                                    ).await;
                                }
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::error!("[SPAWN] Error receiving message #{}: {}", message_count, e);
                            let _ = tx.send(StreamEvent::Status {
                                status: "failed".to_string(),
                                message: Some(format!("Error: {}", e)),
                            }).await;
                            break;
                        }
                    }
                }

                // Final flush to DB
                if let (Some(msg_id), Some(_conv_id)) = (&assistant_message_id, &conversation_id) {
                    let tool_uses_opt = if accumulated_tool_uses.is_empty() {
                        None
                    } else {
                        Some(accumulated_tool_uses.as_slice())
                    };
                    if let Err(e) = conversations::update_message(
                        &db,
                        msg_id,
                        &accumulated_text,
                        tool_uses_opt,
                    ).await {
                        tracing::error!("[SPAWN] Failed to final flush message to DB: {}", e);
                    } else {
                        tracing::info!("[SPAWN] Final flush to DB ({} chars, {} tools)", accumulated_text.len(), accumulated_tool_uses.len());
                    }
                }

                tracing::info!("[SPAWN] Stream ended after {} messages", message_count);
                let _ = tx.send(StreamEvent::Status {
                    status: "completed".to_string(),
                    message: None,
                }).await;
            }
            Err(e) => {
                tracing::error!("[SPAWN] Failed to start query: {}", e);
                let _ = tx.send(StreamEvent::Status {
                    status: "failed".to_string(),
                    message: Some(format!("Failed to start: {}", e)),
                }).await;
            }
        }

        tracing::info!("[SPAWN] Background task exiting");
    });

    tracing::info!("=== WORKSPACE_MANAGER_CHAT returning SSE ===");
    create_sse_stream(rx)
}

/// POST /api/workspace-manager/resume
///
/// Resume an existing workspace manager session with a new message.
/// Messages are persisted to the database incrementally during streaming.
pub async fn workspace_manager_resume(
    State(db): State<Arc<SqlitePool>>,
    Json(req): Json<WorkspaceManagerRequest>,
) -> SseStream {
    tracing::info!("=== WORKSPACE_MANAGER_RESUME START ===");

    let session_id = match &req.session_id {
        Some(id) => id.clone(),
        None => {
            // No session ID provided - return error stream
            return create_error_sse("session_id is required for resume".to_string());
        }
    };

    tracing::info!("Resuming session: {}, Message: {}...", session_id, &req.message[..req.message.len().min(50)]);
    tracing::info!("Conversation ID: {:?}", req.conversation_id);

    let (tx, rx) = mpsc::channel::<StreamEvent>(100);
    let session_id_clone = session_id.clone();
    let conversation_id = req.conversation_id.clone();

    // Spawn the resume execution in background
    tokio::spawn(async move {
        tracing::info!("[RESUME] Background task started for session: {}", session_id_clone);

        let working_dir = PathBuf::from("/Users/jarvisgpt/projects");

        // Get agent config for tools - must re-specify for resume
        let agent_type = AgentType::WorkspaceManager;
        let tools_list: Vec<String> = agent_type
            .allowed_tools()
            .iter()
            .map(|s| s.to_string())
            .collect();

        tracing::info!("[RESUME] Tools config: {:?}", tools_list);

        // Use ToolsConfig to actually restrict which tools are available
        let options = ClaudeCodeOptions::builder()
            .resume(session_id_clone.clone())
            .tools(ToolsConfig::list(tools_list.clone()))
            .allowed_tools(tools_list)
            .cwd(&working_dir)
            .build();

        let _ = tx.send(StreamEvent::Status {
            status: "running".to_string(),
            message: Some("Resuming conversation...".to_string()),
        }).await;

        tracing::info!("[RESUME] Calling cc-sdk query with message: {}...", &req.message[..req.message.len().min(50)]);

        match query(req.message.as_str(), Some(options)).await {
            Ok(stream) => {
                tracing::info!("[RESUME] cc-sdk query returned stream");
                let mut stream = Box::pin(stream);
                let mut message_count = 0u32;

                // Accumulate content for DB persistence
                let mut accumulated_text = String::new();
                let mut accumulated_tool_uses: Vec<ToolUse> = Vec::new();
                let mut assistant_message_id: Option<String> = None;
                let mut last_flush = Instant::now();
                let flush_interval = Duration::from_millis(DB_FLUSH_INTERVAL_MS);

                // Create initial assistant message in DB if we have a conversation_id
                if let Some(conv_id) = &conversation_id {
                    match conversations::add_message(
                        &db,
                        conv_id,
                        AddMessageRequest {
                            role: "assistant".to_string(),
                            content: String::new(),
                            tool_uses: None,
                        },
                    ).await {
                        Ok(msg) => {
                            assistant_message_id = Some(msg.id);
                            tracing::info!("[RESUME] Created assistant message: {:?}", assistant_message_id);
                        }
                        Err(e) => {
                            tracing::error!("[RESUME] Failed to create assistant message: {}", e);
                        }
                    }
                }

                while let Some(message_result) = stream.next().await {
                    message_count += 1;
                    match message_result {
                        Ok(message) => {
                            let msg_type = match &message {
                                Message::System { .. } => "System",
                                Message::Assistant { .. } => "Assistant",
                                Message::User { .. } => "User",
                                Message::Result { .. } => "Result",
                            };
                            tracing::debug!("[RESUME] Message #{}: type={}", message_count, msg_type);

                            if let Message::Assistant { message: assistant_msg } = &message {
                                for block in &assistant_msg.content {
                                    match block {
                                        ContentBlock::Text(text_content) => {
                                            tracing::debug!("[RESUME] Text: {} chars", text_content.text.len());
                                            accumulated_text.push_str(&text_content.text);
                                            let _ = tx.send(StreamEvent::Text {
                                                content: text_content.text.clone()
                                            }).await;
                                        }
                                        ContentBlock::ToolUse(tool_use) => {
                                            tracing::info!("[RESUME] Tool use: {} ({})", tool_use.name, tool_use.id);
                                            accumulated_tool_uses.push(ToolUse {
                                                id: tool_use.id.clone(),
                                                name: tool_use.name.clone(),
                                                input: Some(tool_use.input.clone()),
                                                result: None,
                                                is_error: None,
                                            });
                                            let _ = tx.send(StreamEvent::ToolUse {
                                                id: tool_use.id.clone(),
                                                name: tool_use.name.clone(),
                                                input: tool_use.input.clone(),
                                            }).await;
                                        }
                                        ContentBlock::ToolResult(tool_result) => {
                                            let content = match &tool_result.content {
                                                Some(cc_sdk::ContentValue::Text(s)) => s.clone(),
                                                Some(cc_sdk::ContentValue::Structured(vals)) => {
                                                    serde_json::to_string(vals).unwrap_or_default()
                                                }
                                                None => String::new(),
                                            };
                                            // Update the corresponding tool use with result
                                            if let Some(tu) = accumulated_tool_uses.iter_mut().find(|t| t.id == tool_result.tool_use_id) {
                                                tu.result = Some(content.clone());
                                                tu.is_error = tool_result.is_error;
                                            }
                                            let _ = tx.send(StreamEvent::ToolResult {
                                                tool_use_id: tool_result.tool_use_id.clone(),
                                                content,
                                                is_error: tool_result.is_error.unwrap_or(false),
                                            }).await;
                                        }
                                        ContentBlock::Thinking(thinking) => {
                                            let _ = tx.send(StreamEvent::Thinking {
                                                content: thinking.thinking.clone()
                                            }).await;
                                        }
                                    }
                                }
                            }

                            // Periodically flush to DB
                            if last_flush.elapsed() >= flush_interval {
                                if let (Some(msg_id), Some(_conv_id)) = (&assistant_message_id, &conversation_id) {
                                    let tool_uses_opt = if accumulated_tool_uses.is_empty() {
                                        None
                                    } else {
                                        Some(accumulated_tool_uses.as_slice())
                                    };
                                    if let Err(e) = conversations::update_message(
                                        &db,
                                        msg_id,
                                        &accumulated_text,
                                        tool_uses_opt,
                                    ).await {
                                        tracing::error!("[RESUME] Failed to flush message to DB: {}", e);
                                    } else {
                                        tracing::debug!("[RESUME] Flushed message to DB ({} chars, {} tools)", accumulated_text.len(), accumulated_tool_uses.len());
                                    }
                                }
                                last_flush = Instant::now();
                            }

                            if let Message::Result { session_id: sess_id, is_error, subtype, .. } = &message {
                                tracing::info!("[RESUME] Result: subtype={}, is_error={}, session_id={}", subtype, is_error, sess_id);
                                let _ = tx.send(StreamEvent::Result {
                                    session_id: sess_id.clone(),
                                    status: subtype.clone(),
                                    is_error: *is_error,
                                }).await;

                                // Update conversation with session_id
                                if let Some(conv_id) = &conversation_id {
                                    let _ = conversations::update_conversation(
                                        &db,
                                        conv_id,
                                        UpdateConversationRequest {
                                            title: None,
                                            session_id: Some(sess_id.clone()),
                                        },
                                    ).await;
                                }
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::error!("[RESUME] Error receiving message #{}: {}", message_count, e);
                            let _ = tx.send(StreamEvent::Status {
                                status: "failed".to_string(),
                                message: Some(format!("Error: {}", e)),
                            }).await;
                            break;
                        }
                    }
                }

                // Final flush to DB
                if let (Some(msg_id), Some(_conv_id)) = (&assistant_message_id, &conversation_id) {
                    let tool_uses_opt = if accumulated_tool_uses.is_empty() {
                        None
                    } else {
                        Some(accumulated_tool_uses.as_slice())
                    };
                    if let Err(e) = conversations::update_message(
                        &db,
                        msg_id,
                        &accumulated_text,
                        tool_uses_opt,
                    ).await {
                        tracing::error!("[RESUME] Failed to final flush message to DB: {}", e);
                    } else {
                        tracing::info!("[RESUME] Final flush to DB ({} chars, {} tools)", accumulated_text.len(), accumulated_tool_uses.len());
                    }
                }

                tracing::info!("[RESUME] Stream ended after {} messages", message_count);
                let _ = tx.send(StreamEvent::Status {
                    status: "completed".to_string(),
                    message: None,
                }).await;
            }
            Err(e) => {
                tracing::error!("[RESUME] Failed to start query: {}", e);
                let _ = tx.send(StreamEvent::Status {
                    status: "failed".to_string(),
                    message: Some(format!("Failed to resume: {}", e)),
                }).await;
            }
        }
        tracing::info!("[RESUME] Background task exiting");
    });

    create_sse_stream(rx)
}

/// Helper to create SSE stream from receiver
fn create_sse_stream(rx: mpsc::Receiver<StreamEvent>) -> SseStream {
    let stream = stream! {
        let mut rx = ReceiverStream::new(rx);
        while let Some(event) = futures::StreamExt::next(&mut rx).await {
            if let Ok(json) = serde_json::to_string(&event) {
                yield Ok(Event::default().data(json));
            }
        }
    };
    Sse::new(Box::pin(stream) as Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>)
        .keep_alive(KeepAlive::default())
}

/// Helper to create an error SSE stream
fn create_error_sse(message: String) -> SseStream {
    let stream = stream! {
        let event = StreamEvent::Status {
            status: "failed".to_string(),
            message: Some(message),
        };
        if let Ok(json) = serde_json::to_string(&event) {
            yield Ok(Event::default().data(json));
        }
    };
    Sse::new(Box::pin(stream) as Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>)
        .keep_alive(KeepAlive::default())
}
