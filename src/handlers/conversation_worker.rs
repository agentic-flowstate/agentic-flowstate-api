use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use super::anthropic_event_encoder::AnthropicEventEncoder;
use super::chat_client_manager::ChatClientManager;
use super::chat_stream::{
    get_broadcast_sender, remove_broadcast_channel, ChatAttachmentData, ChatConfig,
};
use crate::agents::codex_app_server::{
    app_server_generated_images_dir, spawn_codex_app_server, CodexAppServerEvent,
    CodexAppServerOptions, CodexSandboxMode, CodexToolProfile,
};
use crate::agents::prompts::load_prompt;
use crate::agents::{AgentType, StreamEvent};
use crate::observability::next_actions::{record_clear, NextActionClearReason};
use crate::observability::streaming::{
    record_gap_detected, record_stream_event_emitted, record_ticket_preflight,
    record_ticket_preflight_error,
};
use ticketing_system::{
    agent_runners, checkpoints, conversations,
    token_usage::{self, TokenUsageBreakdown},
    AddMessageRequest, ContentBlockDesc, ConversationMessage, UpdateConversationRequest,
};

/// How often to flush accumulated content to the database (ms).
const DB_FLUSH_INTERVAL_MS: u64 = 500;

/// How long a worker idles before shutting down.
const IDLE_TIMEOUT_SECS: u64 = 600; // 10 minutes

/// Maximum number of prior messages to load into prompt context per turn.
const PROMPT_HISTORY_MESSAGE_LIMIT: usize = 30;

#[derive(Debug, Clone, Deserialize, Serialize)]
struct AttachmentMeta {
    filename: String,
    display_name: Option<String>,
    path: String,
    mime_type: String,
    size_bytes: Option<i64>,
}

fn codex_tool_profile_for_chat_agent(agent_type: &AgentType) -> CodexToolProfile {
    match agent_type {
        AgentType::HomePlanner
        | AgentType::MeetingAgent
        | AgentType::ConversationEvaluator
        | AgentType::Feedback => CodexToolProfile::ConfiguredMcpOnly,
        AgentType::ScopedWorkspace | AgentType::WorkspaceManager => {
            CodexToolProfile::RestrictedMcpOnly
        }
        _ => CodexToolProfile::Default,
    }
}

fn codex_sandbox_policy_for_chat_agent(
    agent_type: &AgentType,
) -> (CodexSandboxMode, bool, CodexToolProfile) {
    let tool_profile = codex_tool_profile_for_chat_agent(agent_type);
    if matches!(
        tool_profile,
        CodexToolProfile::ConfiguredMcpOnly | CodexToolProfile::RestrictedMcpOnly
    ) {
        (CodexSandboxMode::ReadOnly, false, tool_profile)
    } else {
        (
            CodexSandboxMode::DangerFullAccess,
            true,
            CodexToolProfile::Default,
        )
    }
}

/// Message sent to a ConversationWorker via its mpsc channel.
pub struct WorkerMessage {
    pub user_id: String,
    pub message: String,
    pub config: ChatConfig,
    pub attachments: Option<Vec<ChatAttachmentData>>,
    /// Per-request completion signal. Fired when this message is fully processed,
    /// so the SSE handler exits only when THIS message is done (not on terminal
    /// events from a prior message in the same conversation).
    pub completion_tx: Option<tokio::sync::oneshot::Sender<()>>,
    /// Optional `Idempotency-Key` the client supplied on the HTTP POST
    /// that triggered this turn (T-A819D36B). Threaded from the handler
    /// → [`WorkerMessage`] → [`AnthropicEventEncoder::set_pending_client_id`]
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
    /// Stateful encoder from internal `StreamEvent` to chat-stream events.
    /// Maintains message / content-block lifecycle across emit_event calls.
    /// One encoder instance per worker — it survives the entire
    /// conversation and is reset between turns by the runtime result event.
    encoder: AnthropicEventEncoder,
    /// User id for the turn currently being processed. Populated at the
    /// top of `process_message` from the inbound `WorkerMessage.user_id`
    /// and consumed by `emit_event` to fan out silent pushes on
    /// `message_stop` (T-90C7FAC4). `None` outside a turn.
    current_user_id: Option<String>,
    /// Client trace/idempotency id for the currently processed turn.
    current_client_id: Option<String>,
    first_message_start_logged: bool,
    first_content_delta_logged: bool,
}

