use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use cc_sdk::{
    ClaudeCodeOptions, ClaudeSDKClient, ContentBlock, McpServerConfig, Message, PermissionMode,
    ToolsConfig,
};
use futures::StreamExt;
use serde::Serialize;
use sqlx::SqlitePool;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use super::anthropic_translator::AnthropicTranslator;
use super::chat_client_manager::{ChatClient, ChatClientManager};
use super::chat_stream::{
    get_broadcast_sender, remove_broadcast_channel, ChatConfig, ChatImageData,
};
use crate::agents::prompts::load_prompt;
use crate::agents::{AgentType, StreamEvent};
use crate::observability::streaming::{record_gap_detected, record_stream_event_emitted};
use ticketing_system::{
    checkpoints, conversations, token_usage, AddMessageRequest, ContentBlockDesc,
    ConversationMessage, UpdateConversationRequest,
};

/// Timeout for the ticket-router pre-processing phase.
const ROUTER_TIMEOUT_SECS: u64 = 60;

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
    /// Optional `Idempotency-Key` the client supplied on the HTTP POST
    /// that triggered this turn (T-A819D36B). Threaded from the handler
    /// → [`WorkerMessage`] → [`AnthropicTranslator::set_pending_client_id`]
    /// so the `message_start` SSE frame echoes it back as
    /// `message.client_id`. iOS's `MessageEchoService.reconcileServerMessageStart`
    /// matches on this value to lock its optimistic-echo row.
    /// `None` when the header was absent — no fallbacks.
    pub client_id: Option<String>,
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

/// Short conversational messages that should skip the ticket router entirely.
/// These are acknowledgements, confirmations, and approval-flow signals that
/// will never need ticket context.
const ROUTER_SKIP_MESSAGES: &[&str] = &[
    "approved",
    "rejected", // workspace-manager approval flow
    "yes",
    "no",
    "ok",
    "okay",
    "sure",
    "yep",
    "nope",
    "nah",
    "thanks",
    "thank you",
    "ty",
    "thx",
    "lgtm",
    "sounds good",
    "looks good",
    "got it",
    "cool",
    "nice",
    "great",
    "perfect",
    "awesome",
    "hi",
    "hello",
    "hey",
    "done",
    "noted",
    "\u{1F44D}", // 👍
    "\u{1F44E}", // 👎
    "\u{2705}",  // ✅
    "\u{274C}",  // ❌
    "\u{1F64F}", // 🙏
];

/// A long-lived tokio task that owns the SDK client for a single conversation
/// and processes messages sequentially from a queue.
pub struct ConversationWorker {
    db: Arc<SqlitePool>,
    conversation_id: String,
    manager: Arc<ChatClientManager>,
    message_rx: mpsc::Receiver<WorkerMessage>,
    event_index: i32,
    /// Cached from DB — whether the router has already run for this conversation.
    /// Initialized from DB in `run()`, so it survives server restarts.
    has_routed: bool,
    /// Cached ticket_id from the last successful router match (from DB).
    last_router_ticket_id: Option<String>,
    /// Cached organization from the last successful router match (from DB).
    last_router_organization: Option<String>,
    /// Stateful translator from internal `StreamEvent` → Anthropic events.
    /// Maintains message / content-block lifecycle across emit_event calls.
    /// One translator instance per worker — it survives the entire
    /// conversation and is reset between turns by cc-sdk's `Result` event.
    translator: AnthropicTranslator,
    /// User id for the turn currently being processed. Populated at the
    /// top of `process_message` from the inbound `WorkerMessage.user_id`
    /// and consumed by `emit_event` to fan out silent pushes on
    /// `message_stop` (T-90C7FAC4). `None` outside a turn.
    current_user_id: Option<String>,
}

impl ConversationWorker {
    pub fn new(
        db: Arc<SqlitePool>,
        conversation_id: String,
        manager: Arc<ChatClientManager>,
        message_rx: mpsc::Receiver<WorkerMessage>,
    ) -> Self {
        let translator = AnthropicTranslator::new(&conversation_id);
        Self {
            db,
            conversation_id,
            manager,
            message_rx,
            event_index: 0,
            has_routed: false,
            last_router_ticket_id: None,
            last_router_organization: None,
            translator,
            current_user_id: None,
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
                tracing::info!(
                    "[WORKER] Initialized event_index to {} for {}",
                    self.event_index,
                    self.conversation_id
                );
            }
            Err(e) => {
                tracing::warn!(
                    "[WORKER] Failed to get max event index for {}: {}, starting at 0",
                    self.conversation_id,
                    e
                );
            }
        }

