use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use chrono::Utc;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
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
use crate::observability::cancellation;
use crate::observability::next_actions::{record_clear, NextActionClearReason};
use crate::observability::runtime::{self, RuntimeFailurePhase, RuntimeLatencyPhase};
use crate::observability::streaming::{
    record_gap_detected, record_stream_event_emitted, record_ticket_preflight,
    record_ticket_preflight_error,
};
use ticketing_system::{
    agent_runners, checkpoints, conversation_turn_jobs, conversations,
    token_usage::{self, TokenUsageBreakdown},
    work_context::{
        collect_work_context, CollectWorkContextRequest, CollectWorkContextResponse,
        WorkContextHandoff,
    },
    AddMessageRequest, ContentBlockDesc, ConversationMessage, UpdateConversationRequest,
};

/// How often to flush accumulated content to the database (ms).
const DB_FLUSH_INTERVAL_MS: u64 = 500;

/// How long a worker idles before shutting down.
const IDLE_TIMEOUT_SECS: u64 = 600; // 10 minutes

/// Maximum number of prior messages to load into prompt context per turn.
const PROMPT_HISTORY_MESSAGE_LIMIT: usize = 30;
const ARTIFACT_MEMORY_HANDOFF_VAR: &str = "ARTIFACT_MEMORY_HANDOFF";
const STARTUP_CONTEXT_MAX_RESULTS: usize = 8;
const STARTUP_CONTEXT_MAX_ITEMS: usize = 4;
const STARTUP_CONTEXT_TOKEN_BUDGET: usize = 3_000;
static TICKET_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bT-[0-9A-F]{8}\b").expect("valid ticket id regex"));

#[derive(Debug, Clone, Deserialize, Serialize)]
struct AttachmentMeta {
    filename: String,
    display_name: Option<String>,
    path: String,
    mime_type: String,
    size_bytes: Option<i64>,
}

