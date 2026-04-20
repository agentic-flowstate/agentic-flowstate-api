use axum::{
    extract::{Extension, State},
    response::Response,
    Json,
};
use serde::Deserialize;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use super::chat_client_manager::ChatClientManager;
use super::chat_stream::{self, ChatConfig, ChatImageData};
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
    Json(req): Json<WorkspaceManagerRequest>,
) -> Response {
    tracing::info!("=== WORKSPACE_MANAGER_CHAT START ===");
    chat_stream::chat(
        db,
        manager,
        req.message,
        req.conversation_id,
        config(),
        user.user_id,
        req.images,
    )
}