impl ConversationWorker {
    pub fn new(
        db: Arc<SqlitePool>,
        conversation_id: String,
        manager: Arc<ChatClientManager>,
        message_rx: mpsc::Receiver<WorkerMessage>,
    ) -> Self {
        let encoder = AnthropicEventEncoder::new(&conversation_id);
        Self {
            db,
            conversation_id,
            manager,
            message_rx,
            event_index: 0,
            has_routed: false,
            last_router_ticket_id: None,
            last_router_organization: None,
            encoder,
            current_user_id: None,
            current_client_id: None,
            first_message_start_logged: false,
            first_content_delta_logged: false,
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

    /// Codex app-server turns are per-turn subprocesses. Durable session ids
    /// are written when a turn completes, so idle shutdown has no client state
    /// to persist.
    async fn save_session_and_disconnect(&self) {
        self.manager
            .remove_app_server_turn(&self.conversation_id)
            .await;
    }

    async fn publish_run_status(&self) {
        if let Err(e) =
            super::conversations::publish_conversation_run_status(&self.db, &self.conversation_id)
                .await
        {
            tracing::warn!(
                "[WORKER] Failed to publish run status for {}: {}",
                self.conversation_id,
                e
            );
        }
    }

    async fn mark_checkpoint_interrupted(&self) {
        if let Err(e) = checkpoints::mark_interrupted(&self.db, &self.conversation_id).await {
            tracing::warn!(
                "[WORKER] Failed to mark checkpoint interrupted for {}: {}",
                self.conversation_id,
                e
            );
        }
        self.publish_run_status().await;
    }

    async fn is_turn_cancelled(&self) -> bool {
        if self.manager.is_turn_cancelled(&self.conversation_id).await {
            return true;
        }
        match agent_runners::is_cancel_requested(&self.db, &self.conversation_id).await {
            Ok(cancelled) => cancelled,
            Err(e) => {
                tracing::warn!(
                    "[WORKER] Failed to inspect persistent cancellation for {}: {}",
                    self.conversation_id,
                    e
                );
                false
            }
        }
    }

    async fn consume_cancelled_turn(&self) -> bool {
        let memory_cancelled = self
            .manager
            .consume_cancelled_turn(&self.conversation_id)
            .await;
        let persistent_cancelled =
            match agent_runners::consume_cancel_request(&self.db, &self.conversation_id).await {
                Ok(cancelled) => cancelled,
                Err(e) => {
                    tracing::warn!(
                        "[WORKER] Failed to consume persistent cancellation for {}: {}",
                        self.conversation_id,
                        e
                    );
                    false
                }
            };

        memory_cancelled || persistent_cancelled
    }

    async fn consume_cancelled_turn_before_agent_start(&mut self, stage: &str) -> bool {
        if !self.is_turn_cancelled().await {
            return false;
        }
        if !self.consume_cancelled_turn().await {
            return false;
        }

        tracing::info!(
            "[WORKER] Cancelled queued/starting turn for {} at {}",
            self.conversation_id,
            stage
        );
        self.mark_checkpoint_interrupted().await;
        self.emit_event(&StreamEvent::Status {
            status: "cancelled".to_string(),
            message: Some("Cancelled by user".to_string()),
        })
        .await;
        true
    }

    /// Process a single user message: store in DB, send to SDK, consume entire response.
    async fn process_message(&mut self, mut msg: WorkerMessage) {
        // RAII guard: fires completion signal when this function exits (normal or early return)
        let _completion = CompletionGuard(msg.completion_tx.take());
        self.current_client_id = msg.client_id.clone();
        self.first_message_start_logged = false;
        self.first_content_delta_logged = false;
        // Cache the turn's owner so `emit_event` can fan out silent
        // pushes on `message_stop` without plumbing user_id through
        // every call site (T-90C7FAC4).
        self.current_user_id = Some(msg.user_id.clone());
        tracing::info!(
            "[CHAT_LATENCY] phase=worker_process_message_start conv={} client_id={} agent={} runtime={} model={} effort={} message_chars={} attachments={} started_at_ms={}",
            self.conversation_id,
            msg.client_id.as_deref().unwrap_or("none"),
            msg.config.agent_type.as_str(),
            msg.config.runtime.as_job_runtime(),
            msg.config.codex_options.model,
            msg.config.codex_options.reasoning_effort,
            msg.message.chars().count(),
            msg.attachments.as_ref().map_or(0, Vec::len),
            Utc::now().timestamp_millis()
        );
        // Stage the inbound Idempotency-Key (T-A819D36B) onto the
        // encoder BEFORE any StreamEvent is encoded. The encoder
        // consumes it on the first `open_message_if_needed` call (i.e.,
        // the next `message_start` frame). When `None`, it stays `None`
        // end-to-end — no synthetic fallbacks.
        self.encoder.set_pending_client_id(msg.client_id.clone());
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

        // Process file attachments (save to disk, build metadata)
        let mut attachment_descriptions: Vec<String> = Vec::new();
        let mut attachments_json: Option<String> = None;

        if let Some(ref attachments_to_save) = msg.attachments {
            if !attachments_to_save.is_empty() {
                let chat_attachments_dir = match chat_attachments_dir(&self.conversation_id) {
                    Ok(dir) => dir,
                    Err(e) => {
                        tracing::error!("[WORKER] Failed to resolve chat attachments dir: {}", e);
                        self.emit_event(&StreamEvent::Status {
                            status: "failed".to_string(),
                            message: Some("Failed to resolve attachment storage".to_string()),
                        })
                        .await;
                        return;
                    }
                };
                if let Err(e) = std::fs::create_dir_all(&chat_attachments_dir) {
                    tracing::error!(
                        "[WORKER] Failed to create chat attachments dir {}: {}",
                        chat_attachments_dir.display(),
                        e
                    );
                    self.emit_event(&StreamEvent::Status {
                        status: "failed".to_string(),
                        message: Some("Failed to create attachment storage".to_string()),
                    })
                    .await;
                    return;
                }

                let mut attachments: Vec<AttachmentMeta> = Vec::new();

                for attachment in attachments_to_save {
                    let Some(display_name) = sanitize_display_filename(&attachment.filename) else {
                        tracing::error!(
                            "[WORKER] Attachment filename is invalid: {}",
                            attachment.filename
                        );
                        self.emit_event(&StreamEvent::Status {
                            status: "failed".to_string(),
                            message: Some("Attachment filename is invalid".to_string()),
                        })
                        .await;
                        return;
                    };
                    let bytes = match STANDARD.decode(&attachment.data) {
                        Ok(bytes) => bytes,
                        Err(e) => {
                            tracing::error!(
                                "[WORKER] Failed to decode base64 attachment {}: {}",
                                display_name,
                                e
                            );
                            self.emit_event(&StreamEvent::Status {
                                status: "failed".to_string(),
                                message: Some(format!(
                                    "Failed to decode attachment {}",
                                    display_name
                                )),
                            })
                            .await;
                            return;
                        }
                    };
                    let stored_filename = format!("{}-{}", uuid::Uuid::new_v4(), display_name);
                    let file_path = chat_attachments_dir.join(&stored_filename);
                    if let Err(e) = std::fs::write(&file_path, &bytes) {
                        tracing::error!(
                            "[WORKER] Failed to write attachment {}: {}",
                            stored_filename,
                            e
                        );
                        self.emit_event(&StreamEvent::Status {
                            status: "failed".to_string(),
                            message: Some(format!("Failed to save attachment {}", display_name)),
                        })
                        .await;
                        return;
                    }
                    let path_str = file_path.to_string_lossy().to_string();
                    attachment_descriptions.push(format!(
                        "  - {} ({}; {} bytes): {}",
                        display_name,
                        attachment.mime_type,
                        bytes.len(),
                        path_str
                    ));
                    attachments.push(AttachmentMeta {
                        filename: stored_filename,
                        display_name: Some(display_name),
                        path: path_str,
                        mime_type: attachment.mime_type.clone(),
                        size_bytes: Some(bytes.len() as i64),
                    });
                    tracing::info!("[WORKER] Saved chat attachment: {}", file_path.display());
                }

                if !attachments.is_empty() {
                    attachments_json = serde_json::to_string(&attachments).ok();
                }
            }
        }

        // Build enhanced message for SDK (with attachment paths for the active runtime to read)
        let enhanced_message = if !attachment_descriptions.is_empty() {
            format!(
                "[The user has attached {} file(s). Server-side copies are available at:\n{}\nUse available tools to inspect these paths directly when relevant.]\n\n{}",
                attachment_descriptions.len(),
                attachment_descriptions.join("\n"),
                msg.message
            )
        } else {
            msg.message.clone()
        };

        match ticketing_system::conversation_next_actions::delete_for_conversation(
            &self.db,
            &self.conversation_id,
        )
        .await
        {
            Ok(deleted_count) => record_clear(
                &self.conversation_id,
                NextActionClearReason::NewUserTurn,
                deleted_count,
            ),
            Err(e) => tracing::warn!("[WORKER] Failed to clear stale next actions: {}", e),
        }

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

        // First root message -> generate title + detect org in background.
        if let Ok(ref stored) = stored_msg {
            if stored.message_index == 0 {
                let title_db = (*self.db).clone();
                let title_user = msg.user_id.clone();
                let title_conv = self.conversation_id.clone();
                let title_msg = msg.message.clone();
                let current_conversation =
                    conversations::get_conversation(&title_db, &title_conv, false)
                        .await
                        .ok()
                        .flatten();

                if current_conversation
                    .as_ref()
                    .and_then(|c| c.parent_conversation_id.as_ref())
                    .is_some()
                {
                    tracing::info!(
                        "[WORKER] Skipping title generation for child conversation {}",
                        title_conv
                    );
                } else {
                    let current_org = current_conversation
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
                            let title_event = StreamEvent::TitleUpdate {
                                title: result.title,
                            };
                            if let Ok(json) = serde_json::to_string(&title_event) {
                                let _ = broadcast_tx.send((-1, json));
                            }
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
        }

        if let Err(ref e) = stored_msg {
            tracing::error!("[WORKER] Failed to store user message: {}", e);
        }

        // Create checkpoint
        if let Err(e) =
            checkpoints::upsert_checkpoint(&self.db, &self.conversation_id, "pending", 0).await
        {
            tracing::warn!("[WORKER] Failed to create checkpoint: {}", e);
        } else {
            self.publish_run_status().await;
        }

        if self
            .consume_cancelled_turn_before_agent_start("before_router")
            .await
        {
            return;
        }

        // === TICKET PREFLIGHT ===
        // Use deterministic ticket lookup before agent start. It cannot invoke
        // an LLM and does not create tickets from chat startup, so first useful
        // work is not blocked on router-agent tool loops.
        let final_message = self.run_ticket_router(&enhanced_message, &msg.config).await;

        if self
            .consume_cancelled_turn_before_agent_start("after_router")
            .await
        {
            return;
        }

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

        if self
            .consume_cancelled_turn_before_agent_start("before_assistant_placeholder")
            .await
        {
            return;
        }

        // Create assistant message placeholder AFTER the forwarded message so it gets
        // a higher message_index. This ensures the chat displays in the correct order:
        // user message → forwarded context → assistant response.
        let assistant_message_id = match conversations::add_message(
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
            Ok(message) => {
                tracing::debug!(
                    "[WORKER] Created assistant placeholder: conv={} msg={}",
                    self.conversation_id,
                    message.id
                );
                message.id
            }
            Err(e) => {
                tracing::error!("[WORKER] Failed to create assistant message: {}", e);
                self.mark_checkpoint_interrupted().await;
                self.emit_event(&StreamEvent::Status {
                    status: "failed".to_string(),
                    message: Some(format!("Failed to create assistant message: {}", e)),
                })
                .await;
                return;
            }
        };
        self.encoder
            .set_pending_message_id(assistant_message_id.clone());

        // Bridge the gap between router completion and main agent streaming.
        // Without this, the UI shows no activity during client creation + connection.
        self.emit_event(&StreamEvent::Status {
            status: "running".to_string(),
            message: Some("Preparing agent...".to_string()),
        })
        .await;

        if self
            .consume_cancelled_turn_before_agent_start("before_runtime_start")
            .await
        {
            return;
        }

        self.process_codex_message(&msg, &final_message, assistant_message_id.clone())
            .await;
    }

    /// Run deterministic ticket preflight before the main agent starts.
    /// Returns an enriched message only when an explicit or clear ticket match
    /// is found. No LLM router is invoked and chat startup never creates
    /// tickets, epics, slices, or milestones.
    async fn run_ticket_router(&mut self, user_message: &str, config: &ChatConfig) -> String {
        let original = user_message.to_string();
        if matches!(
            config.agent_type,
            AgentType::ConversationEvaluator | AgentType::Feedback
        ) {
            self.has_routed = true;
            let _ = conversations::set_router_result(
                &self.db,
                &self.conversation_id,
                Some("__skipped__"),
                None,
            )
            .await;
            return original;
        }

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
            "[ROUTER] === RUNNING DETERMINISTIC PREFLIGHT === conv={}",
            self.conversation_id
        );

        let started = Instant::now();
        let result = ticketing_system::work_ticket::ensure_work_ticket(
            &self.db,
            ticketing_system::work_ticket::EnsureWorkTicketRequest {
                request: Some(user_message.to_string()),
                create_if_missing: false,
                mark_in_progress: false,
                ..Default::default()
            },
        )
        .await;

        self.has_routed = true;

        match result {
            Ok(response) => {
                record_ticket_preflight(&response.status, &response.action, response.elapsed_ms);
                tracing::info!(
                    "[ROUTER] === PREFLIGHT DONE ({}ms) === conv={} status={} action={} candidates={}",
                    response.elapsed_ms,
                    self.conversation_id,
                    response.status,
                    response.action,
                    response.candidate_count
                );

                if let Some(ticket) = response.ticket {
                    let enriched_message = Self::enrich_message_with_ticket(user_message, &ticket);
                    self.last_router_ticket_id = Some(ticket.ticket_id.clone());
                    self.last_router_organization = Some(ticket.organization.clone());
                    if let Err(e) = conversations::set_router_result(
                        &self.db,
                        &self.conversation_id,
                        Some(&ticket.ticket_id),
                        Some(&ticket.organization),
                    )
                    .await
                    {
                        tracing::warn!("[ROUTER] Failed to persist preflight result: {}", e);
                    }
                    self.emit_event(&StreamEvent::RouterResult {
                        enriched_message: enriched_message.clone(),
                        ticket_id: Some(ticket.ticket_id),
                        organization: Some(ticket.organization),
                        skipped: false,
                    })
                    .await;
                    enriched_message
                } else {
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
                    original
                }
            }
            Err(e) => {
                let elapsed_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
                record_ticket_preflight_error("failed", elapsed_ms);
                tracing::warn!(
                    "[ROUTER] === PREFLIGHT FAILED ({}ms) === conv={} error={}",
                    elapsed_ms,
                    self.conversation_id,
                    e
                );
                let _ = conversations::set_router_result(
                    &self.db,
                    &self.conversation_id,
                    Some("__failed__"),
                    None,
                )
                .await;
                original
            }
        }
    }

    fn enrich_message_with_ticket(user_message: &str, ticket: &ticketing_system::Ticket) -> String {
        format!(
            "{}\n\n---\nTicket: {} | Organization: {} | Status: {}\nEpic: {}\nSlice: {}\nTitle: {}\n---",
            user_message,
            ticket.ticket_id,
            ticket.organization,
            ticket.status,
            ticket.epic_id,
            ticket.slice_id,
            ticket.title
        )
    }

    async fn process_codex_message(
        &mut self,
        msg: &WorkerMessage,
        final_message: &str,
        assistant_message_id: String,
    ) {
        if self
            .consume_cancelled_turn_before_agent_start("before_codex_prompt")
            .await
        {
            return;
        }

        let system_prompt =
            match build_codex_system_prompt(&self.db, &self.conversation_id, &msg.config).await {
                Ok(prompt) => prompt,
                Err(e) => {
                    let mut accumulated_text = String::new();
                    let mut content_blocks = Vec::new();
                    let failure_message = format!("Failed to build Codex prompt: {}", e);
                    tracing::error!(
                        "[WORKER] Failed to build Codex prompt for {}: {}",
                        self.conversation_id,
                        e
                    );
                    self.mark_checkpoint_interrupted().await;
                    persist_failed_codex_message(
                        self,
                        &assistant_message_id,
                        &mut accumulated_text,
                        &mut content_blocks,
                        failure_message,
                    )
                    .await;
                    return;
                }
            };

        if self
            .consume_cancelled_turn_before_agent_start("before_codex_spawn")
            .await
        {
            return;
        }

        let runner_turn_id = match agent_runners::start_turn(
            &self.db,
            self.manager.runner_generation_id(),
            &self.conversation_id,
        )
        .await
        {
            Ok(turn_id) => turn_id,
            Err(e) => {
                let mut accumulated_text = String::new();
                let mut content_blocks = Vec::new();
                let failure_message = format!("Failed to claim runner turn: {}", e);
                tracing::error!(
                    "[WORKER] Failed to claim runner turn for {}: {}",
                    self.conversation_id,
                    e
                );
                self.mark_checkpoint_interrupted().await;
                persist_failed_codex_message(
                    self,
                    &assistant_message_id,
                    &mut accumulated_text,
                    &mut content_blocks,
                    failure_message,
                )
                .await;
                return;
            }
        };

        let (sandbox, bypass_approvals_and_sandbox, tool_profile) =
            codex_sandbox_policy_for_chat_agent(&msg.config.agent_type);
        let generated_images_root = match app_server_generated_images_dir(tool_profile) {
            Ok(path) => Some(path),
            Err(e) => {
                tracing::warn!(
                    "[WORKER] Failed to resolve Codex generated images dir for {:?}: {}",
                    tool_profile,
                    e
                );
                None
            }
        };
        let generated_attachments_before = generated_images_root
            .as_deref()
            .map(generated_attachment_snapshot)
            .unwrap_or_default();

        let codex_spawn_start_ms = Utc::now().timestamp_millis();
        tracing::info!(
            "[CHAT_LATENCY] phase=codex_spawn_start conv={} client_id={} runner_turn_id={} model={} effort={} started_at_ms={}",
            self.conversation_id,
            self.current_client_id.as_deref().unwrap_or("none"),
            runner_turn_id,
            msg.config.codex_options.model,
            msg.config.codex_options.reasoning_effort,
            codex_spawn_start_ms
        );

        let mut turn = match spawn_codex_app_server(CodexAppServerOptions {
            model: &msg.config.codex_options.model,
            reasoning_effort: &msg.config.codex_options.reasoning_effort,
            system_prompt: &system_prompt,
            working_dir: &msg.config.working_dir,
            prompt: final_message,
            sandbox,
            bypass_approvals_and_sandbox,
            resume_session_id: None,
            ephemeral: true,
            tool_profile,
            scoped_user_id: Some(&msg.user_id),
            approved_mcp_tools: msg.config.agent_type.approved_mcp_tool_names(),
        })
        .await
        {
            Ok(turn) => turn,
            Err(e) => {
                let mut accumulated_text = String::new();
                let mut content_blocks = Vec::new();
                let failure_message = format!("Failed to start Codex: {}", e);
                tracing::error!(
                    "[WORKER] Failed to start Codex turn for {}: {}",
                    self.conversation_id,
                    e
                );
                if let Err(finish_err) =
                    agent_runners::finish_turn(&self.db, &runner_turn_id, "failed").await
                {
                    tracing::warn!(
                        "[WORKER] Failed to mark runner turn failed after Codex spawn error for {}: {}",
                        self.conversation_id,
                        finish_err
                    );
                }
                self.mark_checkpoint_interrupted().await;
                persist_failed_codex_message(
                    self,
                    &assistant_message_id,
                    &mut accumulated_text,
                    &mut content_blocks,
                    failure_message,
                )
                .await;
                return;
            }
        };
        let codex_spawn_ready_ms = Utc::now().timestamp_millis();
        tracing::info!(
            "[CHAT_LATENCY] phase=codex_spawn_ready conv={} client_id={} runner_turn_id={} ready_at_ms={} spawn_duration_ms={}",
            self.conversation_id,
            self.current_client_id.as_deref().unwrap_or("none"),
            runner_turn_id,
            codex_spawn_ready_ms,
            codex_spawn_ready_ms.saturating_sub(codex_spawn_start_ms)
        );

        self.manager
            .insert_app_server_turn(self.conversation_id.clone(), turn.child())
            .await;

        let mut accumulated_text = String::new();
        let mut thread_id: Option<String> = None;
        let mut last_flush = Instant::now();
        let flush_interval = Duration::from_millis(DB_FLUSH_INTERVAL_MS);
        let mut tool_call_count: i32 = 0;
        let mut content_blocks: Vec<ContentBlockDesc> = Vec::new();
        let mut blocks_dirty = false;
        let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
        let mut usage: Option<TokenUsageBreakdown> = None;
        let mut kill_requested = false;
        let mut streamed_agent_message_items: HashSet<String> = HashSet::new();
        heartbeat.tick().await;

        // If Stop lands while Codex is still launching, the cancel endpoint can
        // only record the marker; there may be no registered child yet. Re-check
        // immediately after registration so we do not wait for the first event
        // or the 15s heartbeat before killing the subprocess.
        if self.is_turn_cancelled().await {
            kill_requested = true;
            if let Err(e) = turn.terminate().await {
                tracing::warn!(
                    "[WORKER] Failed to terminate newly-started cancelled Codex turn for {}: {}",
                    self.conversation_id,
                    e
                );
            }
        }

        loop {
            tokio::select! {
                maybe_event = turn.events.recv() => {
                    match maybe_event {
                        Some(CodexAppServerEvent::ThreadStarted { thread_id: tid }) => {
                            tracing::info!(
                                "[CHAT_LATENCY] phase=codex_thread_started conv={} client_id={} runner_turn_id={} thread_id={} started_at_ms={}",
                                self.conversation_id,
                                self.current_client_id.as_deref().unwrap_or("none"),
                                runner_turn_id,
                                tid,
                                Utc::now().timestamp_millis()
                            );
                            if let Err(e) = agent_runners::set_turn_session(
                                &self.db,
                                &runner_turn_id,
                                &tid,
                            )
                            .await
                            {
                                tracing::warn!(
                                    "[WORKER] Failed to persist runner turn session for {}: {}",
                                    self.conversation_id,
                                    e
                                );
                            }
                            thread_id = Some(tid);
                        }
                        Some(CodexAppServerEvent::AgentMessageDelta { id, text }) => {
                            if text.is_empty() {
                                continue;
                            }
                            streamed_agent_message_items.insert(id);

                            accumulated_text.push_str(&text);

                            match content_blocks.last_mut() {
                                Some(ContentBlockDesc::Text { text: existing }) => {
                                    existing.push_str(&text);
                                }
                                _ => {
                                    content_blocks.push(ContentBlockDesc::Text {
                                        text: text.clone(),
                                    });
                                    blocks_dirty = true;
                                }
                            }

                            self.emit_event(&StreamEvent::Text {
                                content: text,
                            })
                            .await;
                        }
                        Some(CodexAppServerEvent::AgentMessageCompleted { id, text }) => {
                            if text.is_empty() || streamed_agent_message_items.contains(&id) {
                                continue;
                            }

                            let text_chunk = if accumulated_text.is_empty() {
                                text
                            } else {
                                format!("\n\n{}", text)
                            };
                            accumulated_text.push_str(&text_chunk);

                            match content_blocks.last_mut() {
                                Some(ContentBlockDesc::Text { text }) => {
                                    text.push_str(&text_chunk);
                                }
                                _ => {
                                    content_blocks.push(ContentBlockDesc::Text {
                                        text: text_chunk.clone(),
                                    });
                                    blocks_dirty = true;
                                }
                            }

                            self.emit_event(&StreamEvent::Text {
                                content: text_chunk,
                            })
                            .await;
                        }
                        Some(CodexAppServerEvent::ReasoningDelta { text }) => {
                            if text.trim().is_empty() {
                                continue;
                            }
                            match content_blocks.last_mut() {
                                Some(ContentBlockDesc::Thinking { text: existing }) => {
                                    existing.push_str(&text);
                                }
                                _ => {
                                    content_blocks.push(ContentBlockDesc::Thinking {
                                        text: text.clone(),
                                    });
                                    blocks_dirty = true;
                                }
                            }
                            self.emit_event(&StreamEvent::Thinking { content: text }).await;
                        }
                        Some(CodexAppServerEvent::ToolCallStarted { id, name, input }) => {
                            tool_call_count += 1;
                            let scoped_tool_id = scoped_tool_call_id(&assistant_message_id, &id);
                            tracing::info!(
                                "[WORKER] Codex tool use #{}: {} ({})",
                                tool_call_count,
                                name,
                                scoped_tool_id
                            );

                            blocks_dirty |=
                                append_tool_group_id(&mut content_blocks, scoped_tool_id.clone());

                            let now = chrono::Utc::now().timestamp();
                            if let Err(e) = conversations::insert_tool_call(
                                &self.db,
                                &scoped_tool_id,
                                &assistant_message_id,
                                &self.conversation_id,
                                &name,
                                Some(&input),
                                now,
                            )
                            .await
                            {
                                tracing::error!("[WORKER] Failed to insert tool call: {}", e);
                            }

                            let checkpoint_session = thread_id.as_deref().unwrap_or("running");
                            if let Err(e) = checkpoints::upsert_checkpoint(
                                &self.db,
                                &self.conversation_id,
                                checkpoint_session,
                                tool_call_count,
                            )
                            .await
                            {
                                tracing::warn!(
                                    "[WORKER] Failed to update tool-call checkpoint count: {}",
                                    e
                                );
                            } else {
                                self.publish_run_status().await;
                            }

                            self.emit_event(&StreamEvent::ToolUse {
                                id: scoped_tool_id,
                                name,
                                input,
                            })
                                .await;
                        }
                        Some(CodexAppServerEvent::ToolCallCompleted {
                            id,
                            content,
                            is_error,
                        }) => {
                            let scoped_tool_id = scoped_tool_call_id(&assistant_message_id, &id);
                            if let Err(e) = conversations::update_tool_call_result(
                                &self.db,
                                &scoped_tool_id,
                                &content,
                                is_error,
                            )
                            .await {
                                tracing::error!("[WORKER] Failed to update tool call result: {}", e);
                            }

                            self.emit_event(&StreamEvent::ToolResult {
                                tool_use_id: scoped_tool_id,
                                content,
                                is_error,
                            })
                            .await;
                        }
                        Some(CodexAppServerEvent::TurnCompleted { usage: event_usage }) => {
                            usage = Some(event_usage);
                        }
                        None => break,
                    }

                    if last_flush.elapsed() >= flush_interval {
                        flush_to_db(&self.db, &assistant_message_id, &accumulated_text).await;
                        last_flush = Instant::now();
                    }

                    if blocks_dirty {
                        blocks_dirty = false;
                        if !content_blocks.is_empty() {
                            let _ = conversations::update_message_blocks(
                                &self.db,
                                &assistant_message_id,
                                &content_blocks,
                            )
                            .await;
                        }
                    }

                    if !kill_requested && self.is_turn_cancelled().await {
                        kill_requested = true;
                        if let Err(e) = turn.terminate().await {
                            tracing::warn!(
                                "[WORKER] Failed to terminate cancelled Codex turn for {}: {}",
                                self.conversation_id,
                                e
                            );
                        }
                    }
                }
                _ = heartbeat.tick() => {
                    self.emit_event(&StreamEvent::Status {
                        status: "heartbeat".to_string(),
                        message: None,
                    }).await;
                    let _ = checkpoints::touch_checkpoint(&self.db, &self.conversation_id).await;
                    let _ = agent_runners::touch_turn(&self.db, &runner_turn_id).await;

                    if !kill_requested && self.is_turn_cancelled().await {
                        kill_requested = true;
                        if let Err(e) = turn.terminate().await {
                            tracing::warn!(
                                "[WORKER] Failed to terminate cancelled Codex turn for {}: {}",
                                self.conversation_id,
                                e
                            );
                        }
                    }
                }
            }
        }

        self.manager
            .remove_app_server_turn(&self.conversation_id)
            .await;
        let outcome = match turn.wait().await {
            Ok(outcome) => outcome,
            Err(e) => {
                let failure_message = format!("Codex turn failed: {}", e);
                tracing::error!(
                    "[WORKER] Failed waiting for Codex turn for {}: {}",
                    self.conversation_id,
                    e
                );
                if let Err(finish_err) =
                    agent_runners::finish_turn(&self.db, &runner_turn_id, "failed").await
                {
                    tracing::warn!(
                        "[WORKER] Failed to mark runner turn failed for {}: {}",
                        self.conversation_id,
                        finish_err
                    );
                }
                self.mark_checkpoint_interrupted().await;
                persist_failed_codex_message(
                    self,
                    &assistant_message_id,
                    &mut accumulated_text,
                    &mut content_blocks,
                    failure_message,
                )
                .await;
                return;
            }
        };

        if let Some(usage) = usage {
            if usage.has_usage() {
                let db_ref = self.db.clone();
                let conv_id = self.conversation_id.clone();
                let uid = msg.user_id.clone();
                tokio::spawn(async move {
                    if let Err(e) = token_usage::insert_token_usage(
                        &db_ref,
                        "conversation",
                        &conv_id,
                        Some(&uid),
                        None,
                        usage,
                    )
                    .await
                    {
                        tracing::warn!("[WORKER] Failed to record token usage: {}", e);
                    }
                });
            }
        }

        enum CodexTurnCompletion {
            Completed { session_id: String },
            Cancelled { session_id: String },
            Failed(String),
        }

        let cancelled = self.consume_cancelled_turn().await;
        let completion = if cancelled {
            CodexTurnCompletion::Cancelled {
                session_id: thread_id
                    .clone()
                    .unwrap_or_else(|| self.conversation_id.clone()),
            }
        } else if !outcome.success() {
            CodexTurnCompletion::Failed(outcome.failure_summary("Codex app-server"))
        } else if let Some(session_id) = thread_id.clone() {
            CodexTurnCompletion::Completed { session_id }
        } else {
            CodexTurnCompletion::Failed(
                "Codex app-server completed without returning a thread id".to_string(),
            )
        };

        let runner_terminal_status = match &completion {
            CodexTurnCompletion::Completed { .. } => "completed",
            CodexTurnCompletion::Cancelled { .. } => "cancelled",
            CodexTurnCompletion::Failed(_) => "failed",
        };
        if let Err(e) =
            agent_runners::finish_turn(&self.db, &runner_turn_id, runner_terminal_status).await
        {
            tracing::warn!(
                "[WORKER] Failed to mark runner turn {} for {}: {}",
                runner_terminal_status,
                self.conversation_id,
                e
            );
        }

        flush_to_db(&self.db, &assistant_message_id, &accumulated_text).await;

        if !content_blocks.is_empty() {
            if let Err(e) = conversations::update_message_blocks(
                &self.db,
                &assistant_message_id,
                &content_blocks,
            )
            .await
            {
                tracing::error!("[WORKER] Failed to store content blocks: {}", e);
            }
        }

        if let Some(root) = generated_images_root.as_deref() {
            let generated_paths = new_generated_attachments(root, &generated_attachments_before);
            if !generated_paths.is_empty() {
                match persist_generated_visual_attachments(
                    &self.db,
                    &self.conversation_id,
                    &assistant_message_id,
                    &generated_paths,
                )
                .await
                {
                    Ok(saved) => tracing::info!(
                        "[WORKER] Attached {} generated visual file(s) to assistant message {}",
                        saved,
                        assistant_message_id
                    ),
                    Err(e) => tracing::error!(
                        "[WORKER] Failed to attach generated visual file(s) to assistant message {}: {}",
                        assistant_message_id,
                        e
                    ),
                }
            }
        }

        match completion {
            CodexTurnCompletion::Completed { session_id } => {
                self.emit_event(&StreamEvent::Result {
                    session_id: session_id.clone(),
                    status: "completed".to_string(),
                    is_error: false,
                })
                .await;

                let _ = conversations::update_conversation(
                    &self.db,
                    &msg.user_id,
                    &self.conversation_id,
                    UpdateConversationRequest {
                        title: None,
                        session_id: Some(session_id.clone()),
                        organization: None,
                        conversation_type: None,
                    },
                )
                .await;

                if let Err(e) = checkpoints::upsert_checkpoint(
                    &self.db,
                    &self.conversation_id,
                    &session_id,
                    tool_call_count,
                )
                .await
                {
                    tracing::warn!("[WORKER] Failed to update checkpoint: {}", e);
                }
                if let Err(e) = checkpoints::mark_completed(&self.db, &self.conversation_id).await {
                    tracing::warn!("[WORKER] Failed to mark checkpoint completed: {}", e);
                }
                self.publish_run_status().await;

                super::conversation_next_actions::spawn_generation(
                    self.db.clone(),
                    msg.user_id.clone(),
                    self.conversation_id.clone(),
                    assistant_message_id.clone(),
                    msg.config.prompt_name.to_string(),
                    msg.message.clone(),
                    accumulated_text.clone(),
                );

                if let Some(apns) = crate::apns::ApnsService::global() {
                    tracing::info!(
                        "[WORKER] Sending push notification for user={}, conv={}",
                        msg.user_id,
                        self.conversation_id
                    );
                    let push_title = match conversations::get_conversation(
                        &self.db,
                        &self.conversation_id,
                        false,
                    )
                    .await
                    {
                        Ok(Some(conversation)) => {
                            let title = conversation.title.trim();
                            if title.is_empty() {
                                tracing::warn!(
                                    "[WORKER] Skipping completion push for conv={} - empty conversation title",
                                    self.conversation_id
                                );
                                None
                            } else {
                                Some(title.to_string())
                            }
                        }
                        Ok(None) => {
                            tracing::warn!(
                                "[WORKER] Skipping completion push for conv={} - conversation not found",
                                self.conversation_id
                            );
                            None
                        }
                        Err(e) => {
                            tracing::warn!(
                                "[WORKER] Skipping completion push for conv={} - title lookup failed: {}",
                                self.conversation_id,
                                e
                            );
                            None
                        }
                    };

                    if let Some(push_title) = push_title {
                        let push_db = (*self.db).clone();
                        let push_user = msg.user_id.clone();
                        let push_agent = msg.config.prompt_name.to_string();
                        let push_conv_id = self.conversation_id.clone();
                        let apns = apns.clone();
                        tokio::spawn(async move {
                            match apns
                                .send_to_user(
                                    &push_db,
                                    &push_user,
                                    &push_title,
                                    "",
                                    Some(&push_conv_id),
                                    Some(&push_agent),
                                )
                                .await
                            {
                                Ok(()) => {
                                    tracing::info!(
                                        "[WORKER] Push notification sent for user={}",
                                        push_user
                                    )
                                }
                                Err(e) => tracing::warn!(
                                    "[WORKER] Push notification failed for user={}: {}",
                                    push_user,
                                    e
                                ),
                            }
                        });
                    }
                } else {
                    tracing::warn!("[WORKER] APNs not initialized — skipping push notification");
                }

                self.emit_event(&StreamEvent::Status {
                    status: "completed".to_string(),
                    message: None,
                })
                .await;
            }
            CodexTurnCompletion::Cancelled { session_id } => {
                self.mark_checkpoint_interrupted().await;
                self.emit_event(&StreamEvent::Result {
                    session_id,
                    status: "cancelled".to_string(),
                    is_error: false,
                })
                .await;
            }
            CodexTurnCompletion::Failed(message) => {
                tracing::error!(
                    "[WORKER] Codex turn failed for {}: {}",
                    self.conversation_id,
                    message
                );
                self.mark_checkpoint_interrupted().await;
                persist_failed_codex_message(
                    self,
                    &assistant_message_id,
                    &mut accumulated_text,
                    &mut content_blocks,
                    message,
                )
                .await;
            }
        }
    }

    /// Emit a StreamEvent: encode to Anthropic events, persist, and broadcast.
    ///
    /// Uses the server-side allocator (`insert_conversation_event`) so the
    /// event_index comes from the DB under a BEGIN IMMEDIATE transaction —
    /// safe under concurrent writers for the same conversation_id.
    ///
    /// The encoder maps each internal `StreamEvent` to zero or more
    /// Anthropic frames from the 8-event vocabulary (`message_start` /
    /// `content_block_start` / `content_block_delta` / `content_block_stop`
    /// / `message_delta` / `message_stop` / `ping` / `error`).
    /// Router/metadata events produce zero frames; a text chunk produces
    /// one; a tool_use turn produces several. Every produced frame is
    /// persisted AND broadcast to live SSE subscribers.
    async fn emit_event(&mut self, event: &StreamEvent) {
        // Encode the StreamEvent into a sequence of Anthropic events.
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
        let anthropic_events = self.encoder.encode(event);
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
                    record_gap_detected(&self.conversation_id, self.event_index, allocated_index);
                    let bytes = ae_json.len();
                    let broadcast_tx = get_broadcast_sender(&self.conversation_id).await;
                    let _ = broadcast_tx.send((allocated_index, ae_json));
                    self.event_index = allocated_index + 1;
                    record_stream_event_emitted(&self.conversation_id, bytes);
                    let persisted_at_ms = Utc::now().timestamp_millis();
                    if ae_type == "message_start" && !self.first_message_start_logged {
                        self.first_message_start_logged = true;
                        tracing::info!(
                            "[CHAT_LATENCY] phase=message_start_persisted conv={} client_id={} event_index={} bytes={} persisted_at_ms={}",
                            self.conversation_id,
                            self.current_client_id.as_deref().unwrap_or("none"),
                            allocated_index,
                            bytes,
                            persisted_at_ms
                        );
                    }
                    if ae_type == "content_block_delta" && !self.first_content_delta_logged {
                        self.first_content_delta_logged = true;
                        tracing::info!(
                            "[CHAT_LATENCY] phase=first_assistant_delta_persisted conv={} client_id={} event_index={} bytes={} persisted_at_ms={}",
                            self.conversation_id,
                            self.current_client_id.as_deref().unwrap_or("none"),
                            allocated_index,
                            bytes,
                            persisted_at_ms
                        );
                    }
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
                        .encoder
                        .current_message_id()
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| {
                            // Encoder should always have a message_id
                            // cached by the time message_stop ships, but
                            // if encoder state was reset mid-turn we
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
}

/// Flush accumulated text content to the database.
async fn flush_to_db(db: &SqlitePool, assistant_message_id: &str, accumulated_text: &str) {
    if let Err(e) = conversations::update_message(db, assistant_message_id, accumulated_text).await
    {
        tracing::error!("[WORKER] Failed to flush message to DB: {}", e);
    }
}

fn chat_attachments_dir(conversation_id: &str) -> Result<PathBuf> {
    let home = dirs::home_dir().context("Failed to resolve home directory for chat attachments")?;
    Ok(home
        .join(".agentic-flowstate")
        .join("chat-attachments")
        .join(conversation_id))
}

fn sanitize_display_filename(filename: &str) -> Option<String> {
    let name = Path::new(filename)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(filename)
        .trim();
    if name.is_empty() {
        return None;
    }
    let sanitized: String = name
        .chars()
        .filter(|ch| !ch.is_control() && *ch != '/' && *ch != '\\' && *ch != ':')
        .take(180)
        .collect::<String>()
        .trim()
        .to_string();
    if sanitized.is_empty() {
        None
    } else {
        Some(sanitized)
    }
}

fn generated_attachment_extension_for_mime(mime_type: &str) -> &'static str {
    match mime_type {
        "application/pdf" => "pdf",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/heic" => "heic",
        "image/jpeg" => "jpg",
        _ => "jpg",
    }
}

fn mime_type_for_generated_attachment_path(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "pdf" => Some("application/pdf"),
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "heic" => Some("image/heic"),
        _ => None,
    }
}

fn generated_attachment_snapshot(root: &Path) -> HashSet<PathBuf> {
    let mut files = HashSet::new();
    collect_generated_attachments(root, &mut files);
    files
}

fn new_generated_attachments(root: &Path, before: &HashSet<PathBuf>) -> Vec<PathBuf> {
    let mut attachments: Vec<PathBuf> = generated_attachment_snapshot(root)
        .into_iter()
        .filter(|path| !before.contains(path))
        .collect();
    attachments.sort();
    attachments
}

fn collect_generated_attachments(dir: &Path, files: &mut HashSet<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_generated_attachments(&path, files);
        } else if file_type.is_file()
            && mime_type_for_generated_attachment_path(&path).is_some()
        {
            files.insert(path);
        }
    }
}

async fn load_message_attachments(
    db: &SqlitePool,
    message_id: &str,
) -> Result<Vec<AttachmentMeta>> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT attachments FROM conversation_messages WHERE id = ?")
            .bind(message_id)
            .fetch_optional(db)
            .await
            .context("Failed to load message attachments")?;

