//! Manual conversation evaluator for Chat.

use anyhow::{anyhow, bail, Context, Result};
use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    Json,
};
use chrono::{DateTime, Utc};
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
    created_at: i64,
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

    let total_message_count = count_conversation_messages(&pool, &id)
        .await
        .map_err(internal_error)?;
    let related_conversations = load_related_conversations(&pool, &user.user_id, &id)
        .await
        .map_err(internal_error)?;
    let result = generate_evaluation(
        &conv,
        &transcript,
        total_message_count,
        &related_conversations,
    )
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

pub(crate) async fn build_conversation_evaluator_user_prompt(
    pool: &SqlitePool,
    user_id: &str,
    conversation: &ticketing_system::Conversation,
) -> Result<String> {
    let transcript = load_evaluation_transcript(pool, &conversation.id).await?;
    if transcript.is_empty() {
        bail!("Conversation has no completed user/assistant messages to evaluate.");
    }

    let total_message_count = count_conversation_messages(pool, &conversation.id).await?;
    let related_conversations = load_related_conversations(pool, user_id, &conversation.id).await?;
    let evaluated_at = Utc::now();

    let mut vars = HashMap::new();
    vars.insert("evaluated_at".to_string(), evaluated_at.to_rfc3339());
    vars.insert(
        "conversation_metadata".to_string(),
        format_conversation_metadata(conversation, &transcript, total_message_count, evaluated_at),
    );
    vars.insert(
        "related_conversations".to_string(),
        format_related_conversations(&related_conversations, evaluated_at),
    );
    vars.insert("transcript".to_string(), format_transcript(&transcript)?);

    load_prompt("conversation-evaluator", vars).context("load conversation evaluator prompt")
}

pub(crate) async fn build_conversation_feedback_user_prompt(
    pool: &SqlitePool,
    user_id: &str,
    conversation: &ticketing_system::Conversation,
    target_message_id: Option<&str>,
) -> Result<String> {
    let transcript = load_evaluation_transcript(pool, &conversation.id).await?;
    if transcript.is_empty() {
        bail!("Conversation has no completed user/assistant messages to use for feedback.");
    }

    let total_message_count = count_conversation_messages(pool, &conversation.id).await?;
    let related_conversations = load_related_conversations(pool, user_id, &conversation.id).await?;
    let created_at = Utc::now();

    let mut vars = HashMap::new();
    vars.insert("created_at".to_string(), created_at.to_rfc3339());
    vars.insert(
        "conversation_metadata".to_string(),
        format_conversation_metadata(conversation, &transcript, total_message_count, created_at),
    );
    vars.insert(
        "related_conversations".to_string(),
        format_related_conversations(&related_conversations, created_at),
    );
    vars.insert("transcript".to_string(), format_transcript(&transcript)?);
    vars.insert(
        "target_response".to_string(),
        load_feedback_target(pool, &conversation.id, target_message_id).await?,
    );

    load_prompt("conversation-feedback-seed", vars)
        .context("load conversation feedback seed prompt")
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

async fn load_feedback_target(
    pool: &SqlitePool,
    conversation_id: &str,
    target_message_id: Option<&str>,
) -> Result<String> {
    let Some(target_message_id) = target_message_id else {
        return Ok(
            "Whole conversation feedback requested; no single message was selected.".to_string(),
        );
    };

    let row = sqlx::query_as::<_, (String, String, i64)>(
        "SELECT role, content, created_at FROM conversation_messages \
         WHERE conversation_id = ? AND id = ?",
    )
    .bind(conversation_id)
    .bind(target_message_id)
    .fetch_optional(pool)
    .await
    .context("load feedback target message")?;

    let Some((role, content, created_at)) = row else {
        bail!("Feedback target message not found in parent conversation.");
    };

    let role_label = match role.as_str() {
        "user" => "User",
        "assistant" => "Assistant",
        "forwarded" => "Forwarded Context",
        other => other,
    };

    Ok(format!(
        "{} message {} at {}:\n{}",
        role_label,
        target_message_id,
        format_unix_timestamp(created_at),
        content.trim()
    ))
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
    let rows = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, role, content, created_at FROM conversation_messages \
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
        .map(
            |(id, role, content, created_at)| EvaluationTranscriptMessage {
                id,
                role,
                content,
                created_at,
            },
        )
        .collect())
}

async fn count_conversation_messages(db: &SqlitePool, conversation_id: &str) -> Result<i64> {
    sqlx::query_scalar("SELECT COUNT(*) FROM conversation_messages WHERE conversation_id = ?")
        .bind(conversation_id)
        .fetch_one(db)
        .await
        .context("count conversation messages")
}

async fn load_related_conversations(
    db: &SqlitePool,
    user_id: &str,
    conversation_id: &str,
) -> Result<Vec<ticketing_system::Conversation>> {
    let conversations = ticketing_system::conversations::list_conversations(
        db,
        None,
        Some(user_id),
        None,
        Some("open,waiting"),
        Some(25),
        None,
    )
    .await
    .context("load related open conversations")?;

    Ok(conversations
        .into_iter()
        .filter(|conversation| conversation.id != conversation_id)
        .collect())
}

