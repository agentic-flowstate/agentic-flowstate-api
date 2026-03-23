use std::sync::Arc;
use std::time::{Duration, Instant};
use std::path::PathBuf;
use tokio::sync::mpsc;
use sqlx::SqlitePool;
use cc_sdk::{ClaudeSDKClient, ClaudeCodeOptions, Message, ContentBlock, ToolsConfig, PermissionMode, McpServerConfig};
use futures::StreamExt;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::Serialize;

use crate::agents::StreamEvent;
use crate::agents::prompts::load_prompt;
use ticketing_system::{conversations, checkpoints, token_usage, AddMessageRequest, ContentBlockDesc, ConversationMessage, UpdateConversationRequest};
use super::chat_stream::{get_broadcast_sender, remove_broadcast_channel, ChatConfig, ChatImageData};
use super::chat_client_manager::{ChatClientManager, ChatClient};

/// How often to flush accumulated content to the database (ms).
const DB_FLUSH_INTERVAL_MS: u64 = 500;

/// How long a worker idles before shutting down.
const IDLE_TIMEOUT_SECS: u64 = 600; // 10 minutes

/// Message sent to a ConversationWorker via its mpsc channel.
pub struct WorkerMessage {
    pub user_id: String,
    pub message: String,
    pub config: ChatConfig,
    pub images: Option<Vec<ChatImageData>>,
    /// Per-request completion signal. Fired when this message is fully processed,
    /// so the SSE handler exits only when THIS message is done (not on terminal
    /// events from a prior message in the same conversation).
    pub completion_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

/// RAII guard that fires a oneshot completion signal on drop.
/// Ensures the SSE handler is notified even if process_message returns early.
struct CompletionGuard(Option<tokio::sync::oneshot::Sender<()>>);
impl Drop for CompletionGuard {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}

/// A long-lived tokio task that owns the SDK client for a single conversation
/// and processes messages sequentially from a queue.
pub struct ConversationWorker {
    db: Arc<SqlitePool>,
    conversation_id: String,
    manager: Arc<ChatClientManager>,
    message_rx: mpsc::Receiver<WorkerMessage>,
    event_index: i32,
}

impl ConversationWorker {
    pub fn new(
        db: Arc<SqlitePool>,
        conversation_id: String,
        manager: Arc<ChatClientManager>,
        message_rx: mpsc::Receiver<WorkerMessage>,
    ) -> Self {
        Self {
            db,
            conversation_id,
            manager,
            message_rx,
            event_index: 0,
        }
    }

    /// Main worker loop: pull messages from queue, process sequentially.
    /// Exits on channel close or idle timeout.
    pub async fn run(mut self) {
        tracing::info!("[WORKER] Started for conversation {}", self.conversation_id);

        // Initialize event_index from DB to ensure monotonically increasing indices
        // across worker sessions (e.g., after idle timeout killed the previous worker).
        // Without this, cursor-based SSE reconnection breaks: the app remembers
        // lastEventIndex=50 from the old session, but new events start at 0.
        match conversations::get_max_event_index(&self.db, &self.conversation_id).await {
            Ok(max_idx) => {
                self.event_index = max_idx + 1;
                tracing::info!("[WORKER] Initialized event_index to {} for {}", self.event_index, self.conversation_id);
            }
            Err(e) => {
                tracing::warn!("[WORKER] Failed to get max event index for {}: {}, starting at 0", self.conversation_id, e);
            }
        }

        loop {
            let msg = tokio::select! {
                msg = self.message_rx.recv() => msg,
                _ = tokio::time::sleep(Duration::from_secs(IDLE_TIMEOUT_SECS)) => {
                    tracing::info!("[WORKER] Idle timeout for {}, shutting down", self.conversation_id);
                    // Save session_id before disconnecting
                    self.save_session_and_disconnect().await;
                    break;
                }
            };

            match msg {
                Some(msg) => self.process_message(msg).await,
                None => {
                    tracing::info!("[WORKER] Channel closed for {}", self.conversation_id);
                    break;
                }
            }
        }

        // Clean up broadcast channel
        remove_broadcast_channel(&self.conversation_id).await;
        tracing::info!("[WORKER] Ended for conversation {}", self.conversation_id);
    }

