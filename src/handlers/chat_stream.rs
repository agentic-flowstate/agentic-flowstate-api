use async_stream::stream;
use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use chrono::Utc;
use futures::stream::Stream;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use ticketing_system::conversations;
use tokio::process::Command;
use tokio::sync::{broadcast, mpsc, OnceCell, RwLock};
use tokio_stream::wrappers::ReceiverStream;

use super::chat_client_manager::ChatClientManager;
use super::conversation_worker::WorkerMessage;
use super::conversation_worker_manager::WORKER_MANAGER;
use super::sse_keepalive::{wrap_stream_with_keepalive, KeepaliveConfig};
use crate::agents::codex_app_server::{launchd_safe_path, normalize_reasoning_effort};
use crate::agents::{AgentType, AgentsConfig, StreamEvent};
use crate::observability::runtime::{self, RuntimeLatencyPhase};
use crate::observability::streaming::{record_stream_event_emitted, DisconnectReason};
use crate::rate_limiting::{self, RateLimitDecision, StreamPermit};
use crate::request_logger::RequestLogDetail;
use ticketing_system::{agent_runners, checkpoints, conversation_turn_jobs};

/// File data attached to a chat message (base64-encoded).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChatAttachmentData {
    /// Original display filename from the native picker.
    pub filename: String,
    /// Base64-encoded file data.
    pub data: String,
    /// MIME type (e.g., "image/jpeg", "application/pdf").
    pub mime_type: String,
}

const MAX_CHAT_ATTACHMENTS: usize = 8;
const MAX_CHAT_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;
const MAX_CHAT_ATTACHMENTS_TOTAL_BYTES: usize = 64 * 1024 * 1024;

fn validate_chat_attachments(attachments: Option<&[ChatAttachmentData]>) -> Result<usize, String> {
    let Some(attachments) = attachments else {
        return Ok(0);
    };
    if attachments.len() > MAX_CHAT_ATTACHMENTS {
        return Err(format!(
            "Too many attachments: maximum is {}",
            MAX_CHAT_ATTACHMENTS
        ));
    }

    let mut total_bytes = 0usize;
    for attachment in attachments {
        let filename = attachment.filename.trim();
        if filename.is_empty() || filename.contains('/') || filename.contains('\\') {
            return Err("Attachment filename must be a plain file name".to_string());
        }
        if attachment.mime_type.trim().is_empty() {
            return Err(format!("Attachment {} is missing a MIME type", filename));
        }
        let bytes = STANDARD
            .decode(&attachment.data)
            .map_err(|_| format!("Attachment {} is not valid base64", filename))?;
        if bytes.is_empty() {
            return Err(format!("Attachment {} is empty", filename));
        }
        if bytes.len() > MAX_CHAT_ATTACHMENT_BYTES {
            return Err(format!(
                "Attachment {} is too large: maximum is {} MB",
                filename,
                MAX_CHAT_ATTACHMENT_BYTES / 1024 / 1024
            ));
        }
        total_bytes += bytes.len();
        if total_bytes > MAX_CHAT_ATTACHMENTS_TOTAL_BYTES {
            return Err(format!(
                "Combined attachment size is too large: maximum is {} MB",
                MAX_CHAT_ATTACHMENTS_TOTAL_BYTES / 1024 / 1024
            ));
        }
    }
    Ok(attachments.len())
}

