use axum::response::sse::{Event, KeepAlive, Sse};
use futures::stream::Stream;
use std::convert::Infallible;
use std::sync::Arc;
use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use async_stream::stream;
use sqlx::SqlitePool;
use cc_sdk::{query, ClaudeCodeOptions, Message, ContentBlock, ToolsConfig};
use futures::StreamExt;
use ticketing_system::{conversations, checkpoints, AddMessageRequest, ToolUse, UpdateConversationRequest};

use crate::agents::{AgentType, StreamEvent};
use crate::agents::prompts::load_prompt;

/// How often to flush accumulated content to the database (ms)
const DB_FLUSH_INTERVAL_MS: u64 = 2000;

pub type SseStream = Sse<Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>>;

/// Configuration for a chat SSE endpoint
pub struct ChatConfig {
    pub agent_type: AgentType,
    pub prompt_name: &'static str,
    pub working_dir: PathBuf,
    pub prompt_vars: HashMap<String, String>,
}

/// Start a new chat session via SSE
pub fn chat(
    db: Arc<SqlitePool>,
    message: String,
    session_id: Option<String>,
    conversation_id: Option<String>,
    config: ChatConfig,
) -> SseStream {
    let (tx, rx) = mpsc::channel::<StreamEvent>(100);

    let session_id = session_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let session_id_clone = session_id.clone();

    tokio::spawn(async move {
        tracing::info!("[CHAT] Background task started for {} session: {}", config.prompt_name, session_id_clone);

        let system_prompt = match load_prompt(config.prompt_name, config.prompt_vars) {
            Ok(prompt) => prompt,
            Err(e) => {
                tracing::error!("[CHAT] Failed to load {} prompt: {}", config.prompt_name, e);
                let _ = tx.send(StreamEvent::Status {
                    status: "failed".to_string(),
                    message: Some(format!("Failed to load prompt: {}", e)),
                }).await;
                return;
            }
        };

        let tools_list: Vec<String> = config.agent_type
            .allowed_tools()
            .iter()
            .map(|s| s.to_string())
            .collect();

        let options = ClaudeCodeOptions::builder()
            .system_prompt(&system_prompt)
            .model(config.agent_type.model())
            .tools(ToolsConfig::list(tools_list.clone()))
            .allowed_tools(tools_list)
            .cwd(&config.working_dir)
            .build();

        let _ = tx.send(StreamEvent::Status {
            status: "running".to_string(),
            message: Some(format!("Session: {}", session_id_clone)),
        }).await;

        run_stream(
            &db, tx, &message, options,
            conversation_id.as_deref(), None,
        ).await;
    });

    create_sse_stream(rx)
}

/// Resume an existing chat session via SSE
pub fn resume(
    db: Arc<SqlitePool>,
    message: String,
    session_id: String,
    conversation_id: Option<String>,
    config: ChatConfig,
) -> SseStream {
    let (tx, rx) = mpsc::channel::<StreamEvent>(100);

    let session_id_clone = session_id.clone();

    tokio::spawn(async move {
        tracing::info!("[RESUME] Background task started for {} session: {}", config.prompt_name, session_id_clone);

        let tools_list: Vec<String> = config.agent_type
            .allowed_tools()
            .iter()
            .map(|s| s.to_string())
            .collect();

        let options = ClaudeCodeOptions::builder()
            .resume(session_id_clone.clone())
            .tools(ToolsConfig::list(tools_list.clone()))
            .allowed_tools(tools_list)
            .cwd(&config.working_dir)
            .build();

        let _ = tx.send(StreamEvent::Status {
            status: "running".to_string(),
            message: Some("Resuming conversation...".to_string()),
        }).await;

        run_stream(
            &db, tx, &message, options,
            conversation_id.as_deref(), Some(&session_id_clone),
        ).await;
    });

    create_sse_stream(rx)
}

