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
pub struct ConversationAgentChatRequest {
    pub message: String,
    pub conversation_id: Option<String>,
    pub images: Option<Vec<ChatImageData>>,
    pub codex_options: Option<ChatCodexOptions>,
}

fn support_agent_config(
    agent_type: AgentType,
    prompt_name: &'static str,
    prompt_vars: HashMap<String, String>,
) -> ChatConfig {
    ChatConfig {
        agent_type: agent_type.clone(),
        runtime: ChatRuntime::CodexAppServer,
        prompt_name,
        working_dir: PathBuf::from("/Users/jarvisgpt/projects"),
        prompt_vars,
        codex_options: ChatCodexOptions::default_for_agent(&agent_type),
    }
}

fn support_agent_prompt_vars(agent_type: &AgentType) -> HashMap<String, String> {
    let mut vars = HashMap::new();
    match agent_type {
        AgentType::ConversationEvaluator => {
            vars.insert("EVALUATION_CONTEXT".to_string(), String::new());
        }
        AgentType::Feedback => {
            vars.insert("FEEDBACK_CONTEXT".to_string(), String::new());
        }
        _ => {}
    }
    vars
}

async fn run_support_agent_chat(
    db: Arc<SqlitePool>,
    manager: Arc<ChatClientManager>,
    user: AuthenticatedUser,
    headers: HeaderMap,
    req: ConversationAgentChatRequest,
    agent_type: AgentType,
    prompt_name: &'static str,
    submit: bool,
) -> Response {
    let client_id = match chat_stream::extract_client_id(&headers) {
        Ok(v) => v,
        Err(e) => return chat_stream::malformed_idempotency_key_response(e),
    };
    let config = match chat_stream::apply_codex_options(
        support_agent_config(
            agent_type.clone(),
            prompt_name,
            support_agent_prompt_vars(&agent_type),
        ),
        req.codex_options.clone(),
    )
    .await
    {
        Ok(config) => config,
        Err(response) => return response,
    };

    if submit {
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
    } else {
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
}

pub async fn conversation_evaluator_chat(
    State(db): State<Arc<SqlitePool>>,
    State(manager): State<Arc<ChatClientManager>>,
    Extension(user): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Json(req): Json<ConversationAgentChatRequest>,
) -> Response {
    run_support_agent_chat(
        db,
        manager,
        user,
        headers,
        req,
        AgentType::ConversationEvaluator,
        "conversation-evaluator-system",
        false,
    )
    .await
}

pub async fn conversation_evaluator_chat_submit(
    State(db): State<Arc<SqlitePool>>,
    State(manager): State<Arc<ChatClientManager>>,
    Extension(user): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Json(req): Json<ConversationAgentChatRequest>,
) -> Response {
    run_support_agent_chat(
        db,
        manager,
        user,
        headers,
        req,
        AgentType::ConversationEvaluator,
        "conversation-evaluator-system",
        true,
    )
    .await
}

pub async fn feedback_chat(
    State(db): State<Arc<SqlitePool>>,
    State(manager): State<Arc<ChatClientManager>>,
    Extension(user): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Json(req): Json<ConversationAgentChatRequest>,
) -> Response {
    run_support_agent_chat(
        db,
        manager,
        user,
        headers,
        req,
        AgentType::Feedback,
        "feedback",
        false,
    )
    .await
}

pub async fn feedback_chat_submit(
    State(db): State<Arc<SqlitePool>>,
    State(manager): State<Arc<ChatClientManager>>,
    Extension(user): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Json(req): Json<ConversationAgentChatRequest>,
) -> Response {
    run_support_agent_chat(
        db,
        manager,
        user,
        headers,
        req,
        AgentType::Feedback,
        "feedback",
        true,
    )
    .await
}
