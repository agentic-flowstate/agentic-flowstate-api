//! Stop-button cancellation observability.
//!
//! The cancel path crosses the API process, durable SQLite state, the
//! conversation runner, and the Codex app-server child process. Keep the phase
//! labels here so logs, metrics, and tests describe the same latency points.

pub const METRIC_CANCEL_PHASE_TOTAL: &str = "agentic_cancel_phase_total";
pub const METRIC_CANCEL_PHASE_ELAPSED_MS: &str = "agentic_cancel_phase_elapsed_ms";

const TARGET: &str = "agentic_api::cancel";

use super::contracts::assert_metric_labels;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelPhase {
    RequestReceived,
    DurableMarkerWritten,
    RunnerCommandDelivered,
    RunnerCommandUnavailable,
    RunnerCancelObserved,
    RunnerCancelConsumed,
    ProcessTerminationSignalled,
    ProcessTerminationElapsed,
    TerminalDbStateWritten,
}

impl CancelPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RequestReceived => "cancel_request_received",
            Self::DurableMarkerWritten => "durable_marker_written",
            Self::RunnerCommandDelivered => "runner_command_delivered",
            Self::RunnerCommandUnavailable => "runner_command_unavailable",
            Self::RunnerCancelObserved => "runner_cancel_observed",
            Self::RunnerCancelConsumed => "runner_cancel_consumed",
            Self::ProcessTerminationSignalled => "process_termination_signalled",
            Self::ProcessTerminationElapsed => "process_termination_elapsed",
            Self::TerminalDbStateWritten => "terminal_db_state_written",
        }
    }

    #[cfg(test)]
    pub const fn measurement_fields(self) -> &'static [&'static str] {
        match self {
            Self::RequestReceived => &["cancel_requested_at_ms"],
            Self::DurableMarkerWritten => &["cancel_marker_written_at_ms", "request_to_marker_ms"],
            Self::RunnerCommandDelivered => &[
                "runner_command_delivered_at_ms",
                "request_to_runner_command_ms",
                "runner_command_elapsed_ms",
            ],
            Self::RunnerCommandUnavailable => &[
                "runner_command_checked_at_ms",
                "request_to_runner_command_check_ms",
            ],
            Self::RunnerCancelObserved => &["runner_cancel_observed_at_ms"],
            Self::RunnerCancelConsumed => &["runner_cancel_consumed_at_ms"],
            Self::ProcessTerminationSignalled => &[
                "process_termination_signalled_at_ms",
                "process_termination_signal_elapsed_ms",
            ],
            Self::ProcessTerminationElapsed => &[
                "process_termination_completed_at_ms",
                "process_wait_elapsed_ms",
            ],
            Self::TerminalDbStateWritten => &[
                "terminal_db_state_written_at_ms",
                "terminal_db_state_elapsed_ms",
            ],
        }
    }
}

#[cfg(test)]
pub const REQUIRED_CANCEL_MEASUREMENT_PHASES: &[CancelPhase] = &[
    CancelPhase::RequestReceived,
    CancelPhase::DurableMarkerWritten,
    CancelPhase::RunnerCommandDelivered,
    CancelPhase::RunnerCancelObserved,
    CancelPhase::RunnerCancelConsumed,
    CancelPhase::ProcessTerminationElapsed,
    CancelPhase::TerminalDbStateWritten,
];

pub fn elapsed_ms(started_at_ms: i64, finished_at_ms: i64) -> i64 {
    if finished_at_ms >= started_at_ms {
        finished_at_ms - started_at_ms
    } else {
        0
    }
}

fn increment_phase(phase: CancelPhase) {
    assert_metric_labels(METRIC_CANCEL_PHASE_TOTAL, &[("phase", phase.as_str())]);
    metrics::counter!(METRIC_CANCEL_PHASE_TOTAL, "phase" => phase.as_str()).increment(1);
}

fn record_phase_elapsed(phase: CancelPhase, elapsed_ms: i64) {
    assert_metric_labels(METRIC_CANCEL_PHASE_ELAPSED_MS, &[("phase", phase.as_str())]);
    metrics::histogram!(METRIC_CANCEL_PHASE_ELAPSED_MS, "phase" => phase.as_str())
        .record(elapsed_ms as f64);
}

pub fn record_request_received(conversation_id: &str, user_id: &str, cancel_requested_at_ms: i64) {
    let phase = CancelPhase::RequestReceived;
    increment_phase(phase);
    tracing::info!(
        target: TARGET,
        cancel_phase = phase.as_str(),
        conversation_id = %conversation_id,
        user_id = %user_id,
        cancel_requested_at_ms,
        "[CANCEL] cancel request received"
    );
}

