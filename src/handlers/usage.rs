//! Token usage tracking REST API handlers

use axum::{
    extract::{Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::sync::Arc;

use ticketing_system::token_usage;

#[derive(Deserialize)]
pub struct UsageQuery {
    /// Optional conversation_id to include per-conversation context stats
    pub conversation_id: Option<String>,
}

/// Response for GET /api/usage
#[derive(Serialize)]
pub struct UsageResponse {
    pub current_window: Option<WindowInfo>,
    pub weekly: WindowInfo,
    pub conversation: Option<ConversationInfo>,
}

#[derive(Serialize)]
pub struct WindowInfo {
    pub window_start: String,
    pub window_end: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub event_count: i64,
}

#[derive(Serialize)]
pub struct ConversationInfo {
    pub conversation_id: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub context_limit: i64,
    pub context_percentage: f64,
    pub event_count: i64,
}

/// GET /api/usage
/// Returns current 5-hour window usage, weekly usage, and optional per-conversation stats.
pub async fn get_usage(
    State(db): State<Arc<SqlitePool>>,
    Query(query): Query<UsageQuery>,
) -> Result<Json<UsageResponse>, (StatusCode, String)> {
    let summary = token_usage::get_usage_summary(&db, query.conversation_id.as_deref())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let current_window = summary.current_window.map(|w| WindowInfo {
        window_start: w.window_start,
        window_end: w.window_end,
        input_tokens: w.input_tokens,
        output_tokens: w.output_tokens,
        total_tokens: w.total_tokens,
        event_count: w.event_count,
    });

    let weekly = WindowInfo {
        window_start: summary.weekly.window_start,
        window_end: summary.weekly.window_end,
        input_tokens: summary.weekly.input_tokens,
        output_tokens: summary.weekly.output_tokens,
        total_tokens: summary.weekly.total_tokens,
        event_count: summary.weekly.event_count,
    };

    let conversation = summary.conversation.map(|c| {
        let context_limit: i64 = 1_000_000; // 1M tokens for Opus 4.6 / Sonnet 4.6
        let pct = if context_limit > 0 {
            (c.total_tokens as f64 / context_limit as f64) * 100.0
        } else {
            0.0
        };
        ConversationInfo {
            conversation_id: c.conversation_id,
            input_tokens: c.input_tokens,
            output_tokens: c.output_tokens,
            total_tokens: c.total_tokens,
            context_limit,
            context_percentage: (pct * 10.0).round() / 10.0, // 1 decimal place
            event_count: c.event_count,
        }
    });

    Ok(Json(UsageResponse {
        current_window,
        weekly,
        conversation,
    }))
}