struct WorkContextPreflight {
    final_message: String,
    artifact_memory_handoff: Option<String>,
    forwarded_metadata: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkContextSkipReason {
    SupportAgent,
    ExistingArtifactHandoff,
    AlreadyRouted,
    TinyConversational,
}

impl WorkContextSkipReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::SupportAgent => "support_agent",
            Self::ExistingArtifactHandoff => "existing_artifact_memory_handoff",
            Self::AlreadyRouted => "already_routed",
            Self::TinyConversational => "tiny_conversational",
        }
    }

    fn persist_router_skip(self) -> bool {
        matches!(self, Self::SupportAgent | Self::ExistingArtifactHandoff)
    }
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
    /// JSON provenance metadata to persist on the visible user message.
    /// Used for orchestrated child-agent kickoff prompts so clients can
    /// style them differently from user-authored text without text heuristics.
    pub message_metadata: Option<String>,
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
    current_turn_started_at_ms: Option<i64>,
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
            current_turn_started_at_ms: None,
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

        let cancelled = memory_cancelled || persistent_cancelled;
        if cancelled {
            cancellation::record_runner_cancel_consumed(
                &self.conversation_id,
                Utc::now().timestamp_millis(),
                memory_cancelled,
                persistent_cancelled,
            );
        }

        cancelled
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
        let turn_started_at_ms = Utc::now().timestamp_millis();
        self.current_turn_started_at_ms = Some(turn_started_at_ms);
        self.first_message_start_logged = false;
        self.first_content_delta_logged = false;
        // Cache the turn's owner so `emit_event` can fan out silent
        // pushes on `message_stop` without plumbing user_id through
        // every call site (T-90C7FAC4).
        self.current_user_id = Some(msg.user_id.clone());
        runtime::record_turn_started(
            &self.conversation_id,
            msg.client_id.as_deref(),
            msg.config.agent_type.as_str(),
            msg.config.runtime.as_job_runtime(),
            &msg.config.codex_options.model,
            &msg.config.codex_options.reasoning_effort,
            msg.message.chars().count(),
            msg.attachments.as_ref().map_or(0, Vec::len),
            turn_started_at_ms,
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
                        log_user_visible_runtime_failure(
                            &self.db,
                            &self.conversation_id,
                            RuntimeFailurePhase::AttachmentStorage,
                            "Failed to resolve attachment storage",
                            &e,
                        )
                        .await;
                        self.emit_event(&StreamEvent::Status {
                            status: "failed".to_string(),
                            message: Some("Failed to resolve attachment storage".to_string()),
                        })
                        .await;
                        return;
                    }
                };
                if let Err(e) = std::fs::create_dir_all(&chat_attachments_dir) {
                    log_user_visible_runtime_failure(
                        &self.db,
                        &self.conversation_id,
                        RuntimeFailurePhase::AttachmentStorage,
                        "Failed to create attachment storage",
                        &e,
                    )
                    .await;
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
                        log_user_visible_runtime_failure(
                            &self.db,
                            &self.conversation_id,
                            RuntimeFailurePhase::AttachmentStorage,
                            "Attachment filename is invalid",
                            "sanitized filename was empty",
                        )
                        .await;
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
                            log_user_visible_runtime_failure(
                                &self.db,
                                &self.conversation_id,
                                RuntimeFailurePhase::AttachmentStorage,
                                "Failed to decode attachment",
                                &e,
                            )
                            .await;
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
                        log_user_visible_runtime_failure(
                            &self.db,
                            &self.conversation_id,
                            RuntimeFailurePhase::AttachmentStorage,
                            "Failed to save attachment",
                            &e,
                        )
                        .await;
                        self.emit_event(&StreamEvent::Status {
                            status: "failed".to_string(),
                            message: Some(format!("Failed to save attachment {}", display_name)),
                        })
                        .await;
                        return;
                    }
                    let path_str = file_path.to_string_lossy().to_string();
                    attachment_descriptions.push(format_attachment_prompt_line(
                        &display_name,
                        &attachment.mime_type,
                        Some(bytes.len() as i64),
                        &path_str,
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
                "[The user has attached {} file(s). Server-side copies are available at:\n{}\nIf an attachment is an image or screenshot, inspect it before answering any question that depends on visual content. Use available local attachment/image inspection tools or filesystem tools when relevant.]\n\n{}",
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
                metadata: msg.message_metadata.clone(),
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
            log_user_visible_runtime_failure(
                &self.db,
                &self.conversation_id,
                RuntimeFailurePhase::StoreUserMessage,
                "Failed to store user message",
                e,
            )
            .await;
        }

        // Create checkpoint
        if let Err(e) =
            checkpoints::upsert_checkpoint(&self.db, &self.conversation_id, "pending", 0).await
        {
            log_user_visible_runtime_failure(
                &self.db,
                &self.conversation_id,
                RuntimeFailurePhase::CreateCheckpoint,
                "Failed to create conversation checkpoint",
                &e,
            )
            .await;
        } else {
            self.publish_run_status().await;
        }

        if self
            .consume_cancelled_turn_before_agent_start("before_router")
            .await
        {
            return;
        }

        // === WORK-CONTEXT PREFLIGHT ===
        // Use deterministic ticket lookup plus artifact-memory retrieval before
        // agent start. It cannot invoke an LLM and does not create tickets from
        // chat startup, so first useful work starts with bounded prior context.
        let preflight = match self
            .run_work_context_preflight(&enhanced_message, &msg.config)
            .await
        {
            Ok(preflight) => preflight,
            Err(e) => {
                self.fail_startup_context_preflight(e).await;
                return;
            }
        };
        apply_artifact_memory_handoff_prompt_var(
            &mut msg.config.prompt_vars,
            preflight.artifact_memory_handoff.as_deref(),
        );
        let final_message = preflight.final_message;
        let forwarded_metadata = preflight.forwarded_metadata;

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
                    metadata: forwarded_metadata.clone(),
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
                metadata: None,
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
                log_user_visible_runtime_failure(
                    &self.db,
                    &self.conversation_id,
                    RuntimeFailurePhase::CreateAssistantMessage,
                    "Failed to create assistant message",
                    &e,
                )
                .await;
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

    /// Run deterministic ticket + artifact-memory preflight before the main
    /// agent starts. Chat startup still never creates tickets, epics, slices,
    /// or milestones; empty retrievals are persisted as context packets and
    /// injected as prompt handoff warnings.
    async fn run_work_context_preflight(
        &mut self,
        user_message: &str,
        config: &ChatConfig,
    ) -> Result<WorkContextPreflight> {
        let original = user_message.to_string();

        tracing::info!(
            target: "agentic_api::router",
            event = "work_context_preflight.enter",
            conversation_id = %self.conversation_id,
            self.has_routed,
            "starting deterministic work-context preflight"
        );

        if let Some(reason) = work_context_skip_reason(user_message, config, self.has_routed) {
            tracing::info!(
                target: "agentic_api::router",
                event = "work_context_preflight.skipped",
                conversation_id = %self.conversation_id,
                skip_reason = reason.as_str(),
                "skipping deterministic work-context preflight"
            );
            if reason.persist_router_skip() {
                self.has_routed = true;
                let _ = conversations::set_router_result(
                    &self.db,
                    &self.conversation_id,
                    Some("__skipped__"),
                    None,
                )
                .await;
            }
            return Ok(WorkContextPreflight {
                final_message: original,
                artifact_memory_handoff: None,
                forwarded_metadata: None,
            });
        }

        tracing::info!(
            target: "agentic_api::router",
            event = "work_context_preflight.running",
            conversation_id = %self.conversation_id,
            "running deterministic work-context preflight"
        );

        let started = Instant::now();
        let mut organization = self.conversation_organization().await?;
        if message_references_ticket_id(user_message) {
            if let Some(org) = organization.as_deref() {
                tracing::info!(
                    target: "agentic_api::router",
                    event = "work_context_preflight.explicit_ticket_id",
                    conversation_id = %self.conversation_id,
                    conversation_organization = org,
                    "using ticket organization lookup for explicit ticket id"
                );
            }
            organization = None;
        }
        let actor_id = format!("api-main-agent-startup:{}", self.conversation_id);
        let result = collect_work_context(
            &self.db,
            CollectWorkContextRequest {
                request: Some(user_message.to_string()),
                organization,
                conversation_id: Some(self.conversation_id.clone()),
                create_if_missing: false,
                mark_in_progress: false,
                actor_type: Some("agent".to_string()),
                actor_id: Some(actor_id.clone()),
                created_by: Some(actor_id),
                created_by_agent: Some(config.agent_type.as_str().to_string()),
                max_results: Some(STARTUP_CONTEXT_MAX_RESULTS),
                max_items: Some(STARTUP_CONTEXT_MAX_ITEMS),
                token_budget: Some(STARTUP_CONTEXT_TOKEN_BUDGET),
                ..Default::default()
            },
        )
        .await;

        self.has_routed = true;

        match result {
            Ok(response) => {
                record_ticket_preflight(
                    &response.ticket_result.status,
                    &response.ticket_result.action,
                    response.ticket_result.elapsed_ms,
                );
                tracing::info!(
                    target: "agentic_api::router",
                    event = "work_context_preflight.completed",
                    conversation_id = %self.conversation_id,
                    duration_ms = response.metrics.elapsed_ms,
                    ticket_status = response.ticket_result.status.as_str(),
                    ticket_action = response.ticket_result.action.as_str(),
                    candidate_count = response.ticket_result.candidate_count,
                    context_packet_id = response
                        .context_packet
                        .as_ref()
                        .map(|packet| packet.packet_id.as_str())
                        .unwrap_or(""),
                    selected_snippet_count = response.selected_snippets.len(),
                    warning_count = response.warnings.len(),
                    "deterministic work-context preflight completed"
                );

                let artifact_memory_handoff = Some(
                    serde_json::to_string(&response.prompt_ready_handoff)
                        .context("serialize collect_work_context prompt-ready handoff")?,
                );

                if let Some(ticket) = response.ticket_result.ticket.as_ref() {
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
                        tracing::warn!(
                            target: "agentic_api::router",
                            event = "work_context_preflight.persist_result_failed",
                            conversation_id = %self.conversation_id,
                            ticket_id = %ticket.ticket_id,
                            organization = %ticket.organization,
                            error = %e,
                            "failed to persist work-context preflight result"
                        );
                    }
                    self.emit_event(&StreamEvent::RouterResult {
                        enriched_message: enriched_message.clone(),
                        ticket_id: Some(ticket.ticket_id.clone()),
                        organization: Some(ticket.organization.clone()),
                        skipped: false,
                    })
                    .await;
                    Ok(WorkContextPreflight {
                        final_message: enriched_message,
                        artifact_memory_handoff,
                        forwarded_metadata: Some(startup_context_forwarded_metadata(&response)?),
                    })
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
                    Ok(WorkContextPreflight {
                        final_message: original,
                        artifact_memory_handoff,
                        forwarded_metadata: None,
                    })
                }
            }
            Err(e) => {
                let elapsed_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
                record_ticket_preflight_error("failed", elapsed_ms);
                tracing::warn!(
                    target: "agentic_api::router",
                    event = "work_context_preflight.failed",
                    conversation_id = %self.conversation_id,
                    duration_ms = elapsed_ms,
                    error = %e,
                    "deterministic work-context preflight failed"
                );
                let _ = conversations::set_router_result(
                    &self.db,
                    &self.conversation_id,
                    Some("__failed__"),
                    None,
                )
                .await;
                Err(e).context("collect startup work context")
            }
        }
    }

    async fn conversation_organization(&self) -> Result<Option<String>> {
        let conversation = conversations::get_conversation(&self.db, &self.conversation_id, false)
            .await
            .with_context(|| format!("load conversation {}", self.conversation_id))?
            .with_context(|| format!("conversation {} not found", self.conversation_id))?;
        Ok(trimmed_non_empty(&conversation.organization))
    }

    async fn fail_startup_context_preflight(&mut self, error: anyhow::Error) {
        let failure_message = format!("Startup context preflight failed: {error}");
        log_user_visible_runtime_failure(
            &self.db,
            &self.conversation_id,
            RuntimeFailurePhase::StartupContextPreflight,
            "Startup context preflight failed",
            &error,
        )
        .await;
        self.mark_checkpoint_interrupted().await;
        if let Err(e) = conversations::add_message(
            &self.db,
            &self.conversation_id,
            AddMessageRequest {
                role: "assistant".to_string(),
                content: failure_message.clone(),
                attachments: None,
                metadata: None,
            },
        )
        .await
        {
            tracing::warn!(
                "[WORKER] Failed to save startup preflight failure message: {}",
                e
            );
        }
        self.emit_event(&StreamEvent::Status {
            status: "failed".to_string(),
            message: Some(failure_message),
        })
        .await;
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
                    log_user_visible_runtime_failure(
                        &self.db,
                        &self.conversation_id,
                        RuntimeFailurePhase::BuildCodexPrompt,
                        "Failed to build Codex prompt",
                        &e,
                    )
                    .await;
                    self.mark_checkpoint_interrupted().await;
                    persist_failed_codex_message(
                        self,
                        &assistant_message_id,
                        &mut accumulated_text,
                        &mut content_blocks,
                        failure_message,
                        msg.message_metadata.as_deref(),
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
                log_user_visible_runtime_failure(
                    &self.db,
                    &self.conversation_id,
                    RuntimeFailurePhase::ClaimRunnerTurn,
                    "Failed to claim runner turn",
                    &e,
                )
                .await;
                self.mark_checkpoint_interrupted().await;
                persist_failed_codex_message(
                    self,
                    &assistant_message_id,
                    &mut accumulated_text,
                    &mut content_blocks,
                    failure_message,
                    msg.message_metadata.as_deref(),
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
        runtime::record_spawn_started(
            &self.conversation_id,
            self.current_client_id.as_deref(),
            &runner_turn_id,
            msg.config.runtime.as_job_runtime(),
            &msg.config.codex_options.model,
            &msg.config.codex_options.reasoning_effort,
            codex_spawn_start_ms,
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
            current_conversation_id: Some(&self.conversation_id),
            approved_mcp_tools: msg.config.agent_type.approved_mcp_tool_names(),
        })
        .await
        {
            Ok(turn) => turn,
            Err(e) => {
                let mut accumulated_text = String::new();
                let mut content_blocks = Vec::new();
                let failure_message = format!("Failed to start Codex: {}", e);
                let failed_at_ms = Utc::now().timestamp_millis();
                runtime::record_spawn_finished(
                    &self.conversation_id,
                    self.current_client_id.as_deref(),
                    &runner_turn_id,
                    msg.config.runtime.as_job_runtime(),
                    "failed",
                    failed_at_ms.saturating_sub(codex_spawn_start_ms) as u64,
                    failed_at_ms,
                );
                log_user_visible_runtime_failure(
                    &self.db,
                    &self.conversation_id,
                    RuntimeFailurePhase::SpawnCodex,
                    "Failed to start Codex",
                    &e,
                )
                .await;
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
                    msg.message_metadata.as_deref(),
                )
                .await;
                return;
            }
        };
        let codex_spawn_ready_ms = Utc::now().timestamp_millis();
        runtime::record_spawn_finished(
            &self.conversation_id,
            self.current_client_id.as_deref(),
            &runner_turn_id,
            msg.config.runtime.as_job_runtime(),
            "ready",
            codex_spawn_ready_ms.saturating_sub(codex_spawn_start_ms) as u64,
            codex_spawn_ready_ms,
        );

        self.manager
            .insert_app_server_turn(self.conversation_id.clone(), turn.turn_handle())
            .await;
        tracing::info!(
            "[CANCEL] Registered live Codex app-server turn for conversation {} runner_turn_id={}",
            self.conversation_id,
            runner_turn_id
        );

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
            let terminate_started_ms = Utc::now().timestamp_millis();
            cancellation::record_runner_cancel_observed(
                &self.conversation_id,
                &runner_turn_id,
                "after_registration",
                terminate_started_ms,
            );
            if let Err(e) = turn.terminate().await {
                tracing::warn!(
                    "[WORKER] Failed to terminate newly-started cancelled Codex turn for {}: {}",
                    self.conversation_id,
                    e
                );
            } else {
                cancellation::record_process_termination_signalled(
                    &self.conversation_id,
                    &runner_turn_id,
                    "after_registration",
                    terminate_started_ms,
                    Utc::now().timestamp_millis(),
                );
            }
        }

        loop {
            tokio::select! {
                maybe_event = turn.events.recv() => {
                    match maybe_event {
                        Some(CodexAppServerEvent::ThreadStarted { thread_id: tid }) => {
                            let started_at_ms = Utc::now().timestamp_millis();
                            if let Some(turn_started_at_ms) = self.current_turn_started_at_ms {
                                runtime::record_latency_marker(
                                    &self.conversation_id,
                                    self.current_client_id.as_deref(),
                                    RuntimeLatencyPhase::CodexThreadStarted,
                                    started_at_ms.saturating_sub(turn_started_at_ms) as u64,
                                    started_at_ms,
                                    None,
                                    None,
                                );
                            }
                            tracing::info!(
                                target: "agentic_api::runtime",
                                event = "agent_runtime.codex_thread_started",
                                conversation_id = %self.conversation_id,
                                client_id = self.current_client_id.as_deref().unwrap_or("none"),
                                runner_turn_id = %runner_turn_id,
                                thread_id = %tid,
                                started_at_ms,
                                "Codex thread started"
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
                        let terminate_started_ms = Utc::now().timestamp_millis();
                        cancellation::record_runner_cancel_observed(
                            &self.conversation_id,
                            &runner_turn_id,
                            "event_loop",
                            terminate_started_ms,
                        );
                        if let Err(e) = turn.terminate().await {
                            tracing::warn!(
                                "[WORKER] Failed to terminate cancelled Codex turn for {}: {}",
                                self.conversation_id,
                                e
                            );
                        } else {
                            cancellation::record_process_termination_signalled(
                                &self.conversation_id,
                                &runner_turn_id,
                                "event_loop",
                                terminate_started_ms,
                                Utc::now().timestamp_millis(),
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
                        let terminate_started_ms = Utc::now().timestamp_millis();
                        cancellation::record_runner_cancel_observed(
                            &self.conversation_id,
                            &runner_turn_id,
                            "heartbeat",
                            terminate_started_ms,
                        );
                        if let Err(e) = turn.terminate().await {
                            tracing::warn!(
                                "[WORKER] Failed to terminate cancelled Codex turn for {}: {}",
                                self.conversation_id,
                                e
                            );
                        } else {
                            cancellation::record_process_termination_signalled(
                                &self.conversation_id,
                                &runner_turn_id,
                                "heartbeat",
                                terminate_started_ms,
                                Utc::now().timestamp_millis(),
                            );
                        }
                    }
                }
            }
        }

        self.manager
            .remove_app_server_turn(&self.conversation_id)
            .await;
        let wait_started_ms = Utc::now().timestamp_millis();
        let outcome = match turn.wait().await {
            Ok(outcome) => {
                if kill_requested {
                    cancellation::record_process_termination_elapsed(
                        &self.conversation_id,
                        &runner_turn_id,
                        wait_started_ms,
                        Utc::now().timestamp_millis(),
                        outcome.success(),
                    );
                }
                outcome
            }
            Err(e) => {
                let failure_message = format!("Codex turn failed: {}", e);
                log_user_visible_runtime_failure(
                    &self.db,
                    &self.conversation_id,
                    RuntimeFailurePhase::WaitCodexTurn,
                    "Failed waiting for Codex turn",
                    &e,
                )
                .await;
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
                if let Some(turn_started_at_ms) = self.current_turn_started_at_ms {
                    let finished_at_ms = Utc::now().timestamp_millis();
                    runtime::record_turn_completed(
                        &self.conversation_id,
                        msg.config.agent_type.as_str(),
                        msg.config.runtime.as_job_runtime(),
                        "failed",
                        finished_at_ms.saturating_sub(turn_started_at_ms) as u64,
                        tool_call_count,
                        accumulated_text.chars().count(),
                    );
                }
                persist_failed_codex_message(
                    self,
                    &assistant_message_id,
                    &mut accumulated_text,
                    &mut content_blocks,
                    failure_message,
                    msg.message_metadata.as_deref(),
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
        let terminal_db_state_started_at_ms = Utc::now().timestamp_millis();
        if let Err(e) =
            agent_runners::finish_turn(&self.db, &runner_turn_id, runner_terminal_status).await
        {
            tracing::warn!(
                "[WORKER] Failed to mark runner turn {} for {}: {}",
                runner_terminal_status,
                self.conversation_id,
                e
            );
        } else if kill_requested || matches!(&completion, CodexTurnCompletion::Cancelled { .. }) {
            cancellation::record_terminal_db_state_written(
                &self.conversation_id,
                &runner_turn_id,
                runner_terminal_status,
                terminal_db_state_started_at_ms,
                Utc::now().timestamp_millis(),
            );
        }
        if let Some(turn_started_at_ms) = self.current_turn_started_at_ms {
            let finished_at_ms = Utc::now().timestamp_millis();
            runtime::record_turn_completed(
                &self.conversation_id,
                msg.config.agent_type.as_str(),
                msg.config.runtime.as_job_runtime(),
                runner_terminal_status,
                finished_at_ms.saturating_sub(turn_started_at_ms) as u64,
                tool_call_count,
                accumulated_text.chars().count(),
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

                if let Err(e) = maybe_insert_child_completion_status_to_parent(
                    &self.db,
                    &self.conversation_id,
                    &assistant_message_id,
                    "completed",
                    accumulated_text.as_str(),
                    None,
                    msg.message_metadata.as_deref(),
                )
                .await
                {
                    tracing::error!(
                        "[WORKER] Failed to insert child completion status for {}: {}",
                        self.conversation_id,
                        e
                    );
                }

                self.send_completion_alert_push_if_eligible(&msg).await;

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
                if let Err(e) = maybe_insert_child_completion_status_to_parent(
                    &self.db,
                    &self.conversation_id,
                    &assistant_message_id,
                    "cancelled",
                    accumulated_text.as_str(),
                    None,
                    msg.message_metadata.as_deref(),
                )
                .await
                {
                    tracing::error!(
                        "[WORKER] Failed to insert child cancellation status for {}: {}",
                        self.conversation_id,
                        e
                    );
                }
            }
            CodexTurnCompletion::Failed(message) => {
                log_user_visible_runtime_failure(
                    &self.db,
                    &self.conversation_id,
                    RuntimeFailurePhase::CodexTurnFailed,
                    "Codex turn failed",
                    &message,
                )
                .await;
                self.mark_checkpoint_interrupted().await;
                persist_failed_codex_message(
                    self,
                    &assistant_message_id,
                    &mut accumulated_text,
                    &mut content_blocks,
                    message,
                    msg.message_metadata.as_deref(),
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
                        if let Some(turn_started_at_ms) = self.current_turn_started_at_ms {
                            runtime::record_latency_marker(
                                &self.conversation_id,
                                self.current_client_id.as_deref(),
                                RuntimeLatencyPhase::MessageStartPersisted,
                                persisted_at_ms.saturating_sub(turn_started_at_ms) as u64,
                                persisted_at_ms,
                                Some(allocated_index),
                                Some(bytes),
                            );
                        }
                    }
                    if ae_type == "content_block_delta" && !self.first_content_delta_logged {
                        self.first_content_delta_logged = true;
                        if let Some(turn_started_at_ms) = self.current_turn_started_at_ms {
                            runtime::record_latency_marker(
                                &self.conversation_id,
                                self.current_client_id.as_deref(),
                                RuntimeLatencyPhase::FirstAssistantDeltaPersisted,
                                persisted_at_ms.saturating_sub(turn_started_at_ms) as u64,
                                persisted_at_ms,
                                Some(allocated_index),
                                Some(bytes),
                            );
                        }
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

    async fn send_completion_alert_push_if_eligible(&self, msg: &WorkerMessage) {
        let Some(apns) = crate::apns::ApnsService::global() else {
            tracing::warn!("[WORKER] APNs not initialized — skipping push notification");
            return;
        };

        let conversation =
            match conversations::get_conversation(&self.db, &self.conversation_id, false).await {
                Ok(Some(conversation)) => conversation,
                Ok(None) => {
                    tracing::warn!(
                        "[WORKER] Skipping completion push for conv={} - conversation not found",
                        self.conversation_id
                    );
                    return;
                }
                Err(e) => {
                    tracing::warn!(
                        "[WORKER] Skipping completion push conv={} - lookup failed: {}",
                        self.conversation_id,
                        e
                    );
                    return;
                }
            };

        let decision = completion_alert_push_decision(&conversation);
        let CompletionAlertPushDecision::Send { title: push_title } = decision else {
            let reason = decision.skip_reason();
            tracing::info!(
                "[WORKER] Skipping completion push for conv={} - {}",
                self.conversation_id,
                reason.unwrap_or("not eligible")
            );
            return;
        };

        tracing::info!(
            "[WORKER] Sending push notification for user={}, conv={}",
            msg.user_id,
            self.conversation_id
        );
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
                    tracing::info!("[WORKER] Push notification sent for user={}", push_user)
                }
                Err(e) => tracing::warn!(
                    "[WORKER] Push notification failed for user={}: {}",
                    push_user,
                    e
                ),
            }
        });
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CompletionAlertPushDecision {
    Send { title: String },
    Skip { reason: &'static str },
}

impl CompletionAlertPushDecision {
    fn skip_reason(&self) -> Option<&'static str> {
        match self {
            Self::Send { .. } => None,
            Self::Skip { reason } => Some(reason),
        }
    }
}

fn completion_alert_push_decision(
    conversation: &ticketing_system::Conversation,
) -> CompletionAlertPushDecision {
    if conversation.parent_conversation_id.is_some() {
        return CompletionAlertPushDecision::Skip {
            reason: "child conversation has parent",
        };
    }

    if conversation.conversation_role == "sub_agent" {
        return CompletionAlertPushDecision::Skip {
            reason: "sub-agent conversation",
        };
    }

    let title = conversation.title.trim();
    if title.is_empty() {
        return CompletionAlertPushDecision::Skip {
            reason: "empty conversation title",
        };
    }

    CompletionAlertPushDecision::Send {
        title: title.to_string(),
    }
}

fn work_context_skip_reason(
    user_message: &str,
    config: &ChatConfig,
    has_routed: bool,
) -> Option<WorkContextSkipReason> {
    if matches!(
        config.agent_type,
        AgentType::ConversationEvaluator | AgentType::Feedback
    ) {
        return Some(WorkContextSkipReason::SupportAgent);
    }
    if prompt_vars_have_artifact_memory_handoff(&config.prompt_vars) {
        return Some(WorkContextSkipReason::ExistingArtifactHandoff);
    }
    if has_routed {
        return Some(WorkContextSkipReason::AlreadyRouted);
    }
    if is_tiny_conversational_message(user_message) {
        return Some(WorkContextSkipReason::TinyConversational);
    }
    None
}

fn is_tiny_conversational_message(user_message: &str) -> bool {
    let trimmed = user_message.trim();
    let lowered = trimmed.to_lowercase();
    if ROUTER_SKIP_MESSAGES.contains(&lowered.as_str()) {
        return true;
    }

    !trimmed.is_empty()
        && trimmed.chars().count() <= 4
        && !trimmed.chars().any(char::is_alphanumeric)
        && trimmed.chars().all(|c| c.is_ascii_punctuation())
}

fn message_references_ticket_id(user_message: &str) -> bool {
    TICKET_ID_RE.is_match(user_message)
}

fn prompt_vars_have_artifact_memory_handoff(vars: &HashMap<String, String>) -> bool {
    vars.iter().any(|(key, value)| {
        key.eq_ignore_ascii_case(ARTIFACT_MEMORY_HANDOFF_VAR) && !value.trim().is_empty()
    })
}

fn apply_artifact_memory_handoff_prompt_var(
    vars: &mut HashMap<String, String>,
    handoff: Option<&str>,
) {
    let Some(handoff) = handoff.map(str::trim).filter(|handoff| !handoff.is_empty()) else {
        return;
    };
    if prompt_vars_have_artifact_memory_handoff(vars) {
        return;
    }
    vars.insert(ARTIFACT_MEMORY_HANDOFF_VAR.to_string(), handoff.to_string());
}

fn startup_context_forwarded_metadata(response: &CollectWorkContextResponse) -> Result<String> {
    let handoff = &response.prompt_ready_handoff;
    serde_json::to_string(&serde_json::json!({
        "origin": "router_preflight",
        "preflight": "collect_work_context",
        "ticket_status": response.ticket_result.status.clone(),
        "ticket_action": response.ticket_result.action.clone(),
        "artifact_memory_handoff": handoff_metadata_json(handoff),
    }))
    .context("serialize startup context forwarded metadata")
}

fn handoff_metadata_json(handoff: &WorkContextHandoff) -> serde_json::Value {
    serde_json::json!({
        "contract_version": handoff.contract_version.clone(),
        "operation": handoff.operation.clone(),
        "organization": handoff.organization.clone(),
        "repository": handoff.repository.clone(),
        "ticket_id": handoff.ticket.as_ref().map(|ticket| ticket.ticket_id.clone()),
        "context_packet_ids": handoff.context_packet_ids.clone(),
        "retrieval_ids": handoff.retrieval_ids.clone(),
        "packet_count": handoff.context_packet_ids.len(),
        "retrieval_count": handoff.retrieval_ids.len(),
        "warning_count": handoff.warnings.len(),
        "source_metadata": handoff.source_metadata.clone(),
    })
}

fn trimmed_non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

async fn log_user_visible_runtime_failure(
    db: &Arc<SqlitePool>,
    conversation_id: &str,
    phase: RuntimeFailurePhase,
    message: &str,
    error: impl std::fmt::Display,
) {
    let error = error.to_string();
    runtime::record_runtime_failure(conversation_id, phase, &error);
    let detail = format!(
        "conversation_id={}; phase={}; error={}",
        conversation_id, phase, error
    );
    crate::system_log_helper::log_error(db, "chat", message, Some(&detail)).await;
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
        } else if file_type.is_file() && mime_type_for_generated_attachment_path(&path).is_some() {
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
        .with_context(|| {
            format!(
                "Failed to create chat attachment dir {}",
                chat_dir.display()
            )
        })?;

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
    message_metadata: Option<&str>,
) {
    let error_message = message.clone();
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

    if let Err(e) = maybe_insert_child_completion_status_to_parent(
        &worker.db,
        &worker.conversation_id,
        assistant_message_id,
        "failed",
        accumulated_text.as_str(),
        Some(error_message.as_str()),
        message_metadata,
    )
    .await
    {
        tracing::error!(
            "[WORKER] Failed to insert child failure status for {}: {}",
            worker.conversation_id,
            e
        );
    }
}

async fn maybe_insert_child_completion_status_to_parent(
    db: &SqlitePool,
    child_conversation_id: &str,
    child_assistant_message_id: &str,
    terminal_status: &str,
    child_output: &str,
    error_message: Option<&str>,
    message_metadata: Option<&str>,
) -> Result<()> {
    if suppress_parent_completion_relay(message_metadata) {
        tracing::info!(
            "[WORKER] Suppressed parent completion relay for child={} status={}",
            child_conversation_id,
            terminal_status
        );
        return Ok(());
    }

    let batch_context = child_batch_context_from_initial_metadata(message_metadata);
    insert_child_completion_status_to_parent(
        db,
        child_conversation_id,
        child_assistant_message_id,
        terminal_status,
        child_output,
        error_message,
        batch_context.as_ref(),
    )
    .await
}

fn suppress_parent_completion_relay(message_metadata: Option<&str>) -> bool {
    let Some(message_metadata) = message_metadata else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(message_metadata) else {
        return false;
    };
    value
        .get("suppress_parent_completion_relay")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

#[derive(Debug, Clone)]
struct ChildBatchContext {
    batch_id: String,
    expected_count: usize,
    child_index: Option<usize>,
}

#[derive(Debug, Clone)]
struct CompletedChildCompletion {
    child_conversation_id: String,
    child_title: String,
    child_agent: String,
    child_conversation_type: String,
    terminal_status: String,
    child_assistant_message_id: Option<String>,
    status_message_id: String,
}

fn child_batch_context_from_initial_metadata(
    message_metadata: Option<&str>,
) -> Option<ChildBatchContext> {
    let value: serde_json::Value = serde_json::from_str(message_metadata?).ok()?;
    if value.get("origin")?.as_str()? != "agent_orchestrated" {
        return None;
    }
    if value.get("orchestration")?.as_str()? != "child_initial_turn" {
        return None;
    }

    let batch_id = value
        .get("child_batch_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())?
        .to_string();
    let expected_count = value
        .get("child_batch_size")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value > 0)?;
    let child_index = value
        .get("child_batch_index")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value > 0);

    Some(ChildBatchContext {
        batch_id,
        expected_count,
        child_index,
    })
}

async fn insert_child_completion_status_to_parent(
    db: &SqlitePool,
    child_conversation_id: &str,
    child_assistant_message_id: &str,
    terminal_status: &str,
    child_output: &str,
    error_message: Option<&str>,
    batch_context: Option<&ChildBatchContext>,
) -> Result<()> {
    let child = match conversations::get_conversation(db, child_conversation_id, false).await? {
        Some(child) => child,
        None => {
            anyhow::bail!(
                "Child conversation not found for completion status: {}",
                child_conversation_id
            );
        }
    };

    let Some(parent_conversation_id) = child.parent_conversation_id.as_deref() else {
        return Ok(());
    };

    if child.conversation_role != "sub_agent" {
        anyhow::bail!(
            "Conversation {} has parent {} but role is {}; expected sub_agent",
            child.id,
            parent_conversation_id,
            child.conversation_role
        );
    }

    let parent = conversations::get_conversation(db, parent_conversation_id, false)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Parent conversation not found for child completion status: {}",
                parent_conversation_id
            )
        })?;

    let child_agent = child.agent.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "Child conversation {} has no agent configured for completion status",
            child.id
        )
    })?;
    let child_conversation_type = child.conversation_type.as_deref().unwrap_or("general");
    let summary = summarize_child_completion_output(child_output, error_message);
    let relay_message = format_child_completion_status_message(
        &child.title,
        &child.id,
        child_agent,
        terminal_status,
        child_assistant_message_id,
        &summary,
    );
    let metadata = child_completion_status_metadata(
        &child.id,
        &child.title,
        child_agent,
        child_assistant_message_id,
        terminal_status,
        child_conversation_type,
        &summary,
        batch_context,
    )?;

    let status_message = conversations::add_message(
        db,
        &parent.id,
        AddMessageRequest {
            role: "assistant".to_string(),
            content: relay_message,
            attachments: None,
            metadata: Some(metadata),
        },
    )
    .await
    .with_context(|| {
        format!(
            "Failed to insert child completion status for parent {} from child {}",
            parent.id, child.id
        )
    })?;

    super::conversations::publish_conversation_run_status(db, &parent.id).await?;
    tracing::info!(
        "[WORKER] Inserted child completion status message {} parent={} child={} child_agent={} status={}",
        status_message.id,
        parent.id,
        child.id,
        child_agent,
        terminal_status
    );

    if let Err(e) = maybe_enqueue_parent_coordinator_wake(
        db,
        &parent,
        &child,
        child_agent,
        child_conversation_type,
        terminal_status,
        child_assistant_message_id,
        &status_message.id,
        batch_context,
    )
    .await
    {
        tracing::error!(
            "[WORKER] Failed to enqueue parent coordinator wake parent={} child={} status={}: {}",
            parent.id,
            child.id,
            terminal_status,
            e
        );
    }

    Ok(())
}

async fn maybe_enqueue_parent_coordinator_wake(
    db: &SqlitePool,
    parent: &ticketing_system::Conversation,
    child: &ticketing_system::Conversation,
    child_agent: &str,
    child_conversation_type: &str,
    terminal_status: &str,
    child_assistant_message_id: &str,
    status_message_id: &str,
    batch_context: Option<&ChildBatchContext>,
) -> Result<()> {
    if let Some(batch_context) = batch_context {
        return enqueue_parent_coordinator_batch_wake_if_complete(db, parent, batch_context).await;
    }

    enqueue_parent_coordinator_wake(
        db,
        parent,
        child,
        child_agent,
        child_conversation_type,
        terminal_status,
        child_assistant_message_id,
        status_message_id,
    )
    .await
}

async fn enqueue_parent_coordinator_wake(
    db: &SqlitePool,
    parent: &ticketing_system::Conversation,
    child: &ticketing_system::Conversation,
    child_agent: &str,
    child_conversation_type: &str,
    terminal_status: &str,
    child_assistant_message_id: &str,
    status_message_id: &str,
) -> Result<()> {
    let agent_type = parent_coordinator_agent_type(parent)?;
    let prompt_vars = parent_coordinator_prompt_vars(&agent_type, &parent.user_id)?;
    let message = format_parent_coordinator_wake_message(
        child,
        child_agent,
        child_conversation_type,
        terminal_status,
        child_assistant_message_id,
        status_message_id,
    );
    let metadata = parent_coordinator_wake_metadata(
        &child.id,
        &child.title,
        child_agent,
        child_conversation_type,
        terminal_status,
        child_assistant_message_id,
        status_message_id,
    )?;

    checkpoints::upsert_checkpoint(db, &parent.id, "queued", 0)
        .await
        .with_context(|| format!("queue parent coordinator checkpoint {}", parent.id))?;

    let payload = conversation_turn_jobs::ConversationTurnJobPayload {
        user_id: parent.user_id.clone(),
        message,
        agent_type: agent_type.as_str().to_string(),
        runtime: super::chat_stream::ChatRuntime::CodexAppServer
            .as_job_runtime()
            .to_string(),
        prompt_name: agent_type.as_str().to_string(),
        working_dir: "/Users/jarvisgpt/projects".to_string(),
        prompt_vars: super::chat_stream::encode_codex_options_for_job(
            prompt_vars,
            &super::chat_stream::ChatCodexOptions::default_for_agent(&agent_type),
        ),
        images_json: None,
        client_id: Some(format!(
            "coordinator-wake:{}:{}",
            child.id, child_assistant_message_id
        )),
        message_metadata: Some(metadata),
    };

    let job_id = conversation_turn_jobs::enqueue_job(db, &parent.id, payload)
        .await
        .with_context(|| {
            format!(
                "enqueue parent coordinator wake parent={} child={}",
                parent.id, child.id
            )
        })?;
    super::conversations::publish_conversation_run_status(db, &parent.id)
        .await
        .with_context(|| format!("publish parent coordinator wake status {}", parent.id))?;
    tracing::info!(
        "[WORKER] Enqueued parent coordinator wake job={} parent={} child={} status={}",
        job_id,
        parent.id,
        child.id,
        terminal_status
    );
    Ok(())
}

async fn enqueue_parent_coordinator_batch_wake_if_complete(
    db: &SqlitePool,
    parent: &ticketing_system::Conversation,
    batch_context: &ChildBatchContext,
) -> Result<()> {
    let completed_children =
        completed_child_cards_for_batch(db, &parent.id, &batch_context.batch_id).await?;
    if completed_children.len() < batch_context.expected_count {
        tracing::info!(
            "[WORKER] Delaying parent coordinator wake parent={} child_batch_id={} completed={}/{}",
            parent.id,
            batch_context.batch_id,
            completed_children.len(),
            batch_context.expected_count
        );
        return Ok(());
    }

    if parent_coordinator_batch_wake_exists(db, &parent.id, &batch_context.batch_id).await? {
        tracing::info!(
            "[WORKER] Parent coordinator wake already exists parent={} child_batch_id={}",
            parent.id,
            batch_context.batch_id
        );
        return Ok(());
    }

    let agent_type = parent_coordinator_agent_type(parent)?;
    let prompt_vars = parent_coordinator_prompt_vars(&agent_type, &parent.user_id)?;
    let message = format_parent_coordinator_batch_wake_message(batch_context, &completed_children);
    let metadata = parent_coordinator_batch_wake_metadata(batch_context, &completed_children)?;

    checkpoints::upsert_checkpoint(db, &parent.id, "queued", 0)
        .await
        .with_context(|| format!("queue parent coordinator checkpoint {}", parent.id))?;

    let payload = conversation_turn_jobs::ConversationTurnJobPayload {
        user_id: parent.user_id.clone(),
        message,
        agent_type: agent_type.as_str().to_string(),
        runtime: super::chat_stream::ChatRuntime::CodexAppServer
            .as_job_runtime()
            .to_string(),
        prompt_name: agent_type.as_str().to_string(),
        working_dir: "/Users/jarvisgpt/projects".to_string(),
        prompt_vars: super::chat_stream::encode_codex_options_for_job(
            prompt_vars,
            &super::chat_stream::ChatCodexOptions::default_for_agent(&agent_type),
        ),
        images_json: None,
        client_id: Some(format!("coordinator-wake-batch:{}", batch_context.batch_id)),
        message_metadata: Some(metadata),
    };

    let job_id = conversation_turn_jobs::enqueue_job(db, &parent.id, payload)
        .await
        .with_context(|| {
            format!(
                "enqueue parent coordinator batch wake parent={} child_batch_id={}",
                parent.id, batch_context.batch_id
            )
        })?;
    super::conversations::publish_conversation_run_status(db, &parent.id)
        .await
        .with_context(|| format!("publish parent coordinator batch wake status {}", parent.id))?;
    tracing::info!(
        "[WORKER] Enqueued parent coordinator batch wake job={} parent={} child_batch_id={} completed={}",
        job_id,
        parent.id,
        batch_context.batch_id,
        completed_children.len()
    );
    Ok(())
}

async fn completed_child_cards_for_batch(
    db: &SqlitePool,
    parent_conversation_id: &str,
    child_batch_id: &str,
) -> Result<Vec<CompletedChildCompletion>> {
    #[derive(sqlx::FromRow)]
    struct CompletionCardRow {
        status_message_id: String,
        child_conversation_id: Option<String>,
        child_title: Option<String>,
        child_agent: Option<String>,
        child_conversation_type: Option<String>,
        terminal_status: Option<String>,
        child_assistant_message_id: Option<String>,
    }

    let rows = sqlx::query_as::<_, CompletionCardRow>(
        r#"
        SELECT
            id AS status_message_id,
            json_extract(metadata, '$.child_conversation_id') AS child_conversation_id,
            json_extract(metadata, '$.child_title') AS child_title,
            json_extract(metadata, '$.child_agent') AS child_agent,
            json_extract(metadata, '$.child_conversation_type') AS child_conversation_type,
            json_extract(metadata, '$.child_terminal_status') AS terminal_status,
            json_extract(metadata, '$.child_assistant_message_id') AS child_assistant_message_id
        FROM conversation_messages
        WHERE conversation_id = ?
          AND metadata IS NOT NULL
          AND json_valid(metadata)
          AND json_extract(metadata, '$.origin') = 'agent_orchestrated'
          AND json_extract(metadata, '$.orchestration') = 'child_completion_status'
          AND json_extract(metadata, '$.child_batch_id') = ?
        ORDER BY message_index ASC
        "#,
    )
    .bind(parent_conversation_id)
    .bind(child_batch_id)
    .fetch_all(db)
    .await
    .context("load completed child cards for batch")?;

    let mut seen_child_ids = HashSet::new();
    let mut completed = Vec::new();
    for row in rows {
        let Some(child_conversation_id) = row.child_conversation_id else {
            continue;
        };
        if !seen_child_ids.insert(child_conversation_id.clone()) {
            continue;
        }
        completed.push(CompletedChildCompletion {
            child_conversation_id,
            child_title: row.child_title.unwrap_or_else(|| "Child agent".to_string()),
            child_agent: row.child_agent.unwrap_or_else(|| "full-access".to_string()),
            child_conversation_type: row
                .child_conversation_type
                .unwrap_or_else(|| "general".to_string()),
            terminal_status: row
                .terminal_status
                .unwrap_or_else(|| "completed".to_string()),
            child_assistant_message_id: row.child_assistant_message_id,
            status_message_id: row.status_message_id,
        });
    }

    Ok(completed)
}

async fn parent_coordinator_batch_wake_exists(
    db: &SqlitePool,
    parent_conversation_id: &str,
    child_batch_id: &str,
) -> Result<bool> {
    let message_count: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)
        FROM conversation_messages
        WHERE conversation_id = ?
          AND metadata IS NOT NULL
          AND json_valid(metadata)
          AND json_extract(metadata, '$.origin') = 'agent_orchestrated'
          AND json_extract(metadata, '$.orchestration') = 'coordinator_child_completion_wake'
          AND json_extract(metadata, '$.child_batch_id') = ?
        "#,
    )
    .bind(parent_conversation_id)
    .bind(child_batch_id)
    .fetch_one(db)
    .await
    .context("inspect existing coordinator batch wake messages")?;
    if message_count > 0 {
        return Ok(true);
    }

    let job_count: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)
        FROM conversation_turn_jobs
        WHERE conversation_id = ?
          AND status IN ('pending', 'running', 'completed')
          AND message_metadata IS NOT NULL
          AND json_valid(message_metadata)
          AND json_extract(message_metadata, '$.origin') = 'agent_orchestrated'
          AND json_extract(message_metadata, '$.orchestration') = 'coordinator_child_completion_wake'
          AND json_extract(message_metadata, '$.child_batch_id') = ?
        "#,
    )
    .bind(parent_conversation_id)
    .bind(child_batch_id)
    .fetch_one(db)
    .await
    .context("inspect existing coordinator batch wake jobs")?;

    Ok(job_count > 0)
}

