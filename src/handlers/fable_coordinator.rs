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
use crate::agents::codex_app_server::verify_codex_subscription_auth;
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
    pub model: String,
    pub effort: String,
    pub prompt_version: &'static str,
    pub thread_continuity: String,
    pub runtime: Option<FableRuntimeState>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct FableCoordinatorStatus {
    pub state: &'static str,
    pub detail: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct FableCoordinatorResponse {
    pub conversation: Conversation,
    pub coordinator_status: FableCoordinatorStatus,
}

fn error_response(status: StatusCode, error: impl ToString) -> Response {
    (
        status,
        Json(serde_json::json!({"error": error.to_string()})),
    )
        .into_response()
}

async fn coordinator_queue_depth(
    db: &SqlitePool,
    conversation_id: &str,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM conversation_turn_jobs WHERE conversation_id = ? AND status IN ('pending', 'running')",
    )
    .bind(conversation_id)
    .fetch_one(db)
    .await
}

async fn load_active_child_count(
    db: &SqlitePool,
    conversation_id: &str,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
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
    .bind(conversation_id)
    .fetch_one(db)
    .await
}

fn resolve_coordinator_status(
    auth_error: Option<String>,
    runtime: Option<&FableRuntimeState>,
    untracked_codex_thread: bool,
    coordinator_busy: bool,
    active_child_count: i64,
) -> FableCoordinatorStatus {
    if let Some(detail) = auth_error {
        return FableCoordinatorStatus {
            state: "needs_login",
            detail: Some(detail),
        };
    }

    if untracked_codex_thread {
        return FableCoordinatorStatus {
            state: "failed",
            detail: Some("Fable Codex thread continuity requires an explicit repair".to_string()),
        };
    }

    if let Some(runtime) = runtime {
        if let Some(error_class) = runtime.last_error_class.as_deref() {
            let normalized = error_class.to_ascii_lowercase();
            if normalized.contains("usage")
                || normalized.contains("capacity")
                || normalized.contains("rate_limit")
            {
                return FableCoordinatorStatus {
                    state: "usage_limited",
                    detail: Some(error_class.to_string()),
                };
            }
        }
        if runtime.thread_state == "repair_required" {
            return FableCoordinatorStatus {
                state: "failed",
                detail: Some(
                    "Fable Codex thread continuity requires an explicit repair".to_string(),
                ),
            };
        }
        if runtime.last_terminal_status.as_deref() == Some("failed") {
            return FableCoordinatorStatus {
                state: "failed",
                detail: runtime.last_error_class.clone(),
            };
        }
    }

    if active_child_count > 0 {
        return FableCoordinatorStatus {
            state: "waiting_on_children",
            detail: Some(format!(
                "{active_child_count} worker{} active",
                if active_child_count == 1 {
                    " is"
                } else {
                    "s are"
                }
            )),
        };
    }
    if coordinator_busy {
        return FableCoordinatorStatus {
            state: "working",
            detail: None,
        };
    }
    FableCoordinatorStatus {
        state: "ready",
        detail: None,
    }
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
    let mut prompt_vars = HashMap::new();
    prompt_vars.insert("USER_ID".to_string(), user_id.to_string());
    prompt_vars.insert(
        "PROMPT_VERSION".to_string(),
        FABLE_PROMPT_VERSION.to_string(),
    );
    Ok(ChatConfig {
        agent_type: AgentType::FableCoordinator,
        runtime: ChatRuntime::CodexAppServer,
        prompt_name: "fable-coordinator",
        working_dir: projects_root,
        prompt_vars,
        codex_options: ChatCodexOptions::default_for_agent(&AgentType::FableCoordinator),
    })
}

