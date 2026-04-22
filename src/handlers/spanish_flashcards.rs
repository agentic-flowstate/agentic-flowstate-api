//! Spanish flashcards REST API handlers.
//!
//! Personal feature — all routes are mounted under `user_scoped_routes` and
//! the authenticated user (`Extension<AuthenticatedUser>`) is the sole data
//! owner. Client-supplied `user_id` fields are always overwritten.

use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::sync::Arc;

use crate::auth_middleware::AuthenticatedUser;
use ticketing_system::spanish_flashcards::{
    self, CreateSpanishCardRequest, ReviewSpanishCardRequest, SpanishCard, SpanishSection,
};

/// GET /api/spanish/sections
pub async fn list_spanish_sections(
    State(db): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<Json<Vec<SpanishSection>>, (StatusCode, String)> {
    // Ensure this user has their seed deck on first call.
    spanish_flashcards::seed_default_data(&db, &user.user_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let sections = spanish_flashcards::list_sections(&db, &user.user_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(sections))
}

/// GET /api/spanish/sections/:section_id/cards
pub async fn list_spanish_cards(
    State(db): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(section_id): Path<String>,
) -> Result<Json<Vec<SpanishCard>>, (StatusCode, String)> {
    let cards = spanish_flashcards::list_cards_by_section(&db, &user.user_id, &section_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(cards))
}

#[derive(Deserialize)]
pub struct CreateCardBody {
    pub section_id: String,
    pub spanish: String,
    pub english: String,
    #[serde(default)]
    pub is_phrase: bool,
}

/// POST /api/spanish/cards
pub async fn create_spanish_card(
    State(db): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(body): Json<CreateCardBody>,
) -> Result<Json<Option<SpanishCard>>, (StatusCode, String)> {
    let req = CreateSpanishCardRequest {
        user_id: user.user_id,
        section_id: body.section_id,
        spanish: body.spanish,
        english: body.english,
        is_phrase: body.is_phrase,
    };

    let card = spanish_flashcards::create_card(&db, req)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(card))
}

#[derive(Deserialize)]
pub struct ReviewCardBody {
    pub known: bool,
}

/// POST /api/spanish/cards/:card_id/review
pub async fn review_spanish_card(
    State(db): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(card_id): Path<String>,
    Json(body): Json<ReviewCardBody>,
) -> Result<Json<SpanishCard>, (StatusCode, String)> {
    let req = ReviewSpanishCardRequest {
        user_id: user.user_id,
        card_id,
        known: body.known,
    };

    let card = spanish_flashcards::review_card(&db, req)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(card))
}

/// DELETE /api/spanish/cards/:card_id
pub async fn delete_spanish_card(
    State(db): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(card_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let deleted = spanish_flashcards::delete_card(&db, &user.user_id, &card_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(serde_json::json!({ "deleted": deleted })))
}

#[derive(Deserialize)]
pub struct GenerateCardsBody {
    #[serde(default = "default_generate_count")]
    pub count: usize,
}

fn default_generate_count() -> usize {
    10
}

#[derive(Serialize)]
pub struct GenerateCardsResponse {
    pub created: usize,
    pub skipped: usize,
    pub cards: Vec<SpanishCard>,
}

/// POST /api/spanish/sections/:section_id/generate
///
/// Uses Codex to generate new vocabulary for the given
/// section. Deduplicates against the section's existing words before inserting.
pub async fn generate_spanish_cards(
    State(db): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(section_id): Path<String>,
    Json(body): Json<GenerateCardsBody>,
) -> Result<Json<GenerateCardsResponse>, (StatusCode, String)> {
    let section = spanish_flashcards::get_section(&db, &user.user_id, &section_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("section {section_id} not found")))?;

    let existing = spanish_flashcards::list_words_in_section(&db, &user.user_id, &section_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let count = body.count.clamp(1, 30);
    let new_words = call_codex_generate(&section.title, count, &existing)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("generation failed: {e}")))?;

    let mut created_cards = Vec::new();
    let mut skipped = 0usize;
    for (spanish, english, is_phrase) in new_words {
        let req = CreateSpanishCardRequest {
            user_id: user.user_id.clone(),
            section_id: section_id.clone(),
            spanish,
            english,
            is_phrase,
        };
        match spanish_flashcards::create_card(&db, req).await {
            Ok(Some(card)) => created_cards.push(card),
            Ok(None) => skipped += 1,
            Err(e) => {
                tracing::warn!("failed to insert generated card: {e}");
                skipped += 1;
            }
        }
    }

    Ok(Json(GenerateCardsResponse {
        created: created_cards.len(),
        skipped,
        cards: created_cards,
    }))
}

/// Invokes Codex to produce N deduped vocabulary entries.
async fn call_codex_generate(
    section_title: &str,
    count: usize,
    existing: &[String],
) -> anyhow::Result<Vec<(String, String, bool)>> {
    use crate::agents::codex_exec::{resolve_codex_model, run_codex_text};

    let existing_list = if existing.is_empty() {
        "(none yet)".to_string()
    } else {
        existing.join(", ")
    };

    let system_prompt = "You are a Spanish-language pedagogy assistant. You generate beginner-friendly \
                        Spanish vocabulary entries in strict JSON format. You never include prose, \
                        markdown fences, or commentary — only the JSON array requested.";

    let user_prompt = format!(
        "Generate Spanish flashcards for the section \"{section_title}\".\n\n\
         Existing Spanish words in this section (DO NOT repeat any of these, even with different accents):\n{existing_list}\n\n\
         Produce exactly {count} NEW Spanish vocabulary entries appropriate for this section.\n\n\
         Rules:\n\
         - Use lowercase Spanish unless a proper noun.\n\
         - Include correct accents (á é í ó ú ñ) and inverted punctuation (¿ ¡) where needed.\n\
         - Keep English translations concise (1–4 words).\n\
         - `is_phrase` is true only for multi-word expressions; single words are false.\n\
         - Stay strictly within the topic \"{section_title}\".\n\n\
         Output ONLY a JSON array (no markdown, no prose). Example shape:\n\
         [{{\"spanish\": \"palabra\", \"english\": \"word\", \"is_phrase\": false}}]"
    );

    let raw = run_codex_text(
        resolve_codex_model("haiku"),
        "low",
        system_prompt,
        std::path::Path::new("/tmp"),
        &user_prompt,
    )
    .await
    .map_err(|e| anyhow::anyhow!("codex exec failed: {e}"))?;
    let text = raw
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    let parsed: Vec<GeneratedCard> = serde_json::from_str(text).map_err(|e| {
        anyhow::anyhow!("failed to parse Codex JSON: {e}; raw response: {text}")
    })?;

    let existing_normalized: std::collections::HashSet<String> =
        existing.iter().map(|s| normalize_spanish(s)).collect();

    let mut out = Vec::new();
    for card in parsed {
        let spanish = card.spanish.trim().to_string();
        let english = card.english.trim().to_string();
        if spanish.is_empty() || english.is_empty() {
            continue;
        }
        let norm = normalize_spanish(&spanish);
        if existing_normalized.contains(&norm) {
            continue;
        }
        out.push((spanish, english, card.is_phrase));
    }

    Ok(out)
}

#[derive(Deserialize)]
struct GeneratedCard {
    spanish: String,
    english: String,
    #[serde(default)]
    is_phrase: bool,
}

/// Lowercase + strip common accents for dedup comparison.
fn normalize_spanish(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| match c {
            'á' => 'a',
            'é' => 'e',
            'í' => 'i',
            'ó' => 'o',
            'ú' | 'ü' => 'u',
            'ñ' => 'n',
            c => c,
        })
        .collect()
}