fn parent_coordinator_agent_type(parent: &ticketing_system::Conversation) -> Result<AgentType> {
    let raw_agent = parent.agent.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "Parent conversation {} has no coordinator agent configured",
            parent.id
        )
    })?;
    AgentType::from_chat_agent_key(raw_agent)
        .or_else(|| serde_json::from_value(serde_json::Value::String(raw_agent.to_string())).ok())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Parent conversation {} uses unsupported coordinator agent {}",
                parent.id,
                raw_agent
            )
        })
}

fn parent_coordinator_prompt_vars(
    agent_type: &AgentType,
    user_id: &str,
) -> Result<HashMap<String, String>> {
    let mut prompt_vars = HashMap::new();
    prompt_vars.insert("USER_ID".to_string(), user_id.to_string());

    if matches!(agent_type, AgentType::FullAccess) {
        let agents_md = std::fs::read_to_string("/Users/jarvisgpt/projects/AGENTS.md")
            .context("read /Users/jarvisgpt/projects/AGENTS.md for full-access wake")?;
        prompt_vars.insert("AGENTS_MD".to_string(), agents_md);
    }

    Ok(prompt_vars)
}

fn format_parent_coordinator_wake_message(
    child: &ticketing_system::Conversation,
    child_agent: &str,
    child_conversation_type: &str,
    terminal_status: &str,
    child_assistant_message_id: &str,
    status_message_id: &str,
) -> String {
    format!(
        "Coordinator wake: child agent {terminal_status}.\n\nChild: {child_title}\nChild conversation: `{child_conversation_id}`\nChild agent: `{child_agent}`\nChild type: `{child_conversation_type}`\nCompletion card message: `{status_message_id}`\nChild assistant message: `{child_assistant_message_id}`\n\nReview the child conversation, then decide the next orchestration step. If the output is incomplete, ask for targeted follow-up. If it is sufficient, queue any unblocked dependent child agents or report the decision back to Alex. Keep the parent response concise.",
        child_title = child.title,
        child_conversation_id = child.id,
    )
}

