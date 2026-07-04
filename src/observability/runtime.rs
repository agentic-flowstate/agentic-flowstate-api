//! Chat/Codex runtime observability.
//!
//! Keep metric labels closed here. Conversation/user/client identifiers are
//! useful in process logs, but they are intentionally not Prometheus labels.

use std::borrow::Cow;
use std::fmt;

use metrics::{counter, histogram};

pub const METRIC_AGENT_TURN_STARTED: &str = "agent_runtime_turn_started_total";
pub const METRIC_AGENT_TURNS: &str = "agent_runtime_turns_total";
pub const METRIC_AGENT_TURN_DURATION_MS: &str = "agent_runtime_turn_duration_ms";
pub const METRIC_AGENT_RUNTIME_FAILURES: &str = "agent_runtime_failures_total";
pub const METRIC_AGENT_SPAWN_STARTED: &str = "agent_runtime_spawn_started_total";
pub const METRIC_AGENT_SPAWN_DURATION_MS: &str = "agent_runtime_spawn_duration_ms";
pub const METRIC_AGENT_EVENT_LATENCY_MS: &str = "agent_runtime_event_latency_ms";

const TARGET: &str = "agentic_api::runtime";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuntimeFailurePhase {
    AttachmentStorage,
    StoreUserMessage,
    CreateCheckpoint,
    CreateAssistantMessage,
    StartupContextPreflight,
    BuildCodexPrompt,
    ClaimRunnerTurn,
    SpawnCodex,
    WaitCodexTurn,
    CodexTurnFailed,
    RunnerJobFailed,
}

impl fmt::Display for RuntimeFailurePhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::AttachmentStorage => "attachment_storage",
            Self::StoreUserMessage => "store_user_message",
            Self::CreateCheckpoint => "create_checkpoint",
            Self::CreateAssistantMessage => "create_assistant_message",
            Self::StartupContextPreflight => "startup_context_preflight",
            Self::BuildCodexPrompt => "build_codex_prompt",
            Self::ClaimRunnerTurn => "claim_runner_turn",
            Self::SpawnCodex => "spawn_codex",
            Self::WaitCodexTurn => "wait_codex_turn",
            Self::CodexTurnFailed => "codex_turn_failed",
            Self::RunnerJobFailed => "runner_job_failed",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuntimeLatencyPhase {
    WorkerProcessMessageStart,
    CodexSpawnStart,
    CodexSpawnReady,
    CodexThreadStarted,
    MessageStartPersisted,
    FirstAssistantDeltaPersisted,
    HandlerReceived,
    SubmitReceived,
    SubmitEnqueued,
    RunnerClaimed,
    WorkerStarting,
    WorkerFinished,
}

impl fmt::Display for RuntimeLatencyPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::WorkerProcessMessageStart => "worker_process_message_start",
            Self::CodexSpawnStart => "codex_spawn_start",
            Self::CodexSpawnReady => "codex_spawn_ready",
            Self::CodexThreadStarted => "codex_thread_started",
            Self::MessageStartPersisted => "message_start_persisted",
            Self::FirstAssistantDeltaPersisted => "first_assistant_delta_persisted",
            Self::HandlerReceived => "handler_received",
            Self::SubmitReceived => "submit_received",
            Self::SubmitEnqueued => "submit_enqueued",
            Self::RunnerClaimed => "runner_claimed",
            Self::WorkerStarting => "worker_starting",
            Self::WorkerFinished => "worker_finished",
        })
    }
}

pub fn record_turn_started(
    conversation_id: &str,
    client_id: Option<&str>,
    agent_name: &str,
    runtime: &str,
    model: &str,
    reasoning_effort: &str,
    message_chars: usize,
    attachment_count: usize,
    started_at_ms: i64,
) {
    counter!(
        METRIC_AGENT_TURN_STARTED,
        "agent" => agent_name.to_string(),
        "runtime" => runtime.to_string(),
    )
    .increment(1);

    tracing::info!(
        target: TARGET,
        event = "agent_runtime.turn_started",
        phase = %RuntimeLatencyPhase::WorkerProcessMessageStart,
        conversation_id = %conversation_id,
        client_id = client_id.unwrap_or("none"),
        agent_name,
        runtime,
        model,
        reasoning_effort,
        message_chars,
        attachment_count,
        started_at_ms,
        "agent runtime turn started"
    );
}

pub fn record_turn_completed(
    conversation_id: &str,
    agent_name: &str,
    runtime: &str,
    status: &str,
    duration_ms: u64,
    tool_call_count: i32,
    output_chars: usize,
) {
    let agent_label: Cow<'static, str> = agent_name.to_string().into();
    let runtime_label: Cow<'static, str> = runtime.to_string().into();
    let status_label: Cow<'static, str> = status.to_string().into();
    counter!(
        METRIC_AGENT_TURNS,
        "agent" => agent_label.clone(),
        "runtime" => runtime_label.clone(),
        "status" => status_label.clone(),
    )
    .increment(1);
    histogram!(
        METRIC_AGENT_TURN_DURATION_MS,
        "agent" => agent_label,
        "runtime" => runtime_label,
        "status" => status_label,
    )
    .record(duration_ms as f64);

    tracing::info!(
        target: TARGET,
        event = "agent_runtime.turn_completed",
        conversation_id = %conversation_id,
        agent_name,
        runtime,
        status,
        duration_ms,
        tool_call_count,
        output_chars,
        "agent runtime turn completed"
    );
}

