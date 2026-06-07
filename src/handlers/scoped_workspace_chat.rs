use axum::{
    extract::{Extension, State},
    http::HeaderMap,
    response::Response,
    Json,
};
use serde::Deserialize;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use super::chat_client_manager::ChatClientManager;
use super::chat_stream::{self, ChatCodexOptions, ChatConfig, ChatImageData, ChatRuntime};
use crate::agents::AgentType;
use crate::auth_middleware::AuthenticatedUser;

#[derive(Debug, Deserialize)]
pub struct ScopedWorkspaceChatRequest {
    pub message: String,
    pub conversation_id: Option<String>,
    pub images: Option<Vec<ChatImageData>>,
    pub codex_options: Option<ChatCodexOptions>,
}

/// POST /api/scoped-workspace/chat
///
/// Restricted workspace manager for external collaborators. The agent only
/// has access to organizations the authenticated user is a member of, cannot
/// read Alex's personal data (home context, daily plan, focus, emails), and
/// cannot execute code or manage services.
pub async fn scoped_workspace_chat(
    State(db): State<Arc<SqlitePool>>,
    State(manager): State<Arc<ChatClientManager>>,
    Extension(user): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Json(req): Json<ScopedWorkspaceChatRequest>,
) -> Response {
    tracing::info!("=== SCOPED_WORKSPACE_CHAT START === user={}", user.user_id);

    let client_id = match chat_stream::extract_client_id(&headers) {
        Ok(v) => v,
        Err(e) => return chat_stream::malformed_idempotency_key_response(e),
    };

    let display_name = lookup_display_name(&db, &user.user_id).await;

    let mut prompt_vars = HashMap::new();
    prompt_vars.insert("USER_ID".to_string(), user.user_id.clone());
    prompt_vars.insert("USER_NAME".to_string(), display_name);

    let agent_type = AgentType::ScopedWorkspace;
    let config = ChatConfig {
        agent_type: agent_type.clone(),
        runtime: ChatRuntime::CodexAppServer,
        prompt_name: "scoped-workspace",
        working_dir: PathBuf::from("/Users/jarvisgpt/projects"),
        prompt_vars,
        codex_options: ChatCodexOptions::default_for_agent(&agent_type),
    };
    let config = match chat_stream::apply_codex_options(config, req.codex_options.clone()) {
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

/// POST /api/scoped-workspace/chat/submit
pub async fn scoped_workspace_chat_submit(
    State(db): State<Arc<SqlitePool>>,
    State(manager): State<Arc<ChatClientManager>>,
    Extension(user): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Json(req): Json<ScopedWorkspaceChatRequest>,
) -> Response {
    tracing::info!(
        "=== SCOPED_WORKSPACE_CHAT_SUBMIT START === user={}",
        user.user_id
    );

    let client_id = match chat_stream::extract_client_id(&headers) {
        Ok(v) => v,
        Err(e) => return chat_stream::malformed_idempotency_key_response(e),
    };

    let display_name = lookup_display_name(&db, &user.user_id).await;

    let mut prompt_vars = HashMap::new();
    prompt_vars.insert("USER_ID".to_string(), user.user_id.clone());
    prompt_vars.insert("USER_NAME".to_string(), display_name);

    let agent_type = AgentType::ScopedWorkspace;
    let config = ChatConfig {
        agent_type: agent_type.clone(),
        runtime: ChatRuntime::CodexAppServer,
        prompt_name: "scoped-workspace",
        working_dir: PathBuf::from("/Users/jarvisgpt/projects"),
        prompt_vars,
        codex_options: ChatCodexOptions::default_for_agent(&agent_type),
    };
    let config = match chat_stream::apply_codex_options(config, req.codex_options.clone()) {
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

async fn lookup_display_name(db: &SqlitePool, user_id: &str) -> String {
    match ticketing_system::users::get_user(db, user_id).await {
        Ok(Some(user)) => user.name,
        _ => user_id.to_string(),
    }
}
