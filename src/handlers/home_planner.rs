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
pub struct HomePlannerRequest {
    pub message: String,
    pub conversation_id: Option<String>,
    pub images: Option<Vec<ChatImageData>>,
}

/// Build config (no data in system prompt) and load home context separately
async fn load_home_context(db: &SqlitePool, user_id: &str) -> Option<String> {
    match ticketing_system::home_context::list_contexts(db, user_id).await {
        Ok(contexts) if !contexts.is_empty() => {
            let parts: Vec<String> = contexts
                .iter()
                .map(|ctx| format!("### {}\n{}", ctx.key, ctx.content))
                .collect();
            Some(parts.join("\n\n"))
        }
        _ => None,
    }
}

/// POST /api/home-planner/chat
pub async fn home_planner_chat(
    State(db): State<Arc<SqlitePool>>,
    State(manager): State<Arc<ChatClientManager>>,
    Extension(user): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Json(req): Json<HomePlannerRequest>,
) -> Response {
    tracing::info!("=== HOME_PLANNER_CHAT START === user={}", user.user_id);

    let client_id = match chat_stream::extract_client_id(&headers) {
        Ok(v) => v,
        Err(e) => return chat_stream::malformed_idempotency_key_response(e),
    };

    // Look up user's display name
    let user_name = match ticketing_system::users::get_user(&db, &user.user_id).await {
        Ok(Some(u)) => u.name,
        _ => user.user_id.clone(),
    };

    let mut prompt_vars = HashMap::new();
    prompt_vars.insert("USER_NAME".to_string(), user_name);
    prompt_vars.insert("USER_ID".to_string(), user.user_id.clone());

    let config = ChatConfig {
        agent_type: AgentType::HomePlanner,
        runtime: ChatRuntime::CodexAppServer,
        prompt_name: "home-planner",
        working_dir: PathBuf::from("/Users/jarvisgpt/projects"),
        prompt_vars,
    };

    // For new conversations (no existing session), prepend home context to user message.
    // For resumed conversations, context is already in the conversation history.
    let message = if let Some(ref conv_id) = req.conversation_id {
        let has_session = ticketing_system::conversations::get_conversation(&db, conv_id, false)
            .await
            .ok()
            .flatten()
            .and_then(|c| c.session_id)
            .is_some();

        if has_session {
            req.message
        } else if let Some(ctx) = load_home_context(&db, &user.user_id).await {
            format!(
                "<home_context>\n{}\n</home_context>\n\n{}",
                ctx, req.message
            )
        } else {
            req.message
        }
    } else {
        req.message
    };

    chat_stream::chat(
        db,
        manager,
        message,
        req.conversation_id,
        config,
        user.user_id,
        req.images,
        client_id,
    )
    .await
}