const CODEX_REASONING_EFFORT_ORDER: &[&str] = &[
    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
];
const JOB_CODEX_MODEL_KEY: &str = "__agentic_codex_model";
const JOB_CODEX_REASONING_EFFORT_KEY: &str = "__agentic_codex_reasoning_effort";
static CODEX_MODEL_CATALOG: OnceCell<Vec<ChatCodexModelOptionItem>> = OnceCell::const_new();

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChatCodexOptions {
    pub model: String,
    pub reasoning_effort: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatCodexOptionItem {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatCodexModelOptionItem {
    pub id: String,
    pub label: String,
    pub default_reasoning_effort: String,
    pub supported_reasoning_efforts: Vec<ChatCodexOptionItem>,
}

#[derive(Debug, Serialize)]
pub struct ChatCodexOptionsResponse {
    pub agent: String,
    pub default_model: String,
    pub default_reasoning_effort: String,
    pub models: Vec<ChatCodexModelOptionItem>,
    pub reasoning_efforts: Vec<ChatCodexOptionItem>,
}

#[derive(Debug, Deserialize)]
pub struct ChatCodexOptionsQuery {
    pub agent: Option<String>,
}

/// Global broadcaster for live conversation events.
/// Reconnect streams subscribe here instead of polling SQLite.
static CONVERSATION_BROADCASTER: Lazy<RwLock<HashMap<String, broadcast::Sender<(i32, String)>>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

/// Get or create a broadcast sender for a conversation.
pub async fn get_broadcast_sender(conversation_id: &str) -> broadcast::Sender<(i32, String)> {
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
pub async fn remove_broadcast_channel(conversation_id: &str) {
    let mut map = CONVERSATION_BROADCASTER.write().await;
    map.remove(conversation_id);
}

pub type SseStream = Sse<Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChatRuntime {
    CodexAppServer,
}

impl ChatRuntime {
    pub fn as_job_runtime(self) -> &'static str {
        match self {
            Self::CodexAppServer => "codex-app-server",
        }
    }
}

/// Configuration for a chat SSE endpoint
#[derive(Clone)]
pub struct ChatConfig {
    pub agent_type: AgentType,
    pub runtime: ChatRuntime,
    pub prompt_name: &'static str,
    pub working_dir: PathBuf,
    pub prompt_vars: HashMap<String, String>,
    pub codex_options: ChatCodexOptions,
}

impl ChatCodexOptions {
    pub fn default_for_agent(agent_type: &AgentType) -> Self {
        Self {
            model: agent_type.model().to_string(),
            reasoning_effort: normalize_reasoning_effort(agent_type.effort()).to_string(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct CodexDebugModelsResponse {
    models: Vec<CodexDebugModel>,
}

#[derive(Debug, Deserialize)]
struct CodexDebugModel {
    slug: String,
    display_name: String,
    default_reasoning_level: String,
    supported_reasoning_levels: Vec<CodexDebugReasoningLevel>,
    visibility: String,
}

#[derive(Debug, Deserialize)]
struct CodexDebugReasoningLevel {
    effort: String,
}

fn codex_reasoning_effort_item(effort: &str) -> ChatCodexOptionItem {
    ChatCodexOptionItem {
        id: effort.to_string(),
        label: effort.to_uppercase(),
    }
}

fn ordered_codex_reasoning_efforts(efforts: impl IntoIterator<Item = String>) -> Vec<String> {
    let available: std::collections::HashSet<String> = efforts.into_iter().collect();
    CODEX_REASONING_EFFORT_ORDER
        .iter()
        .filter_map(|effort| available.contains(*effort).then(|| (*effort).to_string()))
        .collect()
}

async fn load_codex_model_catalog() -> Result<Vec<ChatCodexModelOptionItem>, String> {
    let output = Command::new("codex")
        .args(["debug", "models"])
        .env("PATH", launchd_safe_path())
        .output()
        .await
        .map_err(|e| format!("Failed to read Codex model catalog: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!(
                "Failed to read Codex model catalog: codex debug models exited with {}",
                output.status
            )
        } else {
            format!("Failed to read Codex model catalog: {stderr}")
        });
    }

    let response: CodexDebugModelsResponse = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("Failed to parse Codex model catalog: {e}"))?;
    let models: Vec<ChatCodexModelOptionItem> = response
        .models
        .into_iter()
        .filter(|model| model.visibility == "list")
        .filter_map(|model| {
            let id = model.slug.trim().to_string();
            if id.is_empty() {
                return None;
            }
            let supported = ordered_codex_reasoning_efforts(
                model
                    .supported_reasoning_levels
                    .into_iter()
                    .map(|level| normalize_reasoning_effort(&level.effort).to_string()),
            );
            if supported.is_empty() {
                return None;
            }
            let default = normalize_reasoning_effort(&model.default_reasoning_level).to_string();
            let default_reasoning_effort = if supported.contains(&default) {
                default
            } else {
                supported[0].clone()
            };

            Some(ChatCodexModelOptionItem {
                id,
                label: if model.display_name.trim().is_empty() {
                    model.slug
                } else {
                    model.display_name.trim().to_string()
                },
                default_reasoning_effort,
                supported_reasoning_efforts: supported
                    .iter()
                    .map(|effort| codex_reasoning_effort_item(effort))
                    .collect(),
            })
        })
        .collect();

    if models.is_empty() {
        return Err("Codex model catalog did not expose any selectable models".to_string());
    }

    Ok(models)
}

async fn codex_model_catalog() -> Result<Vec<ChatCodexModelOptionItem>, String> {
    CODEX_MODEL_CATALOG
        .get_or_try_init(load_codex_model_catalog)
        .await
        .cloned()
}

fn codex_options_error_response(
    status: StatusCode,
    error: String,
    detail: serde_json::Value,
) -> Response {
    let mut response = (status, Json(json!({ "error": error }))).into_response();
    response
        .extensions_mut()
        .insert(RequestLogDetail(detail.to_string()));
    response
}

fn resolve_catalog_model<'a>(
    catalog: &'a [ChatCodexModelOptionItem],
    model: &str,
) -> Option<&'a ChatCodexModelOptionItem> {
    let resolved_model = AgentsConfig::get().resolve_model(model).to_string();
    catalog.iter().find(|option| option.id == resolved_model)
}

pub(crate) async fn validate_codex_options(
    agent_type: &AgentType,
    requested: Option<ChatCodexOptions>,
) -> Result<ChatCodexOptions, Response> {
    let catalog = codex_model_catalog().await.map_err(|e| {
        tracing::error!(
            target: "agentic_api::chat_codex_options",
            agent = agent_type.as_str(),
            error = %e,
            "failed to load Codex model catalog while validating turn options"
        );
        codex_options_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            e.clone(),
            json!({
                "reason": "codex_model_catalog_unavailable",
                "operation": "validate_codex_options",
                "agent": agent_type.as_str(),
                "error": e,
            }),
        )
    })?;

    let Some(requested) = requested else {
        return Ok(ChatCodexOptions::default_for_agent(agent_type));
    };

    let model = requested.model.trim();
    let Some(model_option) = resolve_catalog_model(&catalog, model) else {
        return Err(codex_options_error_response(
            StatusCode::BAD_REQUEST,
            format!("Unsupported Codex model: {}", requested.model),
            json!({
                "reason": "unsupported_codex_model",
                "operation": "validate_codex_options",
                "agent": agent_type.as_str(),
                "requested_model": requested.model,
            }),
        ));
    };

    let effort = requested.reasoning_effort.trim().to_lowercase();
    if !model_option
        .supported_reasoning_efforts
        .iter()
        .any(|option| option.id == effort)
    {
        return Err(codex_options_error_response(
            StatusCode::BAD_REQUEST,
            format!(
                "Unsupported Codex reasoning effort: {}",
                requested.reasoning_effort
            ),
            json!({
                "reason": "unsupported_codex_reasoning_effort",
                "operation": "validate_codex_options",
                "agent": agent_type.as_str(),
                "model": model_option.id.as_str(),
                "requested_reasoning_effort": requested.reasoning_effort,
                "supported_reasoning_efforts": model_option
                    .supported_reasoning_efforts
                    .iter()
                    .map(|option| option.id.as_str())
                    .collect::<Vec<_>>(),
            }),
        ));
    }

    Ok(ChatCodexOptions {
        model: model_option.id.clone(),
        reasoning_effort: effort,
    })
}

pub async fn apply_codex_options(
    mut config: ChatConfig,
    requested: Option<ChatCodexOptions>,
) -> Result<ChatConfig, Response> {
    config.codex_options = validate_codex_options(&config.agent_type, requested).await?;
    Ok(config)
}