        // Initialize router state from DB — survives server restarts.
        // If the conversation already has router metadata or previous messages,
        // skip the router on all subsequent messages.
        match conversations::has_router_run(&self.db, &self.conversation_id).await {
            Ok(true) => {
                self.has_routed = true;
                tracing::info!(
                    "[WORKER] Router already ran for {} (loaded from DB)",
                    self.conversation_id
                );
            }
            Ok(false) => {
                tracing::info!(
                    "[WORKER] Router has NOT run for {} (new conversation)",
                    self.conversation_id
                );
            }
            Err(e) => {
                tracing::warn!(
                    "[WORKER] Failed to check router state for {}: {}, assuming not routed",
                    self.conversation_id,
                    e
                );
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
                tracing::info!(
                    "[WORKER] Saving session_id {} before disconnect",
                    session_id
                );
                let _ = conversations::update_conversation(
                    &self.db,
                    "", // user_id not needed for session_id update (WHERE already has id)
                    &self.conversation_id,
                    UpdateConversationRequest {
                        title: None,
                        session_id: Some(session_id.clone()),
                        organization: None,
                    },
                )
                .await;
            }
            drop(client);
        }
        self.manager.remove(&self.conversation_id).await;
    }

    /// Process a single user message: store in DB, send to SDK, consume entire response.
    async fn process_message(&mut self, mut msg: WorkerMessage) {
        // RAII guard: fires completion signal when this function exits (normal or early return)
        let _completion = CompletionGuard(msg.completion_tx.take());
        // Cache the turn's owner so `emit_event` can fan out silent
        // pushes on `message_stop` without plumbing user_id through
        // every call site (T-90C7FAC4).
        self.current_user_id = Some(msg.user_id.clone());
        // Stage the inbound Idempotency-Key (T-A819D36B) onto the
        // translator BEFORE any StreamEvent is translated. The translator
        // consumes it on the first `open_message_if_needed` call (i.e.,
        // the next `message_start` frame). When `None`, it stays `None`
        // end-to-end — no synthetic fallbacks.
        self.translator
            .set_pending_client_id(msg.client_id.clone());
        // Clear stale events from previous message but keep event_index monotonically
        // increasing. Resetting to 0 here was causing the "one turn late" bug: the app's
        // SSE cursor (e.g., lastEventIndex=50) would be higher than all new events (0,1,2...),
        // so reconnection with starting_after=50 returned nothing.
        let _ = conversations::delete_events(&self.db, &self.conversation_id).await;

        // Emit running status
        self.emit_event(&StreamEvent::Status {
            status: "running".to_string(),
            message: Some("running".to_string()),
        })
        .await;

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
                                tracing::error!(
                                    "[WORKER] Failed to write image {}: {}",
                                    filename,
                                    e
                                );
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
            let paths_list = image_paths
                .iter()
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
        )
        .await;

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
                        title_db,
                        title_user,
                        title_conv.clone(),
                        title_msg,
                        current_org,
                    )
                    .await
                    {
                        let broadcast_tx = get_broadcast_sender(&title_conv).await;
                        // Broadcast title update
                        let title_event = StreamEvent::TitleUpdate {
                            title: result.title,
                        };
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
        if let Err(e) =
            checkpoints::upsert_checkpoint(&self.db, &self.conversation_id, "pending", 0).await
        {
            tracing::warn!("[WORKER] Failed to create checkpoint: {}", e);
        }

        // === TICKET ROUTER PRE-PROCESSING ===
        // Run a lightweight router agent to match the user's message to a ticket.
        // The router is ephemeral (no session persistence) and has a strict timeout.
        // On any failure, we fall back to the original enhanced_message.
        let final_message = self.run_ticket_router(&enhanced_message, &msg.config).await;

        // If the router enriched the message (added ticket context), save it as a
        // "forwarded" message in the conversation DB so it persists across fetchMessages calls.
        // The iOS app maps role="forwarded" to isForwardedMessage=true.
        // NOTE: This must be created BEFORE the assistant message stub so that
        // message_index ordering is: user → forwarded → assistant.
        if final_message != enhanced_message {
            if let Err(e) = conversations::add_message(
                &self.db,
                &self.conversation_id,
                AddMessageRequest {
                    role: "forwarded".to_string(),
                    content: final_message.clone(),
                    attachments: None,
                },
            )
            .await
            {
                tracing::warn!("[WORKER] Failed to save forwarded message: {}", e);
            }
        }

        // Create assistant message placeholder AFTER the forwarded message so it gets
        // a higher message_index. This ensures the chat displays in the correct order:
        // user message → forwarded context → assistant response.
        let mut assistant_message_id: Option<String> = None;
        match conversations::add_message(
            &self.db,
            &self.conversation_id,
            AddMessageRequest {
                role: "assistant".to_string(),
                content: String::new(),
                attachments: None,
            },
        )
        .await
        {
            Ok(m) => assistant_message_id = Some(m.id),
            Err(e) => tracing::error!("[WORKER] Failed to create assistant message: {}", e),
        }

        // Bridge the gap between router completion and main agent streaming.
        // Without this, the UI shows no activity during client creation + connection.
        self.emit_event(&StreamEvent::Status {
            status: "running".to_string(),
            message: Some("Preparing agent...".to_string()),
        })
        .await;

        // Get or create SDK client
        let client_arc = match self.get_or_create_client(&msg.config).await {
            Ok(arc) => arc,
            Err(e) => {
                tracing::error!(
                    "[WORKER] Failed to get client for {}: {}",
                    self.conversation_id,
                    e
                );
                // Mark checkpoint as interrupted so the iOS app doesn't think the agent is still running.
                // (upsert_checkpoint always writes status='running', so we use mark_interrupted instead.)
                let _ = checkpoints::mark_interrupted(&self.db, &self.conversation_id).await;
                self.emit_event(&StreamEvent::Status {
                    status: "failed".to_string(),
                    message: Some(format!("Failed to start agent: {}", e)),
                })
                .await;
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

            if let Err(e) = client.sdk_client.send_user_message(final_message).await {
                tracing::error!("[WORKER] Failed to send message: {}", e);
                self.emit_event(&StreamEvent::Status {
                    status: "failed".to_string(),
                    message: Some(format!("Failed to send: {}", e)),
                })
                .await;
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
        let mut last_message_time = Instant::now();

        loop {
            tokio::select! {
                msg_opt = response_stream.next() => {
                    match msg_opt {
                        Some(Ok(sdk_msg)) => {
                            message_count += 1;
                            last_message_time = Instant::now();

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
                                        // Emit a visible status event so the user/app knows
                                        // context was degraded. The conversation history was
                                        // injected into the system prompt as a safety net.
                                        self.emit_event(&StreamEvent::Status {
                                            status: "resume_failed".to_string(),
                                            message: Some(
                                                "Session was interrupted and could not be fully restored. \
                                                The agent is continuing with conversation history context."
                                                .to_string()
                                            ),
                                        }).await;
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
                            // Emit visible status so the user knows the agent stopped
                            self.emit_event(&StreamEvent::Status {
                                status: "failed".to_string(),
                                message: Some(
                                    "Agent session ended unexpectedly. Your conversation history \
                                    is preserved — send another message to continue."
                                    .to_string()
                                ),
                            }).await;
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

                    // Watchdog: check if the subprocess is still alive.
                    // If the SDK client is disconnected (subprocess died), there's no point
                    // waiting for more messages — the stream will never produce them.
                    let subprocess_alive = if let Some(client_arc) = self.manager.get(&self.conversation_id).await {
                        let guard = client_arc.lock().await;
                        guard.sdk_client.is_connected().await
                    } else {
                        false
                    };

                    if !subprocess_alive {
                        tracing::error!(
                            "[WORKER] WATCHDOG: Subprocess is no longer connected for {} \
                            (last message {}s ago, tool_calls: {}). Breaking out of stream loop.",
                            self.conversation_id,
                            last_message_time.elapsed().as_secs(),
                            tool_call_count
                        );
                        let _ = checkpoints::mark_interrupted(&self.db, &self.conversation_id).await;
                        self.emit_event(&StreamEvent::Status {
                            status: "failed".to_string(),
                            message: Some(
                                "Agent subprocess stopped unexpectedly. Your conversation history \
                                is preserved — send another message to continue."
                                .to_string()
                            ),
                        }).await;
                        break;
                    }
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
                if let Err(e) =
                    conversations::update_message_blocks(&self.db, msg_id, &content_blocks).await
                {
                    tracing::error!("[WORKER] Failed to store content blocks: {}", e);
                }
            }
        }

        tracing::info!("[WORKER] Stream ended after {} messages", message_count);
        self.emit_event(&StreamEvent::Status {
            status: "completed".to_string(),
            message: None,
        })
        .await;
    }

    /// Run the ticket-router agent as a pre-processing step.
    /// Returns the enriched message if the router matched a ticket,
    /// or the original message on skip/failure/timeout.
    ///
    /// Applies three early-exit conditions to bypass the router entirely
    /// (no cc-sdk call, no latency penalty):
    /// 1. Short conversational messages (acknowledgements, confirmations)
    /// 2. Workspace-manager approval flow signals ("approved"/"rejected")
    /// 3. Conversations with an established ticket context from a prior message
    async fn run_ticket_router(&mut self, user_message: &str, _config: &ChatConfig) -> String {
        let original = user_message.to_string();

        // Truncate preview at a safe UTF-8 char boundary (floor to nearest boundary at or before 60)
        let msg_preview = if user_message.len() > 60 {
            let end = (0..=60)
                .rev()
                .find(|&i| user_message.is_char_boundary(i))
                .unwrap_or(0);
            &user_message[..end]
        } else {
            user_message
        };

        tracing::info!(
            "[ROUTER] === ENTER === conv={} has_routed={} msg={:?}",
            self.conversation_id,
            self.has_routed,
            msg_preview
        );

        // --- Primary skip: only route the FIRST message in a conversation ---
        // After the first message, skip entirely — no events emitted, no latency.
        if self.has_routed {
            tracing::info!(
                "[ROUTER] === SKIP (already routed) === conv={}",
                self.conversation_id
            );
            return original;
        }

        let trimmed = user_message.trim();
        let lowered = trimmed.to_lowercase();

        // --- Skip condition: short conversational / approval messages ---
        let is_skip_message = ROUTER_SKIP_MESSAGES.contains(&lowered.as_str());
        let is_single_short_word = !trimmed.is_empty()
            && !trimmed.contains(' ')
            && trimmed.len() <= 4
            && trimmed
                .chars()
                .all(|c| c.is_alphanumeric() || c.is_ascii_punctuation());

        if is_skip_message || is_single_short_word {
            tracing::info!(
                "[ROUTER] === SKIP (short/conversational) === conv={} msg={:?}",
                self.conversation_id,
                if trimmed.len() > 30 {
                    &trimmed[..(0..=30)
                        .rev()
                        .find(|&i| trimmed.is_char_boundary(i))
                        .unwrap_or(0)]
                } else {
                    trimmed
                }
            );
            self.has_routed = true;
            // Persist so this survives server restarts
            let _ = conversations::set_router_result(
                &self.db,
                &self.conversation_id,
                Some("__skipped__"),
                None,
            )
            .await;
            return original;
        }

        tracing::info!(
            "[ROUTER] === RUNNING FULL ROUTER === conv={}",
            self.conversation_id
        );

        // Emit heartbeats on the SSE connection while the router is running.
        // The router can take 10-30+ seconds — without heartbeats the iOS watchdog
        // (45s no-byte timeout) could kill the connection.
        let heartbeat_conv_id = self.conversation_id.clone();
        let heartbeat_handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(10));
            interval.tick().await; // consume immediate first tick
            loop {
                interval.tick().await;
                let tx = get_broadcast_sender(&heartbeat_conv_id).await;
                let heartbeat = StreamEvent::Status {
                    status: "heartbeat".to_string(),
                    message: None,
                };
                if let Ok(json) = serde_json::to_string(&heartbeat) {
                    let _ = tx.send((-1, json));
                }
            }
        });

        let start = std::time::Instant::now();
        let result = tokio::time::timeout(
            Duration::from_secs(ROUTER_TIMEOUT_SECS),
            self.run_ticket_router_inner(user_message),
        )
        .await;
        let elapsed = start.elapsed();

        // Stop heartbeats now that the router is done
        heartbeat_handle.abort();

        // Mark as routed regardless of outcome — never route twice
        self.has_routed = true;

        match result {
            Ok(Ok(ref enriched)) => {
                let enriched_preview = if enriched.len() > 100 {
                    &enriched[..(0..=100)
                        .rev()
                        .find(|&i| enriched.is_char_boundary(i))
                        .unwrap_or(0)]
                } else {
                    enriched
                };
                tracing::info!(
                    "[ROUTER] === DONE ({:.1}s) === conv={} result={:?}",
                    elapsed.as_secs_f64(),
                    self.conversation_id,
                    enriched_preview
                );
                enriched.clone()
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    "[ROUTER] === FAILED ({:.1}s) === conv={} error={}",
                    elapsed.as_secs_f64(),
                    self.conversation_id,
                    e
                );
                // Persist so router doesn't retry on next message or after restart
                let _ = conversations::set_router_result(
                    &self.db,
                    &self.conversation_id,
                    Some("__failed__"),
                    None,
                )
                .await;
                original
            }
            Err(_) => {
                tracing::warn!(
                    "[ROUTER] === TIMEOUT ({}s) === conv={}",
                    ROUTER_TIMEOUT_SECS,
                    self.conversation_id
                );
                let _ = conversations::set_router_result(
                    &self.db,
                    &self.conversation_id,
                    Some("__timeout__"),
                    None,
                )
                .await;
                original
            }
        }
    }

    /// Inner router logic, separated so the caller can wrap it in a timeout.
    async fn run_ticket_router_inner(&mut self, user_message: &str) -> Result<String, String> {
        let router_type = AgentType::TicketRouter;

        // Build prompt variables
        let mut vars = std::collections::HashMap::new();
        vars.insert("USER_MESSAGE".to_string(), user_message.to_string());

        // Fetch organizations from DB for the prompt
        let orgs = ticketing_system::organizations::list_organizations(&self.db)
            .await
            .unwrap_or_default();
        let org_names: Vec<String> = orgs.iter().map(|o| o.name.clone()).collect();
        vars.insert("ORGANIZATIONS".to_string(), org_names.join(", "));

        // Load the router prompt with variable substitution
        let system_prompt = load_prompt(router_type.as_str(), vars)
            .map_err(|e| format!("Failed to load router prompt: {}", e))?;

        // Build tools list
        let tools_list: Vec<String> = router_type
            .allowed_tools()
            .iter()
            .map(|s| s.to_string())
            .collect();

        let has_mcp_tools = tools_list.iter().any(|t| t.starts_with("mcp__"));

        // Build cc-sdk client options for the router
        let mut builder = ClaudeCodeOptions::builder()
            .system_prompt(&system_prompt)
            .model(router_type.model())
            .tools(ToolsConfig::list(tools_list.clone()))
            .allowed_tools(tools_list)
            .disallowed_tools(crate::safety::disallowed_tools())
            .permission_mode(PermissionMode::BypassPermissions)
            .cwd(std::path::Path::new("/tmp"));

        // Register MCP server (router uses mcp__agentic-mcp__* tools)
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

        // Set max turns (1 for router — single turn)
        if let Some(turns) = router_type.max_turns() {
            builder = builder.max_turns(turns);
        }

        // Set effort level (low for router)
        builder = builder.add_extra_arg("effort", Some(router_type.effort().to_string()));

        // No session resumption — router is ephemeral
        let options = builder.build();
        let mut sdk_client = ClaudeSDKClient::new(options);

        // Connect with a 30s timeout
        match tokio::time::timeout(Duration::from_secs(30), sdk_client.connect(None)).await {
            Ok(Ok(())) => {
                tracing::info!("[ROUTER] Client connected");
            }
            Ok(Err(e)) => {
                return Err(format!("Router connect failed: {}", e));
            }
            Err(_) => {
                return Err("Router connect timed out after 30s".to_string());
            }
        }

        // Send the user's message (the prompt already has it embedded, so send a short trigger)
        if let Err(e) = sdk_client
            .send_user_message("Route this message.".to_string())
            .await
        {
            return Err(format!("Router send failed: {}", e));
        }

        // Stream the response — router is silent (no SSE events), only logging
        let mut response_stream = sdk_client.receive_messages().await;
        let mut router_text_parts: Vec<String> = Vec::new();
        let mut tool_call_count = 0u32;

        loop {
            match response_stream.next().await {
                Some(Ok(sdk_msg)) => {
                    if let Message::Assistant {
                        message: assistant_msg,
                    } = &sdk_msg
                    {
                        for block in &assistant_msg.content {
                            match block {
                                ContentBlock::Text(text_content) => {
                                    // Collect text for parsing but don't emit RouterText —
                                    // the output is XML (<router_result>) not human-readable.
                                    // Router tool calls already show what it's doing.
                                    router_text_parts.push(text_content.text.clone());
                                }
                                ContentBlock::ToolUse(tool_use) => {
                                    tool_call_count += 1;
                                    tracing::info!(
                                        "[ROUTER] Tool #{}: {} ({})",
                                        tool_call_count,
                                        tool_use.name,
                                        tool_use.id
                                    );
                                }
                                ContentBlock::ToolResult(_tool_result) => {
                                    // Silent — router tool results are not shown to the user.
                                }
                                ContentBlock::Thinking(_) => {
                                    // Ignore thinking blocks from router
                                }
                            }
                        }
                    }

                    // Check for result message — router is done
                    if let Message::Result { .. } = &sdk_msg {
                        tracing::info!(
                            "[ROUTER] Completed — {} tool calls, {} text parts",
                            tool_call_count,
                            router_text_parts.len()
                        );
                        break;
                    }
                }
                Some(Err(e)) => {
                    return Err(format!("Router stream error: {}", e));
                }
                None => {
                    tracing::warn!("[ROUTER] Stream ended without Result");
                    break;
                }
            }
        }

        // Parse the router output
        let full_output = router_text_parts.join("");
        tracing::info!(
            "[ROUTER] Full output ({} chars): {}",
            full_output.len(),
            if full_output.len() > 200 {
                &full_output[..(0..=200)
                    .rev()
                    .find(|&i| full_output.is_char_boundary(i))
                    .unwrap_or(0)]
            } else {
                &full_output
            }
        );

        let parsed = parse_router_result(&full_output);

        match parsed {
            RouterParsed::Skipped => {
                tracing::info!("[ROUTER] Skipped — no ticket match needed");
                // Persist sentinel so has_router_run() returns true even for skipped
                let _ = conversations::set_router_result(
                    &self.db,
                    &self.conversation_id,
                    Some("__skipped__"),
                    None,
                )
                .await;
                self.emit_event(&StreamEvent::RouterResult {
                    enriched_message: user_message.to_string(),
                    ticket_id: None,
                    organization: None,
                    skipped: true,
                })
                .await;
                Ok(user_message.to_string())
            }
            RouterParsed::Enriched {
                enriched_message,
                ticket_id,
                organization,
            } => {
                tracing::info!(
                    "[ROUTER] Matched ticket={:?}, org={:?}",
                    ticket_id,
                    organization
                );
                // Persist router result to DB — survives server restarts
                self.last_router_ticket_id = ticket_id.clone();
                self.last_router_organization = organization.clone();
                if let Err(e) = conversations::set_router_result(
                    &self.db,
                    &self.conversation_id,
                    ticket_id.as_deref(),
                    organization.as_deref(),
                )
                .await
                {
                    tracing::warn!("[ROUTER] Failed to persist router result: {}", e);
                }
                self.emit_event(&StreamEvent::RouterResult {
                    enriched_message: enriched_message.clone(),
                    ticket_id: ticket_id.clone(),
                    organization: organization.clone(),
                    skipped: false,
                })
                .await;
                Ok(enriched_message)
            }
            RouterParsed::ParseFailed(reason) => {
                tracing::warn!("[ROUTER] Parse failed: {}", reason);
                // Persist sentinel so router doesn't re-run on next message
                let _ = conversations::set_router_result(
                    &self.db,
                    &self.conversation_id,
                    Some("__failed__"),
                    None,
                )
                .await;
                // Fall back to original message
                self.emit_event(&StreamEvent::RouterResult {
                    enriched_message: user_message.to_string(),
                    ticket_id: None,
                    organization: None,
                    skipped: true,
                })
                .await;
                Ok(user_message.to_string())
            }
        }
    }

    /// Emit a StreamEvent: translate to Anthropic events, persist, and broadcast.
    ///
    /// Uses the server-side allocator (`insert_conversation_event`) so the
    /// event_index comes from the DB under a BEGIN IMMEDIATE transaction —
    /// safe under concurrent writers for the same conversation_id.
    ///
    /// Single-path emission (T-49352BF5): the translator maps each internal
    /// `StreamEvent` to zero or more Anthropic frames from the 8-event
    /// vocabulary (`message_start` / `content_block_start` / `content_block_delta`
    /// / `content_block_stop` / `message_delta` / `message_stop` / `ping` /
    /// `error`). Router/metadata events produce zero frames; a text chunk
    /// produces one; a tool_use turn produces several. Every produced frame
    /// is persisted AND broadcast to live SSE subscribers — there is no
    /// legacy vocabulary, no dual-write, no feature flag.
    async fn emit_event(&mut self, event: &StreamEvent) {
        // Translate the StreamEvent into a sequence of Anthropic events.
        // An event may produce zero frames (router/metadata events), one
        // frame (a text_delta), or many (tool_use: start + delta + stop).
        //
        // After the write loop, if this StreamEvent produced at least one
        // `message_stop` that was successfully persisted, we trigger the
        // silent-push fan-out (T-90C7FAC4) so every registered device for
        // the turn's owner gets a wake signal. The fan-out is fire-and-forget
        // — spawned onto its own tokio task so it never blocks the SSE
        // stream or holds the worker's state.
        let mut message_stop_persisted = false;
        let anthropic_events = self.translator.translate(event);
        for ae in anthropic_events {
            let ae_type = ae.event_type();
            let ae_json = match serde_json::to_string(&ae) {
                Ok(j) => j,
                Err(e) => {
                    tracing::error!(
                        "[WORKER] Failed to serialize AnthropicEvent for {}: {}",
                        self.conversation_id,
                        e
                    );
                    continue;
                }
            };
            // Payload is JSON text. mime=None defaults to "application/json"
            // inside the allocator (T-E184E642). When ae_json.len() > 4096
            // the allocator transparently offloads to event_blobs and writes
            // the canonical sentinel; the broadcast below still ships the
            // real JSON to live SSE subscribers.
            match conversations::insert_conversation_event(
                &self.db,
                &self.conversation_id,
                ae_type,
                ae_json.as_bytes(),
                None, // mime defaults to application/json
            )
            .await
            {
                Ok(allocated_index) => {
                    // Gap detection: the allocator returns the actual
                    // DB-assigned index. If it skipped ahead of the
                    // worker's expected next index, another writer or a
                    // rollback produced a gap. `record_gap_detected` is
                    // a no-op when the indices match.
                    record_gap_detected(
                        &self.conversation_id,
                        self.event_index,
                        allocated_index,
                    );
                    let bytes = ae_json.len();
                    let broadcast_tx = get_broadcast_sender(&self.conversation_id).await;
                    let _ = broadcast_tx.send((allocated_index, ae_json));
                    self.event_index = allocated_index + 1;
                    record_stream_event_emitted(&self.conversation_id, bytes);
                    if ae_type == "message_stop" {
                        message_stop_persisted = true;
                    }
                }
                Err(e) => {
                    tracing::error!(
                        "[WORKER] Failed to persist event for {}: {}",
                        self.conversation_id,
                        e
                    );
                }
            }
        }

        // Silent-push fan-out hook (T-90C7FAC4). Only fires when:
        //   1. A `message_stop` row was successfully committed above.
        //   2. We have a cached user_id for this turn (set at the top
        //      of `process_message`).
        //   3. The process-wide silent-push config is installed (always
        //      true when main.rs startup succeeded).
        //
        // The config carries the `enabled` flag; when disabled the
        // fan-out still runs and emits
        // `push_attempts_total{result="skipped_disabled"}` for every
        // registered device so dashboards stay accurate.
        //
        // `tokio::spawn` detaches the task so we return to the SSE
        // stream immediately. We intentionally clone every value we
        // need into the task — the worker holds no lock across the
        // spawn and the fan-out never touches the worker state.
        if message_stop_persisted {
            if let Some(user_id) = self.current_user_id.clone() {
                if let Some(config) = crate::apns::silent_fanout::global_config() {
                    let last_message_id = self
                        .translator
                        .current_message_id()
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| {
                            // Translator should always have a message_id
                            // cached by the time message_stop ships, but
                            // if translator state was reset mid-turn we
                            // fall back to the conversation id so the
                            // payload still parses on the client.
                            self.conversation_id.clone()
                        });
                    let pool = self.db.clone();
                    let conv_id = self.conversation_id.clone();
                    let enabled = config.enabled;
                    let sender = config.sender.clone();
                    tokio::spawn(async move {
                        crate::apns::silent_fanout::fan_out_silent_push(
                            pool,
                            sender,
                            enabled,
                            user_id,
                            conv_id,
                            last_message_id,
                        )
                        .await;
                    });
                } else {
                    tracing::debug!(
                        target: "silent_push",
                        conversation_id = %self.conversation_id,
                        "[SILENT_PUSH_FANOUT] global config not installed — skipping fan-out (dev env?)"
                    );
                }
            }
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
            tracing::info!(
                "[WORKER] Client for {} disconnected, will recreate",
                self.conversation_id
            );
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

    let tools_list: Vec<String> = config
        .agent_type
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

    // Always inject conversation history as context when messages exist.
    // For resumed sessions: safety net if --resume fails silently.
    // For new sessions on existing conversations (e.g., nightly scheduler stubs):
    // provides the only context the agent will have.
    let system_prompt = match conversations::list_messages(db, conv_id, None, None).await {
        Ok(messages) if !messages.is_empty() => {
            tracing::info!(
                "[WORKER] Injecting {} messages as resume context for {}",
                messages.len(),
                conv_id
            );
            let history = build_conversation_history(&messages);
            format!("{}\n\n{}", system_prompt, history)
        }
        _ => system_prompt,
    };

    let mut builder = ClaudeCodeOptions::builder()
        .system_prompt(&system_prompt)
        .model(config.agent_type.model())
        .tools(ToolsConfig::list(tools_list.clone()))
        .allowed_tools(tools_list)
        .disallowed_tools(crate::safety::disallowed_tools())
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

    // Increase channel buffer size to handle bursts of simultaneous Agent task
    // results without backpressure. Default is 100, which was insufficient when
    // 4+ Agent tasks completed at the same millisecond (see incident A-EB7EE722).
    builder = builder.cli_channel_buffer_size(500);

    // Enable adaptive thinking via --effort flag
    builder = builder.add_extra_arg("effort", Some(config.agent_type.effort().to_string()));

    if let Some(ref sid) = saved_session_id {
        tracing::info!(
            "[WORKER] Resuming conversation {} from session {}",
            conv_id,
            sid
        );
        builder = builder.resume(sid.clone());
    } else {
        tracing::info!(
            "[WORKER] Creating fresh client for conversation {}",
            conv_id
        );
    }

    let options = builder.build();
    let mut sdk_client = ClaudeSDKClient::new(options);

    tracing::info!(
        "[WORKER] Connecting client for {} (30s timeout)...",
        conv_id
    );
    match tokio::time::timeout(Duration::from_secs(30), sdk_client.connect(None)).await {
        Ok(Ok(())) => {
            tracing::info!("[WORKER] Client connected for {}", conv_id);
        }
        Ok(Err(e)) => {
            tracing::error!("[WORKER] Client connect error for {}: {}", conv_id, e);
            return Err(format!("Failed to connect: {}", e));
        }
        Err(_) => {
            tracing::error!(
                "[WORKER] Client connect timed out after 30s for {}",
                conv_id
            );
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
async fn flush_to_db(db: &SqlitePool, assistant_message_id: Option<&str>, accumulated_text: &str) {
    if let Some(msg_id) = assistant_message_id {
        if let Err(e) = conversations::update_message(db, msg_id, accumulated_text).await {
            tracing::error!("[WORKER] Failed to flush message to DB: {}", e);
        }
    }
}

/// Parsed result from the ticket-router's `<router_result>` XML output.
enum RouterParsed {
    /// Router decided no ticket is needed (skipped="true")
    Skipped,
    /// Router matched or created a ticket
    Enriched {
        enriched_message: String,
        ticket_id: Option<String>,
        organization: Option<String>,
    },
    /// Could not parse the router output
    ParseFailed(String),
}

/// Parse the `<router_result>` XML block from the router agent's text output.
///
/// Expected formats:
///   `<router_result skipped="true">...original message...</router_result>`
///   `<router_result>...enriched message with --- metadata block ---...</router_result>`
fn parse_router_result(output: &str) -> RouterParsed {
    // Check for skipped result first
    if let Some(start) = output.find("<router_result skipped=\"true\">") {
        if let Some(end) = output.find("</router_result>") {
            let inner = output[start + "<router_result skipped=\"true\">".len()..end].trim();
            if !inner.is_empty() {
                return RouterParsed::Skipped;
            }
        }
        return RouterParsed::Skipped;
    }

    // Check for enriched result
    let tag_start = match output.find("<router_result>") {
        Some(idx) => idx,
        None => {
            return RouterParsed::ParseFailed("No <router_result> tag found in output".to_string())
        }
    };

    let tag_end = match output.find("</router_result>") {
        Some(idx) => idx,
        None => {
            return RouterParsed::ParseFailed("No </router_result> closing tag found".to_string())
        }
    };

    let inner = output[tag_start + "<router_result>".len()..tag_end]
        .trim()
        .to_string();
    if inner.is_empty() {
        return RouterParsed::ParseFailed("Empty <router_result> block".to_string());
    }

    // Parse the metadata block between --- markers
    let mut ticket_id: Option<String> = None;
    let mut organization: Option<String> = None;

    // Look for the --- delimited metadata section
    let parts: Vec<&str> = inner.splitn(2, "\n---\n").collect();
    if parts.len() == 2 {
        // There's a metadata block after the first ---
        let metadata_section = parts[1];
        // The metadata block ends at the second ---
        let metadata = if let Some(end_idx) = metadata_section.find("\n---") {
            &metadata_section[..end_idx]
        } else {
            metadata_section
        };

        for line in metadata.lines() {
            let line = line.trim();
            // Parse "Ticket: T-XXXXXXXX | Organization: org-name | Status: open"
            if line.starts_with("Ticket:") {
                for part in line.split('|') {
                    let part = part.trim();
                    if let Some(tid) = part.strip_prefix("Ticket:") {
                        let tid = tid.trim();
                        if tid.starts_with("T-") {
                            ticket_id = Some(tid.to_string());
                        }
                    } else if let Some(org) = part.strip_prefix("Organization:") {
                        organization = Some(org.trim().to_string());
                    }
                }
            }
        }
    }

    RouterParsed::Enriched {
        enriched_message: inner,
        ticket_id,
        organization,
    }
}

/// Build a condensed conversation history for system prompt injection.
/// Used as a safety net when resuming sessions — if --resume works, this is
/// redundant; if it fails silently, Claude still has context.
fn build_conversation_history(messages: &[ConversationMessage]) -> String {
    let mut history = String::from(
        "## Previous Conversation Context\n\n\
         IMPORTANT: This conversation was resumed but the previous session could not be fully \
         restored. You MUST use the context below to continue the conversation seamlessly. \
         Do NOT ask the user to repeat themselves or clarify what they were asking about — \
         the full conversation history is provided here. Pick up exactly where you left off.\n\n",
    );

    // Take last 30 messages to keep prompt size reasonable
    let recent = if messages.len() > 30 {
        &messages[messages.len() - 30..]
    } else {
        messages
    };

    for msg in recent {
        let has_content = !msg.content.is_empty();
        let has_tools = msg
            .tool_call_summaries
            .as_ref()
            .map_or(false, |t| !t.is_empty());
        if !has_content && !has_tools {
            continue;
        }

        let role = if msg.role == "user" {
            "User"
        } else {
            "Assistant"
        };

        // User messages: include in full (they're typically short)
        // Assistant messages: allow up to 2000 chars (was 300 — way too aggressive)
        let content = if msg.role == "assistant" && msg.content.len() > 2000 {
            let truncated: String = msg.content.chars().take(2000).collect();
            format!("{}… [truncated]", truncated)
        } else {
            msg.content.clone()
        };

        if !content.is_empty() {
            history.push_str(&format!("**{}**: {}\n\n", role, content));
        }

        // Include tool call summaries for assistant messages — critical for context
        // preservation. Without these, the agent has no idea what tools were used or
        // what results came back.
        if let Some(ref summaries) = msg.tool_call_summaries {
            if !summaries.is_empty() {
                history.push_str("**Tool calls made:**\n");
                for tc in summaries {
                    let status = if tc.is_error { " [ERROR]" } else { "" };
                    let preview = tc.result_preview.as_deref().unwrap_or("");
                    let preview_truncated = if preview.len() > 200 {
                        let t: String = preview.chars().take(200).collect();
                        format!("{}…", t)
                    } else {
                        preview.to_string()
                    };
                    history.push_str(&format!(
                        "- `{}`{}: {}\n",
                        tc.tool_name, status, preview_truncated
                    ));
                }
                history.push('\n');
            }
        }
    }

    history
}

// =============================================================================
// T-49352BF5 — scope-cut acceptance tests
// =============================================================================
//
// These tests exercise the single-path Anthropic 8-event persistence pipeline
// after the dual-write scaffold was removed. They confirm:
//
//   1. Translator → `insert_conversation_event` → `get_events` round-trips
//      produce ONLY Anthropic vocabulary event_types (no legacy `text`,
//      `tool_use`, `thinking` strings ever land in the row).
//   2. A text-only turn, a tool-use turn, and a thinking turn each produce
//      the canonical Anthropic frame ordering when replayed off the DB.
//   3. Every persisted `event_data` payload parses as valid Anthropic JSON
//      whose `type` field matches the row's `event_type`.
//
// If any dual-write code path regressed back into the emit pipeline these
// assertions would detect it — the replayed event_types would include a
// non-Anthropic tag, or the row counts would diverge from translator output.
#[cfg(test)]
mod t_49352bf5_streaming_persistence_tests {
    use super::*;
    use crate::agents::anthropic_events::ALL_EVENT_TYPES;
    use serde_json::json;
    use sqlx::sqlite::SqlitePoolOptions;
    use sqlx::ConnectOptions;
    use std::str::FromStr;

    /// Allow-list of event_type strings any post-scope-cut row may carry.
    /// These are the canonical 8 Anthropic streaming event type tags, as
    /// exported by [`ALL_EVENT_TYPES`]. If a dual-write regression ever
    /// sneaks a legacy `text`/`tool_use`/`thinking` row back into the
    /// persistence path, this allow-list is what trips first.
    const ALLOWED_EVENT_TYPES: &[&str] = ALL_EVENT_TYPES;

    async fn fresh_test_pool() -> SqlitePool {
        let options = sqlx::sqlite::SqliteConnectOptions::from_str("sqlite::memory:")
            .expect("parse sqlite url")
            .foreign_keys(true)
            .disable_statement_logging();
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .expect("connect test pool");

        sqlx::query(
            r#"
            CREATE TABLE conversations (
                id TEXT PRIMARY KEY,
                user_id TEXT,
                session_id TEXT,
                organization TEXT,
                agent TEXT,
                title TEXT,
                started_at TEXT,
                updated_at TEXT,
                status TEXT NOT NULL DEFAULT 'open',
                archived_at TEXT,
                router_ticket_id TEXT,
                router_organization TEXT
            )
            "#,
        )
        .execute(&pool)
        .await
        .expect("create conversations");

        sqlx::query(
            r#"
            CREATE TABLE conversation_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                event_index INTEGER NOT NULL,
                event_type TEXT NOT NULL,
                event_data TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                UNIQUE(conversation_id, event_index)
            )
            "#,
        )
        .execute(&pool)
        .await
        .expect("create conversation_events");

        sqlx::query(
            r#"
            CREATE TABLE event_blobs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_id INTEGER NOT NULL REFERENCES conversation_events(id) ON DELETE CASCADE,
                mime TEXT NOT NULL,
                bytes BLOB NOT NULL,
                created_at INTEGER NOT NULL
            )
            "#,
        )
        .execute(&pool)
        .await
        .expect("create event_blobs");

        sqlx::query("INSERT INTO conversations (id, status) VALUES (?, 'open')")
            .bind("c-t49352bf5")
            .execute(&pool)
            .await
            .expect("seed conversation");

        pool
    }

    /// Drive a sequence of cc-sdk StreamEvents through the translator, persist
    /// every emitted Anthropic event via `insert_conversation_event`, then
    /// replay from the DB and return (event_types, parsed_json_values).
    async fn run_turn_and_replay(
        pool: &SqlitePool,
        conversation_id: &str,
        events: &[StreamEvent],
    ) -> (Vec<String>, Vec<serde_json::Value>) {
        let mut translator = AnthropicTranslator::new(conversation_id);
        for ev in events {
            for ae in translator.translate(ev) {
                let ae_type = ae.event_type();
                let ae_json = serde_json::to_string(&ae).expect("serialize anthropic event");
                conversations::insert_conversation_event(
                    pool,
                    conversation_id,
                    ae_type,
                    ae_json.as_bytes(),
                    None,
                )
                .await
                .expect("persist anthropic event");
            }
        }

        let rows = conversations::get_events(pool, conversation_id)
            .await
            .expect("replay events");
        let types: Vec<String> = rows.iter().map(|r| r.event_type.clone()).collect();
        let values: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| serde_json::from_str(&r.event_data).expect("parse persisted event_data"))
            .collect();
        (types, values)
    }

    fn assert_all_anthropic(types: &[String]) {
        for t in types {
            assert!(
                ALLOWED_EVENT_TYPES.contains(&t.as_str()),
                "forbidden event_type leaked into DB: {} (allow-list: {:?})",
                t,
                ALLOWED_EVENT_TYPES
            );
        }
    }

    fn assert_payload_type_matches_column(types: &[String], values: &[serde_json::Value]) {
        assert_eq!(types.len(), values.len());
        for (t, v) in types.iter().zip(values.iter()) {
            assert_eq!(
                v["type"].as_str(),
                Some(t.as_str()),
                "persisted event_data.type disagrees with event_type column (row type={}, data={})",
                t,
                v
            );
        }
    }

    /// Turn 1: a text-only assistant reply.
    /// Canonical Anthropic frame sequence: message_start, content_block_start(text),
    /// content_block_delta(text_delta), content_block_stop, message_delta, message_stop.
    #[tokio::test]
    async fn text_only_turn_persists_anthropic_frames_only() {
        let pool = fresh_test_pool().await;
        let (types, values) = run_turn_and_replay(
            &pool,
            "c-t49352bf5",
            &[
                StreamEvent::Text {
                    content: "Hello from the agent.".into(),
                },
                StreamEvent::Result {
                    session_id: "s-text".into(),
                    status: "success".into(),
                    is_error: false,
                },
            ],
        )
        .await;

        assert_all_anthropic(&types);
        assert_eq!(
            types,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_payload_type_matches_column(&types, &values);

        // Sanity: the text delta actually carries the text payload.
        let delta = &values[2];
        assert_eq!(delta["delta"]["type"], "text_delta");
        assert_eq!(delta["delta"]["text"], "Hello from the agent.");
    }

    /// Turn 2: tool-use turn (assistant says something, invokes a tool,
    /// emits a text trailer, then Result closes the turn).
    #[tokio::test]
    async fn tool_use_turn_persists_anthropic_frames_only() {
        let pool = fresh_test_pool().await;
        let (types, values) = run_turn_and_replay(
            &pool,
            "c-t49352bf5",
            &[
                StreamEvent::Text {
                    content: "Searching.".into(),
                },
                StreamEvent::ToolUse {
                    id: "tool-xyz".into(),
                    name: "search".into(),
                    input: json!({"q": "anthropic streaming"}),
                },
                StreamEvent::Text {
                    content: "Done.".into(),
                },
                StreamEvent::Result {
                    session_id: "s-tool".into(),
                    status: "success".into(),
                    is_error: false,
                },
            ],
        )
        .await;

        assert_all_anthropic(&types);
        assert_eq!(
            types,
            vec![
                "message_start",
                "content_block_start", // text 0
                "content_block_delta",
                "content_block_stop", // text 0 closes before tool_use
                "content_block_start", // tool_use 1
                "content_block_delta", // input_json_delta
                "content_block_stop",
                "content_block_start", // text 2
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_payload_type_matches_column(&types, &values);

        // The tool_use content_block_start must carry the tool id + name.
        let tool_start = &values[4];
        assert_eq!(tool_start["content_block"]["type"], "tool_use");
        assert_eq!(tool_start["content_block"]["id"], "tool-xyz");
        assert_eq!(tool_start["content_block"]["name"], "search");

        // The input_json_delta serializes the input as a partial JSON string.
        let input_delta = &values[5];
        assert_eq!(input_delta["delta"]["type"], "input_json_delta");
        let partial: serde_json::Value =
            serde_json::from_str(input_delta["delta"]["partial_json"].as_str().unwrap())
                .expect("partial_json parses");
        assert_eq!(partial, json!({"q": "anthropic streaming"}));
    }

    /// Turn 3: thinking + text turn. Thinking opens a dedicated block that
    /// streams `thinking_delta` payloads, then closes before the text block
    /// takes over.
    #[tokio::test]
    async fn thinking_turn_persists_anthropic_frames_only() {
        let pool = fresh_test_pool().await;
        let (types, values) = run_turn_and_replay(
            &pool,
            "c-t49352bf5",
            &[
                StreamEvent::Thinking {
                    content: "Reasoning step...".into(),
                },
                StreamEvent::Text {
                    content: "Here is the answer.".into(),
                },
                StreamEvent::Result {
                    session_id: "s-think".into(),
                    status: "success".into(),
                    is_error: false,
                },
            ],
        )
        .await;

        assert_all_anthropic(&types);
        assert_eq!(
            types,
            vec![
                "message_start",
                "content_block_start", // thinking
                "content_block_delta", // thinking_delta
                "content_block_stop",
                "content_block_start", // text
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_payload_type_matches_column(&types, &values);

        let thinking_start = &values[1];
        assert_eq!(thinking_start["content_block"]["type"], "thinking");
        let thinking_delta = &values[2];
        assert_eq!(thinking_delta["delta"]["type"], "thinking_delta");
        assert_eq!(thinking_delta["delta"]["thinking"], "Reasoning step...");
    }

    /// Silencer: confirm that legacy cc-sdk router/replay tags are NOT
    /// emitted onto the wire. If a dual-write regression snuck them in, this
    /// test would see extra rows with event_type outside the allow-list.
    #[tokio::test]
    async fn router_and_replay_tags_are_never_persisted() {
        let pool = fresh_test_pool().await;
        let (types, _values) = run_turn_and_replay(
            &pool,
            "c-t49352bf5",
            &[
                StreamEvent::RouterText {
                    content: "internal routing note".into(),
                },
                StreamEvent::TitleUpdate {
                    title: "ignored".into(),
                },
                StreamEvent::ReplayComplete {
                    total_events: 0,
                    agent_status: "running".into(),
                },
                StreamEvent::Text {
                    content: "real content".into(),
                },
                StreamEvent::Result {
                    session_id: "s-router".into(),
                    status: "success".into(),
                    is_error: false,
                },
            ],
        )
        .await;

        assert_all_anthropic(&types);
        // Exactly the text-only canonical shape — router/title/replay frames
        // produced ZERO persisted rows.
        assert_eq!(
            types,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
    }

    /// Sanity check: the allow-list we assert against is sourced directly
    /// from the canonical `ALL_EVENT_TYPES` export in `anthropic_events`.
    /// If the wire vocabulary ever grows or shrinks, both the enum surface
    /// and this allow-list must move in lock-step — and the test below
    /// will fail loudly if someone edits one without the other.
    #[test]
    fn allow_list_matches_anthropic_event_surface() {
        assert_eq!(
            ALLOWED_EVENT_TYPES, ALL_EVENT_TYPES,
            "scope-cut allow-list drifted from canonical AnthropicEvent surface"
        );
    }
}