fn format_parent_coordinator_batch_wake_message(
    batch_context: &ChildBatchContext,
    completed_children: &[CompletedChildCompletion],
) -> String {
    let child_lines = completed_children
        .iter()
        .enumerate()
        .map(|(idx, child)| {
            let assistant_line = child
                .child_assistant_message_id
                .as_ref()
                .map(|id| format!("\n   Child assistant message: `{id}`"))
                .unwrap_or_default();
            format!(
                "{}. Child: {}\n   Child conversation: `{}`\n   Child agent: `{}`\n   Child type: `{}`\n   Terminal status: `{}`\n   Completion card message: `{}`{}",
                idx + 1,
                child.child_title,
                child.child_conversation_id,
                child.child_agent,
                child.child_conversation_type,
                child.terminal_status,
                child.status_message_id,
                assistant_line
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "Coordinator wake: all queued child agents in the batch have responded.\n\nChild batch: `{child_batch_id}`\nCompleted children: {completed_count}/{expected_count}\n\n{child_lines}\n\nReview all listed child conversations, then decide the next orchestration step. If any output is incomplete, ask for targeted follow-up. If the set is sufficient, queue any unblocked dependent child agents or report the decision back to Alex. Keep the parent response concise.",
        child_batch_id = batch_context.batch_id,
        completed_count = completed_children.len(),
        expected_count = batch_context.expected_count,
    )
}

fn parent_coordinator_wake_metadata(
    child_conversation_id: &str,
    child_title: &str,
    child_agent: &str,
    child_conversation_type: &str,
    terminal_status: &str,
    child_assistant_message_id: &str,
    status_message_id: &str,
) -> Result<String> {
    serde_json::to_string(&serde_json::json!({
        "origin": "agent_orchestrated",
        "orchestrated_by": "agent-runner",
        "orchestration": "coordinator_child_completion_wake",
        "display": "coordinator_wake_request",
        "child_conversation_id": child_conversation_id,
        "child_title": child_title,
        "child_agent": child_agent,
        "child_conversation_type": child_conversation_type,
        "child_terminal_status": terminal_status,
        "child_assistant_message_id": child_assistant_message_id,
        "child_completion_message_id": status_message_id,
    }))
    .context("serialize parent coordinator wake metadata")
}

fn parent_coordinator_batch_wake_metadata(
    batch_context: &ChildBatchContext,
    completed_children: &[CompletedChildCompletion],
) -> Result<String> {
    let children = completed_children
        .iter()
        .map(|child| {
            serde_json::json!({
                "child_conversation_id": child.child_conversation_id,
                "child_title": child.child_title,
                "child_agent": child.child_agent,
                "child_conversation_type": child.child_conversation_type,
                "child_terminal_status": child.terminal_status,
                "child_assistant_message_id": child.child_assistant_message_id,
                "child_completion_message_id": child.status_message_id,
            })
        })
        .collect::<Vec<_>>();

    serde_json::to_string(&serde_json::json!({
        "origin": "agent_orchestrated",
        "orchestrated_by": "agent-runner",
        "orchestration": "coordinator_child_completion_wake",
        "display": "coordinator_wake_request",
        "child_batch_id": batch_context.batch_id,
        "child_batch_size": batch_context.expected_count,
        "child_completion_count": completed_children.len(),
        "child_conversations": children,
    }))
    .context("serialize parent coordinator batch wake metadata")
}

fn format_child_completion_status_message(
    child_title: &str,
    child_conversation_id: &str,
    child_agent: &str,
    terminal_status: &str,
    child_assistant_message_id: &str,
    summary: &str,
) -> String {
    let summary = summary.trim();
    let summary = if summary.is_empty() {
        "No assistant summary was available."
    } else {
        summary
    };

    format!(
        "Child agent {terminal_status}: {child_title}\n\n{summary}\n\nOpen child chat: agenticflowstate://conversation/{child_conversation_id}?agent={child_agent}\n\nChild conversation: `{child_conversation_id}`\nAssistant message: `{child_assistant_message_id}`"
    )
}

fn child_completion_status_metadata(
    child_conversation_id: &str,
    child_title: &str,
    child_agent: &str,
    child_assistant_message_id: &str,
    terminal_status: &str,
    child_conversation_type: &str,
    summary: &str,
    batch_context: Option<&ChildBatchContext>,
) -> Result<String> {
    let mut value = serde_json::json!({
        "origin": "agent_orchestrated",
        "orchestrated_by": "agent-runner",
        "orchestration": "child_completion_status",
        "display": "agent_completion_status_card",
        "child_conversation_id": child_conversation_id,
        "child_title": child_title,
        "child_agent": child_agent,
        "child_assistant_message_id": child_assistant_message_id,
        "child_terminal_status": terminal_status,
        "child_conversation_type": child_conversation_type,
        "summary": summary,
        "open_url": format!(
            "agenticflowstate://conversation/{}?agent={}",
            child_conversation_id, child_agent
        ),
    });
    if let Some(batch_context) = batch_context {
        value["child_batch_id"] = serde_json::Value::String(batch_context.batch_id.clone());
        value["child_batch_size"] = serde_json::json!(batch_context.expected_count);
        if let Some(child_index) = batch_context.child_index {
            value["child_batch_index"] = serde_json::json!(child_index);
        }
    }

    serde_json::to_string(&value).context("serialize child completion status metadata")
}

fn summarize_child_completion_output(child_output: &str, error_message: Option<&str>) -> String {
    if let Some(error_message) = error_message.filter(|message| !message.trim().is_empty()) {
        return truncate_summary_sentence(error_message.trim());
    }

    let normalized = child_output.replace("\r\n", "\n");
    if let Some(section) = extract_summary_section(&normalized) {
        return truncate_summary_sentence(&section);
    }

    let fallback = normalized
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .find(|line| {
            !line.starts_with('#')
                && !line.starts_with("Coordinator instruction:")
                && !line.starts_with("Child conversation:")
                && !line.starts_with("Agent:")
                && !line.starts_with("Status:")
                && !line.starts_with("Assistant message:")
        })
        .unwrap_or("No assistant text was produced.");

    truncate_summary_sentence(fallback)
}

fn extract_summary_section(output: &str) -> Option<String> {
    let lines: Vec<&str> = output.lines().collect();
    for (idx, line) in lines.iter().enumerate() {
        if !is_summary_heading(line) {
            continue;
        }

        let mut section = Vec::new();
        for next in lines.iter().skip(idx + 1) {
            let trimmed = next.trim();
            if trimmed.is_empty() {
                if !section.is_empty() {
                    break;
                }
                continue;
            }
            if is_markdown_heading(trimmed) && !section.is_empty() {
                break;
            }
            if is_numbered_section_heading(trimmed) && !section.is_empty() {
                break;
            }
            section.push(trimmed);
            if section.join(" ").chars().count() >= 220 {
                break;
            }
        }

        let summary = section.join(" ");
        if !summary.trim().is_empty() {
            return Some(summary);
        }
    }

    None
}

fn is_summary_heading(line: &str) -> bool {
    let normalized = line
        .trim()
        .trim_start_matches('#')
        .trim()
        .trim_start_matches(|c: char| c.is_ascii_digit() || c == '.')
        .trim()
        .to_ascii_lowercase();
    normalized == "summary"
}

fn is_markdown_heading(line: &str) -> bool {
    line.starts_with('#')
}

fn is_numbered_section_heading(line: &str) -> bool {
    let mut chars = line.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_digit() && chars.next() == Some('.')
}

fn truncate_summary_sentence(input: &str) -> String {
    let normalized = input.split_whitespace().collect::<Vec<_>>().join(" ");
    const MAX_CHARS: usize = 220;
    if normalized.chars().count() <= MAX_CHARS {
        return normalized;
    }

    let mut out = String::new();
    for ch in normalized.chars().take(MAX_CHARS.saturating_sub(1)) {
        out.push(ch);
    }
    out.push_str("...");
    out
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
    let sanitized_recent: Vec<ConversationMessage> = prompt_history_recent_window(messages)
        .into_iter()
        .map(super::child_completion_status::sanitize_message_for_display)
        .collect();

    for msg in &sanitized_recent {
        let has_content = !msg.content.is_empty();
        let has_tools = msg
            .tool_call_summaries
            .as_ref()
            .map_or(false, |t| !t.is_empty());
        let attachment_description = format_message_attachments_for_prompt(msg);
        let has_attachments = attachment_description.is_some();
        if !has_content && !has_tools && !has_attachments {
            continue;
        }

        let role = prompt_history_role_label(&msg.role);

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

        if let Some(attachment_description) = attachment_description {
            history.push_str(&attachment_description);
            history.push('\n');
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

fn prompt_history_role_label(role: &str) -> &'static str {
    match role {
        "user" => "User",
        "assistant" => "Assistant",
        "system" => "System",
        "forwarded" => "Forwarded",
        _ => "Message",
    }
}

fn prompt_history_recent_window(messages: &[ConversationMessage]) -> Vec<ConversationMessage> {
    let mut transcript_count = 0usize;
    let mut kept_reversed = Vec::new();

    for msg in messages.iter().rev() {
        if transcript_count >= PROMPT_HISTORY_MESSAGE_LIMIT {
            break;
        }

        kept_reversed.push(msg.clone());
        if counts_against_prompt_history_limit(msg) {
            transcript_count += 1;
        }
    }

    kept_reversed.reverse();
    kept_reversed
}

fn counts_against_prompt_history_limit(msg: &ConversationMessage) -> bool {
    !super::child_completion_status::is_child_completion_status_message(msg)
        && !super::child_completion_status::is_coordinator_wake_message(msg)
}

fn format_message_attachments_for_prompt(msg: &ConversationMessage) -> Option<String> {
    let attachments_json = msg.attachments.as_deref()?.trim();
    if attachments_json.is_empty() {
        return None;
    }

    let attachments: Vec<AttachmentMeta> = match serde_json::from_str(attachments_json) {
        Ok(attachments) => attachments,
        Err(e) => {
            return Some(format!(
                "**Attachments:** failed to decode attachment metadata for message `{}`: {}\n",
                msg.id, e
            ));
        }
    };

    if attachments.is_empty() {
        return None;
    }

    let mut lines = Vec::with_capacity(attachments.len() + 2);
    lines.push("**Attachments:**".to_string());
    for attachment in attachments {
        let display_name = attachment
            .display_name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(&attachment.filename);
        lines.push(format_attachment_prompt_line(
            display_name,
            &attachment.mime_type,
            attachment.size_bytes,
            &attachment.path,
        ));
    }
    if lines
        .iter()
        .any(|line| line.contains("image attachment") || line.contains("screenshot"))
    {
        lines.push(
            "  Image/screenshot attachments should be inspected before answering questions that depend on visual content."
                .to_string(),
        );
    }

    Some(format!("{}\n", lines.join("\n")))
}

fn format_attachment_prompt_line(
    display_name: &str,
    mime_type: &str,
    size_bytes: Option<i64>,
    path: &str,
) -> String {
    let attachment_kind = if mime_type.starts_with("image/") {
        "image attachment"
    } else {
        "attachment"
    };
    let size = size_bytes
        .map(|bytes| format!("; {} bytes", bytes))
        .unwrap_or_default();
    format!(
        "  - {} `{}` ({}{}): {}",
        attachment_kind, display_name, mime_type, size, path
    )
}

#[cfg(test)]
mod completion_alert_push_tests {
    use super::*;

    fn conversation(
        id: &str,
        title: &str,
        parent_conversation_id: Option<&str>,
        conversation_role: &str,
    ) -> ticketing_system::Conversation {
        ticketing_system::Conversation {
            id: id.to_string(),
            user_id: "alex".to_string(),
            session_id: None,
            organization: "agentic-flowstate".to_string(),
            agent: Some("full-access".to_string()),
            conversation_type: Some("general".to_string()),
            parent_conversation_id: parent_conversation_id.map(ToOwned::to_owned),
            conversation_role: conversation_role.to_string(),
            child_conversation_count: Some(0),
            child_sort_order: None,
            title: title.to_string(),
            started_at: "2026-07-01T00:00:00Z".to_string(),
            updated_at: "2026-07-01T00:00:00Z".to_string(),
            status: "open".to_string(),
            archived_at: None,
            router_ticket_id: None,
            router_organization: None,
            message_count: Some(0),
            last_event_index: Some(0),
            last_read_event_index: None,
            unread_event_count: None,
            is_active: Some(true),
            messages: None,
        }
    }

    #[test]
    fn root_conversations_are_eligible_for_completion_alerts() {
        let conversation = conversation("parent-1", "  Main chat response  ", None, "standard");

        assert_eq!(
            completion_alert_push_decision(&conversation),
            CompletionAlertPushDecision::Send {
                title: "Main chat response".to_string()
            }
        );
    }

    #[test]
    fn child_conversations_do_not_send_completion_alerts() {
        let conversation = conversation("child-1", "Child lane", Some("parent-1"), "sub_agent");

        assert_eq!(
            completion_alert_push_decision(&conversation),
            CompletionAlertPushDecision::Skip {
                reason: "child conversation has parent"
            }
        );
    }

    #[test]
    fn sub_agent_role_without_parent_is_not_alert_eligible() {
        let conversation = conversation("child-legacy", "Child lane", None, "sub_agent");

        assert_eq!(
            completion_alert_push_decision(&conversation),
            CompletionAlertPushDecision::Skip {
                reason: "sub-agent conversation"
            }
        );
    }
}

#[cfg(test)]
mod work_context_preflight_tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn test_config(agent_type: AgentType) -> ChatConfig {
        ChatConfig {
            agent_type: agent_type.clone(),
            runtime: crate::handlers::chat_stream::ChatRuntime::CodexAppServer,
            prompt_name: "full-access",
            working_dir: PathBuf::from("/Users/jarvisgpt/projects"),
            prompt_vars: HashMap::new(),
            codex_options: crate::handlers::chat_stream::ChatCodexOptions::default_for_agent(
                &agent_type,
            ),
        }
    }

    #[test]
    fn tiny_conversational_turns_skip_without_consuming_substantive_work() {
        let config = test_config(AgentType::FullAccess);

        assert_eq!(
            work_context_skip_reason("ok", &config, false),
            Some(WorkContextSkipReason::TinyConversational)
        );
        assert_eq!(
            work_context_skip_reason("?", &config, false),
            Some(WorkContextSkipReason::TinyConversational)
        );
        assert!(!WorkContextSkipReason::TinyConversational.persist_router_skip());
        assert_eq!(work_context_skip_reason("fix", &config, false), None);
        assert_eq!(work_context_skip_reason("test", &config, false), None);
    }

    #[test]
    fn support_agents_and_existing_handoffs_skip_startup_collection() {
        let support_config = test_config(AgentType::Feedback);
        assert_eq!(
            work_context_skip_reason("Review this conversation", &support_config, false),
            Some(WorkContextSkipReason::SupportAgent)
        );
        assert!(WorkContextSkipReason::SupportAgent.persist_router_skip());

        let mut handoff_config = test_config(AgentType::FullAccess);
        handoff_config.prompt_vars.insert(
            "artifact_memory_handoff".to_string(),
            "{\"contract_version\":\"artifact-memory-handoff-v1\"}".to_string(),
        );
        assert_eq!(
            work_context_skip_reason("Implement the ticket", &handoff_config, false),
            Some(WorkContextSkipReason::ExistingArtifactHandoff)
        );
    }

    #[test]
    fn ticket_references_are_detected_for_startup_org_override() {
        assert!(message_references_ticket_id(
            "Implement the backend lane for ticket T-8A598325."
        ));
        assert!(message_references_ticket_id("please use t-deadBEEF here"));
        assert!(!message_references_ticket_id("This is not-a-ticket."));
        assert!(!message_references_ticket_id("T-123"));
    }

    #[test]
    fn startup_handoff_injection_preserves_existing_prompt_handoff() {
        let mut vars = HashMap::new();
        apply_artifact_memory_handoff_prompt_var(
            &mut vars,
            Some("{\"context_packet_ids\":[\"CP-1\"]}"),
        );
        assert_eq!(
            vars.get(ARTIFACT_MEMORY_HANDOFF_VAR).map(String::as_str),
            Some("{\"context_packet_ids\":[\"CP-1\"]}")
        );

        let mut existing_vars = HashMap::new();
        existing_vars.insert(
            "artifact_memory_handoff".to_string(),
            "{\"context_packet_ids\":[\"CP-existing\"]}".to_string(),
        );
        apply_artifact_memory_handoff_prompt_var(
            &mut existing_vars,
            Some("{\"context_packet_ids\":[\"CP-new\"]}"),
        );
        assert_eq!(
            existing_vars
                .get("artifact_memory_handoff")
                .map(String::as_str),
            Some("{\"context_packet_ids\":[\"CP-existing\"]}")
        );
        assert!(!existing_vars.contains_key(ARTIFACT_MEMORY_HANDOFF_VAR));
    }
}

#[cfg(test)]
mod prompt_history_attachment_tests {
    use super::*;

    fn message_with_attachments(content: &str, attachments: Option<String>) -> ConversationMessage {
        ConversationMessage {
            id: "msg-attachment-test".to_string(),
            conversation_id: "conv-attachment-test".to_string(),
            role: "user".to_string(),
            content: content.to_string(),
            attachments,
            metadata: None,
            tool_call_summaries: None,
            content_blocks: None,
            assistant_turn_duration_seconds: None,
            created_at: 1,
            message_index: 0,
        }
    }

    fn history_message(
        idx: i32,
        role: &str,
        content: &str,
        metadata: Option<String>,
    ) -> ConversationMessage {
        ConversationMessage {
            id: format!("msg-{idx}"),
            conversation_id: "conv-history-window-test".to_string(),
            role: role.to_string(),
            content: content.to_string(),
            attachments: None,
            metadata,
            tool_call_summaries: None,
            content_blocks: None,
            assistant_turn_duration_seconds: None,
            created_at: i64::from(idx),
            message_index: idx,
        }
    }

    #[test]
    fn history_includes_image_attachment_paths() {
        let attachments = serde_json::json!([
            {
                "filename": "stored-image.jpg",
                "display_name": "screenshot.jpg",
                "path": "/tmp/chat-attachments/conv/stored-image.jpg",
                "mime_type": "image/jpeg",
                "size_bytes": 372713
            }
        ])
        .to_string();
        let message = message_with_attachments("Did you read the screenshot?", Some(attachments));

        let history = build_codex_conversation_history(&[message]);

        assert!(history.contains("Did you read the screenshot?"));
        assert!(history.contains("**Attachments:**"));
        assert!(history.contains("image attachment `screenshot.jpg`"));
        assert!(history.contains("/tmp/chat-attachments/conv/stored-image.jpg"));
        assert!(
            history.contains("inspected before answering questions that depend on visual content")
        );
    }

    #[test]
    fn history_keeps_attachment_only_messages() {
        let attachments = serde_json::json!([
            {
                "filename": "stored-image.png",
                "display_name": "image.png",
                "path": "/tmp/chat-attachments/conv/stored-image.png",
                "mime_type": "image/png",
                "size_bytes": 120
            }
        ])
        .to_string();
        let message = message_with_attachments("", Some(attachments));

        let history = build_codex_conversation_history(&[message]);

        assert!(history.contains("image attachment `image.png`"));
        assert!(history.contains("/tmp/chat-attachments/conv/stored-image.png"));
    }

    #[test]
    fn history_limit_counts_transcript_turns_not_child_completion_cards() {
        let mut messages = Vec::new();
        for idx in 0..31 {
            messages.push(history_message(
                idx,
                if idx % 2 == 0 { "user" } else { "assistant" },
                &format!("turn-{idx:02}"),
                None,
            ));
        }
        for idx in 31..36 {
            let metadata = serde_json::json!({
                "origin": "agent_orchestrated",
                "orchestration": "child_completion_status",
                "child_conversation_id": format!("child-{idx}"),
                "child_title": format!("child-card-{idx}"),
                "child_agent": "full-access",
                "child_terminal_status": "completed"
            })
            .to_string();
            messages.push(history_message(
                idx,
                "assistant",
                &format!("Child agent completed: child-card-{idx}"),
                Some(metadata),
            ));
        }

        let history = build_codex_conversation_history(&messages);

        assert!(!history.contains("turn-00"));
        assert!(history.contains("turn-01"));
        assert!(history.contains("turn-30"));
        assert!(history.contains("child-card-31"));
        assert!(history.contains("child-card-35"));
    }

    #[test]
    fn history_preserves_system_role_label() {
        let message = history_message(
            0,
            "system",
            "This conversation was branched from another thread.",
            None,
        );

        let history = build_codex_conversation_history(&[message]);

        assert!(history.contains("**System**: This conversation was branched from another thread."));
        assert!(!history.contains("**Assistant**: This conversation was branched"));
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

    #[test]
    fn child_completion_status_message_carries_summary_and_open_link() {
        let message = format_child_completion_status_message(
            "Architecture Synthesis",
            "child-1",
            "codebase-research",
            "completed",
            "assistant-1",
            "Use a layered memory model.",
        );

        assert!(message.contains("Child agent completed: Architecture Synthesis"));
        assert!(message.contains("Use a layered memory model."));
        assert!(message.contains("agenticflowstate://conversation/child-1?agent=codebase-research"));
        assert!(message.contains("Child conversation: `child-1`"));
        assert!(message.contains("Assistant message: `assistant-1`"));
        assert!(!message.contains("Coordinator instruction:"));
        assert!(!message.contains("Final output:"));
    }

    #[test]
    fn child_completion_status_metadata_marks_card_payload() {
        let batch_context = ChildBatchContext {
            batch_id: "child-batch-test".to_string(),
            expected_count: 3,
            child_index: Some(2),
        };
        let metadata = child_completion_status_metadata(
            "child-1",
            "Architecture Synthesis",
            "codebase-research",
            "assistant-1",
            "completed",
            "research",
            "Use a layered memory model.",
            Some(&batch_context),
        )
        .expect("serialize status metadata");
        let value: serde_json::Value =
            serde_json::from_str(&metadata).expect("parse relay metadata");

        assert_eq!(value["origin"], "agent_orchestrated");
        assert_eq!(value["orchestration"], "child_completion_status");
        assert_eq!(value["display"], "agent_completion_status_card");
        assert_eq!(value["child_conversation_id"], "child-1");
        assert_eq!(value["child_agent"], "codebase-research");
        assert_eq!(value["child_terminal_status"], "completed");
        assert_eq!(value["child_conversation_type"], "research");
        assert_eq!(value["summary"], "Use a layered memory model.");
        assert_eq!(value["child_batch_id"], "child-batch-test");
        assert_eq!(value["child_batch_size"], 3);
        assert_eq!(value["child_batch_index"], 2);
    }

    #[test]
    fn parent_completion_relay_can_be_suppressed_by_turn_metadata() {
        let metadata = serde_json::json!({
            "origin": "agent_orchestrated",
            "orchestration": "child_initial_turn",
            "agent": "conversation-evaluator",
            "suppress_parent_completion_relay": true
        })
        .to_string();

        assert!(suppress_parent_completion_relay(Some(&metadata)));
        assert!(!suppress_parent_completion_relay(None));
        assert!(!suppress_parent_completion_relay(Some("{not json")));
    }

    #[test]
    fn parent_coordinator_wake_metadata_marks_review_request() {
        let metadata = parent_coordinator_wake_metadata(
            "child-1",
            "Architecture Synthesis",
            "codebase-research",
            "research",
            "completed",
            "assistant-1",
            "status-message-1",
        )
        .expect("serialize wake metadata");
        let value: serde_json::Value =
            serde_json::from_str(&metadata).expect("parse wake metadata");

        assert_eq!(value["origin"], "agent_orchestrated");
        assert_eq!(value["orchestration"], "coordinator_child_completion_wake");
        assert_eq!(value["display"], "coordinator_wake_request");
        assert_eq!(value["child_conversation_id"], "child-1");
        assert_eq!(value["child_agent"], "codebase-research");
        assert_eq!(value["child_terminal_status"], "completed");
        assert_eq!(value["child_completion_message_id"], "status-message-1");
    }

    #[test]
    fn parent_coordinator_batch_wake_metadata_marks_aggregate_review_request() {
        let batch_context = ChildBatchContext {
            batch_id: "child-batch-test".to_string(),
            expected_count: 2,
            child_index: None,
        };
        let completed_children = vec![
            CompletedChildCompletion {
                child_conversation_id: "child-1".to_string(),
                child_title: "Assay lane".to_string(),
                child_agent: "full-access".to_string(),
                child_conversation_type: "research".to_string(),
                terminal_status: "completed".to_string(),
                child_assistant_message_id: Some("assistant-1".to_string()),
                status_message_id: "status-1".to_string(),
            },
            CompletedChildCompletion {
                child_conversation_id: "child-2".to_string(),
                child_title: "Optics lane".to_string(),
                child_agent: "full-access".to_string(),
                child_conversation_type: "research".to_string(),
                terminal_status: "completed".to_string(),
                child_assistant_message_id: Some("assistant-2".to_string()),
                status_message_id: "status-2".to_string(),
            },
        ];

        let message =
            format_parent_coordinator_batch_wake_message(&batch_context, &completed_children);
        assert!(message.contains("all queued child agents in the batch have responded"));
        assert!(message.contains("Completed children: 2/2"));
        assert!(message.contains("Child conversation: `child-1`"));
        assert!(message.contains("Child conversation: `child-2`"));

        let metadata = parent_coordinator_batch_wake_metadata(&batch_context, &completed_children)
            .expect("serialize batch wake metadata");
        let value: serde_json::Value =
            serde_json::from_str(&metadata).expect("parse batch wake metadata");

        assert_eq!(value["origin"], "agent_orchestrated");
        assert_eq!(value["orchestration"], "coordinator_child_completion_wake");
        assert_eq!(value["display"], "coordinator_wake_request");
        assert_eq!(value["child_batch_id"], "child-batch-test");
        assert_eq!(value["child_batch_size"], 2);
        assert_eq!(value["child_completion_count"], 2);
        assert_eq!(
            value["child_conversations"][0]["child_conversation_id"],
            "child-1"
        );
        assert_eq!(
            value["child_conversations"][1]["child_completion_message_id"],
            "status-2"
        );
    }

    #[test]
    fn child_completion_summary_prefers_summary_section() {
        let summary = summarize_child_completion_output(
            "Progress note\n\n### 1. Summary\nCreated artifact `A-12345678` and attached it to the ticket.\n\n### 2. Files\nNone",
            None,
        );

        assert_eq!(
            summary,
            "Created artifact `A-12345678` and attached it to the ticket."
        );
    }
}