pub fn encode_codex_options_for_job(
    mut prompt_vars: HashMap<String, String>,
    options: &ChatCodexOptions,
) -> HashMap<String, String> {
    prompt_vars.insert(JOB_CODEX_MODEL_KEY.to_string(), options.model.clone());
    prompt_vars.insert(
        JOB_CODEX_REASONING_EFFORT_KEY.to_string(),
        options.reasoning_effort.clone(),
    );
    prompt_vars
}

pub fn take_codex_options_from_job(
    agent_type: &AgentType,
    prompt_vars: &mut HashMap<String, String>,
) -> ChatCodexOptions {
    let model = prompt_vars.remove(JOB_CODEX_MODEL_KEY);
    let reasoning_effort = prompt_vars.remove(JOB_CODEX_REASONING_EFFORT_KEY);
    match (model, reasoning_effort) {
        (Some(model), Some(reasoning_effort)) => ChatCodexOptions {
            model,
            reasoning_effort,
        },
        _ => ChatCodexOptions::default_for_agent(agent_type),
    }
}

pub async fn codex_chat_options(
    Query(params): Query<ChatCodexOptionsQuery>,
) -> Result<Json<ChatCodexOptionsResponse>, Response> {
    let agent_key = params.agent.as_deref().unwrap_or("full-access");
    let Some(agent_type) = AgentType::from_chat_agent_key(agent_key) else {
        return Err(codex_options_error_response(
            StatusCode::BAD_REQUEST,
            format!("Unsupported chat agent: {}", agent_key),
            json!({
                "reason": "unsupported_chat_agent",
                "operation": "codex_chat_options",
                "requested_agent": agent_key,
            }),
        ));
    };

    let catalog = codex_model_catalog().await.map_err(|e| {
        tracing::error!(
            target: "agentic_api::chat_codex_options",
            requested_agent = agent_key,
            agent = agent_type.as_str(),
            error = %e,
            "failed to load Codex model catalog"
        );
        codex_options_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            e.clone(),
            json!({
                "reason": "codex_model_catalog_unavailable",
                "operation": "codex_chat_options",
                "requested_agent": agent_key,
                "agent": agent_type.as_str(),
                "error": e,
            }),
        )
    })?;
    let defaults = ChatCodexOptions::default_for_agent(&agent_type);
    let default_model = resolve_catalog_model(&catalog, &defaults.model).ok_or_else(|| {
        let available_models = catalog
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>();
        tracing::error!(
            target: "agentic_api::chat_codex_options",
            requested_agent = agent_key,
            agent = agent_type.as_str(),
            configured_model = %defaults.model,
            ?available_models,
            "configured Codex model is not available in the runtime catalog"
        );
        codex_options_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "Configured Codex model is not available: {}",
                defaults.model
            ),
            json!({
                "reason": "configured_codex_model_unavailable",
                "operation": "codex_chat_options",
                "requested_agent": agent_key,
                "agent": agent_type.as_str(),
                "configured_model": defaults.model,
                "available_models": available_models,
            }),
        )
    })?;
    let default_reasoning_effort = if default_model
        .supported_reasoning_efforts
        .iter()
        .any(|option| option.id == defaults.reasoning_effort)
    {
        defaults.reasoning_effort
    } else {
        default_model.default_reasoning_effort.clone()
    };

    let reasoning_efforts = CODEX_REASONING_EFFORT_ORDER
        .iter()
        .filter(|effort| {
            catalog.iter().any(|model| {
                model
                    .supported_reasoning_efforts
                    .iter()
                    .any(|item| item.id == **effort)
            })
        })
        .map(|effort| codex_reasoning_effort_item(effort))
        .collect();

    Ok(Json(ChatCodexOptionsResponse {
        agent: agent_type.as_str().to_string(),
        default_model: default_model.id.clone(),
        default_reasoning_effort,
        models: catalog,
        reasoning_efforts,
    }))
}

#[cfg(test)]
mod codex_options_error_tests {
    use super::*;

    #[test]
    fn error_response_carries_request_log_detail() {
        let response = codex_options_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "catalog failed".to_string(),
            json!({
                "reason": "codex_model_catalog_unavailable",
                "agent": "full-access",
            }),
        );

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let detail = response
            .extensions()
            .get::<RequestLogDetail>()
            .expect("request log detail extension");
        assert!(detail
            .0
            .contains("\"reason\":\"codex_model_catalog_unavailable\""));
        assert!(detail.0.contains("\"agent\":\"full-access\""));
    }
}

#[derive(Debug, Serialize)]
pub struct ChatSubmitResponse {
    pub conversation_id: String,
    pub status: &'static str,
    pub job_id: String,
    pub accepted_at_ms: i64,
}

async fn has_existing_active_conversation_turn(
    db: &SqlitePool,
    manager: &ChatClientManager,
    conversation_id: &str,
) -> bool {
    if manager.has_runtime_turn(conversation_id).await {
        return true;
    }
    if WORKER_MANAGER.has_worker(conversation_id).await {
        return true;
    }
    if agent_runners::has_active_turn_for_conversation(db, conversation_id)
        .await
        .unwrap_or(false)
    {
        return true;
    }
    if conversation_turn_jobs::has_active_job_for_conversation(db, conversation_id)
        .await
        .unwrap_or(false)
    {
        return true;
    }
    checkpoints::get_checkpoint(db, conversation_id)
        .await
        .ok()
        .flatten()
        .map(|checkpoint| matches!(checkpoint.status.as_str(), "queued" | "pending" | "running"))
        .unwrap_or(false)
}