    /// Save session_id from the current client and remove it from the manager.
    async fn save_session_and_disconnect(&self) {
        if let Some(client_arc) = self.manager.get(&self.conversation_id).await {
            let client = client_arc.lock().await;
            if let Some(ref session_id) = client.session_id {
                tracing::info!("[WORKER] Saving session_id {} before disconnect", session_id);
                let _ = conversations::update_conversation(
                    &self.db,
                    "", // user_id not needed for session_id update (WHERE already has id)
                    &self.conversation_id,
                    UpdateConversationRequest {
                        title: None,
                        session_id: Some(session_id.clone()),
                        organization: None,
                    },
                ).await;
            }
            drop(client);
        }
        self.manager.remove(&self.conversation_id).await;
    }

    /// Process a single user message: store in DB, send to SDK, consume entire response.
    async fn process_message(&mut self, mut msg: WorkerMessage) {
        // RAII guard: fires completion signal when this function exits (normal or early return)
        let _completion = CompletionGuard(msg.completion_tx.take());
        // Clear stale events from previous message but keep event_index monotonically
        // increasing. Resetting to 0 here was causing the "one turn late" bug: the app's
        // SSE cursor (e.g., lastEventIndex=50) would be higher than all new events (0,1,2...),
        // so reconnection with starting_after=50 returned nothing.
        let _ = conversations::delete_events(&self.db, &self.conversation_id).await;

        // Emit running status
        self.emit_event(&StreamEvent::Status {
            status: "running".to_string(),
            message: Some("running".to_string()),
        }).await;

        // Process image attachments (save to disk, build metadata)
        let mut image_paths: Vec<String> = Vec::new();
        let mut attachments_json: Option<String> = None;

        if let Some(ref images) = msg.images {
            if !images.is_empty() {
                let chat_images_dir = dirs::home_dir()
                    .unwrap_or_default()
                    .join(".agentic-flowstate")
                    .join("chat-images")
                    .join(&self.conversation_id);
                std::fs::create_dir_all(&chat_images_dir).ok();

                #[derive(Serialize)]
                struct AttachmentMeta {
                    filename: String,
                    path: String,
                    mime_type: String,
                }

                let mut attachments: Vec<AttachmentMeta> = Vec::new();

                for image in images {
                    let ext = match image.mime_type.as_str() {
                        "image/png" => "png",
                        "image/gif" => "gif",
                        "image/webp" => "webp",
                        "image/heic" => "heic",
                        _ => "jpg",
                    };
                    let filename = format!("{}.{}", uuid::Uuid::new_v4(), ext);
                    let file_path = chat_images_dir.join(&filename);

                    match STANDARD.decode(&image.data) {
                        Ok(bytes) => {
                            if let Err(e) = std::fs::write(&file_path, &bytes) {
                                tracing::error!("[WORKER] Failed to write image {}: {}", filename, e);
                                continue;
                            }
                            let path_str = file_path.to_string_lossy().to_string();
                            image_paths.push(path_str.clone());
                            attachments.push(AttachmentMeta {
                                filename: filename.clone(),
                                path: path_str,
                                mime_type: image.mime_type.clone(),
                            });
                            tracing::info!("[WORKER] Saved chat image: {}", file_path.display());
                        }
                        Err(e) => {
                            tracing::error!("[WORKER] Failed to decode base64 image: {}", e);
                        }
                    }
                }

                if !attachments.is_empty() {
                    attachments_json = serde_json::to_string(&attachments).ok();
                }
            }
        }

        // Build enhanced message for SDK (with image paths for Claude to read)
        let enhanced_message = if !image_paths.is_empty() {
            let paths_list = image_paths.iter()
                .map(|p| format!("  - {}", p))
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "[The user has attached {} image(s). View them using the Read tool:\n{}\n]\n\n{}",
                image_paths.len(),
                paths_list,
                msg.message
            )
        } else {
            msg.message.clone()
        };

        // Store user message in DB (original text, not enhanced)
        let stored_msg = conversations::add_message(
            &self.db,
            &self.conversation_id,
            AddMessageRequest {
                role: "user".to_string(),
                content: msg.message.clone(),
                attachments: attachments_json,
            },
        ).await;

