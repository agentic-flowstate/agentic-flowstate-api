//! Token usage tracking REST API handlers

use axum::{
    extract::{Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::sync::Arc;

use crate::agents::codex_app_server::{
    read_codex_account_rate_limits, CodexAccountRateLimits, CodexCreditsSnapshot,
    CodexRateLimitSnapshot, CodexRateLimitWindow,
};
use ticketing_system::token_usage;
use ticketing_system::token_usage::ConversationUsage;

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
    pub latest_context: Option<ContextInfo>,
    pub account_rate_limits: AccountRateLimitsInfo,
}

#[derive(Serialize)]
pub struct WindowInfo {
    pub window_start: String,
    pub window_end: String,
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_output_tokens: i64,
    pub total_tokens: i64,
    pub event_count: i64,
}

#[derive(Serialize)]
pub struct ConversationInfo {
    pub conversation_id: String,
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_output_tokens: i64,
    pub total_tokens: i64,
    pub event_count: i64,
}

#[derive(Serialize)]
pub struct ContextInfo {
    pub conversation_id: String,
    pub thread_total_tokens: i64,
    pub context_used_tokens: i64,
    pub context_window_tokens: i64,
    pub context_percentage: f64,
    pub event_count: i64,
}

#[derive(Serialize)]
pub struct AccountRateLimitsInfo {
    pub current: RateLimitBucketInfo,
    pub buckets: Vec<RateLimitBucketInfo>,
}

#[derive(Serialize, Clone)]
pub struct RateLimitBucketInfo {
    pub limit_id: Option<String>,
    pub limit_name: Option<String>,
    pub plan_type: Option<String>,
    pub rate_limit_reached_type: Option<String>,
    pub primary: Option<RateLimitWindowInfo>,
    pub secondary: Option<RateLimitWindowInfo>,
    pub credits: Option<CreditsInfo>,
}

#[derive(Serialize, Clone)]
pub struct RateLimitWindowInfo {
    pub used_percent: i32,
    pub window_duration_mins: Option<i64>,
    pub resets_at: Option<i64>,
}

#[derive(Serialize, Clone)]
pub struct CreditsInfo {
    pub has_credits: bool,
    pub unlimited: bool,
    pub balance: Option<String>,
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
    let account_rate_limits = read_codex_account_rate_limits().await.map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            format!("Failed to read Codex account rate limits: {e}"),
        )
    })?;

    let current_window = summary.current_window.map(|w| WindowInfo {
        window_start: w.window_start,
        window_end: w.window_end,
        input_tokens: w.input_tokens,
        cached_input_tokens: w.cached_input_tokens,
        output_tokens: w.output_tokens,
        reasoning_output_tokens: w.reasoning_output_tokens,
        total_tokens: w.total_tokens,
        event_count: w.event_count,
    });

    let weekly = WindowInfo {
        window_start: summary.weekly.window_start,
        window_end: summary.weekly.window_end,
        input_tokens: summary.weekly.input_tokens,
        cached_input_tokens: summary.weekly.cached_input_tokens,
        output_tokens: summary.weekly.output_tokens,
        reasoning_output_tokens: summary.weekly.reasoning_output_tokens,
        total_tokens: summary.weekly.total_tokens,
        event_count: summary.weekly.event_count,
    };

    let conversation = summary.conversation.map(conversation_info);
    let latest_context = summary.latest_context.and_then(context_info);

    Ok(Json(UsageResponse {
        current_window,
        weekly,
        conversation,
        latest_context,
        account_rate_limits: account_rate_limits_info(account_rate_limits),
    }))
}

fn conversation_info(c: ConversationUsage) -> ConversationInfo {
    ConversationInfo {
        conversation_id: c.conversation_id,
        input_tokens: c.input_tokens,
        cached_input_tokens: c.cached_input_tokens,
        output_tokens: c.output_tokens,
        reasoning_output_tokens: c.reasoning_output_tokens,
        total_tokens: c.total_tokens,
        event_count: c.event_count,
    }
}

fn context_info(c: ConversationUsage) -> Option<ContextInfo> {
    let context_window_tokens = c.context_window_tokens?;
    if context_window_tokens <= 0 {
        return None;
    }
    let context_used_tokens = c.context_used_tokens?;
    let percentage = (context_used_tokens as f64 / context_window_tokens as f64) * 100.0;
    Some(ContextInfo {
        conversation_id: c.conversation_id,
        thread_total_tokens: c.thread_total_tokens,
        context_used_tokens,
        context_window_tokens,
        context_percentage: ((percentage * 10.0).round() / 10.0).clamp(0.0, 100.0),
        event_count: c.event_count,
    })
}

fn account_rate_limits_info(rate_limits: CodexAccountRateLimits) -> AccountRateLimitsInfo {
    let current = rate_limit_bucket_info(rate_limits.rate_limits);
    let mut buckets = rate_limits
        .rate_limits_by_limit_id
        .unwrap_or_default()
        .into_values()
        .map(rate_limit_bucket_info)
        .collect::<Vec<_>>();

    buckets.sort_by(|a, b| {
        a.limit_id
            .as_deref()
            .unwrap_or_default()
            .cmp(b.limit_id.as_deref().unwrap_or_default())
    });

    if buckets.is_empty() {
        buckets.push(current.clone());
    }

    AccountRateLimitsInfo { current, buckets }
}

fn rate_limit_bucket_info(snapshot: CodexRateLimitSnapshot) -> RateLimitBucketInfo {
    RateLimitBucketInfo {
        limit_id: snapshot.limit_id,
        limit_name: snapshot.limit_name,
        plan_type: snapshot.plan_type,
        rate_limit_reached_type: snapshot.rate_limit_reached_type,
        primary: snapshot.primary.map(rate_limit_window_info),
        secondary: snapshot.secondary.map(rate_limit_window_info),
        credits: snapshot.credits.map(credits_info),
    }
}

fn rate_limit_window_info(window: CodexRateLimitWindow) -> RateLimitWindowInfo {
    RateLimitWindowInfo {
        used_percent: window.used_percent,
        window_duration_mins: window.window_duration_mins,
        resets_at: window.resets_at,
    }
}

fn credits_info(credits: CodexCreditsSnapshot) -> CreditsInfo {
    CreditsInfo {
        has_credits: credits.has_credits,
        unlimited: credits.unlimited,
        balance: credits.balance,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_percentage_uses_active_context_tokens_not_cumulative_thread_total() {
        let context = context_info(ConversationUsage {
            conversation_id: "conv-1".to_string(),
            input_tokens: 0,
            cached_input_tokens: 0,
            output_tokens: 0,
            reasoning_output_tokens: 0,
            total_tokens: 0,
            thread_total_tokens: 691_491,
            context_used_tokens: Some(76_280),
            context_window_tokens: Some(258_400),
            event_count: 2,
        })
        .expect("context info");

        assert_eq!(context.thread_total_tokens, 691_491);
        assert_eq!(context.context_used_tokens, 76_280);
        assert_eq!(context.context_percentage, 29.5);
    }

    #[test]
    fn context_percentage_is_clamped_to_display_bounds() {
        let context = context_info(ConversationUsage {
            conversation_id: "conv-1".to_string(),
            input_tokens: 0,
            cached_input_tokens: 0,
            output_tokens: 0,
            reasoning_output_tokens: 0,
            total_tokens: 0,
            thread_total_tokens: 1_000_000,
            context_used_tokens: Some(300_000),
            context_window_tokens: Some(258_400),
            event_count: 2,
        })
        .expect("context info");

        assert_eq!(context.context_percentage, 100.0);
    }
}