async fn authorize_conversation_turn(
    db: &SqlitePool,
    conversation_id: &str,
    user_id: &str,
    agent_type: &AgentType,
) -> Result<(), Response> {
    match conversations::get_conversation(db, conversation_id, false).await {
        Ok(Some(conv)) if conv.user_id == user_id => {
            crate::codex_coordinator::validate_runtime_assignment(
                &conv,
                *agent_type == AgentType::CodexCoordinator,
            )
            .map_err(|error| (StatusCode::CONFLICT, error.to_string()).into_response())?;

            if conv.conversation_role == "sub_agent"
                && !worker_profile_allows_follow_up(conv.agent.as_deref())
            {
                let prior_turn_count: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM conversation_turn_jobs WHERE conversation_id = ?",
                )
                .bind(&conv.id)
                .fetch_one(db)
                .await
                .map_err(|error| {
                    tracing::error!(
                        conversation_id = conv.id.as_str(),
                        error = %error,
                        "failed to enforce one-shot worker turn policy"
                    );
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Failed to enforce worker turn policy".to_string(),
                    )
                        .into_response()
                })?;
                if prior_turn_count > 0 {
                    return Err((
                        StatusCode::CONFLICT,
                        "This worker profile is one-shot. Queue a fresh coordinator-managed worker instead of continuing this conversation."
                            .to_string(),
                    )
                        .into_response());
                }
            }

            Ok(())
        }
        Ok(Some(_)) | Ok(None) => {
            Err((StatusCode::NOT_FOUND, "Conversation not found".to_string()).into_response())
        }
        Err(e) => {
            tracing::error!(
                "[CHAT] Failed to authorize conversation {} for user {}: {}",
                conversation_id,
                user_id,
                e
            );
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to authorize conversation".to_string(),
            )
                .into_response())
        }
    }
}

fn worker_profile_allows_follow_up(agent: Option<&str>) -> bool {
    matches!(agent, Some("research" | "codebase-research"))
}

/// Submit a chat turn to the conversation worker and return immediately.
///
/// This is the non-streaming chat path used by the native app. The worker
/// already persists every generated event to `conversation_events` and keeps
/// `messages` updated as the turn progresses; clients catch up through the
/// JSON event/page and message endpoints instead of holding an SSE response
/// open.
pub async fn submit(
    db: Arc<SqlitePool>,
    manager: Arc<ChatClientManager>,
    message: String,
    conversation_id: Option<String>,
    config: ChatConfig,
    user_id: String,
    attachments: Option<Vec<ChatAttachmentData>>,
    client_id: Option<String>,
) -> Response {
    let received_at_ms = Utc::now().timestamp_millis();
    let conv_id = match conversation_id {
        Some(id) => id,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                "conversation_id is required for non-streaming chat submit",
            )
                .into_response();
        }
    };
    let attachment_count = match validate_chat_attachments(attachments.as_deref()) {
        Ok(count) => count,
        Err(message) => return (StatusCode::BAD_REQUEST, message).into_response(),
    };
    runtime::record_latency_marker(
        &conv_id,
        client_id.as_deref(),
        RuntimeLatencyPhase::SubmitReceived,
        0,
        received_at_ms,
        None,
        None,
    );
    tracing::info!(
        target: "agentic_api::runtime",
        event = "chat_submit.received",
        conversation_id = %conv_id,
        client_id = client_id.as_deref().unwrap_or("none"),
        agent_name = config.agent_type.as_str(),
        runtime = config.runtime.as_job_runtime(),
        model = %config.codex_options.model,
        reasoning_effort = %config.codex_options.reasoning_effort,
        message_chars = message.chars().count(),
        attachment_count,
        received_at_ms,
        "chat submit received"
    );

    if let Err(response) =
        authorize_conversation_turn(&db, &conv_id, &user_id, &config.agent_type).await
    {
        return response;
    }

    let queued_behind_existing_turn =
        has_existing_active_conversation_turn(&db, &manager, &conv_id).await;

    if let Err(e) = checkpoints::upsert_checkpoint(&db, &conv_id, "queued", 0).await {
        tracing::error!(
            "[CHAT] Failed to create run status for non-streaming submit {}: {}",
            conv_id,
            e
        );
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to create conversation run status: {}", e),
        )
            .into_response();
    }

    let attachments_json = match attachments {
        Some(attachments) => match serde_json::to_string(&attachments) {
            Ok(json) => Some(json),
            Err(e) => {
                tracing::error!(
                    "[CHAT] Failed to serialize attachments for non-streaming submit {}: {}",
                    conv_id,
                    e
                );
                return (
                    StatusCode::BAD_REQUEST,
                    format!("Failed to serialize chat attachments: {}", e),
                )
                    .into_response();
            }
        },
        None => None,
    };
    let client_id_for_log = client_id.clone();

    let payload = conversation_turn_jobs::ConversationTurnJobPayload {
        user_id,
        message,
        agent_type: config.agent_type.as_str().to_string(),
        runtime: config.runtime.as_job_runtime().to_string(),
        prompt_name: config.prompt_name.to_string(),
        working_dir: config.working_dir.to_string_lossy().to_string(),
        prompt_vars: encode_codex_options_for_job(config.prompt_vars, &config.codex_options),
        images_json: attachments_json,
        client_id,
        message_metadata: None,
    };

    let job_id = match conversation_turn_jobs::enqueue_job(&db, &conv_id, payload).await {
        Ok(job_id) => job_id,
        Err(e) => {
            tracing::error!(
                "[CHAT] Failed to enqueue durable non-streaming submit {}: {}",
                conv_id,
                e
            );
            if let Err(e) = checkpoints::mark_interrupted(&db, &conv_id).await {
                tracing::warn!(
                    "[CHAT] Failed to mark run status interrupted after enqueue failure {}: {}",
                    conv_id,
                    e
                );
            }
            if let Err(e) =
                super::conversations::publish_conversation_run_status(&db, &conv_id).await
            {
                tracing::warn!(
                    "[CHAT] Failed to publish interrupted run status after enqueue failure {}: {}",
                    conv_id,
                    e
                );
            }
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "Runner queue unavailable for conversation",
            )
                .into_response();
        }
    };
    let accepted_at_ms = Utc::now().timestamp_millis();
    let submit_duration_ms = accepted_at_ms.saturating_sub(received_at_ms);
    runtime::record_latency_marker(
        &conv_id,
        client_id_for_log.as_deref(),
        RuntimeLatencyPhase::SubmitEnqueued,
        submit_duration_ms as u64,
        accepted_at_ms,
        None,
        None,
    );
    tracing::info!(
        target: "agentic_api::runtime",
        event = "chat_submit.enqueued",
        conversation_id = %conv_id,
        client_id = client_id_for_log.as_deref().unwrap_or("none"),
        job_id = %job_id,
        received_at_ms,
        accepted_at_ms,
        submit_duration_ms,
        "chat submit enqueued"
    );
    if let Err(e) = super::conversations::publish_conversation_run_status(&db, &conv_id).await {
        tracing::warn!(
            "[CHAT] Failed to publish run status for non-streaming submit {}: {}",
            conv_id,
            e
        );
    }

    (
        StatusCode::ACCEPTED,
        Json(ChatSubmitResponse {
            conversation_id: conv_id,
            status: if queued_behind_existing_turn {
                "queued"
            } else {
                "running"
            },
            job_id,
            accepted_at_ms,
        }),
    )
        .into_response()
}