        // First message → generate title + detect org in background
        if let Ok(ref stored) = stored_msg {
            if stored.message_index == 0 {
                let title_db = (*self.db).clone();
                let title_user = msg.user_id.clone();
                let title_conv = self.conversation_id.clone();
                let title_msg = msg.message.clone();
                // Look up the current org so the detector knows what's already set
                let current_org = conversations::get_conversation(&title_db, &title_conv, false)
                    .await
                    .ok()
                    .flatten()
                    .map(|c| c.organization)
                    .unwrap_or_default();
                tokio::spawn(async move {
                    if let Some(result) = super::title_generator::generate_title_and_org(
                        title_db, title_user, title_conv.clone(), title_msg, current_org,
                    ).await {
                        let broadcast_tx = get_broadcast_sender(&title_conv).await;
                        // Broadcast title update
                        let title_event = StreamEvent::TitleUpdate { title: result.title };
                        if let Ok(json) = serde_json::to_string(&title_event) {
                            let _ = broadcast_tx.send((-1, json));
                        }
                        // Broadcast org update if it changed
                        if let Some(org) = result.organization {
                            let org_event = StreamEvent::OrgUpdate { organization: org };
                            if let Ok(json) = serde_json::to_string(&org_event) {
                                let _ = broadcast_tx.send((-1, json));
                            }
                        }
                    }
                });
            }
        }

        if let Err(ref e) = stored_msg {
            tracing::error!("[WORKER] Failed to store user message: {}", e);
        }

        // Create checkpoint
        if let Err(e) = checkpoints::upsert_checkpoint(&self.db, &self.conversation_id, "pending", 0).await {
            tracing::warn!("[WORKER] Failed to create checkpoint: {}", e);
        }

        // Create initial assistant message placeholder
        let mut assistant_message_id: Option<String> = None;
        match conversations::add_message(
            &self.db,
            &self.conversation_id,
            AddMessageRequest {
                role: "assistant".to_string(),
                content: String::new(),
                attachments: None,
            },
        ).await {
            Ok(m) => assistant_message_id = Some(m.id),
            Err(e) => tracing::error!("[WORKER] Failed to create assistant message: {}", e),
        }

        // Get or create SDK client
        let client_arc = match self.get_or_create_client(&msg.config).await {
            Ok(arc) => arc,
            Err(e) => {
                tracing::error!("[WORKER] Failed to get client for {}: {}", self.conversation_id, e);
                // Mark checkpoint as interrupted so the iOS app doesn't think the agent is still running.
                // (upsert_checkpoint always writes status='running', so we use mark_interrupted instead.)
                let _ = checkpoints::mark_interrupted(&self.db, &self.conversation_id).await;
                self.emit_event(&StreamEvent::Status {
                    status: "failed".to_string(),
                    message: Some(format!("Failed to start agent: {}", e)),
                }).await;
                return;
            }
        };

        // Send message to SDK and get response stream.
        // The mutex is released after getting the stream so the cancel handler
        // can acquire it to call interrupt().
        let (mut response_stream, resume_session_id) = {
            let mut client = client_arc.lock().await;
            client.last_used = Instant::now();

            // Capture the session_id we're trying to resume from (if any)
            let resume_sid = client.session_id.clone();

            if let Err(e) = client.sdk_client.send_user_message(enhanced_message).await {
                tracing::error!("[WORKER] Failed to send message: {}", e);
                self.emit_event(&StreamEvent::Status {
                    status: "failed".to_string(),
                    message: Some(format!("Failed to send: {}", e)),
                }).await;
                return;
            }

            (client.sdk_client.receive_messages().await, resume_sid)
            // MutexGuard dropped here — allows cancel handler to call interrupt()
        };
        let mut accumulated_text = String::new();
        let mut result_session_id: Option<String> = None;
        let mut last_flush = Instant::now();
        let flush_interval = Duration::from_millis(DB_FLUSH_INTERVAL_MS);
        let mut tool_call_count: i32 = 0;
        let mut message_count = 0u32;
        let mut content_blocks: Vec<ContentBlockDesc> = Vec::new();
        let mut blocks_dirty = false;
        let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
        heartbeat.tick().await; // consume immediate first tick

