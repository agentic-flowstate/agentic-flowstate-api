//! Conversation-scoped next-action suggestions for the mobile Chat composer.

use anyhow::{anyhow, bail, Context, Result};
use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use crate::agents::prompts::load_prompt;
use crate::agents::run_oneshot;
use crate::auth_middleware::AuthenticatedUser;
use crate::observability::next_actions::{
    record_generation, record_storage, NextActionGenerationStatus,
};

const NEXT_ACTION_ICONS: [&str; 3] = ["sparkles", "checklist", "questionmark.bubble"];

#[derive(Serialize)]
pub struct ConversationNextActionResponse {
    pub id: String,
    pub label: String,
    pub icon: String,
    pub message: String,
    pub sort_order: i64,
}

#[derive(Deserialize)]
struct GeneratedNextActions {
    suggestions: Vec<GeneratedNextAction>,
}

#[derive(Deserialize)]
struct GeneratedNextAction {
    label: String,
    message: String,
}

struct NextActionPromptContext {
    first_user_message: String,
    triggering_user_message: String,
    assistant_output: String,
}

/// GET /api/conversations/:id/next-actions
pub async fn list_conversation_next_actions(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
) -> Result<Json<Vec<ConversationNextActionResponse>>, (StatusCode, String)> {
    let conv = ticketing_system::conversations::get_conversation(&pool, &id, false)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Conversation not found".to_string()))?;
    if conv.user_id != user.user_id {
        return Err((StatusCode::NOT_FOUND, "Conversation not found".to_string()));
    }

    let actions = ticketing_system::conversation_next_actions::list_for_conversation(&pool, &id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(
        actions
            .into_iter()
            .map(|action| ConversationNextActionResponse {
                id: action.id,
                label: action.label,
                icon: action.icon,
                message: action.message,
                sort_order: action.sort_order,
            })
            .collect(),
    ))
}

pub fn spawn_generation(
    db: Arc<SqlitePool>,
    user_id: String,
    conversation_id: String,
    source_message_id: String,
    agent_name: String,
    triggering_user_message: String,
    assistant_output: String,
) {
    tokio::spawn(async move {
        let started = Instant::now();
        tracing::info!(
            "[NEXT-ACTIONS] Starting generation conv={} msg={} agent={}",
            conversation_id,
            source_message_id,
            agent_name
        );

        match generate_and_store(
            db,
            &user_id,
            &conversation_id,
            &source_message_id,
            &triggering_user_message,
            &assistant_output,
        )
        .await
        {
            Ok(suggestion_count) => {
                let status = if suggestion_count == 0 {
                    NextActionGenerationStatus::SkippedEmptyOutput
                } else {
                    NextActionGenerationStatus::Success
                };
                record_generation(
                    &conversation_id,
                    &source_message_id,
                    &agent_name,
                    status,
                    elapsed_ms(started),
                    suggestion_count,
                );
            }
            Err(error) => {
                record_generation(
                    &conversation_id,
                    &source_message_id,
                    &agent_name,
                    NextActionGenerationStatus::Error,
                    elapsed_ms(started),
                    0,
                );
                tracing::warn!(
                    "[NEXT-ACTIONS] Failed to generate suggestions for conv={} msg={}: {}",
                    conversation_id,
                    source_message_id,
                    error
                );
            }
        }
    });
}

