use axum::response::sse::{Event, KeepAlive, Sse};
use futures::stream::Stream;
use std::convert::Infallible;
use std::sync::Arc;
use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc, RwLock};
use once_cell::sync::Lazy;
use tokio_stream::wrappers::ReceiverStream;
use async_stream::stream;
use sqlx::SqlitePool;
use cc_sdk::{ClaudeSDKClient, ClaudeCodeOptions, Message, ContentBlock, ToolsConfig, PermissionMode, McpServerConfig};
use futures::StreamExt;
use ticketing_system::{conversations, checkpoints, AddMessageRequest, ContentBlockDesc, UpdateConversationRequest};

use crate::agents::{AgentType, StreamEvent};
use crate::agents::prompts::load_prompt;
use super::chat_client_manager::{ChatClientManager, ChatClient};

/// How often to flush accumulated content to the database (ms)
const DB_FLUSH_INTERVAL_MS: u64 = 2000;

/// Global broadcaster for live conversation events.
/// Reconnect streams subscribe here instead of polling SQLite.
static CONVERSATION_BROADCASTER: Lazy<RwLock<HashMap<String, broadcast::Sender<(i32, String)>>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

/// Get or create a broadcast sender for a conversation.
async fn get_broadcast_sender(conversation_id: &str) -> broadcast::Sender<(i32, String)> {
    {
        let map = CONVERSATION_BROADCASTER.read().await;
        if let Some(tx) = map.get(conversation_id) {
            return tx.clone();
        }
    }
    let mut map = CONVERSATION_BROADCASTER.write().await;
    // Double-check after acquiring write lock
    if let Some(tx) = map.get(conversation_id) {
        return tx.clone();
    }
    let (tx, _) = broadcast::channel(256);
    map.insert(conversation_id.to_string(), tx.clone());
    tx
}

/// Remove a broadcast channel when conversation completes.
async fn remove_broadcast_channel(conversation_id: &str) {
    let mut map = CONVERSATION_BROADCASTER.write().await;
    map.remove(conversation_id);
}

pub type SseStream = Sse<Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>>;

/// Configuration for a chat SSE endpoint
pub struct ChatConfig {
    pub agent_type: AgentType,
    pub prompt_name: &'static str,
    pub working_dir: PathBuf,
    pub prompt_vars: HashMap<String, String>,
}