    let Some((Some(raw),)) = row else {
        return Ok(Vec::new());
    };
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }

    serde_json::from_str(&raw).context("Failed to decode message attachments JSON")
}

async fn update_message_attachments(
    db: &SqlitePool,
    message_id: &str,
    attachments: &[AttachmentMeta],
) -> Result<()> {
    let json =
        serde_json::to_string(attachments).context("Failed to encode attachment metadata")?;
    sqlx::query("UPDATE conversation_messages SET attachments = ? WHERE id = ?")
        .bind(json)
        .bind(message_id)
        .execute(db)
        .await
        .context("Failed to update message attachments")?;
    Ok(())
}

async fn persist_generated_visual_attachments(
    db: &SqlitePool,
    conversation_id: &str,
    assistant_message_id: &str,
    generated_paths: &[PathBuf],
) -> Result<usize> {
    if generated_paths.is_empty() {
        return Ok(0);
    }

    let chat_dir = chat_attachments_dir(conversation_id)?;
    tokio::fs::create_dir_all(&chat_dir)
        .await
        .with_context(|| format!("Failed to create chat attachment dir {}", chat_dir.display()))?;

    let mut attachments = load_message_attachments(db, assistant_message_id).await?;
    let mut saved = 0usize;
    for source in generated_paths {
        let Some(mime_type) = mime_type_for_generated_attachment_path(source) else {
            continue;
        };
        let filename = format!(
            "generated-{}.{}",
            uuid::Uuid::new_v4(),
            generated_attachment_extension_for_mime(mime_type)
        );
        let destination = chat_dir.join(&filename);
        tokio::fs::copy(source, &destination)
            .await
            .with_context(|| {
                format!(
                    "Failed to copy generated attachment {} to {}",
                    source.display(),
                    destination.display()
                )
            })?;

        attachments.push(AttachmentMeta {
            filename,
            display_name: None,
            path: destination.to_string_lossy().to_string(),
            mime_type: mime_type.to_string(),
            size_bytes: None,
        });
        saved += 1;
    }

    if saved > 0 {
        update_message_attachments(db, assistant_message_id, &attachments).await?;
    }
    Ok(saved)
}