async fn generate_and_store(
    db: Arc<SqlitePool>,
    user_id: &str,
    conversation_id: &str,
    source_message_id: &str,
    triggering_user_message: &str,
    assistant_output: &str,
) -> Result<usize> {
    let output = assistant_output.trim();
    if output.is_empty() {
        return Ok(0);
    }
    let triggering_user_message = triggering_user_message.trim();
    if triggering_user_message.is_empty() {
        bail!("triggering user message is empty");
    }

    let conv = ticketing_system::conversations::get_conversation(&db, conversation_id, false)
        .await?
        .ok_or_else(|| anyhow!("conversation not found"))?;
    if conv.user_id != user_id {
        bail!("conversation does not belong to user");
    }

    let system_prompt = load_prompt("conversation-next-actions-system", HashMap::new())
        .context("load next-actions system prompt")?;
    let prompt_context =
        load_prompt_context(&db, conversation_id, triggering_user_message, output).await?;
    tracing::info!(
        "[NEXT-ACTIONS] Loaded prompt context conv={} msg={} first_user_chars={} triggering_user_chars={} assistant_output_chars={}",
        conversation_id,
        source_message_id,
        prompt_context.first_user_message.chars().count(),
        prompt_context.triggering_user_message.chars().count(),
        prompt_context.assistant_output.chars().count()
    );

    let mut vars = HashMap::new();
    vars.insert(
        "first_user_message".to_string(),
        prompt_context.first_user_message,
    );
    vars.insert(
        "triggering_user_message".to_string(),
        prompt_context.triggering_user_message,
    );
    vars.insert(
        "assistant_output".to_string(),
        prompt_context.assistant_output,
    );
    let prompt =
        load_prompt("conversation-next-actions", vars).context("load next-actions user prompt")?;

    let working_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let result = run_oneshot(None, &system_prompt, &working_dir, prompt)
        .await
        .map_err(|e| anyhow!(e))?;

    let generated = parse_generated_actions(&result.text)?;
    let actions = normalize_generated_actions(generated)?;
    let replacement = ticketing_system::conversation_next_actions::replace_for_conversation(
        &db,
        conversation_id,
        source_message_id,
        actions,
    )
    .await?;
    let suggestion_count = replacement.inserted.len();
    record_storage(
        conversation_id,
        source_message_id,
        replacement.deleted_count,
        suggestion_count,
    );

    Ok(suggestion_count)
}

async fn load_prompt_context(
    db: &SqlitePool,
    conversation_id: &str,
    triggering_user_message: &str,
    assistant_output: &str,
) -> Result<NextActionPromptContext> {
    let first_user_message = sqlx::query_scalar::<_, String>(
        "SELECT content FROM conversation_messages \
         WHERE conversation_id = ? AND role = 'user' \
         ORDER BY message_index ASC LIMIT 1",
    )
    .bind(conversation_id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| anyhow!("conversation has no user messages"))?
    .trim()
    .to_string();
    if first_user_message.is_empty() {
        bail!("first user message is empty");
    }

    Ok(NextActionPromptContext {
        first_user_message,
        triggering_user_message: triggering_user_message.to_string(),
        assistant_output: assistant_output.to_string(),
    })
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

fn parse_generated_actions(text: &str) -> Result<GeneratedNextActions> {
    let json = extract_json_object(text)?;
    serde_json::from_str(json).context("parse next-actions JSON")
}

fn extract_json_object(text: &str) -> Result<&str> {
    let trimmed = text.trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        return Ok(trimmed);
    }
    let start = trimmed
        .find('{')
        .ok_or_else(|| anyhow!("next-actions output did not contain a JSON object"))?;
    let end = trimmed
        .rfind('}')
        .ok_or_else(|| anyhow!("next-actions output did not contain a complete JSON object"))?;
    if end < start {
        bail!("next-actions JSON bounds are invalid");
    }
    Ok(&trimmed[start..=end])
}

fn normalize_generated_actions(
    generated: GeneratedNextActions,
) -> Result<Vec<ticketing_system::NewConversationNextAction>> {
    if generated.suggestions.len() != 3 {
        bail!(
            "next-actions generator returned {} suggestions, expected 3",
            generated.suggestions.len()
        );
    }

    generated
        .suggestions
        .into_iter()
        .enumerate()
        .map(|(idx, suggestion)| {
            let label = suggestion.label.trim().to_string();
            let message = suggestion.message.trim().to_string();
            if label.is_empty() || message.is_empty() {
                bail!("next-actions suggestion contains an empty field");
            }
            if label.chars().count() > 24 {
                bail!("next-actions label is longer than 24 characters: {}", label);
            }
            Ok(ticketing_system::NewConversationNextAction {
                label,
                icon: NEXT_ACTION_ICONS[idx].to_string(),
                preview: String::new(),
                message,
                sort_order: idx as i64,
            })
        })
        .collect()
}
