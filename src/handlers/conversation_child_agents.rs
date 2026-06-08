use anyhow::{Context, Result};
use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::Arc;
use ticketing_system::{
    checkpoints, conversation_turn_jobs, conversations, Conversation, ConversationHierarchyScope,
    CreateChildConversationRequest,
};

use crate::agents::AgentType;
use crate::auth_middleware::AuthenticatedUser;
use crate::handlers::chat_stream::{self, ChatCodexOptions, ChatRuntime};

#[derive(Debug, Deserialize)]
pub struct LaunchConversationChildAgentRequest {
    pub kind: String,
    #[serde(default)]
    pub target_message_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct LaunchConversationChildAgentResponse {
    pub parent: Conversation,
    pub child: Conversation,
    pub did_enqueue_initial_turn: bool,
}

#[derive(Clone, Copy, Debug)]
enum ChildAgentKind {
    Evaluator,
    Feedback,
}

impl ChildAgentKind {
    fn parse(value: &str) -> Result<Self, (StatusCode, String)> {
        match value.trim() {
            "evaluator" | "evaluation" | "conversation-evaluator" => Ok(Self::Evaluator),
            "feedback" => Ok(Self::Feedback),
            other => Err((
                StatusCode::BAD_REQUEST,
                format!("Unsupported child agent kind: {other}"),
            )),
        }
    }

    fn agent_key(self) -> &'static str {
        match self {
            Self::Evaluator => "conversation-evaluator",
            Self::Feedback => "feedback",
        }
    }

    fn agent_type(self) -> AgentType {
        match self {
            Self::Evaluator => AgentType::ConversationEvaluator,
            Self::Feedback => AgentType::Feedback,
        }
    }

    fn conversation_type(self) -> &'static str {
        match self {
            Self::Evaluator => "evaluation",
            Self::Feedback => "feedback",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Evaluator => "Evaluator",
            Self::Feedback => "Feedback",
        }
    }

    fn child_sort_order(self) -> i32 {
        match self {
            Self::Evaluator => 0,
            Self::Feedback => 1,
        }
    }

    fn prompt_name(self) -> &'static str {
        match self {
            Self::Evaluator => "conversation-evaluator-system",
            Self::Feedback => "feedback",
        }
    }

    fn visible_initial_message(self) -> &'static str {
        match self {
            Self::Evaluator => "Run the evaluator for the parent conversation.",
            Self::Feedback => "Start a feedback review for the parent conversation.",
        }
    }
}

/// Create or reuse an evaluator/feedback child conversation and queue its first
/// seeded turn when the child is still empty.
pub async fn launch_conversation_child_agent(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
    Json(req): Json<LaunchConversationChildAgentRequest>,
) -> Result<Json<LaunchConversationChildAgentResponse>, (StatusCode, String)> {
    let kind = ChildAgentKind::parse(&req.kind)?;
    let parent = resolve_launch_parent(&pool, &user.user_id, &id).await?;

    if conversation_has_active_turn(&pool, &parent.id)
        .await
        .map_err(internal_error)?
    {
        return Err((
            StatusCode::CONFLICT,
            "Parent conversation is still processing; wait for the active turn to finish first."
                .to_string(),
        ));
    }

    let mut child = match find_existing_child(&pool, &user.user_id, &parent, kind)
        .await
        .map_err(internal_error)?
    {
        Some(child) => child,
        None => create_child(&pool, &user.user_id, &parent, kind)
            .await
            .map_err(internal_error)?,
    };

    let did_enqueue_initial_turn = maybe_enqueue_initial_turn(
        &pool,
        &user.user_id,
        &parent,
        &mut child,
        kind,
        req.target_message_id.as_deref(),
    )
    .await
    .map_err(internal_error)?;

    let parent = conversations::get_conversation(&pool, &parent.id, false)
        .await
        .map_err(internal_error)?
        .ok_or((StatusCode::NOT_FOUND, "Conversation not found".to_string()))?;

    Ok(Json(LaunchConversationChildAgentResponse {
        parent,
        child,
        did_enqueue_initial_turn,
    }))
}

async fn resolve_launch_parent(
    pool: &SqlitePool,
    user_id: &str,
    id: &str,
) -> Result<Conversation, (StatusCode, String)> {
    let conversation = conversations::get_conversation(pool, id, false)
        .await
        .map_err(internal_error)?
        .ok_or((StatusCode::NOT_FOUND, "Conversation not found".to_string()))?;
    if conversation.user_id != user_id {
        return Err((StatusCode::NOT_FOUND, "Conversation not found".to_string()));
    }

    let Some(parent_id) = conversation.parent_conversation_id.as_deref() else {
        return Ok(conversation);
    };

    let parent = conversations::get_conversation(pool, parent_id, false)
        .await
        .map_err(internal_error)?
        .ok_or((
            StatusCode::NOT_FOUND,
            "Parent conversation not found".to_string(),
        ))?;
    if parent.user_id != user_id {
        return Err((
            StatusCode::NOT_FOUND,
            "Parent conversation not found".to_string(),
        ));
    }
    Ok(parent)
}