/// Forward worker/broadcast events into the per-request SSE channel.
///
/// The returned future owns the generate-stream permit indirectly via the
/// surrounding task in [`chat`]. It therefore MUST exit as soon as the HTTP
/// client disconnects; otherwise the rate limiter keeps the old POST /chat
/// slot occupied and the next send on the same conversation gets a bogus 429.
async fn forward_worker_events_to_http(
    tx: mpsc::Sender<(i32, String)>,
    mut broadcast_rx: broadcast::Receiver<(i32, String)>,
    mut completion_rx: tokio::sync::oneshot::Receiver<()>,
    conv_id: String,
) {
    let timeout = tokio::time::sleep(Duration::from_secs(600));
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            _ = tx.closed() => {
                tracing::info!(
                    "[CHAT] SSE receiver dropped for {}, closing forwarder",
                    conv_id
                );
                break;
            }
            result = broadcast_rx.recv() => {
                match result {
                    Ok((index, json)) => {
                        timeout.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(600));
                        if tx.send((index, json)).await.is_err() {
                            tracing::info!(
                                "[CHAT] SSE send failed for {}, closing forwarder",
                                conv_id
                            );
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // Worker ended — broadcast channel dropped
                        break;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("[CHAT] SSE lagged by {} events for {}, catching up", n, conv_id);
                        // Continue — broadcast will deliver the next available event
                    }
                }
            }
            _ = &mut completion_rx => {
                // Worker signaled THIS message is done — drain remaining events
                while let Ok((index, json)) = broadcast_rx.try_recv() {
                    if tx.send((index, json)).await.is_err() {
                        tracing::info!(
                            "[CHAT] SSE send failed while draining {}, closing forwarder",
                            conv_id
                        );
                        break;
                    }
                }
                break;
            }
            _ = &mut timeout => {
                tracing::warn!("[CHAT] SSE forwarding timed out for {}", conv_id);
                break;
            }
        }
    }
}

/// Start or continue a chat session via SSE.
///
/// Pushes the message to a ConversationWorker (one per conversation) which
/// processes messages sequentially, eliminating the dual-consumer race.
/// Returns either an SSE stream (on admission) or a 429 JSON response
/// (when the per-(user, conversation) rate limiter denies the open).
///
/// Rate-limit admission (T-C410DD96): when a `conversation_id` is supplied
/// the function consults the process-wide [`rate_limiting::StreamRateLimiter`]
/// (installed in `main.rs`) before opening the stream. On
/// [`RateLimitDecision::Deny`] we short-circuit with the contract 429
/// response. On [`RateLimitDecision::Allow`] the permit is moved into the
/// spawned broadcast-forwarder task and dropped when the loop exits —
/// guaranteeing the slot releases on natural EOS, client disconnect,
/// worker timeout, or a panic.
///
/// When the limiter is not installed (unit tests that never called
/// `install_global`) the rate check is skipped and every open is admitted.
/// When no `conversation_id` is supplied the rate check is skipped too:
/// the underlying handler will emit a `status: failed` frame in that case
/// and the stream closes immediately.
pub async fn chat(
    db: Arc<SqlitePool>,
    manager: Arc<ChatClientManager>,
    message: String,
    conversation_id: Option<String>,
    config: ChatConfig,
    user_id: String,
    attachments: Option<Vec<ChatAttachmentData>>,
    client_id: Option<String>,
) -> Response {
    // ---- Rate-limit admission (T-C410DD96). ----
    //
    // We evaluate the limiter BEFORE spawning the worker or opening the
    // mpsc channel. A denied request must cost the server only one
    // DashMap lookup — not a worker spawn, not a broadcast subscription,
    // not a new SSE connection accepted by axum. Skip the check when
    // either:
    //   * the limiter is uninstalled (unit tests), OR
    //   * no conversation_id is supplied (the bucket key would be
    //     meaningless; the handler below will emit `status: failed`
    //     and exit cleanly anyway).
    let permit: Option<StreamPermit> = if let (Some(conv_id), Some(limiter)) =
        (conversation_id.as_ref(), rate_limiting::global())
    {
        // T-1BEAA41E: `StreamKind::Generate` so POST /chat does NOT
        // contend with the client's durable GET /events reader on the
        // same conversation. Before this split a legitimate client that
        // was already tailing events via resume_stream.rs would have its
        // next POST 429'd with `concurrent_stream_limit` — which is
        // exactly the "stuck on Connecting..." bug that motivated the
        // ticket.
        match limiter.check(&user_id, conv_id, rate_limiting::StreamKind::Generate) {
            RateLimitDecision::Allow(permit) => Some(permit),
            RateLimitDecision::Deny {
                retry_after,
                reason,
            } => {
                return rate_limited_response(retry_after, reason);
            }
        }
    } else {
        None
    };

    let (tx, rx) = mpsc::channel::<(i32, String)>(100);

    // Capture the conversation id early so the SSE wrapper can attribute
    // keepalives/closes even on the malformed-request path below.
    let wrapper_conv_id = conversation_id
        .clone()
        .unwrap_or_else(|| "unknown".to_string());

    let conv_id = match conversation_id {
        Some(id) => id,
        None => {
            tracing::error!("[CHAT] No conversation_id provided");
            if let Ok(json) = serde_json::to_string(&StreamEvent::Status {
                status: "failed".to_string(),
                message: Some("No conversation_id".to_string()),
            }) {
                let _ = tx.send((0, json)).await;
            }
            return create_sse_stream_raw(rx, wrapper_conv_id).into_response();
        }
    };

    if let Err(response) =
        authorize_conversation_turn(&db, &conv_id, &user_id, &config.agent_type).await
    {
        return response;
    }
    if let Err(message) = validate_chat_attachments(attachments.as_deref()) {
        return (StatusCode::BAD_REQUEST, message).into_response();
    }

    // Subscribe before enqueue so we do not miss the worker's first frames.
    let broadcast_tx = get_broadcast_sender(&conv_id).await;
    let broadcast_rx = broadcast_tx.subscribe();
    drop(broadcast_tx);

    // T-2AE66881/T-DCA37215: enqueue the worker turn before returning the
    // HTTP response so an immediate Stop can already see a live worker
    // instead of racing the detached setup task.
    let (completion_tx, completion_rx) = tokio::sync::oneshot::channel::<()>();
    let worker_tx = WORKER_MANAGER
        .get_or_create(conv_id.clone(), db, manager)
        .await;

    if worker_tx
        .send(WorkerMessage {
            user_id,
            message,
            config,
            attachments,
            completion_tx: Some(completion_tx),
            client_id,
            message_metadata: None,
        })
        .await
        .is_err()
    {
        tracing::error!("[CHAT] Worker channel closed for {}", conv_id);
        if let Ok(json) = serde_json::to_string(&StreamEvent::Status {
            status: "failed".to_string(),
            message: Some("Worker unavailable".to_string()),
        }) {
            let _ = tx.send((0, json)).await;
        }
        return create_sse_stream_raw(rx, wrapper_conv_id).into_response();
    }

    tokio::spawn(async move {
        // Hold the generate-stream permit until the forwarder exits. Dropping
        // it earlier would reopen the bucket while this SSE is still live.
        let _rate_limit_permit = permit;
        forward_worker_events_to_http(tx, broadcast_rx, completion_rx, conv_id).await;
    });

    create_sse_stream_raw(rx, wrapper_conv_id).into_response()
}

