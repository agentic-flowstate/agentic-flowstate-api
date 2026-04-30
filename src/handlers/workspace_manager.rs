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
use super::chat_stream::{self, ChatConfig, ChatImageData, ChatRuntime};
use crate::agents::AgentType;
use crate::auth_middleware::AuthenticatedUser;

#[derive(Debug, Deserialize)]
pub struct WorkspaceManagerRequest {
    pub message: String,
    /// Accepted from frontend but not used server-side (agent works cross-org)
    #[allow(dead_code)]
    pub organization: Option<String>,
    pub conversation_id: Option<String>,
    pub images: Option<Vec<ChatImageData>>,
}

fn config() -> ChatConfig {
    ChatConfig {
        agent_type: AgentType::WorkspaceManager,
        runtime: ChatRuntime::CodexAppServer,
        prompt_name: "workspace-manager",
        working_dir: PathBuf::from("/Users/jarvisgpt/projects"),
        prompt_vars: HashMap::new(),
    }
}

/// POST /api/workspace-manager/chat
pub async fn workspace_manager_chat(
    State(db): State<Arc<SqlitePool>>,
    State(manager): State<Arc<ChatClientManager>>,
    Extension(user): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Json(req): Json<WorkspaceManagerRequest>,
) -> Response {
    tracing::info!("=== WORKSPACE_MANAGER_CHAT START ===");
    let client_id = match chat_stream::extract_client_id(&headers) {
        Ok(v) => v,
        Err(e) => return chat_stream::malformed_idempotency_key_response(e),
    };
    chat_stream::chat(
        db,
        manager,
        req.message,
        req.conversation_id,
        config(),
        user.user_id,
        req.images,
        client_id,
    )
    .await
}

/// POST /api/workspace-manager/chat/submit
pub async fn workspace_manager_chat_submit(
    State(db): State<Arc<SqlitePool>>,
    State(manager): State<Arc<ChatClientManager>>,
    Extension(user): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Json(req): Json<WorkspaceManagerRequest>,
) -> Response {
    tracing::info!("=== WORKSPACE_MANAGER_CHAT_SUBMIT START ===");
    let client_id = match chat_stream::extract_client_id(&headers) {
        Ok(v) => v,
        Err(e) => return chat_stream::malformed_idempotency_key_response(e),
    };
    chat_stream::submit(
        db,
        manager,
        req.message,
        req.conversation_id,
        config(),
        user.user_id,
        req.images,
        client_id,
    )
    .await
}
