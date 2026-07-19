use metrics::{counter, gauge, histogram};

use super::contracts::assert_metric_labels;

pub const AUTH_FAILURES: &str = "codex_coordinator_auth_failures_total";
pub const SESSION_FAILURES: &str = "codex_coordinator_session_failures_total";
pub const TURN_DURATION_MS: &str = "codex_coordinator_turn_duration_ms";
pub const BUSY: &str = "codex_coordinator_busy";
pub const QUEUE_DEPTH: &str = "codex_coordinator_queue_depth";
pub const SESSION_STATE: &str = "codex_coordinator_session_state";
pub const PROMPT_INFO: &str = "codex_coordinator_prompt_info";
const PROMPT_METRIC_VERSION: &str = "v1";

pub fn record_auth_failure(error_class: &'static str) {
    assert_metric_labels(AUTH_FAILURES, &[("error_class", error_class)]);
    counter!(AUTH_FAILURES, "error_class" => error_class).increment(1);
    tracing::error!(
        target: "agentic_api::codex_coordinator",
        event = "codex_coordinator.auth_failure",
        error_class,
        "Codex coordinator subscription authentication failed"
    );
}

pub fn record_session_failure(error_class: &'static str, resume: bool) {
    let operation = if resume { "resume" } else { "initialize" };
    assert_metric_labels(
        SESSION_FAILURES,
        &[("error_class", error_class), ("operation", operation)],
    );
    counter!(
        SESSION_FAILURES,
        "error_class" => error_class,
        "operation" => operation,
    )
    .increment(1);
    tracing::error!(
        target: "agentic_api::codex_coordinator",
        event = "codex_coordinator.session_failure",
        error_class,
        operation,
        "Codex coordinator thread operation failed"
    );
}

pub fn record_turn_terminal(status: &str, duration_ms: u64, tool_call_count: i32) {
    let status = status.to_string();
    assert_metric_labels(TURN_DURATION_MS, &[("status", status.as_str())]);
    histogram!(TURN_DURATION_MS, "status" => status.clone()).record(duration_ms as f64);
    tracing::info!(
        target: "agentic_api::codex_coordinator",
        event = "codex_coordinator.turn_terminal",
        status,
        duration_ms,
        tool_call_count,
        "Codex coordinator turn reached terminal state"
    );
}

pub fn set_health(busy: bool, queue_depth: i64, session_state: &str) {
    assert_metric_labels(BUSY, &[]);
    gauge!(BUSY).set(if busy { 1.0 } else { 0.0 });
    assert_metric_labels(QUEUE_DEPTH, &[]);
    gauge!(QUEUE_DEPTH).set(queue_depth.max(0) as f64);

    for state in ["uninitialized", "starting", "ready", "repair_required"] {
        assert_metric_labels(SESSION_STATE, &[("state", state)]);
        gauge!(SESSION_STATE, "state" => state).set(if state == session_state { 1.0 } else { 0.0 });
    }
    assert_metric_labels(PROMPT_INFO, &[("prompt_version", PROMPT_METRIC_VERSION)]);
    gauge!(PROMPT_INFO, "prompt_version" => PROMPT_METRIC_VERSION).set(1.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_coordinator_metric_contracts_accept_only_low_cardinality_labels() {
        assert_metric_labels(AUTH_FAILURES, &[("error_class", "auth_invalid")]);
        assert_metric_labels(
            SESSION_FAILURES,
            &[("error_class", "session_missing"), ("operation", "resume")],
        );
        assert_metric_labels(TURN_DURATION_MS, &[("status", "completed")]);
        assert_metric_labels(SESSION_STATE, &[("state", "ready")]);
        assert_metric_labels(PROMPT_INFO, &[("prompt_version", PROMPT_METRIC_VERSION)]);
    }
}