/// Extract and validate an `Idempotency-Key` header (T-A819D36B).
///
/// The iOS client sets this header to the per-outbox-row UUID it uses
/// locally to key optimistic-echo rows. The server echoes it back in
/// the `message_start` SSE frame's `client_id` field so
/// `MessageEchoService.reconcileServerMessageStart` can promote the
/// local echo to server-confirmed.
///
/// Validation rules:
///
/// * Absent header → `Ok(None)`. This is the expected path for web
///   clients and any call that doesn't participate in the optimistic-echo
///   protocol. NO fallback / NO synthetic value — `None` propagates
///   end-to-end so the emitted frame simply omits `client_id`.
/// * Non-ASCII bytes → `Err(IdempotencyKeyError::Malformed)`. Header
///   values are ASCII-only per RFC 7230; any other bytes are a client
///   bug worth surfacing as 400.
/// * Length > 128 chars → `Err(IdempotencyKeyError::Malformed)`. UUIDs
///   are 36 chars; 128 is a generous ceiling that still stops
///   accidental body content leaking into the header slot.
/// * Non-printable characters (below 0x20 or DEL 0x7F) →
///   `Err(IdempotencyKeyError::Malformed)`. Matches the iOS-side UUID
///   alphabet plus common punctuation without opening up to framing
///   attacks.
/// * Empty string → `Err(IdempotencyKeyError::Malformed)`. An empty key
///   is semantically meaningless and almost certainly a client bug.
///
/// The header name check is case-insensitive per `HeaderMap` semantics.
pub fn extract_client_id(
    headers: &axum::http::HeaderMap,
) -> Result<Option<String>, IdempotencyKeyError> {
    let Some(raw) = headers.get("Idempotency-Key") else {
        return Ok(None);
    };
    let s = raw
        .to_str()
        .map_err(|_| IdempotencyKeyError::Malformed("non-ASCII bytes"))?;
    if s.is_empty() {
        return Err(IdempotencyKeyError::Malformed("empty value"));
    }
    if s.len() > 128 {
        return Err(IdempotencyKeyError::Malformed("exceeds 128 chars"));
    }
    if s.chars().any(|c| (c as u32) < 0x20 || (c as u32) == 0x7F) {
        return Err(IdempotencyKeyError::Malformed(
            "contains non-printable characters",
        ));
    }
    Ok(Some(s.to_string()))
}

/// Error returned by [`extract_client_id`] when the `Idempotency-Key`
/// header is present but violates the validation rules. Handlers map
/// this to a 400 Bad Request response with a short diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdempotencyKeyError {
    Malformed(&'static str),
}

impl IdempotencyKeyError {
    pub fn detail(&self) -> &'static str {
        match self {
            IdempotencyKeyError::Malformed(d) => d,
        }
    }
}

/// Build a 400 response for a malformed `Idempotency-Key` header.
/// Shared helper so every chat handler returns the same shape.
pub fn malformed_idempotency_key_response(err: IdempotencyKeyError) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": "malformed_idempotency_key",
            "detail": err.detail(),
        })),
    )
        .into_response()
}

/// Build the 429 response emitted when the per-(user, conversation) rate
/// limiter denies admission for a chat open.
///
/// Mirrors the contract used by [`super::resume_stream::rate_limited_response`]:
///
///   * Status: 429
///   * Header: `Retry-After: <seconds>` — rounded up, minimum 1.
///   * Body:   `{"error":"rate_limited","reason":"...","retry_after_seconds":N}`
///
/// Kept local to this module (not pub-use'd from resume_stream) so that
/// each SSE entry point can tweak its wire shape independently if the
/// product ever needs to differentiate them. Today they are byte-for-byte
/// identical.
fn rate_limited_response(
    retry_after: Duration,
    reason: crate::rate_limiting::DenyReason,
) -> Response {
    let seconds = retry_after
        .as_millis()
        .div_ceil(1_000)
        .try_into()
        .unwrap_or(u64::MAX)
        .max(1);

    let body = json!({
        "error": "rate_limited",
        "reason": reason.as_wire_str(),
        "retry_after_seconds": seconds,
    });

    let mut resp = (StatusCode::TOO_MANY_REQUESTS, Json(body)).into_response();
    if let Ok(val) = axum::http::HeaderValue::from_str(&seconds.to_string()) {
        resp.headers_mut()
            .insert(axum::http::header::RETRY_AFTER, val);
    }
    resp
}