fn codex_failure_text_chunk(accumulated_text: &str, message: &str) -> String {
    let failure_notice = format!("Codex error: {message}");
    if accumulated_text.is_empty() {
        failure_notice
    } else {
        format!("\n\n{failure_notice}")
    }
}

/// Scope per-turn tool ids by assistant message id before they hit durable
/// storage or the SSE/UI layer. Codex emits short ids like `item_2`, which
/// are only unique inside one turn.
fn scoped_tool_call_id(assistant_message_id: &str, raw_tool_id: &str) -> String {
    format!("{assistant_message_id}::{raw_tool_id}")
}

/// Append a tool id to the trailing tool-group block, or start a new tool
/// group when the previous block was text/thinking.
///
/// Returns `true` because any appended tool id changes the durable
/// interleaving snapshot, including the "same group, one more tool" case.
fn append_tool_group_id(content_blocks: &mut Vec<ContentBlockDesc>, tool_id: String) -> bool {
    match content_blocks.last_mut() {
        Some(ContentBlockDesc::ToolGroup { tool_ids }) => {
            tool_ids.push(tool_id);
        }
        _ => {
            content_blocks.push(ContentBlockDesc::ToolGroup {
                tool_ids: vec![tool_id],
            });
        }
    }
    true
}

fn append_text_block(content_blocks: &mut Vec<ContentBlockDesc>, text_chunk: &str) {
    match content_blocks.last_mut() {
        Some(ContentBlockDesc::Text { text }) => {
            text.push_str(text_chunk);
        }
        _ => {
            content_blocks.push(ContentBlockDesc::Text {
                text: text_chunk.to_string(),
            });
        }
    }
}