pub fn record_runtime_failure(conversation_id: &str, phase: RuntimeFailurePhase, error: &str) {
    let phase_label: Cow<'static, str> = phase.to_string().into();
    counter!(METRIC_AGENT_RUNTIME_FAILURES, "phase" => phase_label).increment(1);

    tracing::error!(
        target: TARGET,
        event = "agent_runtime.failure",
        conversation_id = %conversation_id,
        phase = %phase,
        error = %error,
        "agent runtime failure"
    );
}

pub fn record_spawn_started(
    conversation_id: &str,
    client_id: Option<&str>,
    runner_turn_id: &str,
    runtime: &str,
    model: &str,
    reasoning_effort: &str,
    started_at_ms: i64,
) {
    counter!(METRIC_AGENT_SPAWN_STARTED, "runtime" => runtime.to_string()).increment(1);

    tracing::info!(
        target: TARGET,
        event = "agent_runtime.spawn_started",
        phase = %RuntimeLatencyPhase::CodexSpawnStart,
        conversation_id = %conversation_id,
        client_id = client_id.unwrap_or("none"),
        runner_turn_id = %runner_turn_id,
        runtime,
        model,
        reasoning_effort,
        started_at_ms,
        "Codex runtime spawn started"
    );
}

pub fn record_spawn_finished(
    conversation_id: &str,
    client_id: Option<&str>,
    runner_turn_id: &str,
    runtime: &str,
    status: &str,
    duration_ms: u64,
    finished_at_ms: i64,
) {
    histogram!(
        METRIC_AGENT_SPAWN_DURATION_MS,
        "runtime" => runtime.to_string(),
        "status" => status.to_string(),
    )
    .record(duration_ms as f64);

    tracing::info!(
        target: TARGET,
        event = "agent_runtime.spawn_finished",
        phase = %RuntimeLatencyPhase::CodexSpawnReady,
        conversation_id = %conversation_id,
        client_id = client_id.unwrap_or("none"),
        runner_turn_id = %runner_turn_id,
        runtime,
        status,
        duration_ms,
        finished_at_ms,
        "Codex runtime spawn finished"
    );
}

pub fn record_latency_marker(
    conversation_id: &str,
    client_id: Option<&str>,
    phase: RuntimeLatencyPhase,
    elapsed_ms: u64,
    observed_at_ms: i64,
    event_index: Option<i32>,
    bytes: Option<usize>,
) {
    histogram!(
        METRIC_AGENT_EVENT_LATENCY_MS,
        "phase" => phase.to_string(),
    )
    .record(elapsed_ms as f64);

    tracing::info!(
        target: TARGET,
        event = "agent_runtime.latency_marker",
        conversation_id = %conversation_id,
        client_id = client_id.unwrap_or("none"),
        phase = %phase,
        elapsed_ms,
        observed_at_ms,
        event_index,
        bytes,
        "agent runtime latency marker"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_failure_phase_labels_are_stable() {
        let cases = [
            (RuntimeFailurePhase::AttachmentStorage, "attachment_storage"),
            (RuntimeFailurePhase::StoreUserMessage, "store_user_message"),
            (RuntimeFailurePhase::CreateCheckpoint, "create_checkpoint"),
            (
                RuntimeFailurePhase::CreateAssistantMessage,
                "create_assistant_message",
            ),
            (
                RuntimeFailurePhase::StartupContextPreflight,
                "startup_context_preflight",
            ),
            (RuntimeFailurePhase::BuildCodexPrompt, "build_codex_prompt"),
            (RuntimeFailurePhase::ClaimRunnerTurn, "claim_runner_turn"),
            (RuntimeFailurePhase::SpawnCodex, "spawn_codex"),
            (RuntimeFailurePhase::WaitCodexTurn, "wait_codex_turn"),
            (RuntimeFailurePhase::CodexTurnFailed, "codex_turn_failed"),
            (RuntimeFailurePhase::RunnerJobFailed, "runner_job_failed"),
        ];

        for (phase, expected) in cases {
            assert_eq!(phase.to_string(), expected);
        }
    }

    #[test]
    fn runtime_latency_phase_labels_are_stable() {
        let cases = [
            (
                RuntimeLatencyPhase::WorkerProcessMessageStart,
                "worker_process_message_start",
            ),
            (RuntimeLatencyPhase::CodexSpawnStart, "codex_spawn_start"),
            (RuntimeLatencyPhase::CodexSpawnReady, "codex_spawn_ready"),
            (
                RuntimeLatencyPhase::CodexThreadStarted,
                "codex_thread_started",
            ),
            (
                RuntimeLatencyPhase::MessageStartPersisted,
                "message_start_persisted",
            ),
            (
                RuntimeLatencyPhase::FirstAssistantDeltaPersisted,
                "first_assistant_delta_persisted",
            ),
            (RuntimeLatencyPhase::HandlerReceived, "handler_received"),
            (RuntimeLatencyPhase::SubmitReceived, "submit_received"),
            (RuntimeLatencyPhase::SubmitEnqueued, "submit_enqueued"),
            (RuntimeLatencyPhase::RunnerClaimed, "runner_claimed"),
            (RuntimeLatencyPhase::WorkerStarting, "worker_starting"),
            (RuntimeLatencyPhase::WorkerFinished, "worker_finished"),
        ];

        for (phase, expected) in cases {
            assert_eq!(phase.to_string(), expected);
        }
    }
}