pub async fn get_fable_coordinator(
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
    let queue_depth = match coordinator_queue_depth(&db, &conversation.id).await {
        Ok(count) => count,
        Err(error) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, error),
    };
    let active_child_count = match load_active_child_count(&db, &conversation.id).await {
        Ok(count) => count,
        Err(error) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, error),
    };
    let auth_error = verify_codex_subscription_auth()
        .await
        .err()
        .map(|error| error.to_string());
    let coordinator_busy = manager.has_runtime_turn(&conversation.id).await || queue_depth > 0;
    let coordinator_status = resolve_coordinator_status(
        auth_error,
        runtime.as_ref(),
        runtime.is_none() && conversation.session_id.is_some(),
        coordinator_busy,
        active_child_count,
    );
    Json(FableCoordinatorResponse {
        conversation,
        coordinator_status,
    })
    .into_response()
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
    let queue_depth = match coordinator_queue_depth(&db, &conversation.id).await {
        Ok(count) => count,
        Err(error) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, error),
    };
    let active_child_count = match load_active_child_count(&db, &conversation.id).await {
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
    let (auth_state, auth_error) = match verify_codex_subscription_auth().await {
        Ok(()) => ("ready".to_string(), None),
        Err(error) => ("needs_login".to_string(), Some(error)),
    };
    let thread_continuity = match runtime.as_ref() {
        Some(runtime) => runtime.thread_state.clone(),
        None if conversation.session_id.is_some() => "untracked".to_string(),
        None => "uninitialized".to_string(),
    };
    let coordinator_busy = manager.has_runtime_turn(&conversation.id).await || queue_depth > 0;
    crate::observability::fable::set_health(coordinator_busy, queue_depth, &thread_continuity);

    Json(FableCoordinatorHealth {
        conversation_id: conversation.id,
        coordinator_busy,
        queue_depth,
        active_child_count,
        pending_child_wake_count,
        auth_state,
        auth_error,
        model: AgentType::FableCoordinator.model().to_string(),
        effort: AgentType::FableCoordinator.effort().to_string(),
        prompt_version: FABLE_PROMPT_VERSION,
        thread_continuity,
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
            "confirm_recovery=true is required for a Codex thread reset",
        );
    }
    let conversation = match coordinator_for_user(&db, &user).await {
        Ok(conversation) => conversation,
        Err(response) => return response,
    };
    match fable_coordinator::repair_thread(&db, &conversation.id, &user.user_id).await {
        Ok(repair) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({
                "conversation_id": conversation.id,
                "retired_codex_thread_id": repair.retired_thread_id,
                "status": "reset",
                "rehydrate_required": repair.rehydrate_required,
            })),
        )
            .into_response(),
        Err(error) => error_response(StatusCode::CONFLICT, error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinator_submits_to_the_codex_runtime() {
        let config = chat_config(fable_coordinator::ALEX_USER_ID).unwrap();

        assert_eq!(config.runtime, ChatRuntime::CodexAppServer);
        assert_eq!(
            config.codex_options.model,
            AgentType::FableCoordinator.model()
        );
        assert_eq!(
            config.codex_options.reasoning_effort,
            AgentType::FableCoordinator.effort()
        );
        assert!(!config.prompt_vars.contains_key("AGENTS_MD"));

        let prompt = crate::agents::prompts::load_prompt(config.prompt_name, config.prompt_vars)
            .expect("render Fable coordinator prompt");
        assert!(prompt.contains(&format!(
            "Prompt version: {}",
            fable_coordinator::FABLE_PROMPT_VERSION
        )));
        assert!(!prompt.contains("{{AGENTS_MD}}"));
        assert!(!prompt.contains("Open HSV-2 therapeutics research"));
    }

    #[test]
    fn coordinator_v5_prompt_contains_workspace_ticket_and_efficiency_contracts() {
        assert_eq!(
            fable_coordinator::FABLE_PROMPT_VERSION,
            "fable-coordinator/v5"
        );

        let config = chat_config(fable_coordinator::ALEX_USER_ID).unwrap();
        let prompt = crate::agents::prompts::load_prompt(config.prompt_name, config.prompt_vars)
            .expect("render Fable coordinator prompt");

        for required in [
            "## Baseline workspace context",
            "The organization is `agentic-flowstate`.",
            "`agentic-flowstate-api`",
            "`agentic-flowstate-mcp`",
            "`agentic-flowstate-app`",
            "`agentic-flowstate-ticketing-system`",
            "`agentic-flowstate-setup`",
            "`agentic-flowstate-templates`",
            "`projects-agents`",
            "`/Users/jarvisgpt/projects`",
            "## Ticket conventions",
            "explicit `due_date` in strict `YYYY-MM-DD` format",
            "Pass `organization: \"agentic-flowstate\"` explicitly",
            "Resolve creation milestones from `ensure_work_ticket` target candidates",
            "## Coordination efficiency",
            "batch-load all of their schemas in one `ToolSearch` call",
            "full `mcp__agentic-mcp__<tool_name>` name",
            "`mcp__agentic-mcp__search_tickets`",
            "`mcp__agentic-mcp__get_ticket`",
            "Keep FTS queries narrow and specific",
            "do not re-probe that surface with an equivalent query",
            "Never ask Alex for context available from those sources",
        ] {
            assert!(
                prompt.contains(required),
                "Fable v5 prompt is missing required contract text: {required}"
            );
        }

        for preserved_boundary in [
            "single permanent text-only entry point",
            "You are a conversation manager, not an implementation agent.",
            "You are the only coordinator.",
            "There is no nested worker hierarchy.",
            "A code worker's first terminal result ends that conversation.",
            "Authentication, credential, billing, security, runtime, retention, and coordinator-session recovery changes require explicit Alex approval.",
            "This experience is text-only. Do not create or invoke voice functionality.",
        ] {
            assert!(
                prompt.contains(preserved_boundary),
                "Fable v5 prompt lost coordinator boundary text: {preserved_boundary}"
            );
        }
    }

    fn runtime_state(
        thread_state: &str,
        terminal_status: Option<&str>,
        error: Option<&str>,
    ) -> FableRuntimeState {
        FableRuntimeState {
            conversation_id: "coordinator".to_string(),
            codex_thread_id: Some("thread".to_string()),
            thread_state: thread_state.to_string(),
            prompt_version: FABLE_PROMPT_VERSION.to_string(),
            model: AgentType::FableCoordinator.model().to_string(),
            effort: AgentType::FableCoordinator.effort().to_string(),
            auth_method: "chatgpt".to_string(),
            last_started_at: None,
            last_completed_at: None,
            last_success_at: None,
            last_turn_duration_ms: None,
            last_tool_call_count: 0,
            last_terminal_status: terminal_status.map(str::to_string),
            last_error_class: error.map(str::to_string),
            last_wake_at: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn coordinator_status_prioritizes_actionable_runtime_states() {
        let auth = resolve_coordinator_status(
            Some("Codex ChatGPT login required".to_string()),
            None,
            false,
            true,
            2,
        );
        assert_eq!(auth.state, "needs_login");

        let usage_runtime = runtime_state("ready", Some("failed"), Some("usage_limit"));
        let usage = resolve_coordinator_status(None, Some(&usage_runtime), false, false, 0);
        assert_eq!(usage.state, "usage_limited");

        let recovery_runtime = runtime_state("repair_required", Some("failed"), None);
        let recovery = resolve_coordinator_status(None, Some(&recovery_runtime), false, false, 0);
        assert_eq!(recovery.state, "failed");

        let untracked = resolve_coordinator_status(None, None, true, false, 0);
        assert_eq!(untracked.state, "failed");
        assert!(untracked
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("Codex thread")));
    }

    #[test]
    fn coordinator_status_reports_children_work_and_ready() {
        assert_eq!(
            resolve_coordinator_status(None, None, false, true, 2).state,
            "waiting_on_children"
        );
        assert_eq!(
            resolve_coordinator_status(None, None, false, true, 0).state,
            "working"
        );
        assert_eq!(
            resolve_coordinator_status(None, None, false, false, 0).state,
            "ready"
        );
    }
}