async fn persist_failed_codex_message(
    worker: &mut ConversationWorker,
    assistant_message_id: &str,
    accumulated_text: &mut String,
    content_blocks: &mut Vec<ContentBlockDesc>,
    message: String,
) {
    let text_chunk = codex_failure_text_chunk(accumulated_text, &message);
    accumulated_text.push_str(&text_chunk);
    append_text_block(content_blocks, &text_chunk);

    worker
        .emit_event(&StreamEvent::Text {
            content: text_chunk.clone(),
        })
        .await;
    flush_to_db(&worker.db, assistant_message_id, accumulated_text).await;

    if let Err(e) =
        conversations::update_message_blocks(&worker.db, assistant_message_id, content_blocks).await
    {
        tracing::error!("[WORKER] Failed to store failure content blocks: {}", e);
    }

    worker
        .emit_event(&StreamEvent::Status {
            status: "failed".to_string(),
            message: Some(message),
        })
        .await;
}

async fn build_codex_system_prompt(
    db: &SqlitePool,
    conversation_id: &str,
    config: &ChatConfig,
) -> Result<String> {
    let prompt_vars: HashMap<String, String> = config.prompt_vars.clone();
    let mut system_prompt = load_prompt(config.prompt_name, prompt_vars)?;
    let messages = conversations::list_messages(db, conversation_id, None, None).await?;

    if !messages.is_empty() {
        system_prompt.push_str("\n\n");
        system_prompt.push_str(&build_codex_conversation_history(&messages));
    }

    Ok(system_prompt)
}

