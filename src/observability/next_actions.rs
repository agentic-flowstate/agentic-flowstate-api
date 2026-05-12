//! Observability for post-turn next-action suggestion generation.

use std::fmt;

use metrics::{counter, histogram};

pub const METRIC_NEXT_ACTIONS_GENERATION_TOTAL: &str = "next_actions_generation_total";
pub const METRIC_NEXT_ACTIONS_GENERATION_DURATION_MS: &str = "next_actions_generation_duration_ms";
pub const METRIC_NEXT_ACTIONS_SUGGESTIONS_GENERATED: &str =
    "next_actions_suggestions_generated_total";

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

pub fn record_generation(
    conversation_id: &str,
    source_message_id: &str,
    agent_name: &str,
    status: NextActionGenerationStatus,
    duration_ms: u64,
    suggestion_count: usize,
) {
    let status_label = status.to_string();
    counter!(METRIC_NEXT_ACTIONS_GENERATION_TOTAL, "status" => status_label.clone()).increment(1);
    histogram!(METRIC_NEXT_ACTIONS_GENERATION_DURATION_MS, "status" => status_label)
        .record(duration_ms as f64);
    if suggestion_count > 0 {
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
