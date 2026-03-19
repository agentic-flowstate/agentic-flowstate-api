use axum::{extract::{Extension, State}, Json};
use std::sync::Arc;
use std::path::PathBuf;
use std::collections::HashMap;
use sqlx::SqlitePool;
use serde::Deserialize;

use crate::agents::AgentType;
use crate::auth_middleware::AuthenticatedUser;
use super::chat_stream::{self, ChatConfig, SseStream};
use super::chat_client_manager::ChatClientManager;

#[derive(Debug, Deserialize)]
pub struct FullAccessChatRequest {
    pub message: String,
    pub conversation_id: Option<String>,
}

/// POST /api/full-access/chat
pub async fn full_access_chat(
    State(db): State<Arc<SqlitePool>>,
    State(manager): State<Arc<ChatClientManager>>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(req): Json<FullAccessChatRequest>,
) -> SseStream {
    tracing::info!("=== FULL_ACCESS_CHAT START === user={}", user.user_id);

    let claude_md = std::fs::read_to_string("/Users/jarvisgpt/projects/CLAUDE.md")
        .unwrap_or_else(|e| format!("(Failed to read CLAUDE.md: {})", e));

    let mut prompt_vars = HashMap::new();
    prompt_vars.insert("USER_ID".to_string(), user.user_id.clone());
    prompt_vars.insert("CLAUDE_MD".to_string(), claude_md);

    let config = ChatConfig {
        agent_type: AgentType::FullAccess,
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
    )
}
