//! Conversation-scoped next-action suggestions for the mobile Chat composer.

use anyhow::{anyhow, bail, Context, Result};
use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    Json,
};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

use crate::agents::prompts::load_prompt;
use crate::agents::run_oneshot;
use crate::auth_middleware::AuthenticatedUser;
use crate::observability::next_actions::{
    record_generation, record_storage, NextActionGenerationStatus,
};

const NEXT_ACTION_ICONS: [&str; 3] = ["sparkles", "checklist", "questionmark.bubble"];

static BACKFILL_IN_FLIGHT: Lazy<Mutex<HashSet<String>>> = Lazy::new(|| Mutex::new(HashSet::new()));

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
#[serde(untagged)]
enum GeneratedNextActionsPayload {
    Object(GeneratedNextActions),
    Array(Vec<GeneratedNextAction>),
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

struct LatestCompletedTurn {
    source_message_id: String,
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

    if actions.is_empty() {
        if let Err(e) = spawn_backfill_for_completed_conversation(
            pool.clone(),
            user.user_id.clone(),
            id.clone(),
        )
        .await
        {
            tracing::warn!(
                "[NEXT-ACTIONS] Failed to schedule backfill for conv={}: {}",
                id,
                e
            );
        }
    }

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

async fn spawn_backfill_for_completed_conversation(
    db: Arc<SqlitePool>,
    user_id: String,
    conversation_id: String,
) -> Result<()> {
    if conversation_has_active_turn(&db, &conversation_id).await? {
        tracing::debug!(
            "[NEXT-ACTIONS] Skipping backfill for active conversation conv={}",
            conversation_id
        );
        return Ok(());
    }

    let Some(source) = latest_completed_turn(&db, &conversation_id).await? else {
        tracing::debug!(
            "[NEXT-ACTIONS] No completed assistant turn available for backfill conv={}",
            conversation_id
        );
        return Ok(());
    };

    {
        let mut in_flight = BACKFILL_IN_FLIGHT.lock().await;
        if !in_flight.insert(conversation_id.clone()) {
            tracing::debug!(
                "[NEXT-ACTIONS] Backfill already in flight conv={}",
                conversation_id
            );
            return Ok(());
        }
    }

    tokio::spawn(async move {
        let started = Instant::now();
        let source_message_id = source.source_message_id;
        tracing::info!(
            "[NEXT-ACTIONS] Starting backfill generation conv={} msg={}",
            conversation_id,
            source_message_id
        );

        match generate_and_store(
            db,
            &user_id,
            &conversation_id,
            &source_message_id,
            &source.triggering_user_message,
            &source.assistant_output,
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
                    "conversation-open-backfill",
                    status,
                    elapsed_ms(started),
                    suggestion_count,
                );
            }
            Err(error) => {
                record_generation(
                    &conversation_id,
                    &source_message_id,
                    "conversation-open-backfill",
                    NextActionGenerationStatus::Error,
                    elapsed_ms(started),
                    0,
                );
                tracing::warn!(
                    "[NEXT-ACTIONS] Failed backfill generation conv={} msg={}: {}",
                    conversation_id,
                    source_message_id,
                    error
                );
            }
        }

        BACKFILL_IN_FLIGHT.lock().await.remove(&conversation_id);
    });