async fn generate_evaluation(
    conversation: &ticketing_system::Conversation,
    transcript: &[EvaluationTranscriptMessage],
    total_message_count: i64,
    related_conversations: &[ticketing_system::Conversation],
) -> Result<String> {
    let mut system_vars = HashMap::new();
    system_vars.insert("EVALUATION_CONTEXT".to_string(), String::new());
    let system_prompt = load_prompt("conversation-evaluator-system", system_vars)
        .context("load conversation evaluator system prompt")?;
    let mut vars = HashMap::new();
    let evaluated_at = Utc::now();
    vars.insert("evaluated_at".to_string(), evaluated_at.to_rfc3339());
    vars.insert(
        "conversation_metadata".to_string(),
        format_conversation_metadata(conversation, transcript, total_message_count, evaluated_at),
    );
    vars.insert(
        "related_conversations".to_string(),
        format_related_conversations(related_conversations, evaluated_at),
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

fn format_conversation_metadata(
    conversation: &ticketing_system::Conversation,
    transcript: &[EvaluationTranscriptMessage],
    total_message_count: i64,
    evaluated_at: DateTime<Utc>,
) -> String {
    let user_count = transcript
        .iter()
        .filter(|message| message.role == "user")
        .count();
    let assistant_count = transcript
        .iter()
        .filter(|message| message.role == "assistant")
        .count();
    let transcript_count = transcript.len();
    let last_transcript_message = transcript.last().map(|message| {
        format!(
            "{} ({})",
            format_unix_timestamp(message.created_at),
            elapsed_phrase_from_unix(message.created_at, evaluated_at)
        )
    });

    [
        format!(
            "Title: {}",
            if conversation.title.trim().is_empty() {
                "Untitled"
            } else {
                conversation.title.trim()
            }
        ),
        format!("Conversation ID: {}", conversation.id),
        format!("Organization: {}", conversation.organization),
        format!(
            "Agent: {}",
            conversation
                .agent
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("unknown")
        ),
        format!("Current list status: {}", conversation.status),
        format!(
            "Started at: {}",
            format_iso_timestamp(&conversation.started_at, evaluated_at)
        ),
        format!(
            "Updated at: {}",
            format_iso_timestamp(&conversation.updated_at, evaluated_at)
        ),
        format!("Total stored messages: {}", total_message_count),
        format!(
            "Evaluation transcript messages: {} ({} user, {} assistant final)",
            transcript_count, user_count, assistant_count
        ),
        format!(
            "Last transcript message: {}",
            last_transcript_message.unwrap_or_else(|| "none".to_string())
        ),
    ]
    .join("\n")
}

fn format_related_conversations(
    conversations: &[ticketing_system::Conversation],
    evaluated_at: DateTime<Utc>,
) -> String {
    if conversations.is_empty() {
        return "No other open or waiting conversations are visible for this user.".to_string();
    }

    conversations
        .iter()
        .map(|conversation| {
            let title = if conversation.title.trim().is_empty() {
                "Untitled"
            } else {
                conversation.title.trim()
            };
            let agent = conversation
                .agent
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("unknown");
            let active = if conversation.is_active == Some(true) {
                ", active now"
            } else {
                ""
            };
            let message_count = conversation
                .message_count
                .map(|count| count.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            format!(
                "- {} [{}] org={}, agent={}, messages={}, updated={}{}",
                title,
                conversation.status,
                conversation.organization,
                agent,
                message_count,
                format_iso_timestamp(&conversation.updated_at, evaluated_at),
                active
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
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

fn format_unix_timestamp(timestamp: i64) -> String {
    DateTime::<Utc>::from_timestamp(timestamp, 0)
        .map(|datetime| datetime.to_rfc3339())
        .unwrap_or_else(|| format!("invalid unix timestamp {}", timestamp))
}

fn format_iso_timestamp(value: &str, evaluated_at: DateTime<Utc>) -> String {
    match DateTime::parse_from_rfc3339(value) {
        Ok(datetime) => {
            let utc = datetime.with_timezone(&Utc);
            format!(
                "{} ({})",
                utc.to_rfc3339(),
                elapsed_phrase(utc, evaluated_at)
            )
        }
        Err(_) => value.to_string(),
    }
}

fn elapsed_phrase_from_unix(timestamp: i64, evaluated_at: DateTime<Utc>) -> String {
    DateTime::<Utc>::from_timestamp(timestamp, 0)
        .map(|datetime| elapsed_phrase(datetime, evaluated_at))
        .unwrap_or_else(|| "unknown age".to_string())
}

fn elapsed_phrase(instant: DateTime<Utc>, evaluated_at: DateTime<Utc>) -> String {
    let seconds = evaluated_at.signed_duration_since(instant).num_seconds();
    if seconds < 0 {
        return format!("in {}", human_duration(seconds.saturating_abs()));
    }
    format!("{} ago", human_duration(seconds))
}

fn human_duration(seconds: i64) -> String {
    if seconds < 60 {
        return "less than 1 minute".to_string();
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return pluralized(minutes, "minute");
    }
    let hours = minutes / 60;
    if hours < 48 {
        return pluralized(hours, "hour");
    }
    let days = hours / 24;
    if days < 60 {
        return pluralized(days, "day");
    }
    let months = days / 30;
    if months < 24 {
        return pluralized(months, "month");
    }
    pluralized(days / 365, "year")
}

fn pluralized(count: i64, unit: &str) -> String {
    if count == 1 {
        format!("1 {}", unit)
    } else {
        format!("{} {}s", count, unit)
    }
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