/// Core streaming logic shared between chat and resume
async fn run_stream(
    db: &SqlitePool,
    tx: mpsc::Sender<StreamEvent>,
    message: &str,
    options: ClaudeCodeOptions,
    conversation_id: Option<&str>,
    known_session_id: Option<&str>,
) {
    // Create initial checkpoint
    if let Some(conv_id) = conversation_id {
        let checkpoint_session = known_session_id.unwrap_or("pending");
        if let Err(e) = checkpoints::upsert_checkpoint(db, conv_id, checkpoint_session, 0).await {
            tracing::warn!("[STREAM] Failed to create initial checkpoint: {}", e);
        }
    }

    // Create initial assistant message in DB
    let mut assistant_message_id: Option<String> = None;
    if let Some(conv_id) = conversation_id {
        match conversations::add_message(
            db,
            conv_id,
            AddMessageRequest {
                role: "assistant".to_string(),
                content: String::new(),
                tool_uses: None,
            },
        ).await {
            Ok(msg) => {
                assistant_message_id = Some(msg.id);
            }
            Err(e) => {
                tracing::error!("[STREAM] Failed to create assistant message: {}", e);
            }
        }
    }

    match query(message, Some(options)).await {
        Ok(stream) => {
            let mut stream = Box::pin(stream);
            let mut message_count = 0u32;

            let mut accumulated_text = String::new();
            let mut accumulated_tool_uses: Vec<ToolUse> = Vec::new();
            let mut last_flush = Instant::now();
            let flush_interval = Duration::from_millis(DB_FLUSH_INTERVAL_MS);
            let mut tool_call_count: i32 = 0;
            let captured_session_id: Option<String> = known_session_id.map(|s| s.to_string());

            while let Some(message_result) = stream.next().await {
                message_count += 1;
                match message_result {
                    Ok(message) => {
                        if let Message::Assistant { message: assistant_msg } = &message {
                            for block in &assistant_msg.content {
                                match block {
                                    ContentBlock::Text(text_content) => {
                                        accumulated_text.push_str(&text_content.text);
                                        let _ = tx.send(StreamEvent::Text {
                                            content: text_content.text.clone()
                                        }).await;
                                    }
                                    ContentBlock::ToolUse(tool_use) => {
                                        tool_call_count += 1;
                                        tracing::info!("[STREAM] Tool use #{}: {} ({})", tool_call_count, tool_use.name, tool_use.id);
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
                                        if let Some(tu) = accumulated_tool_uses.iter_mut().find(|t| t.id == tool_result.tool_use_id) {
                                            tu.result = Some(content.clone());
                                            tu.is_error = tool_result.is_error;
                                        }
                                        let _ = tx.send(StreamEvent::ToolResult {
                                            tool_use_id: tool_result.tool_use_id.clone(),
                                            content,
                                            is_error: tool_result.is_error.unwrap_or(false),
                                        }).await;

                                        // Checkpoint after each tool result
                                        if let (Some(sess_id), Some(conv_id)) = (&captured_session_id, conversation_id) {
                                            if let Err(e) = checkpoints::upsert_checkpoint(db, conv_id, sess_id, tool_call_count).await {
                                                tracing::warn!("[STREAM] Failed to checkpoint after tool result: {}", e);
                                            }
                                        }
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
                            flush_to_db(db, assistant_message_id.as_deref(), &accumulated_text, &accumulated_tool_uses).await;
                            last_flush = Instant::now();
                        }

                        // Check for result message
                        if let Message::Result { session_id: sess_id, is_error, subtype, .. } = &message {
                            tracing::info!("[STREAM] Result: subtype={}, is_error={}", subtype, is_error);

                            let _ = tx.send(StreamEvent::Result {
                                session_id: sess_id.clone(),
                                status: subtype.clone(),
                                is_error: *is_error,
                            }).await;

                            // Update conversation with session_id and mark checkpoint completed
                            if let Some(conv_id) = conversation_id {
                                let _ = conversations::update_conversation(
                                    db,
                                    conv_id,
                                    UpdateConversationRequest {
                                        title: None,
                                        session_id: Some(sess_id.clone()),
                                    },
                                ).await;

                                if let Err(e) = checkpoints::upsert_checkpoint(db, conv_id, sess_id, tool_call_count).await {
                                    tracing::warn!("[STREAM] Failed to update checkpoint with session_id: {}", e);
                                }
                                if let Err(e) = checkpoints::mark_completed(db, conv_id).await {
                                    tracing::warn!("[STREAM] Failed to mark checkpoint completed: {}", e);
                                }
                            }
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::error!("[STREAM] Error receiving message #{}: {}", message_count, e);
                        let _ = tx.send(StreamEvent::Status {
                            status: "failed".to_string(),
                            message: Some(format!("Error: {}", e)),
                        }).await;
                        break;
                    }
                }
            }

            // Final flush to DB
            flush_to_db(db, assistant_message_id.as_deref(), &accumulated_text, &accumulated_tool_uses).await;

            tracing::info!("[STREAM] Stream ended after {} messages", message_count);
            let _ = tx.send(StreamEvent::Status {
                status: "completed".to_string(),
                message: None,
            }).await;
        }
        Err(e) => {
            tracing::error!("[STREAM] Failed to start query: {}", e);
            let _ = tx.send(StreamEvent::Status {
                status: "failed".to_string(),
                message: Some(format!("Failed to start: {}", e)),
            }).await;
        }
    }
}

/// Flush accumulated content to the database
async fn flush_to_db(
    db: &SqlitePool,
    assistant_message_id: Option<&str>,
    accumulated_text: &str,
    accumulated_tool_uses: &[ToolUse],
) {
    if let Some(msg_id) = assistant_message_id {
        let tool_uses_opt = if accumulated_tool_uses.is_empty() {
            None
        } else {
            Some(accumulated_tool_uses)
        };
        if let Err(e) = conversations::update_message(
            db,
            msg_id,
            accumulated_text,
            tool_uses_opt,
        ).await {
            tracing::error!("[STREAM] Failed to flush message to DB: {}", e);
        }
    }
}

/// Create SSE stream from receiver
pub fn create_sse_stream(rx: mpsc::Receiver<StreamEvent>) -> SseStream {
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

/// Create an error SSE stream
pub fn create_error_sse(message: String) -> SseStream {
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
