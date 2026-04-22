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
pub struct FullAccessChatRequest {
    pub message: String,
    pub conversation_id: Option<String>,
    pub images: Option<Vec<ChatImageData>>,
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

    let client_id = match chat_stream::extract_client_id(&headers) {
        Ok(v) => v,
        Err(e) => return chat_stream::malformed_idempotency_key_response(e),
    };

    let agents_md = std::fs::read_to_string("/Users/jarvisgpt/projects/AGENTS.md")
        .unwrap_or_else(|e| format!("(Failed to read AGENTS.md: {})", e));

    let mut prompt_vars = HashMap::new();
    prompt_vars.insert("USER_ID".to_string(), user.user_id.clone());
    prompt_vars.insert("AGENTS_MD".to_string(), agents_md);

    let config = ChatConfig {
        agent_type: AgentType::FullAccess,
        runtime: ChatRuntime::CodexExec,
        prompt_name: "full-access",
        working_dir: PathBuf::from("/Users/jarvisgpt/projects"),
        prompt_vars,
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
}