/// Create SSE stream from raw (index, json) broadcast tuples.
///
/// The inner event stream is wrapped with the production-hardening
/// keepalive layer (T-CFFAF032): conversation-protocol `ping` frames
/// every 15s (cadence resets on every real event), a 10-minute idle
/// timeout that emits a terminal `message:end reason:server_idle_timeout`
/// frame, and a graceful-shutdown hook that emits
/// `message:end reason:server_shutdown` when the process receives
/// SIGTERM. The former in-tree `KeepAlive::new().text("ping")` helper
/// emitted comment-style pings which our iOS reader ignores — we
/// switched to the provider-neutral named-event form so idle
/// tracking works end-to-end.
fn create_sse_stream_raw(rx: mpsc::Receiver<(i32, String)>, conversation_id: String) -> SseStream {
    let stream = stream! {
        let mut rx = ReceiverStream::new(rx);
        while let Some((index, json)) = futures::StreamExt::next(&mut rx).await {
            yield Ok(Event::default().id(index.to_string()).data(json));
        }
    };
    let inner: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> = Box::pin(stream);
    let wrapped = wrap_stream_with_keepalive(inner, KeepaliveConfig::from_env(), conversation_id);
    // axum's built-in KeepAlive helper is disabled here (interval=1h) —
    // we own the ping cadence via wrap_stream_with_keepalive so the
    // frames are conversation-protocol `ping` events, not `: ping\n\n`
    // comments.
    Sse::new(Box::pin(wrapped) as Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(3600)))
}

#[cfg(test)]
mod tests {
    use super::forward_worker_events_to_http;
    use tokio::sync::{broadcast, mpsc, oneshot};
    use tokio::time::{timeout, Duration};

