use axum::{extract::State, Json};
use std::sync::Arc;
use std::path::PathBuf;
use std::collections::HashMap;
use sqlx::SqlitePool;
use serde::Deserialize;

use crate::agents::AgentType;
use super::chat_stream::{self, ChatConfig, SseStream};

#[derive(Debug, Deserialize)]
pub struct WorkspaceManagerRequest {
    pub message: String,
    /// Accepted from frontend but not used server-side (agent works cross-org)
    #[allow(dead_code)]
    pub organization: Option<String>,
    pub session_id: Option<String>,
    pub conversation_id: Option<String>,
}

fn config() -> ChatConfig {
    ChatConfig {
        agent_type: AgentType::WorkspaceManager,
        prompt_name: "workspace-manager",
        working_dir: PathBuf::from("/Users/jarvisgpt/projects"),
        prompt_vars: HashMap::new(),
    }
}

/// POST /api/workspace-manager/chat
pub async fn workspace_manager_chat(
    State(db): State<Arc<SqlitePool>>,
    Json(req): Json<WorkspaceManagerRequest>,
) -> SseStream {
    tracing::info!("=== WORKSPACE_MANAGER_CHAT START ===");
    chat_stream::chat(
        db,
        req.message,
        req.session_id,
        req.conversation_id,
        config(),
    )
}

/// POST /api/workspace-manager/resume
pub async fn workspace_manager_resume(
    State(db): State<Arc<SqlitePool>>,
    Json(req): Json<WorkspaceManagerRequest>,
) -> SseStream {
    tracing::info!("=== WORKSPACE_MANAGER_RESUME START ===");
    let session_id = match req.session_id {
        Some(id) => id,
        None => return chat_stream::create_error_sse("session_id is required for resume".to_string()),
    };
    chat_stream::resume(
        db,
        req.message,
        session_id,
        req.conversation_id,
        config(),
    )
}
