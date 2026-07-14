use axum::{
    extract::{Extension, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::Arc;
use ticketing_system::Conversation;

use super::chat_client_manager::ChatClientManager;
use super::chat_stream::{self, ChatAttachmentData, ChatCodexOptions, ChatConfig, ChatRuntime};
use crate::agents::claude_code::{verify_claude_subscription_auth, FABLE_EFFORT, FABLE_MODEL};
use crate::agents::AgentType;
use crate::auth_middleware::AuthenticatedUser;
use crate::fable_coordinator::{self, FableRuntimeState, FABLE_PROMPT_VERSION};

#[derive(Debug, Deserialize)]
pub struct FableChatRequest {
    pub message: String,
    pub attachments: Option<Vec<ChatAttachmentData>>,
}

#[derive(Debug, Deserialize)]
pub struct RepairFableSessionRequest {
    pub confirm_recovery: bool,
}

#[derive(Debug, Serialize)]
pub struct FableCoordinatorHealth {
    pub conversation_id: String,
    pub coordinator_busy: bool,
    pub queue_depth: i64,
    pub active_child_count: i64,
    pub pending_child_wake_count: i64,
    pub auth_state: String,
    pub auth_error: Option<String>,
    pub model: &'static str,
    pub effort: &'static str,
    pub prompt_version: &'static str,
    pub native_session_continuity: String,
    pub runtime: Option<FableRuntimeState>,
}

fn error_response(status: StatusCode, error: impl ToString) -> Response {
    (
        status,
        Json(serde_json::json!({"error": error.to_string()})),
    )
        .into_response()
}

async fn coordinator_for_user(
    db: &SqlitePool,
    user: &AuthenticatedUser,
) -> Result<Conversation, Response> {
    fable_coordinator::ensure_singleton(db, &user.user_id)
        .await
        .map_err(|error| {
            let status = if user.user_id == fable_coordinator::ALEX_USER_ID {
                StatusCode::INTERNAL_SERVER_ERROR
            } else {
                StatusCode::FORBIDDEN
            };
            error_response(status, error)
        })
}

fn chat_config(user_id: &str) -> Result<ChatConfig, Response> {
    let projects_root = dirs::home_dir()
        .ok_or_else(|| {
            error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Cannot resolve the projects root for the Fable coordinator",
            )
        })?
        .join("projects");
    let agents_md = std::fs::read_to_string(projects_root.join("AGENTS.md")).map_err(|error| {
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Required coordinator instructions are unavailable: {error}"),
        )
    })?;
    let mut prompt_vars = HashMap::new();
    prompt_vars.insert("USER_ID".to_string(), user_id.to_string());
    prompt_vars.insert("AGENTS_MD".to_string(), agents_md);
    prompt_vars.insert(
        "PROMPT_VERSION".to_string(),
        FABLE_PROMPT_VERSION.to_string(),
    );
    Ok(ChatConfig {
        agent_type: AgentType::FableCoordinator,
        runtime: ChatRuntime::ClaudeCodeFable,
        prompt_name: "fable-coordinator",
        working_dir: projects_root,
        prompt_vars,
        codex_options: ChatCodexOptions {
            model: FABLE_MODEL.to_string(),
            reasoning_effort: FABLE_EFFORT.to_string(),
        },
    })
}

