use axum::{extract::State, Json};
use std::sync::Arc;
use std::path::PathBuf;
use std::collections::HashMap;
use sqlx::SqlitePool;
use serde::Deserialize;

use crate::agents::AgentType;
use super::chat_stream::{self, ChatConfig, SseStream};

#[derive(Debug, Deserialize)]
pub struct LifePlannerRequest {
    pub message: String,
    pub session_id: Option<String>,
    pub conversation_id: Option<String>,
}

fn config() -> ChatConfig {
    ChatConfig {
        agent_type: AgentType::LifePlanner,
        prompt_name: "life-planner",
        working_dir: PathBuf::from("/Users/jarvisgpt/projects"),
        prompt_vars: HashMap::new(),
    }
}

/// Build the context-injected message by prepending all life context entries
async fn inject_life_context(db: &SqlitePool, message: &str) -> String {
    match ticketing_system::life_context::list_contexts(db).await {
        Ok(contexts) if !contexts.is_empty() => {
            let mut parts = vec!["[Life Context]".to_string()];
            for ctx in &contexts {
                parts.push(format!("\n## {}\n{}", ctx.key, ctx.content));
            }
            parts.push("---".to_string());
            parts.push(String::new());
            parts.push(message.to_string());
            parts.join("\n")
        }
        _ => message.to_string(),
    }
}

/// POST /api/life-planner/chat
pub async fn life_planner_chat(
    State(db): State<Arc<SqlitePool>>,
    Json(req): Json<LifePlannerRequest>,
) -> SseStream {
    tracing::info!("=== LIFE_PLANNER_CHAT START ===");
    let injected_message = inject_life_context(&db, &req.message).await;
    chat_stream::chat(
        db,
        injected_message,
        req.session_id,
        req.conversation_id,
        config(),
    )
}

/// POST /api/life-planner/resume
pub async fn life_planner_resume(
    State(db): State<Arc<SqlitePool>>,
    Json(req): Json<LifePlannerRequest>,
) -> SseStream {
    tracing::info!("=== LIFE_PLANNER_RESUME START ===");
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