pub fn record_durable_marker_written(
    conversation_id: &str,
    cancel_requested_at_ms: i64,
    cancel_marker_written_at_ms: i64,
) {
    let phase = CancelPhase::DurableMarkerWritten;
    let request_to_marker_ms = elapsed_ms(cancel_requested_at_ms, cancel_marker_written_at_ms);
    increment_phase(phase);
    record_phase_elapsed(phase, request_to_marker_ms);
    tracing::info!(
        target: TARGET,
        cancel_phase = phase.as_str(),
        conversation_id = %conversation_id,
        cancel_requested_at_ms,
        cancel_marker_written_at_ms,
        request_to_marker_ms,
        "[CANCEL] durable cancellation marker written"
    );
}

pub fn record_runner_command_delivered(
    conversation_id: &str,
    cancel_requested_at_ms: i64,
    runner_command_started_at_ms: i64,
    runner_command_delivered_at_ms: i64,
    worker_exists: bool,
    runner_turn_exists: bool,
    cancelled_pending_jobs: u64,
) {
    let phase = CancelPhase::RunnerCommandDelivered;
    let request_to_runner_command_ms =
        elapsed_ms(cancel_requested_at_ms, runner_command_delivered_at_ms);
    let runner_command_elapsed_ms =
        elapsed_ms(runner_command_started_at_ms, runner_command_delivered_at_ms);
    increment_phase(phase);
    record_phase_elapsed(phase, request_to_runner_command_ms);
    tracing::info!(
        target: TARGET,
        cancel_phase = phase.as_str(),
        conversation_id = %conversation_id,
        cancel_requested_at_ms,
        runner_command_started_at_ms,
        runner_command_delivered_at_ms,
        request_to_runner_command_ms,
        runner_command_elapsed_ms,
        worker_exists,
        runner_turn_exists,
        cancelled_pending_jobs,
        "[CANCEL] runner command delivered"
    );
}

pub fn record_runner_command_unavailable(
    conversation_id: &str,
    cancel_requested_at_ms: i64,
    runner_command_checked_at_ms: i64,
    worker_exists: bool,
    runner_turn_exists: bool,
    cancelled_pending_jobs: u64,
) {
    let phase = CancelPhase::RunnerCommandUnavailable;
    let request_to_runner_command_check_ms =
        elapsed_ms(cancel_requested_at_ms, runner_command_checked_at_ms);
    increment_phase(phase);
    record_phase_elapsed(phase, request_to_runner_command_check_ms);
    tracing::info!(
        target: TARGET,
        cancel_phase = phase.as_str(),
        conversation_id = %conversation_id,
        cancel_requested_at_ms,
        runner_command_checked_at_ms,
        request_to_runner_command_check_ms,
        worker_exists,
        runner_turn_exists,
        cancelled_pending_jobs,
        "[CANCEL] no live runner command target registered"
    );
}

pub fn record_runner_cancel_observed(
    conversation_id: &str,
    runner_turn_id: &str,
    worker_phase: &str,
    runner_cancel_observed_at_ms: i64,
) {
    let phase = CancelPhase::RunnerCancelObserved;
    increment_phase(phase);
    tracing::info!(
        target: TARGET,
        cancel_phase = phase.as_str(),
        conversation_id = %conversation_id,
        runner_turn_id = %runner_turn_id,
        worker_phase,
        runner_cancel_observed_at_ms,
        "[CANCEL] runner observed cancellation"
    );
}

pub fn record_runner_cancel_consumed(
    conversation_id: &str,
    runner_cancel_consumed_at_ms: i64,
    memory_cancelled: bool,
    persistent_cancelled: bool,
) {
    let phase = CancelPhase::RunnerCancelConsumed;
    increment_phase(phase);
    tracing::info!(
        target: TARGET,
        cancel_phase = phase.as_str(),
        conversation_id = %conversation_id,
        runner_cancel_consumed_at_ms,
        memory_cancelled,
        persistent_cancelled,
        "[CANCEL] runner consumed cancellation marker"
    );
}

pub fn record_process_termination_signalled(
    conversation_id: &str,
    runner_turn_id: &str,
    worker_phase: &str,
    process_termination_started_at_ms: i64,
    process_termination_signalled_at_ms: i64,
) {
    let phase = CancelPhase::ProcessTerminationSignalled;
    let process_termination_signal_elapsed_ms = elapsed_ms(
        process_termination_started_at_ms,
        process_termination_signalled_at_ms,
    );
    increment_phase(phase);
    record_phase_elapsed(phase, process_termination_signal_elapsed_ms);
    tracing::info!(
        target: TARGET,
        cancel_phase = phase.as_str(),
        conversation_id = %conversation_id,
        runner_turn_id = %runner_turn_id,
        worker_phase,
        process_termination_started_at_ms,
        process_termination_signalled_at_ms,
        process_termination_signal_elapsed_ms,
        "[CANCEL] process termination signalled"
    );
}

