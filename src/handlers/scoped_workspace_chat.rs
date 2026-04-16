use axum::{extract::{Extension, State}, Json};
use std::sync::Arc;
use std::path::PathBuf;
use std::collections::HashMap;
use sqlx::SqlitePool;
use serde::Deserialize;

use crate::agents::AgentType;
use crate::auth_middleware::AuthenticatedUser;
use super::chat_stream::{self, ChatConfig, ChatImageData, SseStream};
use super::chat_client_manager::ChatClientManager;

#[derive(Debug, Deserialize)]
pub struct ScopedWorkspaceChatRequest {
    pub message: String,
    pub conversation_id: Option<String>,
    pub images: Option<Vec<ChatImageData>>,
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
    Json(req): Json<ScopedWorkspaceChatRequest>,
) -> SseStream {
    tracing::info!(
        "=== SCOPED_WORKSPACE_CHAT START === user={}",
        user.user_id
    );

    let display_name = lookup_display_name(&db, &user.user_id).await;

    let mut prompt_vars = HashMap::new();
    prompt_vars.insert("USER_ID".to_string(), user.user_id.clone());
    prompt_vars.insert("USER_NAME".to_string(), display_name);

    let config = ChatConfig {
        agent_type: AgentType::ScopedWorkspace,
        prompt_name: "scoped-workspace",
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
    )
}

async fn lookup_display_name(db: &SqlitePool, user_id: &str) -> String {
    match ticketing_system::users::get_user(db, user_id).await {
        Ok(Some(user)) => user.name,
        _ => user_id.to_string(),
    }
}
