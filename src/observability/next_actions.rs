//! Observability for post-turn next-action suggestion generation.

use std::fmt;

use metrics::{counter, histogram};

use super::contracts::assert_metric_labels;

pub const METRIC_NEXT_ACTIONS_GENERATION_TOTAL: &str = "next_actions_generation_total";
pub const METRIC_NEXT_ACTIONS_GENERATION_DURATION_MS: &str = "next_actions_generation_duration_ms";
pub const METRIC_NEXT_ACTIONS_SUGGESTIONS_GENERATED: &str =
    "next_actions_suggestions_generated_total";
pub const METRIC_NEXT_ACTIONS_STORAGE_REPLACEMENTS_TOTAL: &str =
    "next_actions_storage_replacements_total";
pub const METRIC_NEXT_ACTIONS_STORAGE_ROWS_DELETED_TOTAL: &str =
    "next_actions_storage_rows_deleted_total";
pub const METRIC_NEXT_ACTIONS_STORAGE_ROWS_INSERTED_TOTAL: &str =
    "next_actions_storage_rows_inserted_total";
pub const METRIC_NEXT_ACTIONS_CLEARS_TOTAL: &str = "next_actions_clears_total";
pub const METRIC_NEXT_ACTIONS_CLEAR_ROWS_DELETED_TOTAL: &str =
    "next_actions_clear_rows_deleted_total";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NextActionGenerationStatus {
    Success,
    SkippedEmptyOutput,
    Error,
}

impl fmt::Display for NextActionGenerationStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            NextActionGenerationStatus::Success => "success",
            NextActionGenerationStatus::SkippedEmptyOutput => "skipped_empty_output",
            NextActionGenerationStatus::Error => "error",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NextActionClearReason {
    NewUserTurn,
}

impl fmt::Display for NextActionClearReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            NextActionClearReason::NewUserTurn => "new_user_turn",
        })
    }
}

pub fn record_generation(
    conversation_id: &str,
    source_message_id: &str,
    agent_name: &str,
    status: NextActionGenerationStatus,
    duration_ms: u64,
    suggestion_count: usize,
) {
    let status_label = status.to_string();
    assert_metric_labels(
        METRIC_NEXT_ACTIONS_GENERATION_TOTAL,
        &[("status", status_label.as_str())],
    );
    counter!(METRIC_NEXT_ACTIONS_GENERATION_TOTAL, "status" => status_label.clone()).increment(1);
    assert_metric_labels(
        METRIC_NEXT_ACTIONS_GENERATION_DURATION_MS,
        &[("status", status_label.as_str())],
    );
    histogram!(METRIC_NEXT_ACTIONS_GENERATION_DURATION_MS, "status" => status_label)
        .record(duration_ms as f64);
    if suggestion_count > 0 {
        assert_metric_labels(METRIC_NEXT_ACTIONS_SUGGESTIONS_GENERATED, &[]);
        counter!(METRIC_NEXT_ACTIONS_SUGGESTIONS_GENERATED).increment(suggestion_count as u64);
    }

    tracing::info!(
        target: "observability.next_actions",
        event = "next_actions.generation",
        conversation_id = %conversation_id,
        source_message_id = %source_message_id,
        agent_name = %agent_name,
        status = %status,
        duration_ms,
        suggestion_count,
        "next-action suggestion generation completed"
    );
}

pub fn record_storage(
    conversation_id: &str,
    source_message_id: &str,
    deleted_count: u64,
    inserted_count: usize,
) {
    assert_metric_labels(METRIC_NEXT_ACTIONS_STORAGE_REPLACEMENTS_TOTAL, &[]);
    counter!(METRIC_NEXT_ACTIONS_STORAGE_REPLACEMENTS_TOTAL).increment(1);
    if deleted_count > 0 {
        assert_metric_labels(METRIC_NEXT_ACTIONS_STORAGE_ROWS_DELETED_TOTAL, &[]);
        counter!(METRIC_NEXT_ACTIONS_STORAGE_ROWS_DELETED_TOTAL).increment(deleted_count);
    }
    if inserted_count > 0 {
        assert_metric_labels(METRIC_NEXT_ACTIONS_STORAGE_ROWS_INSERTED_TOTAL, &[]);
        counter!(METRIC_NEXT_ACTIONS_STORAGE_ROWS_INSERTED_TOTAL).increment(inserted_count as u64);
    }

    tracing::info!(
        target: "observability.next_actions",
        event = "next_actions.storage_replace",
        conversation_id = %conversation_id,
        source_message_id = %source_message_id,
        deleted_count,
        inserted_count,
        "next-action suggestions replaced for conversation"
    );
}

pub fn record_clear(conversation_id: &str, reason: NextActionClearReason, deleted_count: u64) {
    let reason_label = reason.to_string();
    assert_metric_labels(
        METRIC_NEXT_ACTIONS_CLEARS_TOTAL,
        &[("reason", reason_label.as_str())],
    );
    counter!(METRIC_NEXT_ACTIONS_CLEARS_TOTAL, "reason" => reason_label.clone()).increment(1);
    if deleted_count > 0 {
        assert_metric_labels(
            METRIC_NEXT_ACTIONS_CLEAR_ROWS_DELETED_TOTAL,
            &[("reason", reason_label.as_str())],
        );
        counter!(METRIC_NEXT_ACTIONS_CLEAR_ROWS_DELETED_TOTAL, "reason" => reason_label.clone())
            .increment(deleted_count);
    }

    tracing::info!(
        target: "observability.next_actions",
        event = "next_actions.clear",
        conversation_id = %conversation_id,
        reason = %reason,
        deleted_count,
        "next-action suggestions cleared"
    );
}