        loop {
            tokio::select! {
                msg_opt = response_stream.next() => {
                    match msg_opt {
                        Some(Ok(sdk_msg)) => {
                            message_count += 1;

                            if let Message::Assistant { message: assistant_msg } = &sdk_msg {
                                for block in &assistant_msg.content {
                                    match block {
                                        ContentBlock::Text(text_content) => {
                                            accumulated_text.push_str(&text_content.text);
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
                                            self.emit_event(&StreamEvent::Text {
                                                content: text_content.text.clone(),
                                            }).await;
                                        }
                                        ContentBlock::ToolUse(tool_use) => {
                                            tool_call_count += 1;
                                            tracing::info!("[WORKER] Tool use #{}: {} ({})", tool_call_count, tool_use.name, tool_use.id);
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

                                            if let Some(msg_id) = &assistant_message_id {
                                                let now = chrono::Utc::now().timestamp();
                                                if let Err(e) = conversations::insert_tool_call(
                                                    &self.db,
                                                    &tool_use.id,
                                                    msg_id,
                                                    &self.conversation_id,
                                                    &tool_use.name,
                                                    Some(&tool_use.input),
                                                    now,
                                                ).await {
                                                    tracing::error!("[WORKER] Failed to insert tool call: {}", e);
                                                }
                                            }

                                            self.emit_event(&StreamEvent::ToolUse {
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

                                            if let Err(e) = conversations::update_tool_call_result(
                                                &self.db,
                                                &tool_result.tool_use_id,
                                                &content,
                                                tool_result.is_error.unwrap_or(false),
                                            ).await {
                                                tracing::error!("[WORKER] Failed to update tool call result: {}", e);
                                            }

                                            self.emit_event(&StreamEvent::ToolResult {
                                                tool_use_id: tool_result.tool_use_id.clone(),
                                                content,
                                                is_error: tool_result.is_error.unwrap_or(false),
                                            }).await;
                                        }
                                        ContentBlock::Thinking(thinking) => {
                                            self.emit_event(&StreamEvent::Thinking {
                                                content: thinking.thinking.clone(),
                                            }).await;
                                        }
                                    }
                                }
                            }

                            // Periodically flush accumulated text to DB
                            if last_flush.elapsed() >= flush_interval {
                                flush_to_db(&self.db, assistant_message_id.as_deref(), &accumulated_text).await;
                                last_flush = Instant::now();
                            }

                            // Flush content blocks when structure changes
                            if blocks_dirty {
                                blocks_dirty = false;
                                if let Some(msg_id) = &assistant_message_id {
                                    if content_blocks.len() > 1 {
                                        let _ = conversations::update_message_blocks(&self.db, msg_id, &content_blocks).await;
                                    }
                                }
                            }

                            // Check for result message
                            if let Message::Result { session_id: sess_id, is_error, subtype, usage, .. } = &sdk_msg {
                                tracing::info!("[WORKER] Result: subtype={}, is_error={}", subtype, is_error);

                                // Track token usage
                                if let Some(usage_json) = usage {
                                    let input_tok = usage_json.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                                    let output_tok = usage_json.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                                    if input_tok > 0 || output_tok > 0 {
                                        let db_ref = self.db.clone();
                                        let conv_id = self.conversation_id.clone();
                                        let uid = msg.user_id.clone();
                                        tokio::spawn(async move {
                                            if let Err(e) = token_usage::insert_token_usage(
                                                &db_ref, "conversation", &conv_id,
                                                Some(&uid), None, input_tok, output_tok,
                                            ).await {
                                                tracing::warn!("[WORKER] Failed to record token usage: {}", e);
                                            }
                                        });
                                    }
                                }

                                // Detect silent --resume failure: if the Result session_id
                                // differs from what we passed to --resume, the CLI silently
                                // started a fresh session. DO NOT overwrite the original
                                // session_id — it may still be resumable later.
                                let resume_failed = if let Some(ref original_sid) = resume_session_id {
                                    if sess_id != original_sid {
                                        tracing::warn!(
                                            "[WORKER] RESUME FAILURE DETECTED for {}: \
                                            requested --resume {} but CLI returned session {}. \
                                            Original session_id preserved — NOT overwriting.",
                                            self.conversation_id, original_sid, sess_id
                                        );
                                        true
                                    } else {
                                        false
                                    }
                                } else {
                                    false
                                };

                                result_session_id = Some(sess_id.clone());

                                self.emit_event(&StreamEvent::Result {
                                    session_id: sess_id.clone(),
                                    status: subtype.clone(),
                                    is_error: *is_error,
                                }).await;

                                // Only update the conversation's session_id if resume
                                // succeeded (or this is a fresh conversation with no prior
                                // session). When resume fails, preserving the original
                                // session_id allows future resume attempts.
                                if !resume_failed {
                                    let _ = conversations::update_conversation(
                                        &self.db,
                                        &msg.user_id,
                                        &self.conversation_id,
                                        UpdateConversationRequest {
                                            title: None,
                                            session_id: Some(sess_id.clone()),
                                            organization: None,
                                        },
                                    ).await;
                                }

                                if let Err(e) = checkpoints::upsert_checkpoint(&self.db, &self.conversation_id, sess_id, tool_call_count).await {
                                    tracing::warn!("[WORKER] Failed to update checkpoint: {}", e);
                                }
                                if let Err(e) = checkpoints::mark_completed(&self.db, &self.conversation_id).await {
                                    tracing::warn!("[WORKER] Failed to mark checkpoint completed: {}", e);
                                }

                                // Send push notification
                                if let Some(apns) = crate::apns::ApnsService::global() {
                                    tracing::info!("[WORKER] Sending push notification for user={}, conv={}", msg.user_id, self.conversation_id);
                                    let push_db = (*self.db).clone();
                                    let push_user = msg.user_id.clone();
                                    let push_agent = msg.config.prompt_name.to_string();
                                    let push_conv_id = self.conversation_id.clone();
                                    let apns = apns.clone();
                                    tokio::spawn(async move {
                                        match apns.send_to_user(
                                            &push_db,
                                            &push_user,
                                            "Agent finished",
                                            &format!("{} has completed processing.", push_agent),
                                            Some(&push_conv_id),
                                            Some(&push_agent),
                                        ).await {
                                            Ok(()) => tracing::info!("[WORKER] Push notification sent for user={}", push_user),
                                            Err(e) => tracing::warn!("[WORKER] Push notification failed for user={}: {}", push_user, e),
                                        }
                                    });
                                } else {
                                    tracing::warn!("[WORKER] APNs not initialized — skipping push notification");
                                }

                                break;
                            }
                        }
                        Some(Err(e)) => {
                            tracing::error!("[WORKER] Error receiving message #{}: {}", message_count, e);
                            // Mark checkpoint as interrupted so it doesn't block restarts
                            let _ = checkpoints::mark_interrupted(&self.db, &self.conversation_id).await;
                            self.emit_event(&StreamEvent::Status {
                                status: "failed".to_string(),
                                message: Some(format!("Error: {}", e)),
                            }).await;
                            break;
                        }
                        None => {
                            tracing::error!(
                                "[WORKER] INCOMPLETE SESSION: Stream ended without Result \
                                for conversation {} (session: {:?}). The session file will \
                                have no result entry — future --resume may fail silently. \
                                Tool calls processed: {}",
                                self.conversation_id,
                                resume_session_id,
                                tool_call_count
                            );
                            // Mark checkpoint as interrupted — stream ended unexpectedly
                            let _ = checkpoints::mark_interrupted(&self.db, &self.conversation_id).await;
                            break;
                        }
                    }
                }
                _ = heartbeat.tick() => {
                    self.emit_event(&StreamEvent::Status {
                        status: "heartbeat".to_string(),
                        message: None,
                    }).await;
                    // Touch checkpoint updated_at so staleness detection knows we're alive.
                    // Without this, a long-running agent (e.g., 10-minute tool call) would
                    // look stale and get auto-cleaned by the restart watcher.
                    let _ = checkpoints::touch_checkpoint(&self.db, &self.conversation_id).await;
                }
            }
        }

        // Write session_id back to client (needed for resume on next message)
        if let Some(ref sid) = result_session_id {
            if let Some(client_arc) = self.manager.get(&self.conversation_id).await {
                let mut client = client_arc.lock().await;
                client.session_id = Some(sid.clone());
            }
        }

        // Final flush of accumulated text to DB
        flush_to_db(&self.db, assistant_message_id.as_deref(), &accumulated_text).await;

        // Store content block ordering
        if let Some(msg_id) = &assistant_message_id {
            if content_blocks.len() > 1 {
                if let Err(e) = conversations::update_message_blocks(&self.db, msg_id, &content_blocks).await {
                    tracing::error!("[WORKER] Failed to store content blocks: {}", e);
                }
            }
        }

        tracing::info!("[WORKER] Stream ended after {} messages", message_count);
        self.emit_event(&StreamEvent::Status {
            status: "completed".to_string(),
            message: None,
        }).await;
    }

    /// Emit a StreamEvent: store in DB and broadcast to all subscribers.
    async fn emit_event(&mut self, event: &StreamEvent) {
        let event_type = get_stream_event_type(event);
        let current_index = self.event_index;

        if let Ok(json) = serde_json::to_string(event) {
            let _ = conversations::store_event(
                &self.db,
                &self.conversation_id,
                current_index,
                event_type,
                &json,
            ).await;

            let broadcast_tx = get_broadcast_sender(&self.conversation_id).await;
            let _ = broadcast_tx.send((current_index, json));

            self.event_index += 1;
        }
    }

    /// Get an existing connected client or create a new one.
    async fn get_or_create_client(
        &self,
        config: &ChatConfig,
    ) -> Result<Arc<tokio::sync::Mutex<ChatClient>>, String> {
        // Try existing client
        if let Some(existing) = self.manager.get(&self.conversation_id).await {
            let guard = existing.lock().await;
            if guard.sdk_client.is_connected().await {
                drop(guard);
                return Ok(existing);
            }
            drop(guard);
            tracing::info!("[WORKER] Client for {} disconnected, will recreate", self.conversation_id);
            self.manager.remove(&self.conversation_id).await;
        }

        // Create new client
        create_client(&self.db, &self.conversation_id, config, &self.manager).await
    }
}

/// Create a new ClaudeSDKClient, optionally resuming from a saved session.
pub(crate) async fn create_client(
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
        "[WORKER] Configuring {} tools for {}: {:?}",
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

    // When resuming, inject conversation history as a safety net.
    // If --resume works, this context is redundant (harmless).
    // If --resume fails silently (session file expired/gone), Claude still has context.
    let system_prompt = if saved_session_id.is_some() {
        match conversations::list_messages(db, conv_id).await {
            Ok(messages) if !messages.is_empty() => {
                tracing::info!("[WORKER] Injecting {} messages as resume context for {}", messages.len(), conv_id);
                let history = build_conversation_history(&messages);
                format!("{}\n\n{}", system_prompt, history)
            }
            _ => system_prompt,
        }
    } else {
        system_prompt
    };

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
        tracing::info!("[WORKER] Registering MCP server: {}", mcp_binary.display());
        builder = builder.add_mcp_server(
            "agentic-mcp",
            McpServerConfig::Stdio {
                command: mcp_binary.to_string_lossy().to_string(),
                args: None,
                env: None,
            },
        );
    }