    Ok(())
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

async fn latest_completed_turn(
    db: &SqlitePool,
    conversation_id: &str,
) -> Result<Option<LatestCompletedTurn>> {
    let row = sqlx::query_as::<_, (String, String, String)>(
        "SELECT assistant.id, user.content, assistant.content \
         FROM conversation_messages assistant \
         JOIN conversation_messages user ON user.id = ( \
             SELECT prior_user.id FROM conversation_messages prior_user \
             WHERE prior_user.conversation_id = assistant.conversation_id \
               AND prior_user.role = 'user' \
               AND prior_user.message_index < assistant.message_index \
             ORDER BY prior_user.message_index DESC \
             LIMIT 1 \
         ) \
         WHERE assistant.conversation_id = ? \
           AND assistant.role = 'assistant' \
           AND trim(assistant.content) != '' \
         ORDER BY assistant.message_index DESC \
         LIMIT 1",
    )
    .bind(conversation_id)
    .fetch_optional(db)
    .await?;

    Ok(row.map(
        |(source_message_id, triggering_user_message, assistant_output)| LatestCompletedTurn {
            source_message_id,
            triggering_user_message,
            assistant_output,
        },
    ))
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

    let vars = prompt_vars(&prompt_context);
    let prompt =
        load_prompt("conversation-next-actions", vars).context("load next-actions user prompt")?;

    let working_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let result = run_oneshot(None, &system_prompt, &working_dir, prompt)
        .await
        .map_err(|e| anyhow!(e))?;

    let actions = match parse_and_normalize_actions(&result.text) {
        Ok(actions) => actions,
        Err(initial_error) => {
            let initial_error_text = format!("{:#}", initial_error);
            tracing::warn!(
                "[NEXT-ACTIONS] Initial JSON parse/validation failed conv={} msg={}: {}; attempting repair; output_preview={}",
                conversation_id,
                source_message_id,
                initial_error_text,
                output_preview(&result.text)
            );

            let repair_system_prompt =
                load_prompt("conversation-next-actions-repair-system", HashMap::new())
                    .context("load next-actions repair system prompt")?;
            let mut repair_vars = prompt_vars(&prompt_context);
            repair_vars.insert("raw_output".to_string(), result.text.clone());
            let repair_prompt = load_prompt("conversation-next-actions-repair", repair_vars)
                .context("load next-actions repair prompt")?;
            let repair_result =
                run_oneshot(None, &repair_system_prompt, &working_dir, repair_prompt)
                    .await
                    .map_err(|e| anyhow!(e))?;

            parse_and_normalize_actions(&repair_result.text).with_context(|| {
                format!(
                    "parse repaired next-actions JSON; initial_error={}; initial_output_preview={}; repair_output_preview={}",
                    initial_error_text,
                    output_preview(&result.text),
                    output_preview(&repair_result.text)
                )
            })?
        }
    };

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

fn prompt_vars(context: &NextActionPromptContext) -> HashMap<String, String> {
    let mut vars = HashMap::new();
    vars.insert(
        "first_user_message".to_string(),
        context.first_user_message.clone(),
    );
    vars.insert(
        "triggering_user_message".to_string(),
        context.triggering_user_message.clone(),
    );
    vars.insert(
        "assistant_output".to_string(),
        context.assistant_output.clone(),
    );
    vars
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

fn parse_and_normalize_actions(
    text: &str,
) -> Result<Vec<ticketing_system::NewConversationNextAction>> {
    let generated = parse_generated_actions(text)?;
    normalize_generated_actions(generated)
}

fn parse_generated_actions(text: &str) -> Result<GeneratedNextActions> {
    let json = extract_json_payload(text)?;
    match parse_generated_payload(json) {
        Ok(generated) => Ok(generated),
        Err(first_error) => {
            let repaired = remove_trailing_commas(json);
            if repaired == json {
                return Err(first_error).context("parse next-actions JSON");
            }
            parse_generated_payload(&repaired).with_context(|| {
                format!(
                    "parse next-actions JSON after removing trailing commas; original_error={}",
                    first_error
                )
            })
        }
    }
}

fn parse_generated_payload(json: &str) -> Result<GeneratedNextActions, serde_json::Error> {
    match serde_json::from_str::<GeneratedNextActionsPayload>(json)? {
        GeneratedNextActionsPayload::Object(generated) => Ok(generated),
        GeneratedNextActionsPayload::Array(suggestions) => Ok(GeneratedNextActions { suggestions }),
    }
}

fn extract_json_payload(text: &str) -> Result<&str> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        bail!("next-actions output is empty");
    }

    if let Some(end) = balanced_json_end(trimmed, 0) {
        return Ok(&trimmed[..=end]);
    }

    for (start, ch) in trimmed.char_indices() {
        if (ch == '{' || ch == '[') && start > 0 {
            if let Some(end) = balanced_json_end(trimmed, start) {
                return Ok(&trimmed[start..=end]);
            }
        }
    }

    Err(anyhow!(
        "next-actions output did not contain a complete JSON object or array"
    ))
}

fn balanced_json_end(text: &str, start: usize) -> Option<usize> {
    let first = text[start..].chars().next()?;
    if first != '{' && first != '[' {
        return None;
    }

    let mut stack = Vec::new();
    let mut in_string = false;
    let mut escaped = false;

    for (offset, ch) in text[start..].char_indices() {
        let idx = start + offset;
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' => {
                if stack.pop() != Some(ch) {
                    return None;
                }
                if stack.is_empty() {
                    return Some(idx);
                }
            }
            _ => {}
        }
    }

    None
}