    #[tokio::test]
    async fn forwarder_exits_when_http_client_disconnects() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);

        let (_broadcast_tx, broadcast_rx) = broadcast::channel(8);
        let (_completion_tx, completion_rx) = oneshot::channel();

        let task = tokio::spawn(forward_worker_events_to_http(
            tx,
            broadcast_rx,
            completion_rx,
            "conv-stop-429".to_string(),
        ));

        timeout(Duration::from_secs(1), task)
            .await
            .expect("forwarder should exit promptly once the SSE receiver drops")
            .expect("forwarder task should not panic");
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

        // Phase 1: Replay stored events.
        // load_event_payload_str materializes blob-offloaded payloads
        // (T-E184E642 / v46 event_blobs table) back into raw JSON. Inline
        // events round-trip as a cheap clone. Callers must NEVER ship
        // `event.event_data` directly — for offloaded events that value
        // is the `{"$blob":...}` sentinel, which would break SSE parsers.
        for db_event in &events {
            event_count += 1;
            last_event_index = db_event.event_index;
            let payload = match conversations::load_event_payload_str(&db, db_event).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(
                        "[RECONNECT] Failed to materialize payload for event {}/{}: {}",
                        db_event.conversation_id, db_event.event_index, e
                    );
                    continue;
                }
            };
            record_stream_event_emitted(&conversation_id, payload.len());
            yield Ok(Event::default()
                .id(db_event.event_index.to_string())
                .data(payload));
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
        if matches!(checkpoint_status.as_str(), "running" | "pending" | "queued") {
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
                                    record_stream_event_emitted(&conversation_id, event_data.len());
                                    yield Ok(Event::default()
                                        .id(event_index.to_string())
                                        .data(event_data));
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(n)) => {
                                tracing::warn!("[RECONNECT] Broadcast lagged by {} events for {}, falling back to DB", n, conversation_id);
                                // Catch up from DB — materialize blob payloads.
                                if let Ok(missed) = conversations::get_events_after(&db, &conversation_id, last_event_index).await {
                                    for ev in &missed {
                                        last_event_index = ev.event_index;
                                        let payload = match conversations::load_event_payload_str(&db, ev).await {
                                            Ok(s) => s,
                                            Err(e) => {
                                                tracing::error!(
                                                    "[RECONNECT] Failed to materialize payload for event {}/{}: {}",
                                                    ev.conversation_id, ev.event_index, e
                                                );
                                                continue;
                                            }
                                        };
                                        record_stream_event_emitted(&conversation_id, payload.len());
                                        yield Ok(Event::default()
                                            .id(ev.event_index.to_string())
                                            .data(payload));
                                    }
                                }
                            }
                            Err(broadcast::error::RecvError::Closed) => {
                                // Persister finished — agent is done.
                                // Fetch any remaining events from DB and materialize blob payloads.
                                if let Ok(final_events) = conversations::get_events_after(&db, &conversation_id, last_event_index).await {
                                    for ev in &final_events {
                                        let payload = match conversations::load_event_payload_str(&db, ev).await {
                                            Ok(s) => s,
                                            Err(e) => {
                                                tracing::error!(
                                                    "[RECONNECT] Failed to materialize payload for event {}/{}: {}",
                                                    ev.conversation_id, ev.event_index, e
                                                );
                                                continue;
                                            }
                                        };
                                        record_stream_event_emitted(&conversation_id, payload.len());
                                        yield Ok(Event::default()
                                            .id(ev.event_index.to_string())
                                            .data(payload));
                                    }
                                }
                                let done = StreamEvent::Status {
                                    status: "completed".to_string(),
                                    message: None,
                                };
                                if let Ok(json) = serde_json::to_string(&done) {
                                    yield Ok(Event::default().data(json));
                                }
                                // Yielded close reason: worker finished cleanly.
                                let _ = DisconnectReason::Normal;
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
                        // Yielded close reason: idle timeout.
                        let _ = DisconnectReason::ServerIdleTimeout;
                        break;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod conversation_authorization_tests {
    use super::*;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use sqlx::ConnectOptions;
    use std::str::FromStr;

    async fn fresh_pool() -> SqlitePool {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")
            .expect("parse sqlite url")
            .foreign_keys(true)
            .disable_statement_logging();
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("connect sqlite");

        sqlx::query(
            r#"
            CREATE TABLE conversations (
                id TEXT PRIMARY KEY,
                user_id TEXT NOT NULL,
                session_id TEXT,
                organization TEXT NOT NULL,
                agent TEXT,
                conversation_type TEXT,
                parent_conversation_id TEXT,
                conversation_role TEXT NOT NULL DEFAULT 'standard',
                child_conversation_count INTEGER NOT NULL DEFAULT 0,
                child_sort_order INTEGER,
                title TEXT NOT NULL,
                started_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
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
            CREATE TABLE conversation_turn_jobs (
                id TEXT PRIMARY KEY,
                conversation_id TEXT NOT NULL
            )
            "#,
        )
        .execute(&pool)
        .await
        .expect("create conversation turn jobs");

        sqlx::query(
            r#"
            INSERT INTO conversations
                (id, user_id, organization, agent, title, started_at, updated_at)
            VALUES
                ('conv-alex', 'alex', 'global', 'full-access', 'Alex conversation', 'now', 'now')
            "#,
        )
        .execute(&pool)
        .await
        .expect("seed conversation");

        pool
    }

    #[tokio::test]
    async fn authorizes_owner_for_chat_turns() {
        let pool = fresh_pool().await;
        assert!(
            authorize_conversation_turn(&pool, "conv-alex", "alex", &AgentType::FullAccess,)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn rejects_other_user_for_chat_turns() {
        let pool = fresh_pool().await;
        let response = match authorize_conversation_turn(
            &pool,
            "conv-alex",
            "jakegreene",
            &AgentType::FullAccess,
        )
        .await
        {
            Ok(()) => panic!("expected authorization failure"),
            Err(response) => response,
        };
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn enforces_runtime_boundary_before_chat_turn_persistence() {
        let pool = fresh_pool().await;
        sqlx::query(
            r#"
            UPDATE conversations
            SET agent = 'codex-coordinator', conversation_type = 'codex_coordinator',
                conversation_role = 'multi_agent_parent',
                organization = 'agentic-flowstate', title = 'Alex'
            WHERE id = 'conv-alex'
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

        let wrong_runtime =
            authorize_conversation_turn(&pool, "conv-alex", "alex", &AgentType::FullAccess)
                .await
                .expect_err("A non-coordinator agent must not enter the Codex singleton");
        assert_eq!(wrong_runtime.status(), StatusCode::CONFLICT);

        assert!(authorize_conversation_turn(
            &pool,
            "conv-alex",
            "alex",
            &AgentType::CodexCoordinator,
        )
        .await
        .is_ok());
    }

    #[tokio::test]
    async fn rejects_follow_up_turns_for_one_shot_workers() {
        let pool = fresh_pool().await;
        sqlx::query(
            "UPDATE conversations SET agent = 'code-execution', conversation_role = 'sub_agent' WHERE id = 'conv-alex'",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO conversation_turn_jobs (id, conversation_id) VALUES ('job-1', 'conv-alex')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let response =
            authorize_conversation_turn(&pool, "conv-alex", "alex", &AgentType::CodeExecution)
                .await
                .expect_err("code workers must be one-shot");
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn permits_follow_up_turns_for_research_workers() {
        let pool = fresh_pool().await;
        sqlx::query(
            "UPDATE conversations SET agent = 'codebase-research', conversation_role = 'sub_agent' WHERE id = 'conv-alex'",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO conversation_turn_jobs (id, conversation_id) VALUES ('job-1', 'conv-alex')",
        )
        .execute(&pool)
        .await
        .unwrap();

        assert!(authorize_conversation_turn(
            &pool,
            "conv-alex",
            "alex",
            &AgentType::CodebaseResearch,
        )
        .await
        .is_ok());
    }
}

#[cfg(test)]
mod idempotency_key_tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};

    fn hdrs(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            let name: axum::http::HeaderName = k.parse().unwrap();
            h.insert(name, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn absent_header_yields_none() {
        let h = HeaderMap::new();
        assert_eq!(extract_client_id(&h).unwrap(), None);
    }

    #[test]
    fn well_formed_uuid_passes() {
        let h = hdrs(&[("Idempotency-Key", "01H2X9GK8X7C8ZABCDEF01234567")]);
        assert_eq!(
            extract_client_id(&h).unwrap(),
            Some("01H2X9GK8X7C8ZABCDEF01234567".to_string())
        );
    }

    #[test]
    fn lowercase_header_name_passes() {
        // HeaderMap matches case-insensitively, so lowercase works too.
        let h = hdrs(&[("idempotency-key", "abc-123")]);
        assert_eq!(extract_client_id(&h).unwrap(), Some("abc-123".to_string()));
    }

    #[test]
    fn empty_value_rejected() {
        let h = hdrs(&[("Idempotency-Key", "")]);
        assert!(matches!(
            extract_client_id(&h),
            Err(IdempotencyKeyError::Malformed(_))
        ));
    }

    #[test]
    fn too_long_rejected() {
        let long = "a".repeat(129);
        let h = hdrs(&[("Idempotency-Key", long.as_str())]);
        assert!(matches!(
            extract_client_id(&h),
            Err(IdempotencyKeyError::Malformed(_))
        ));
    }

    #[test]
    fn at_boundary_length_accepted() {
        let max = "a".repeat(128);
        let h = hdrs(&[("Idempotency-Key", max.as_str())]);
        assert_eq!(extract_client_id(&h).unwrap(), Some(max));
    }
}