async fn find_existing_child(
    pool: &SqlitePool,
    user_id: &str,
    parent: &Conversation,
    kind: ChildAgentKind,
) -> Result<Option<Conversation>> {
    let children = conversations::list_conversations_with_hierarchy(
        pool,
        Some(parent.organization.as_str()),
        Some(user_id),
        Some(kind.agent_key()),
        Some("open,waiting"),
        None,
        None,
        ConversationHierarchyScope::Children,
        Some(parent.id.as_str()),
    )
    .await?;

    Ok(children.into_iter().find(|child| {
        child.parent_conversation_id.as_deref() == Some(parent.id.as_str())
            && child.agent.as_deref() == Some(kind.agent_key())
            && child.conversation_type.as_deref() == Some(kind.conversation_type())
    }))
}

async fn create_child(
    pool: &SqlitePool,
    user_id: &str,
    parent: &Conversation,
    kind: ChildAgentKind,
) -> Result<Conversation> {
    let children = conversations::create_child_conversations(
        pool,
        user_id,
        &parent.id,
        vec![CreateChildConversationRequest {
            title: kind.title().to_string(),
            agent: Some(kind.agent_key().to_string()),
            conversation_type: Some(kind.conversation_type().to_string()),
            child_sort_order: Some(kind.child_sort_order()),
        }],
    )
    .await?;

    children
        .into_iter()
        .next()
        .context("child conversation was not created")
}

async fn maybe_enqueue_initial_turn(
    pool: &SqlitePool,
    user_id: &str,
    parent: &Conversation,
    child: &mut Conversation,
    kind: ChildAgentKind,
    target_message_id: Option<&str>,
) -> Result<bool> {
    if count_conversation_messages(pool, &child.id).await? > 0 {
        return Ok(false);
    }
    if conversation_has_active_turn(pool, &child.id).await? {
        child.is_active = Some(true);
        return Ok(false);
    }

    ensure_parent_has_transcript(pool, &parent.id, kind).await?;

    enqueue_child_turn(pool, user_id, &child.id, kind, target_message_id).await?;
    child.is_active = Some(true);
    Ok(true)
}

async fn enqueue_child_turn(
    pool: &SqlitePool,
    user_id: &str,
    child_id: &str,
    kind: ChildAgentKind,
    target_message_id: Option<&str>,
) -> Result<()> {
    checkpoints::upsert_checkpoint(pool, child_id, "queued", 0)
        .await
        .context("queue child-agent checkpoint")?;

    let agent_type = kind.agent_type();
    let codex_options = ChatCodexOptions::default_for_agent(&agent_type);
    let mut prompt_vars = HashMap::new();
    prompt_vars.insert(
        "SUPPORT_CONTEXT_TOOL_ARGS".to_string(),
        support_context_tool_args(child_id, user_id, target_message_id),
    );
    let payload = conversation_turn_jobs::ConversationTurnJobPayload {
        user_id: user_id.to_string(),
        message: kind.visible_initial_message().to_string(),
        agent_type: agent_type.as_str().to_string(),
        runtime: ChatRuntime::CodexAppServer.as_job_runtime().to_string(),
        prompt_name: kind.prompt_name().to_string(),
        working_dir: "/Users/jarvisgpt/projects".to_string(),
        prompt_vars: chat_stream::encode_codex_options_for_job(prompt_vars, &codex_options),
        images_json: None,
        client_id: None,
    };

    conversation_turn_jobs::enqueue_job(pool, child_id, payload)
        .await
        .context("enqueue child-agent turn")?;
    super::conversations::publish_conversation_run_status(pool, child_id)
        .await
        .context("publish child-agent run status")?;
    Ok(())
}

async fn ensure_parent_has_transcript(
    pool: &SqlitePool,
    parent_id: &str,
    kind: ChildAgentKind,
) -> Result<()> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM conversation_messages \
         WHERE conversation_id = ? \
           AND role IN ('user', 'assistant') \
           AND trim(content) != ''",
    )
    .bind(parent_id)
    .fetch_one(pool)
    .await
    .context("count parent transcript messages")?;

    if count == 0 {
        let label = match kind {
            ChildAgentKind::Evaluator => "evaluate",
            ChildAgentKind::Feedback => "use for feedback",
        };
        anyhow::bail!("Conversation has no completed user/assistant messages to {label}.");
    }

    Ok(())
}

fn support_context_tool_args(
    child_id: &str,
    user_id: &str,
    target_message_id: Option<&str>,
) -> String {
    let mut args = serde_json::json!({
        "child_conversation_id": child_id,
        "user_id": user_id,
    });
    if let Some(target_message_id) = target_message_id {
        args["target_message_id"] = serde_json::Value::String(target_message_id.to_string());
    }
    args.to_string()
}

async fn count_conversation_messages(pool: &SqlitePool, conversation_id: &str) -> Result<i64> {
    sqlx::query_scalar("SELECT COUNT(*) FROM conversation_messages WHERE conversation_id = ?")
        .bind(conversation_id)
        .fetch_one(pool)
        .await
        .context("count conversation messages")
}

async fn conversation_has_active_turn(pool: &SqlitePool, conversation_id: &str) -> Result<bool> {
    let active_jobs =
        conversation_turn_jobs::has_active_job_for_conversation(pool, conversation_id)
            .await
            .context("check active child-agent jobs")?;
    if active_jobs {
        return Ok(true);
    }

    let active_runner_turns: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_runner_turns \
         WHERE conversation_id = ? AND status IN ('queued', 'running')",
    )
    .bind(conversation_id)
    .fetch_one(pool)
    .await
    .context("check active child-agent runner turns")?;

    Ok(active_runner_turns > 0)
}

fn internal_error(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}