fn remove_trailing_commas(json: &str) -> String {
    let mut repaired = String::with_capacity(json.len());
    let mut chars = json.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;

    while let Some(ch) = chars.next() {
        if in_string {
            repaired.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => {
                in_string = true;
                repaired.push(ch);
            }
            ',' => {
                let mut lookahead = chars.clone();
                while matches!(lookahead.peek(), Some(next) if next.is_whitespace()) {
                    lookahead.next();
                }
                if matches!(lookahead.peek(), Some('}' | ']')) {
                    continue;
                }
                repaired.push(ch);
            }
            _ => repaired.push(ch),
        }
    }

    repaired
}

fn output_preview(text: &str) -> String {
    const MAX_PREVIEW_CHARS: usize = 500;
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut preview = collapsed
        .chars()
        .take(MAX_PREVIEW_CHARS)
        .collect::<String>();
    if collapsed.chars().count() > MAX_PREVIEW_CHARS {
        preview.push_str("...");
    }
    preview
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

#[cfg(test)]
mod tests {
    use super::*;

    fn object_payload() -> &'static str {
        r#"{
  "suggestions": [
    {
      "label": "Add tests",
      "message": "Add focused regression tests."
    },
    {
      "label": "Deploy it",
      "message": "Deploy the change and check logs."
    },
    {
      "label": "Show diff",
      "message": "Show me the final diff."
    }
  ]
}"#
    }

    #[test]
    fn parses_fenced_json_object() {
        let text = format!("```json\n{}\n```", object_payload());
        let actions = parse_and_normalize_actions(&text).unwrap();

        assert_eq!(actions.len(), 3);
        assert_eq!(actions[0].label, "Add tests");
        assert_eq!(actions[1].sort_order, 1);
        assert_eq!(actions[2].icon, "questionmark.bubble");
    }

    #[test]
    fn parses_top_level_array_payload() {
        let actions = parse_and_normalize_actions(
            r#"[
  {"label":"Add tests","message":"Add focused regression tests."},
  {"label":"Deploy it","message":"Deploy the change and check logs."},
  {"label":"Show diff","message":"Show me the final diff."}
]"#,
        )
        .unwrap();

        assert_eq!(actions.len(), 3);
        assert_eq!(actions[0].icon, "sparkles");
    }

    #[test]
    fn removes_trailing_commas_before_parse() {
        let actions = parse_and_normalize_actions(
            r#"{
  "suggestions": [
    {"label":"Add tests","message":"Add focused regression tests.",},
    {"label":"Deploy it","message":"Deploy the change and check logs.",},
    {"label":"Show diff","message":"Show me the final diff.",},
  ],
}"#,
        )
        .unwrap();

        assert_eq!(actions.len(), 3);
    }

    #[test]
    fn rejects_wrong_suggestion_count() {
        let err = parse_and_normalize_actions(
            r#"{"suggestions":[{"label":"Add tests","message":"Add focused regression tests."}]}"#,
        )
        .unwrap_err();

        assert!(err.to_string().contains("expected 3"));
    }
}