    // Enable adaptive thinking via --effort flag
    builder = builder.add_extra_arg("effort", Some(config.agent_type.effort().to_string()));

    if let Some(ref sid) = saved_session_id {
        tracing::info!("[WORKER] Resuming conversation {} from session {}", conv_id, sid);
        builder = builder.resume(sid.clone());
    } else {
        tracing::info!("[WORKER] Creating fresh client for conversation {}", conv_id);
    }

    let options = builder.build();
    let mut sdk_client = ClaudeSDKClient::new(options);

    tracing::info!("[WORKER] Connecting client for {} (30s timeout)...", conv_id);
    match tokio::time::timeout(Duration::from_secs(30), sdk_client.connect(None)).await {
        Ok(Ok(())) => {
            tracing::info!("[WORKER] Client connected for {}", conv_id);
        }
        Ok(Err(e)) => {
            tracing::error!("[WORKER] Client connect error for {}: {}", conv_id, e);
            return Err(format!("Failed to connect: {}", e));
        }
        Err(_) => {
            tracing::error!("[WORKER] Client connect timed out after 30s for {}", conv_id);
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

/// Flush accumulated text content to the database.
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
            tracing::error!("[WORKER] Failed to flush message to DB: {}", e);
        }
    }
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
        StreamEvent::TitleUpdate { .. } => "title_update",
        StreamEvent::OrgUpdate { .. } => "org_update",
        StreamEvent::RouterText { .. } => "router_text",
        StreamEvent::RouterToolUse { .. } => "router_tool_use",
        StreamEvent::RouterToolResult { .. } => "router_tool_result",
        StreamEvent::RouterResult { .. } => "router_result",
    }
}

/// Build a condensed conversation history for system prompt injection.
/// Used as a safety net when resuming sessions — if --resume works, this is
/// redundant; if it fails silently, Claude still has context.
fn build_conversation_history(messages: &[ConversationMessage]) -> String {
    let mut history = String::from(
        "## Previous Conversation Context\n\n\
         This is a resumed conversation. If you already have full context from the session, \
         ignore this section. Otherwise, here is what was discussed:\n\n"
    );

    // Take last 20 messages to keep prompt size reasonable
    let recent = if messages.len() > 20 {
        &messages[messages.len() - 20..]
    } else {
        messages
    };

    for msg in recent {
        if msg.content.is_empty() {
            continue;
        }
        let role = if msg.role == "user" { "User" } else { "Assistant" };
        let content = if msg.role == "assistant" && msg.content.len() > 300 {
            let truncated: String = msg.content.chars().take(300).collect();
            format!("{}…", truncated)
        } else {
            msg.content.clone()
        };
        history.push_str(&format!("**{}**: {}\n\n", role, content));
    }

    history
}
