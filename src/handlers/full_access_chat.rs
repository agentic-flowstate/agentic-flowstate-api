use axum::{
    extract::{Extension, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use super::chat_client_manager::ChatClientManager;
use super::chat_stream::{self, ChatCodexOptions, ChatConfig, ChatImageData, ChatRuntime};
use crate::agents::AgentType;
use crate::auth_middleware::AuthenticatedUser;

#[derive(Debug, Deserialize)]
pub struct FullAccessChatRequest {
    pub message: String,
    pub conversation_id: Option<String>,
    pub images: Option<Vec<ChatImageData>>,
    pub codex_options: Option<ChatCodexOptions>,
}

async fn reject_unless_admin(db: &SqlitePool, user_id: &str) -> Option<Response> {
    match ticketing_system::system_logs::is_admin(db, user_id).await {
        Ok(true) => None,
        Ok(false) => Some(
            (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "Admin access required"})),
            )
                .into_response(),
        ),
        Err(e) => {
            tracing::error!("Full-access admin check failed for {}: {:?}", user_id, e);
            Some(
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "Admin access check failed"})),
                )
                    .into_response(),
            )
        }
    }
}

/// POST /api/full-access/chat
pub async fn full_access_chat(
    State(db): State<Arc<SqlitePool>>,
    State(manager): State<Arc<ChatClientManager>>,
    Extension(user): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Json(req): Json<FullAccessChatRequest>,
) -> Response {
    tracing::info!("=== FULL_ACCESS_CHAT START === user={}", user.user_id);

    if let Some(response) = reject_unless_admin(&db, &user.user_id).await {
        return response;
    }

    let client_id = match chat_stream::extract_client_id(&headers) {
        Ok(v) => v,
        Err(e) => return chat_stream::malformed_idempotency_key_response(e),
    };

    let agents_md = std::fs::read_to_string("/Users/jarvisgpt/projects/AGENTS.md")
        .unwrap_or_else(|e| format!("(Failed to read AGENTS.md: {})", e));

    let mut prompt_vars = HashMap::new();
    prompt_vars.insert("USER_ID".to_string(), user.user_id.clone());
    prompt_vars.insert("AGENTS_MD".to_string(), agents_md);

    let agent_type = AgentType::FullAccess;
    let config = ChatConfig {
        agent_type: agent_type.clone(),
        runtime: ChatRuntime::CodexAppServer,
        prompt_name: "full-access",
        working_dir: PathBuf::from("/Users/jarvisgpt/projects"),
        prompt_vars,
        codex_options: ChatCodexOptions::default_for_agent(&agent_type),
    };
    let config = match chat_stream::apply_codex_options(config, req.codex_options.clone()).await {
        Ok(config) => config,
        Err(response) => return response,
    };

    chat_stream::chat(
        db,
        manager,
        req.message,
        req.conversation_id,
        config,
        user.user_id,
        req.images,
        client_id,
    )
    .await
}

/// POST /api/full-access/chat/submit
pub async fn full_access_chat_submit(
    State(db): State<Arc<SqlitePool>>,
    State(manager): State<Arc<ChatClientManager>>,
    Extension(user): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Json(req): Json<FullAccessChatRequest>,
) -> Response {
    tracing::info!(
        "=== FULL_ACCESS_CHAT_SUBMIT START === user={}",
        user.user_id
    );

    if let Some(response) = reject_unless_admin(&db, &user.user_id).await {
        return response;
    }

    let client_id = match chat_stream::extract_client_id(&headers) {
        Ok(v) => v,
        Err(e) => return chat_stream::malformed_idempotency_key_response(e),
    };

    let agents_md = std::fs::read_to_string("/Users/jarvisgpt/projects/AGENTS.md")
        .unwrap_or_else(|e| format!("(Failed to read AGENTS.md: {})", e));

    let mut prompt_vars = HashMap::new();
    prompt_vars.insert("USER_ID".to_string(), user.user_id.clone());
    prompt_vars.insert("AGENTS_MD".to_string(), agents_md);

    let agent_type = AgentType::FullAccess;
    let config = ChatConfig {
        agent_type: agent_type.clone(),
        runtime: ChatRuntime::CodexAppServer,
        prompt_name: "full-access",
        working_dir: PathBuf::from("/Users/jarvisgpt/projects"),
        prompt_vars,
        codex_options: ChatCodexOptions::default_for_agent(&agent_type),
    };
    let config = match chat_stream::apply_codex_options(config, req.codex_options.clone()).await {
        Ok(config) => config,
        Err(response) => return response,
    };

    chat_stream::submit(
        db,
        manager,
        req.message,
        req.conversation_id,
        config,
        user.user_id,
        req.images,
        client_id,
    )
    .await
}
