//! Manual conversation evaluator for Chat.

use anyhow::{anyhow, bail, Context, Result};
use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    Json,
};
use serde::Serialize;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::Arc;

use crate::agents::prompts::load_prompt;
use crate::agents::run_oneshot;
use crate::auth_middleware::AuthenticatedUser;

#[derive(Serialize)]
pub struct ConversationEvaluationResponse {
    pub id: String,
    pub conversation_id: String,
    pub source_message_count: i64,
    pub source_last_message_id: Option<String>,
    pub result: String,
    pub created_at: i64,
}

struct EvaluationTranscriptMessage {
    id: String,
    role: String,
    content: String,
}

/// GET /api/conversations/:id/evaluations
pub async fn list_conversation_evaluations(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
) -> Result<Json<Vec<ConversationEvaluationResponse>>, (StatusCode, String)> {
    verify_conversation_owner(&pool, &user.user_id, &id).await?;

    let evaluations = ticketing_system::conversation_evaluations::list_for_conversation(&pool, &id)
        .await
        .map_err(internal_error)?;

    Ok(Json(
        evaluations
            .into_iter()
            .map(ConversationEvaluationResponse::from)
            .collect(),
    ))
}

/// POST /api/conversations/:id/evaluations
pub async fn create_conversation_evaluation(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
) -> Result<Json<ConversationEvaluationResponse>, (StatusCode, String)> {
    let conv = verify_conversation_owner(&pool, &user.user_id, &id).await?;

    if conversation_has_active_turn(&pool, &id)
        .await
        .map_err(internal_error)?
    {
        return Err((
            StatusCode::CONFLICT,
            "Conversation is still processing; wait for the active turn to finish before evaluating."
                .to_string(),
        ));
    }

    let transcript = load_evaluation_transcript(&pool, &id)
        .await
        .map_err(internal_error)?;
    if transcript.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Conversation has no completed user/assistant messages to evaluate.".to_string(),
        ));
    }

    let result = generate_evaluation(&conv.title, conv.agent.as_deref(), &transcript)
        .await
        .map_err(internal_error)?;
    let source_last_message_id = transcript.last().map(|message| message.id.clone());
    let source_message_count = transcript.len() as i64;

    let evaluation = ticketing_system::conversation_evaluations::insert(
        &pool,
        &id,
        ticketing_system::NewConversationEvaluation {
            user_id: user.user_id,
            source_message_count,
            source_last_message_id,
            result,
        },
    )
    .await
    .map_err(internal_error)?;

    Ok(Json(ConversationEvaluationResponse::from(evaluation)))
}

async fn verify_conversation_owner(
    pool: &SqlitePool,
    user_id: &str,
    conversation_id: &str,
) -> Result<ticketing_system::Conversation, (StatusCode, String)> {
    let conv = ticketing_system::conversations::get_conversation(pool, conversation_id, false)
        .await
        .map_err(internal_error)?
        .ok_or((StatusCode::NOT_FOUND, "Conversation not found".to_string()))?;
    if conv.user_id != user_id {
        return Err((StatusCode::NOT_FOUND, "Conversation not found".to_string()));
    }
    Ok(conv)
}

async fn conversation_has_active_turn(db: &SqlitePool, conversation_id: &str) -> Result<bool> {
    let active_jobs = ticketing_system::conversation_turn_jobs::has_active_job_for_conversation(
        db,
        conversation_id,
    )
    .await?;
    if active_jobs {
        return Ok(true);
    }

    let active_runner_turns: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_runner_turns \
         WHERE conversation_id = ? AND status IN ('queued', 'running')",
    )
    .bind(conversation_id)
    .fetch_one(db)
    .await?;

    Ok(active_runner_turns > 0)
}

async fn load_evaluation_transcript(
    db: &SqlitePool,
    conversation_id: &str,
) -> Result<Vec<EvaluationTranscriptMessage>> {
    let rows = sqlx::query_as::<_, (String, String, String)>(
        "SELECT id, role, content FROM conversation_messages \
         WHERE conversation_id = ? \
           AND role IN ('user', 'assistant') \
           AND trim(content) != '' \
         ORDER BY message_index ASC",
    )
    .bind(conversation_id)
    .fetch_all(db)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(id, role, content)| EvaluationTranscriptMessage { id, role, content })
        .collect())
}

async fn generate_evaluation(
    title: &str,
    agent: Option<&str>,
    transcript: &[EvaluationTranscriptMessage],
) -> Result<String> {
    let system_prompt = load_prompt("conversation-evaluator-system", HashMap::new())
        .context("load conversation evaluator system prompt")?;
    let mut vars = HashMap::new();
    vars.insert(
        "conversation_title".to_string(),
        if title.trim().is_empty() {
            "Untitled".to_string()
        } else {
            title.trim().to_string()
        },
    );
    vars.insert(
        "conversation_agent".to_string(),
        agent
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("unknown")
            .to_string(),
    );
    vars.insert("transcript".to_string(), format_transcript(transcript)?);

    let prompt = load_prompt("conversation-evaluator", vars)
        .context("load conversation evaluator prompt")?;
    let working_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let result = run_oneshot(None, &system_prompt, &working_dir, prompt)
        .await
        .map_err(|e| anyhow!(e))?;

    let text = result.text.trim();
    if text.is_empty() {
        bail!("conversation evaluator returned empty output");
    }
    Ok(text.to_string())
}

fn format_transcript(transcript: &[EvaluationTranscriptMessage]) -> Result<String> {
    let mut formatted = String::new();
    for message in transcript {
        let role = match message.role.as_str() {
            "user" => "User",
            "assistant" => "Assistant Final",
            other => bail!("unsupported transcript role: {}", other),
        };
        formatted.push_str(role);
        formatted.push_str(":\n");
        formatted.push_str(message.content.trim());
        formatted.push_str("\n\n");
    }
    Ok(formatted.trim().to_string())
}

fn internal_error(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

impl From<ticketing_system::ConversationEvaluation> for ConversationEvaluationResponse {
    fn from(evaluation: ticketing_system::ConversationEvaluation) -> Self {
        Self {
            id: evaluation.id,
            conversation_id: evaluation.conversation_id,
            source_message_count: evaluation.source_message_count,
            source_last_message_id: evaluation.source_last_message_id,
            result: evaluation.result,
            created_at: evaluation.created_at,
        }
    }
}