pub async fn get_fable_coordinator(
    State(db): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Response {
    match coordinator_for_user(&db, &user).await {
        Ok(conversation) => Json(conversation).into_response(),
        Err(response) => response,
    }
}

pub async fn get_fable_coordinator_health(
    State(db): State<Arc<SqlitePool>>,
    State(manager): State<Arc<ChatClientManager>>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Response {
    let conversation = match coordinator_for_user(&db, &user).await {
        Ok(conversation) => conversation,
        Err(response) => return response,
    };
    let runtime = match fable_coordinator::runtime_state(&db, &conversation.id).await {
        Ok(runtime) => runtime,
        Err(error) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, error),
    };
    let queue_depth = match sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM conversation_turn_jobs WHERE conversation_id = ? AND status IN ('pending', 'running')",
    )
    .bind(&conversation.id)
    .fetch_one(db.as_ref())
    .await
    {
        Ok(count) => count,
        Err(error) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, error),
    };
    let active_child_count = match sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(DISTINCT child.id)
        FROM conversations child
        LEFT JOIN conversation_turn_jobs job ON job.conversation_id = child.id
        LEFT JOIN agent_runner_turns turn ON turn.conversation_id = child.id
        WHERE child.parent_conversation_id = ?
          AND child.status <> 'archived'
          AND (job.status IN ('pending', 'running') OR turn.status IN ('queued', 'running'))
        "#,
    )
    .bind(&conversation.id)
    .fetch_one(db.as_ref())
    .await
    {
        Ok(count) => count,
        Err(error) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, error),
    };
    let pending_child_wake_count = match sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)
        FROM conversation_turn_jobs
        WHERE conversation_id = ?
          AND status IN ('pending', 'running')
          AND message_metadata IS NOT NULL
          AND json_valid(message_metadata)
          AND json_extract(message_metadata, '$.orchestration') = 'coordinator_child_completion_wake'
        "#,
    )
    .bind(&conversation.id)
    .fetch_one(db.as_ref())
    .await
    {
        Ok(count) => count,
        Err(error) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, error),
    };
    let (auth_state, auth_error) = match verify_claude_subscription_auth().await {
        Ok(()) => ("ready".to_string(), None),
        Err(error) => ("needs_login".to_string(), Some(error)),
    };
    let native_session_continuity = runtime
        .as_ref()
        .map(|runtime| runtime.session_state.clone())
        .unwrap_or_else(|| "uninitialized".to_string());
    let coordinator_busy = manager.has_runtime_turn(&conversation.id).await || queue_depth > 0;
    crate::observability::fable::set_health(
        coordinator_busy,
        queue_depth,
        &native_session_continuity,
    );

    Json(FableCoordinatorHealth {
        conversation_id: conversation.id,
        coordinator_busy,
        queue_depth,
        active_child_count,
        pending_child_wake_count,
        auth_state,
        auth_error,
        model: FABLE_MODEL,
        effort: FABLE_EFFORT,
        prompt_version: FABLE_PROMPT_VERSION,
        native_session_continuity,
        runtime,
    })
    .into_response()
}

pub async fn fable_coordinator_chat_submit(
    State(db): State<Arc<SqlitePool>>,
    State(manager): State<Arc<ChatClientManager>>,
    Extension(user): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Json(request): Json<FableChatRequest>,
) -> Response {
    run_chat(db, manager, user, headers, request, true).await
}

pub async fn fable_coordinator_chat(
    State(db): State<Arc<SqlitePool>>,
    State(manager): State<Arc<ChatClientManager>>,
    Extension(user): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Json(request): Json<FableChatRequest>,
) -> Response {
    run_chat(db, manager, user, headers, request, false).await
}

async fn run_chat(
    db: Arc<SqlitePool>,
    manager: Arc<ChatClientManager>,
    user: AuthenticatedUser,
    headers: HeaderMap,
    request: FableChatRequest,
    submit: bool,
) -> Response {
    let conversation = match coordinator_for_user(&db, &user).await {
        Ok(conversation) => conversation,
        Err(response) => return response,
    };
    let config = match chat_config(&user.user_id) {
        Ok(config) => config,
        Err(response) => return response,
    };
    let client_id = match chat_stream::extract_client_id(&headers) {
        Ok(client_id) => client_id,
        Err(error) => return chat_stream::malformed_idempotency_key_response(error),
    };
    if submit {
        chat_stream::submit(
            db,
            manager,
            request.message,
            Some(conversation.id),
            config,
            user.user_id,
            request.attachments,
            client_id,
        )
        .await
    } else {
        chat_stream::chat(
            db,
            manager,
            request.message,
            Some(conversation.id),
            config,
            user.user_id,
            request.attachments,
            client_id,
        )
        .await
    }
}

pub async fn repair_fable_coordinator_session(
    State(db): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(request): Json<RepairFableSessionRequest>,
) -> Response {
    if !request.confirm_recovery {
        return error_response(
            StatusCode::BAD_REQUEST,
            "confirm_recovery=true is required for an audited native-session replacement",
        );
    }
    let conversation = match coordinator_for_user(&db, &user).await {
        Ok(conversation) => conversation,
        Err(response) => return response,
    };
    match fable_coordinator::repair_session(&db, &conversation.id, &user.user_id).await {
        Ok(plan) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({
                "conversation_id": conversation.id,
                "native_session_id": plan.session_id,
                "status": "recovery_approved",
                "rehydrate_required": plan.rehydrate_required,
            })),
        )
            .into_response(),
        Err(error) => error_response(StatusCode::CONFLICT, error),
    }
}