/// Start or continue a chat session via SSE using a persistent ClaudeSDKClient.
///
/// - If a client already exists for this conversation: reuse it (send_request)
/// - If no client exists but DB has a session_id: create client with .resume(session_id)
/// - If no client and no session_id: create fresh client
pub fn chat(
    db: Arc<SqlitePool>,
    manager: Arc<ChatClientManager>,
    message: String,
    conversation_id: Option<String>,
    config: ChatConfig,
    user_id: String,
) -> SseStream {
    let (tx, rx) = mpsc::channel::<StreamEvent>(100);

    // Persist events to conversation_events table for SSE reconnection
    let sse_rx = if let Some(ref conv_id) = conversation_id {
        spawn_conversation_event_persister((*db).clone(), conv_id.clone(), rx)
    } else {
        // No conversation — wrap events with sequential index
        wrap_with_index(rx)
    };

    tokio::spawn(async move {
        tracing::info!("[CHAT] Background task started for {}", config.prompt_name);

        let conv_id = match conversation_id {
            Some(id) => id,
            None => {
                tracing::error!("[CHAT] No conversation_id provided");
                let _ = tx.send(StreamEvent::Status {
                    status: "failed".to_string(),
                    message: Some("No conversation_id".to_string()),
                }).await;
                return;
            }
        };

        // Try to get existing connected client
        let client_arc = if let Some(existing) = manager.get(&conv_id).await {
            // Check if still connected
            let guard = existing.lock().await;
            if guard.sdk_client.is_connected().await {
                drop(guard);
                existing
            } else {
                drop(guard);
                tracing::info!("[CHAT] Client for {} disconnected, will recreate", conv_id);
                manager.remove(&conv_id).await;
                match create_client(&db, &conv_id, &config, &manager).await {
                    Ok(arc) => arc,
                    Err(e) => {
                        tracing::error!("[CHAT] Failed to create client for {}: {}", conv_id, e);
                        let _ = checkpoints::upsert_checkpoint(&db, &conv_id, "failed", 0).await;
                        let _ = tx.send(StreamEvent::Status {
                            status: "failed".to_string(),
                            message: Some(format!("Failed to start agent: {}", e)),
                        }).await;
                        return;
                    }
                }
            }
        } else {
            match create_client(&db, &conv_id, &config, &manager).await {
                Ok(arc) => arc,
                Err(e) => {
                    let _ = tx.send(StreamEvent::Status {
                        status: "failed".to_string(),
                        message: Some(format!("Failed to create client: {}", e)),
                    }).await;
                    return;
                }
            }
        };

        let _ = tx.send(StreamEvent::Status {
            status: "running".to_string(),
            message: Some("running".to_string()),
        }).await;

        // Store the user message in DB
        if let Err(e) = conversations::add_message(
            &db,
            &conv_id,
            AddMessageRequest {
                role: "user".to_string(),
                content: message.clone(),
            },
        ).await {
            tracing::error!("[CHAT] Failed to store user message: {}", e);
        }

        // Create checkpoint
        if let Err(e) = checkpoints::upsert_checkpoint(&db, &conv_id, "pending", 0).await {
            tracing::warn!("[CHAT] Failed to create checkpoint: {}", e);
        }

        // Create initial assistant message in DB
        let mut assistant_message_id: Option<String> = None;
        match conversations::add_message(
            &db,
            &conv_id,
            AddMessageRequest {
                role: "assistant".to_string(),
                content: String::new(),
            },
        ).await {
            Ok(msg) => assistant_message_id = Some(msg.id),
            Err(e) => tracing::error!("[CHAT] Failed to create assistant message: {}", e),
        }

        // Send message and stream response
        let mut client = client_arc.lock().await;
        client.last_used = Instant::now();

        if let Err(e) = client.sdk_client.send_user_message(message).await {
            tracing::error!("[CHAT] Failed to send message: {}", e);
            let _ = tx.send(StreamEvent::Status {
                status: "failed".to_string(),
                message: Some(format!("Failed to send: {}", e)),
            }).await;
            return;
        }

        let mut response_stream = client.sdk_client.receive_messages().await;
        let mut accumulated_text = String::new();
        let mut last_flush = Instant::now();
        let flush_interval = Duration::from_millis(DB_FLUSH_INTERVAL_MS);
        let mut tool_call_count: i32 = 0;
        let mut message_count = 0u32;
        let mut content_blocks: Vec<ContentBlockDesc> = Vec::new();
        let mut blocks_dirty = false;

        while let Some(msg_result) = response_stream.next().await {
            message_count += 1;
            match msg_result {
                Ok(msg) => {
                    if let Message::Assistant { message: assistant_msg } = &msg {
                        for block in &assistant_msg.content {
                            match block {
                                ContentBlock::Text(text_content) => {
                                    accumulated_text.push_str(&text_content.text);
                                    // Track block ordering: append to existing text block or start new one
                                    match content_blocks.last_mut() {
                                        Some(ContentBlockDesc::Text { text }) => {
                                            text.push_str(&text_content.text);
                                        }
                                        _ => {
                                            content_blocks.push(ContentBlockDesc::Text {
                                                text: text_content.text.clone(),
                                            });
                                            blocks_dirty = true;
                                        }
                                    }
                                    let _ = tx.send(StreamEvent::Text {
                                        content: text_content.text.clone()
                                    }).await;
                                }
                                ContentBlock::ToolUse(tool_use) => {
                                    tool_call_count += 1;
                                    tracing::info!("[CHAT] Tool use #{}: {} ({})", tool_call_count, tool_use.name, tool_use.id);
                                    // Track block ordering: append to existing tool group or start new one
                                    match content_blocks.last_mut() {
                                        Some(ContentBlockDesc::ToolGroup { tool_ids }) => {
                                            tool_ids.push(tool_use.id.clone());
                                        }
                                        _ => {
                                            content_blocks.push(ContentBlockDesc::ToolGroup {
                                                tool_ids: vec![tool_use.id.clone()],
                                            });
                                            blocks_dirty = true;
                                        }
                                    }

                                    // Insert tool call row immediately
                                    if let Some(msg_id) = &assistant_message_id {
                                        let now = chrono::Utc::now().timestamp();
                                        if let Err(e) = conversations::insert_tool_call(
                                            &db,
                                            &tool_use.id,
                                            msg_id,
                                            &conv_id,
                                            &tool_use.name,
                                            Some(&tool_use.input),
                                            now,
                                        ).await {
                                            tracing::error!("[CHAT] Failed to insert tool call: {}", e);
                                        }
                                    }

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

                                    // Update tool call row with result
                                    if let Err(e) = conversations::update_tool_call_result(
                                        &db,
                                        &tool_result.tool_use_id,
                                        &content,
                                        tool_result.is_error.unwrap_or(false),
                                    ).await {
                                        tracing::error!("[CHAT] Failed to update tool call result: {}", e);
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

                    // Periodically flush accumulated text to DB
                    if last_flush.elapsed() >= flush_interval {
                        flush_to_db(&db, assistant_message_id.as_deref(), &accumulated_text).await;
                        last_flush = Instant::now();
                    }

                    // Flush content blocks immediately when structure changes (new block type added)
                    // so REST clients see interleaved layout even while streaming
                    if blocks_dirty {
                        blocks_dirty = false;
                        if let Some(msg_id) = &assistant_message_id {
                            if content_blocks.len() > 1 {
                                let _ = conversations::update_message_blocks(&db, msg_id, &content_blocks).await;
                            }
                        }
                    }

                    // Check for result message
                    if let Message::Result { session_id: sess_id, is_error, subtype, .. } = &msg {
                        tracing::info!("[CHAT] Result: subtype={}, is_error={}", subtype, is_error);

                        // Save session_id on the client for future resume
                        client.session_id = Some(sess_id.clone());

                        let _ = tx.send(StreamEvent::Result {
                            session_id: sess_id.clone(),
                            status: subtype.clone(),
                            is_error: *is_error,
                        }).await;

                        // Update conversation with session_id
                        let _ = conversations::update_conversation(
                            &db,
                            &user_id,
                            &conv_id,
                            UpdateConversationRequest {
                                title: None,
                                session_id: Some(sess_id.clone()),
                            },
                        ).await;

                        if let Err(e) = checkpoints::upsert_checkpoint(&db, &conv_id, sess_id, tool_call_count).await {
                            tracing::warn!("[CHAT] Failed to update checkpoint: {}", e);
                        }
                        if let Err(e) = checkpoints::mark_completed(&db, &conv_id).await {
                            tracing::warn!("[CHAT] Failed to mark checkpoint completed: {}", e);
                        }

                        // Send push notification that agent completed
                        if let Some(apns) = crate::apns::ApnsService::global() {
                            let push_db = (*db).clone();
                            let push_user = user_id.clone();
                            let push_agent = config.prompt_name.to_string();
                            let push_conv_id = conv_id.clone();
                            let apns = apns.clone();
                            tokio::spawn(async move {
                                let _ = apns.send_to_user(
                                    &push_db,
                                    &push_user,
                                    "Agent finished",
                                    &format!("{} has completed processing.", push_agent),
                                    Some(&push_conv_id),
                                ).await;
                            });
                        }

                        break;
                    }
                }
                Err(e) => {
                    tracing::error!("[CHAT] Error receiving message #{}: {}", message_count, e);
                    let _ = tx.send(StreamEvent::Status {
                        status: "failed".to_string(),
                        message: Some(format!("Error: {}", e)),
                    }).await;
                    break;
                }
            }
        }

        // Final flush of text to DB
        flush_to_db(&db, assistant_message_id.as_deref(), &accumulated_text).await;

        // Store content block ordering so clients can reconstruct interleaved layout
        if let Some(msg_id) = &assistant_message_id {
            if content_blocks.len() > 1 {
                if let Err(e) = conversations::update_message_blocks(&db, msg_id, &content_blocks).await {
                    tracing::error!("[CHAT] Failed to store content blocks: {}", e);
                }
            }
        }

        tracing::info!("[CHAT] Stream ended after {} messages", message_count);
        let _ = tx.send(StreamEvent::Status {
            status: "completed".to_string(),
            message: None,
        }).await;
    });

    create_sse_stream(sse_rx)
}

/// Create a new ClaudeSDKClient, optionally resuming from a saved session.
async fn create_client(
    db: &SqlitePool,
    conv_id: &str,
    config: &ChatConfig,
    manager: &ChatClientManager,
) -> Result<Arc<tokio::sync::Mutex<ChatClient>>, String> {
    let system_prompt = load_prompt(config.prompt_name, config.prompt_vars.clone())
        .map_err(|e| format!("Failed to load prompt: {}", e))?;

    let tools_list: Vec<String> = config.agent_type
        .allowed_tools()
        .iter()
        .map(|s| s.to_string())
        .collect();

    let has_mcp_tools = tools_list.iter().any(|t| t.starts_with("mcp__"));
    tracing::info!(
        "[CHAT] Configuring {} tools for {}: {:?}",
        tools_list.len(),
        config.prompt_name,
        tools_list
    );

    // Check if conversation has a saved session_id for resume
    let saved_session_id = conversations::get_conversation(db, conv_id, false)
        .await
        .ok()
        .flatten()
        .and_then(|c| c.session_id);

    let mut builder = ClaudeCodeOptions::builder()
        .system_prompt(&system_prompt)
        .model(config.agent_type.model())
        .tools(ToolsConfig::list(tools_list.clone()))
        .allowed_tools(tools_list)
        .permission_mode(PermissionMode::BypassPermissions)
        .cwd(&config.working_dir);

    // Register MCP server so Claude CLI can resolve mcp__agentic-mcp__* tools
    if has_mcp_tools {
        let mcp_binary = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../agentic-flowstate-mcp/target/release/agentic_mcp");
        let mcp_binary = mcp_binary.canonicalize().unwrap_or(mcp_binary);
        tracing::info!("[CHAT] Registering MCP server: {}", mcp_binary.display());
        builder = builder.add_mcp_server(
            "agentic-mcp",
            McpServerConfig::Stdio {
                command: mcp_binary.to_string_lossy().to_string(),
                args: None,
                env: None,
            },
        );
    }

    // Enable adaptive thinking via --effort flag (Opus 4.6 recommended approach)
    builder = builder.add_extra_arg("effort", Some(config.agent_type.effort().to_string()));

    if let Some(ref sid) = saved_session_id {
        tracing::info!("[CHAT] Resuming conversation {} from session {}", conv_id, sid);
        builder = builder.resume(sid.clone());
    } else {
        tracing::info!("[CHAT] Creating fresh client for conversation {}", conv_id);
    }

    let options = builder.build();
    let mut sdk_client = ClaudeSDKClient::new(options);

    // Timeout the connect call — if the Claude CLI hangs (API unresponsive,
    // MCP server stuck), fail fast instead of blocking forever.
    tracing::info!("[CHAT] Connecting client for {} (30s timeout)...", conv_id);
    match tokio::time::timeout(Duration::from_secs(30), sdk_client.connect(None)).await {
        Ok(Ok(())) => {
            tracing::info!("[CHAT] Client connected for {}", conv_id);
        }
        Ok(Err(e)) => {
            tracing::error!("[CHAT] Client connect error for {}: {}", conv_id, e);
            return Err(format!("Failed to connect: {}", e));
        }
        Err(_) => {
            tracing::error!("[CHAT] Client connect timed out after 30s for {}", conv_id);
            // Kill the spawned process by dropping the client
            drop(sdk_client);
            return Err("Connection timed out — the agent failed to start within 30 seconds. Please try again.".to_string());
        }
    }

    let client = ChatClient {
        sdk_client,
        session_id: saved_session_id,
        last_used: Instant::now(),
    };

    Ok(manager.insert(conv_id.to_string(), client).await)
}

/// Flush accumulated text content to the database
async fn flush_to_db(
    db: &SqlitePool,
    assistant_message_id: Option<&str>,
    accumulated_text: &str,
) {
    if let Some(msg_id) = assistant_message_id {
        if let Err(e) = conversations::update_message(
            db,
            msg_id,
            accumulated_text,
        ).await {
            tracing::error!("[CHAT] Failed to flush message to DB: {}", e);
        }
    }
}

/// Create SSE stream from receiver
/// Indexed event: pairs an event with its sequential index for SSE `id:` field.
pub type IndexedStreamEvent = (i32, StreamEvent);

pub fn create_sse_stream(rx: mpsc::Receiver<IndexedStreamEvent>) -> SseStream {
    let stream = stream! {
        let mut rx = ReceiverStream::new(rx);
        while let Some((index, event)) = futures::StreamExt::next(&mut rx).await {
            if let Ok(json) = serde_json::to_string(&event) {
                yield Ok(Event::default().id(index.to_string()).data(json));
            }
        }
    };
    Sse::new(Box::pin(stream) as Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("ping"),
        )
}

/// Persist conversation events to DB and forward indexed events to SSE stream.
/// Returns the receiver that the SSE stream should read from.
fn spawn_conversation_event_persister(
    db: SqlitePool,
    conversation_id: String,
    mut agent_rx: mpsc::Receiver<StreamEvent>,
) -> mpsc::Receiver<IndexedStreamEvent> {
    let (sse_tx, sse_rx) = mpsc::channel::<IndexedStreamEvent>(100);

    tokio::spawn(async move {
        let mut event_index: i32 = 0;

        // Delete any stale events from a previous run of this conversation
        let _ = conversations::delete_events(&db, &conversation_id).await;

        while let Some(event) = agent_rx.recv().await {
            let event_type = get_stream_event_type(&event);
            let current_index = event_index;

            if let Ok(json) = serde_json::to_string(&event) {
                if let Err(e) = conversations::store_event(
                    &db,
                    &conversation_id,
                    current_index,
                    event_type,
                    &json,
                ).await {
                    tracing::warn!("[CONV-PERSIST] Failed to store event #{}: {}", current_index, e);
                }

                // Broadcast for any reconnect streams listening
                let broadcast_tx = get_broadcast_sender(&conversation_id).await;
                let _ = broadcast_tx.send((current_index, json));

                event_index += 1;
            }

            // Forward indexed event to SSE stream — OK if client disconnected (send fails silently)
            let _ = sse_tx.send((current_index, event)).await;
        }

        // Clean up broadcast channel now that the agent is done
        remove_broadcast_channel(&conversation_id).await;

        tracing::info!("[CONV-PERSIST] Event persister ended after {} events for conversation {}", event_index, conversation_id);
    });

    sse_rx
}

/// Wrap a stream of events with sequential indices (for non-persisted streams).
fn wrap_with_index(mut rx: mpsc::Receiver<StreamEvent>) -> mpsc::Receiver<IndexedStreamEvent> {
    let (tx, indexed_rx) = mpsc::channel::<IndexedStreamEvent>(100);
    tokio::spawn(async move {
        let mut index: i32 = 0;
        while let Some(event) = rx.recv().await {
            let _ = tx.send((index, event)).await;
            index += 1;
        }
    });
    indexed_rx
}

fn get_stream_event_type(event: &StreamEvent) -> &'static str {
    match event {
        StreamEvent::Text { .. } => "text",
        StreamEvent::ToolUse { .. } => "tool_use",
        StreamEvent::ToolResult { .. } => "tool_result",
        StreamEvent::Thinking { .. } => "thinking",
        StreamEvent::Status { .. } => "status",
        StreamEvent::Result { .. } => "result",
        StreamEvent::UserMessage { .. } => "user_message",
        StreamEvent::ReplayComplete { .. } => "replay_complete",
    }
}

/// Create an SSE stream that replays stored events for a conversation, then
/// tails the event log while the agent is still running.
pub fn create_conversation_reconnect_stream(
    db: Arc<SqlitePool>,
    conversation_id: String,
    events: Vec<ticketing_system::ConversationEvent>,
    checkpoint_status: String,
) -> impl Stream<Item = Result<Event, Infallible>> {
    stream! {
        let mut event_count = 0usize;
        let mut last_event_index: i32 = -1;

        // Phase 1: Replay stored events
        for db_event in &events {
            event_count += 1;
            last_event_index = db_event.event_index;
            yield Ok(Event::default()
                .id(db_event.event_index.to_string())
                .data(db_event.event_data.clone()));
        }

        // Send replay_complete
        let replay = StreamEvent::ReplayComplete {
            total_events: event_count,
            agent_status: checkpoint_status.clone(),
        };
        if let Ok(json) = serde_json::to_string(&replay) {
            yield Ok(Event::default().data(json));
        }

        // Phase 2: If agent is still running, subscribe to live broadcast
        if checkpoint_status == "running" || checkpoint_status == "pending" {
            let broadcast_tx = get_broadcast_sender(&conversation_id).await;
            let mut broadcast_rx = broadcast_tx.subscribe();
            // Drop our clone of the sender so we don't keep the channel alive
            drop(broadcast_tx);

            let timeout = tokio::time::sleep(Duration::from_secs(600));
            tokio::pin!(timeout);

            loop {
                tokio::select! {
                    result = broadcast_rx.recv() => {
                        match result {
                            Ok((event_index, event_data)) => {
                                if event_index > last_event_index {
                                    last_event_index = event_index;
                                    // Reset inactivity timeout on each received event
                                    timeout.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(600));
                                    yield Ok(Event::default()
                                        .id(event_index.to_string())
                                        .data(event_data));
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(n)) => {
                                tracing::warn!("[RECONNECT] Broadcast lagged by {} events for {}, falling back to DB", n, conversation_id);
                                // Catch up from DB
                                if let Ok(missed) = conversations::get_events_after(&db, &conversation_id, last_event_index).await {
                                    for ev in &missed {
                                        last_event_index = ev.event_index;
                                        yield Ok(Event::default()
                                            .id(ev.event_index.to_string())
                                            .data(ev.event_data.clone()));
                                    }
                                }
                            }
                            Err(broadcast::error::RecvError::Closed) => {
                                // Persister finished — agent is done
                                // Fetch any remaining events from DB
                                if let Ok(final_events) = conversations::get_events_after(&db, &conversation_id, last_event_index).await {
                                    for ev in &final_events {
                                        yield Ok(Event::default()
                                            .id(ev.event_index.to_string())
                                            .data(ev.event_data.clone()));
                                    }
                                }
                                let done = StreamEvent::Status {
                                    status: "completed".to_string(),
                                    message: None,
                                };
                                if let Ok(json) = serde_json::to_string(&done) {
                                    yield Ok(Event::default().data(json));
                                }
                                break;
                            }
                        }
                    }
                    _ = &mut timeout => {
                        tracing::warn!("[CHAT] Reconnect stream for {} timed out after 10 minutes", conversation_id);
                        let timeout_event = StreamEvent::Status {
                            status: "timeout".to_string(),
                            message: Some("Agent appears stuck — no activity for 10 minutes".to_string()),
                        };
                        if let Ok(json) = serde_json::to_string(&timeout_event) {
                            yield Ok(Event::default().data(json));
                        }
                        break;
                    }
                }
            }
        }
    }
}