pub fn record_process_termination_elapsed(
    conversation_id: &str,
    runner_turn_id: &str,
    process_wait_started_at_ms: i64,
    process_termination_completed_at_ms: i64,
    exit_success: bool,
) {
    let phase = CancelPhase::ProcessTerminationElapsed;
    let process_wait_elapsed_ms = elapsed_ms(
        process_wait_started_at_ms,
        process_termination_completed_at_ms,
    );
    increment_phase(phase);
    record_phase_elapsed(phase, process_wait_elapsed_ms);
    tracing::info!(
        target: TARGET,
        cancel_phase = phase.as_str(),
        conversation_id = %conversation_id,
        runner_turn_id = %runner_turn_id,
        process_wait_started_at_ms,
        process_termination_completed_at_ms,
        process_wait_elapsed_ms,
        exit_success,
        "[CANCEL] process termination completed"
    );
}

pub fn record_terminal_db_state_written(
    conversation_id: &str,
    runner_turn_id: &str,
    terminal_status: &str,
    terminal_db_state_started_at_ms: i64,
    terminal_db_state_written_at_ms: i64,
) {
    let phase = CancelPhase::TerminalDbStateWritten;
    let terminal_db_state_elapsed_ms = elapsed_ms(
        terminal_db_state_started_at_ms,
        terminal_db_state_written_at_ms,
    );
    increment_phase(phase);
    record_phase_elapsed(phase, terminal_db_state_elapsed_ms);
    tracing::info!(
        target: TARGET,
        cancel_phase = phase.as_str(),
        conversation_id = %conversation_id,
        runner_turn_id = %runner_turn_id,
        terminal_status,
        terminal_db_state_started_at_ms,
        terminal_db_state_written_at_ms,
        terminal_db_state_elapsed_ms,
        "[CANCEL] terminal DB state written"
    );
}

#[cfg(test)]
mod tests {
    use super::{elapsed_ms, CancelPhase, REQUIRED_CANCEL_MEASUREMENT_PHASES};

    #[test]
    fn required_cancel_measurement_phases_match_stop_button_checklist() {
        let phases: Vec<&str> = REQUIRED_CANCEL_MEASUREMENT_PHASES
            .iter()
            .map(|phase| phase.as_str())
            .collect();

        assert_eq!(
            phases,
            vec![
                "cancel_request_received",
                "durable_marker_written",
                "runner_command_delivered",
                "runner_cancel_observed",
                "runner_cancel_consumed",
                "process_termination_elapsed",
                "terminal_db_state_written",
            ]
        );
    }

    #[test]
    fn cancel_phase_measurement_fields_are_stable() {
        assert_eq!(
            CancelPhase::RequestReceived.measurement_fields(),
            ["cancel_requested_at_ms"]
        );
        assert_eq!(
            CancelPhase::DurableMarkerWritten.measurement_fields(),
            ["cancel_marker_written_at_ms", "request_to_marker_ms"]
        );
        assert_eq!(
            CancelPhase::RunnerCommandDelivered.measurement_fields(),
            [
                "runner_command_delivered_at_ms",
                "request_to_runner_command_ms",
                "runner_command_elapsed_ms",
            ]
        );
        assert_eq!(
            CancelPhase::RunnerCancelObserved.measurement_fields(),
            ["runner_cancel_observed_at_ms"]
        );
        assert_eq!(
            CancelPhase::RunnerCancelConsumed.measurement_fields(),
            ["runner_cancel_consumed_at_ms"]
        );
        assert_eq!(
            CancelPhase::ProcessTerminationElapsed.measurement_fields(),
            [
                "process_termination_completed_at_ms",
                "process_wait_elapsed_ms",
            ]
        );
        assert_eq!(
            CancelPhase::TerminalDbStateWritten.measurement_fields(),
            [
                "terminal_db_state_written_at_ms",
                "terminal_db_state_elapsed_ms",
            ]
        );
    }

    #[test]
    fn elapsed_ms_clamps_backward_clock_movement() {
        assert_eq!(elapsed_ms(100, 125), 25);
        assert_eq!(elapsed_ms(125, 100), 0);
    }
}