fn build_codex_conversation_history(messages: &[ConversationMessage]) -> String {
    let mut history = String::from(
        "## Conversation History\n\n\
         Use the prior conversation context below to continue seamlessly. \
         Do not ask the user to repeat information that is already present in the history.\n\n",
    );
    append_conversation_history(&mut history, messages);
    history
}

fn append_conversation_history(history: &mut String, messages: &[ConversationMessage]) {
    let recent = if messages.len() > PROMPT_HISTORY_MESSAGE_LIMIT {
        &messages[messages.len() - PROMPT_HISTORY_MESSAGE_LIMIT..]
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
}

// =============================================================================
// Streaming persistence invariants
// =============================================================================
//
// These tests exercise the Anthropic 8-event persistence pipeline. They
// confirm:
//
//   1. Encoder → `insert_conversation_event` → `get_events` round-trips
//      produce ONLY Anthropic vocabulary event_types — no the previous SDK
//      discriminants (`text`, `tool_use`, `thinking`) ever land in the row.
//   2. A text-only turn, a tool-use turn, and a thinking turn each produce
//      the canonical Anthropic frame ordering when replayed off the DB.
//   3. Every persisted `event_data` payload parses as valid Anthropic JSON
//      whose `type` field matches the row's `event_type`.
//
// If a non-Anthropic discriminant ever regressed back into the emit
// pipeline these assertions would detect it — the replayed event_types
// would include a non-Anthropic tag, or the row counts would diverge
// from encoder output.
#[cfg(test)]
mod streaming_persistence_tests {
    use super::*;
    use crate::agents::anthropic_events::ALL_EVENT_TYPES;
    use serde_json::json;
    use sqlx::sqlite::SqlitePoolOptions;
    use sqlx::ConnectOptions;
    use std::fs;
    use std::str::FromStr;

    /// Allow-list of event_type strings any persisted row may carry.
    /// These are the canonical 8 Anthropic streaming event type tags, as
    /// exported by [`ALL_EVENT_TYPES`]. If a the previous SDK discriminant ever
    /// sneaks through the persistence path, this allow-list is what
    /// trips first.
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
                conversation_type TEXT,
                title TEXT,
                started_at TEXT,
                updated_at TEXT,
                status TEXT NOT NULL DEFAULT 'open',
                archived_at TEXT,
                router_ticket_id TEXT,
                router_organization TEXT,
                last_event_index INTEGER NOT NULL DEFAULT -1
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

    /// Drive a sequence of the previous SDK StreamEvents through the encoder, persist
    /// every emitted Anthropic event via `insert_conversation_event`, then
    /// replay from the DB and return (event_types, parsed_json_values).
    async fn run_turn_and_replay(
        pool: &SqlitePool,
        conversation_id: &str,
        events: &[StreamEvent],
    ) -> (Vec<String>, Vec<serde_json::Value>) {
        let mut encoder = AnthropicEventEncoder::new(conversation_id);
        encoder.set_pending_message_id("assistant-test-message");
        for ev in events {
            for ae in encoder.encode(ev) {
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
        assert_eq!(values[0]["message"]["id"], "assistant-test-message");

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
                "content_block_stop",  // text 0 closes before tool_use
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

    /// Silencer: confirm that the previous SDK router/replay tags are NOT
    /// emitted onto the wire. If they ever regressed into the pipeline,
    /// this test would see extra rows with event_type outside the allow-list.
    #[tokio::test]
    async fn router_and_replay_tags_are_never_persisted() {
        let pool = fresh_test_pool().await;
        let (types, _values) = run_turn_and_replay(
            &pool,
            "c-t49352bf5",
            &[
                StreamEvent::RouterResult {
                    enriched_message: "internal routing note".into(),
                    ticket_id: None,
                    organization: None,
                    skipped: true,
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

    #[test]
    fn scoped_tool_call_id_is_unique_per_assistant_message() {
        let first = scoped_tool_call_id("msg-1", "item_2");
        let second = scoped_tool_call_id("msg-2", "item_2");

        assert_eq!(first, "msg-1::item_2");
        assert_eq!(second, "msg-2::item_2");
        assert_ne!(first, second);
    }

    #[test]
    fn generated_attachment_snapshot_detects_new_previewable_files_only() {
        let root = std::env::temp_dir().join(format!(
            "agentic-generated-attachments-test-{}",
            uuid::Uuid::new_v4()
        ));
        let batch = root.join("batch");
        fs::create_dir_all(&batch).expect("create generated attachments dir");
        let old_image = batch.join("old.png");
        let ignored_text = batch.join("notes.txt");
        fs::write(&old_image, b"old").expect("write old image");
        fs::write(&ignored_text, b"notes").expect("write text file");

        let before = generated_attachment_snapshot(&root);
        let new_image = batch.join("new.jpg");
        let new_pdf = batch.join("diagram.pdf");
        let nested_dir = batch.join("nested");
        fs::create_dir_all(&nested_dir).expect("create nested generated dir");
        let nested_image = nested_dir.join("new.webp");
        fs::write(&new_image, b"new").expect("write new image");
        fs::write(&new_pdf, b"%PDF").expect("write new pdf");
        fs::write(&nested_image, b"webp").expect("write nested image");

        let new_attachments = new_generated_attachments(&root, &before);
        let mut expected = vec![new_image, new_pdf, nested_image];
        expected.sort();

        assert_eq!(new_attachments, expected);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn append_tool_group_id_updates_existing_group() {
        let mut blocks = vec![ContentBlockDesc::ToolGroup {
            tool_ids: vec!["msg-1::item_1".to_string()],
        }];

        let changed = append_tool_group_id(&mut blocks, "msg-1::item_2".to_string());

        assert!(changed);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            ContentBlockDesc::ToolGroup { tool_ids } => {
                assert_eq!(
                    tool_ids,
                    &vec!["msg-1::item_1".to_string(), "msg-1::item_2".to_string()]
                );
            }
            other => panic!("expected tool_group, got {:?}", other),
        }
    }

    #[test]
    fn codex_failure_text_chunk_is_full_message_when_empty() {
        assert_eq!(codex_failure_text_chunk("", "boom"), "Codex error: boom");
    }

    #[test]
    fn codex_failure_text_chunk_appends_after_existing_text() {
        assert_eq!(
            codex_failure_text_chunk("Partial answer", "boom"),
            "\n\nCodex error: boom"
        );
    }

    #[test]
    fn append_text_block_updates_existing_text_block() {
        let mut blocks = vec![ContentBlockDesc::Text {
            text: "Hello".to_string(),
        }];

        append_text_block(&mut blocks, "\n\nCodex error: boom");

        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            ContentBlockDesc::Text { text } => {
                assert_eq!(text, "Hello\n\nCodex error: boom");
            }
            other => panic!("expected text block, got {:?}", other),
        }
    }

    #[test]
    fn append_text_block_starts_new_text_block_after_tool_group() {
        let mut blocks = vec![ContentBlockDesc::ToolGroup {
            tool_ids: vec!["msg-1::item_1".to_string()],
        }];

        append_text_block(&mut blocks, "Codex error: boom");

        assert_eq!(blocks.len(), 2);
        match &blocks[1] {
            ContentBlockDesc::Text { text } => {
                assert_eq!(text, "Codex error: boom");
            }
            other => panic!("expected trailing text block, got {:?}", other),
        }
    }
}
