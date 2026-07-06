//! Agent run child-batch and coordinator-wake observability.
//!
//! High-cardinality IDs are intentionally limited to structured tracing and
//! durable system-log details. Prometheus labels in this module stay bounded.

use std::borrow::Cow;
use std::fmt;

use anyhow::{Context, Result};
use chrono::Utc;
use metrics::{counter, gauge, histogram};
use serde_json::{json, Value};
use sqlx::SqlitePool;
use ticketing_system::conversation_turn_jobs::ConversationTurnJob;
use uuid::Uuid;

pub const METRIC_CHILD_BATCHES_TOTAL: &str = "af_child_batches_total";
pub const METRIC_WAKE_EVENTS_TOTAL: &str = "af_wake_events_total";
pub const METRIC_PARENT_WAIT_DURATION_SECONDS: &str = "af_parent_wait_duration_seconds";
pub const METRIC_TASK_QUEUE_DEPTH: &str = "af_task_queue_depth";
pub const METRIC_TASK_ACTIVE: &str = "af_task_active";
pub const METRIC_COORDINATOR_WAKE_QUEUE_DEPTH: &str = "af_coordinator_wake_queue_depth";
pub const METRIC_COORDINATOR_WAKE_OLDEST_AGE_SECONDS: &str =
    "af_coordinator_wake_oldest_age_seconds";
pub const METRIC_COORDINATOR_WAKE_DEAD_LETTER_DEPTH: &str = "af_coordinator_wake_dead_letter_depth";
pub const METRIC_TASK_TERMINAL_TOTAL: &str = "af_task_terminal_total";
pub const METRIC_REPORT_IDS_TOTAL: &str = "af_report_ids_total";

const COMPONENT: &str = "agent_run_lifecycle";
const QUEUE_CONVERSATION_TURN_JOBS: &str = "conversation_turn_jobs";
const WORKER_AGENT_RUNNER: &str = "agent_runner";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChildBatchState {
    Pending,
    Completed,
}

impl fmt::Display for ChildBatchState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Pending => "pending",
            Self::Completed => "completed",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WakeKind {
    SingleChild,
    BatchCompletion,
    CompletionRelay,
}

impl fmt::Display for WakeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::SingleChild => "single_child",
            Self::BatchCompletion => "batch_completion",
            Self::CompletionRelay => "completion_relay",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WakeOutcome {
    Delayed,
    Fired,
    Deduped,
    Suppressed,
    Missed,
}

impl fmt::Display for WakeOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Delayed => "delayed",
            Self::Fired => "fired",
            Self::Deduped => "deduped",
            Self::Suppressed => "suppressed",
            Self::Missed => "missed",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildBatchTelemetryContext {
    pub batch_id: String,
    pub expected_count: usize,
    pub child_index: Option<usize>,
    pub report_id: Option<String>,
}

pub fn new_report_id() -> String {
    format!("af-report-{}", Uuid::new_v4())
}

pub fn child_batch_context_from_metadata(
    message_metadata: Option<&str>,
) -> Option<ChildBatchTelemetryContext> {
    let value: Value = serde_json::from_str(message_metadata?).ok()?;
    if value.get("origin")?.as_str()? != "agent_orchestrated" {
        return None;
    }
    if value.get("orchestration")?.as_str()? != "child_initial_turn" {
        return None;
    }

    let batch_id = string_field(&value, "child_batch_id")?;
    let expected_count = value
        .get("child_batch_size")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value > 0)?;
    let child_index = value
        .get("child_batch_index")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value > 0);

    Some(ChildBatchTelemetryContext {
        batch_id,
        expected_count,
        child_index,
        report_id: string_field(&value, "report_id"),
    })
}

pub fn coordinator_wake_kind_from_metadata(metadata: Option<&str>) -> Option<WakeKind> {
    let value = coordinator_wake_metadata_value(metadata)?;
    if string_field(&value, "child_batch_id").is_some() {
        Some(WakeKind::BatchCompletion)
    } else {
        Some(WakeKind::SingleChild)
    }
}

pub fn coordinator_wake_report_id_from_metadata(metadata: Option<&str>) -> Option<String> {
    let value = coordinator_wake_metadata_value(metadata)?;
    string_field(&value, "report_id")
}

pub fn coordinator_wake_child_id_from_metadata(metadata: Option<&str>) -> Option<String> {
    let value = coordinator_wake_metadata_value(metadata)?;
    string_field(&value, "child_conversation_id")
}

pub fn coordinator_wake_batch_id_from_metadata(metadata: Option<&str>) -> Option<String> {
    let value = coordinator_wake_metadata_value(metadata)?;
    string_field(&value, "child_batch_id")
}

pub async fn record_child_batch_pending(
    db: &SqlitePool,
    parent_conversation_id: &str,
    context: &ChildBatchTelemetryContext,
    queued_children: usize,
) {
    record_child_batch_metric(ChildBatchState::Pending);
    let report_id = context.report_id.clone().unwrap_or_else(new_report_id);
    record_report_id("agent_run_lifecycle", "created");
    write_system_event(
        db,
        "info",
        "agent_run.child_batch_pending",
        json!({
            "event_name": "agent_run.child_batch_pending",
            "report_id": report_id,
            "parent_conversation_id": parent_conversation_id,
            "child_batch_id": context.batch_id.as_str(),
            "child_batch_size": context.expected_count,
            "queued_children": queued_children,
            "children_pending": context.expected_count,
            "queue": QUEUE_CONVERSATION_TURN_JOBS,
        }),
    )
    .await;
}

pub async fn record_child_batch_completed(
    db: &SqlitePool,
    parent_conversation_id: &str,
    context: &ChildBatchTelemetryContext,
    completed_children: usize,
    wait_duration_ms: Option<i64>,
) {
    record_child_batch_metric(ChildBatchState::Completed);
    let report_id = context.report_id.clone().unwrap_or_else(new_report_id);
    record_report_id("agent_run_lifecycle", "completed");
    if let Some(wait_duration_ms) = wait_duration_ms {
        histogram!(
            METRIC_PARENT_WAIT_DURATION_SECONDS,
            "queue" => QUEUE_CONVERSATION_TURN_JOBS,
            "outcome" => "completed",
        )
        .record(wait_duration_ms.max(0) as f64 / 1000.0);
    }
    write_system_event(
        db,
        "info",
        "agent_run.child_batch_completed",
        json!({
            "event_name": "agent_run.child_batch_completed",
            "report_id": report_id,
            "parent_conversation_id": parent_conversation_id,
            "child_batch_id": context.batch_id.as_str(),
            "child_batch_size": context.expected_count,
            "completed_children": completed_children,
            "wait_duration_ms": wait_duration_ms,
            "queue": QUEUE_CONVERSATION_TURN_JOBS,
        }),
    )
    .await;
}

pub async fn record_wake_delayed(
    db: &SqlitePool,
    parent_conversation_id: &str,
    context: &ChildBatchTelemetryContext,
    completed_children: usize,
    wait_duration_ms: Option<i64>,
) {
    record_wake_metric(WakeKind::BatchCompletion, WakeOutcome::Delayed);
    if let Some(wait_duration_ms) = wait_duration_ms {
        histogram!(
            METRIC_PARENT_WAIT_DURATION_SECONDS,
            "queue" => QUEUE_CONVERSATION_TURN_JOBS,
            "outcome" => "delayed",
        )
        .record(wait_duration_ms.max(0) as f64 / 1000.0);
    }
    let report_id = context.report_id.clone().unwrap_or_else(new_report_id);
    write_system_event(
        db,
        "info",
        "agent_run.wake_delayed",
        json!({
            "event_name": "agent_run.wake_delayed",
            "report_id": report_id,
            "wake_kind": WakeKind::BatchCompletion.to_string(),
            "parent_conversation_id": parent_conversation_id,
            "child_batch_id": context.batch_id.as_str(),
            "expected_children": context.expected_count,
            "completed_children": completed_children,
            "children_pending": context.expected_count.saturating_sub(completed_children),
            "wait_duration_ms": wait_duration_ms,
            "queue": QUEUE_CONVERSATION_TURN_JOBS,
        }),
    )
    .await;
}

pub async fn record_wake_deduped(
    db: &SqlitePool,
    parent_conversation_id: &str,
    context: &ChildBatchTelemetryContext,
    reason: &str,
) {
    record_wake_metric(WakeKind::BatchCompletion, WakeOutcome::Deduped);
    let report_id = context.report_id.clone().unwrap_or_else(new_report_id);
    write_system_event(
        db,
        "info",
        "agent_run.wake_deduped",
        json!({
            "event_name": "agent_run.wake_deduped",
            "report_id": report_id,
            "wake_kind": WakeKind::BatchCompletion.to_string(),
            "parent_conversation_id": parent_conversation_id,
            "child_batch_id": context.batch_id.as_str(),
            "reason": reason,
            "queue": QUEUE_CONVERSATION_TURN_JOBS,
        }),
    )
    .await;
}

pub async fn record_wake_suppressed(
    db: &SqlitePool,
    wake_kind: WakeKind,
    parent_conversation_id: Option<&str>,
    child_conversation_id: Option<&str>,
    child_batch_id: Option<&str>,
    reason: &str,
    pending_jobs: Option<i64>,
    max_pending_jobs: Option<i64>,
) {
    record_wake_metric(wake_kind, WakeOutcome::Suppressed);
    let report_id = new_report_id();
    record_report_id("agent_run_lifecycle", "suppressed");
    let level = if reason == "completion_relay_suppressed" {
        "info"
    } else {
        "warn"
    };
    write_system_event(
        db,
        level,
        "agent_run.wake_suppressed",
        json!({
            "event_name": "agent_run.wake_suppressed",
            "report_id": report_id,
            "wake_kind": wake_kind.to_string(),
            "parent_conversation_id": parent_conversation_id,
            "child_conversation_id": child_conversation_id,
            "child_batch_id": child_batch_id,
            "reason": reason,
            "pending_jobs": pending_jobs,
            "max_pending_jobs": max_pending_jobs,
            "queue": QUEUE_CONVERSATION_TURN_JOBS,
        }),
    )
    .await;
    refresh_queue_metrics(db).await;
}

pub async fn record_wake_missed(
    db: &SqlitePool,
    wake_kind: WakeKind,
    parent_conversation_id: &str,
    child_conversation_id: Option<&str>,
    child_batch_id: Option<&str>,
    error_type: &str,
    safe_error_summary: &str,
) {
    record_wake_metric(wake_kind, WakeOutcome::Missed);
    let report_id = new_report_id();
    record_report_id("agent_run_lifecycle", "error");
    write_system_event(
        db,
        "error",
        "agent_run.wake_missed",
        json!({
            "event_name": "agent_run.wake_missed",
            "report_id": report_id,
            "wake_kind": wake_kind.to_string(),
            "parent_conversation_id": parent_conversation_id,
            "child_conversation_id": child_conversation_id,
            "child_batch_id": child_batch_id,
            "error_type": error_type,
            "safe_error_summary": safe_error_summary,
            "queue": QUEUE_CONVERSATION_TURN_JOBS,
        }),
    )
    .await;
    refresh_queue_metrics(db).await;
}

pub async fn record_wake_fired(
    db: &SqlitePool,
    wake_kind: WakeKind,
    parent_conversation_id: &str,
    child_conversation_id: Option<&str>,
    child_batch_id: Option<&str>,
    job_id: &str,
    report_id: Option<&str>,
    completed_children: Option<usize>,
    wait_duration_ms: Option<i64>,
) {
    record_wake_metric(wake_kind, WakeOutcome::Fired);
    if let Some(wait_duration_ms) = wait_duration_ms {
        histogram!(
            METRIC_PARENT_WAIT_DURATION_SECONDS,
            "queue" => QUEUE_CONVERSATION_TURN_JOBS,
            "outcome" => "fired",
        )
        .record(wait_duration_ms.max(0) as f64 / 1000.0);
    }
    let report_id = report_id
        .map(ToOwned::to_owned)
        .unwrap_or_else(new_report_id);
    record_report_id("agent_run_lifecycle", "fired");
    write_system_event(
        db,
        "info",
        "agent_run.wake_fired",
        json!({
            "event_name": "agent_run.wake_fired",
            "report_id": report_id,
            "wake_kind": wake_kind.to_string(),
            "parent_conversation_id": parent_conversation_id,
            "child_conversation_id": child_conversation_id,
            "child_batch_id": child_batch_id,
            "job_id": job_id,
            "completed_children": completed_children,
            "wait_duration_ms": wait_duration_ms,
            "queue": QUEUE_CONVERSATION_TURN_JOBS,
        }),
    )
    .await;
    refresh_queue_metrics(db).await;
}

pub async fn record_coordinator_wake_terminal(
    db: &SqlitePool,
    job: &ConversationTurnJob,
    outcome: &str,
    error_message: Option<&str>,
) {
    let Some(wake_kind) =
        coordinator_wake_kind_from_metadata(job.payload.message_metadata.as_deref())
    else {
        refresh_queue_metrics(db).await;
        return;
    };
    let metadata = job.payload.message_metadata.as_deref();
    let error_type = error_message
        .map(ticketing_system::runner_capacity::classify_failure_message)
        .unwrap_or("none");
    let dead_letter_state = match outcome {
        "failed" => "terminal_failed",
        "cancelled" => "cancelled",
        _ => "none",
    };
    let retry_state = "none";

    counter!(
        METRIC_TASK_TERMINAL_TOTAL,
        "queue" => QUEUE_CONVERSATION_TURN_JOBS,
        "outcome" => outcome.to_string(),
        "retry_state" => retry_state,
        "dead_letter_state" => dead_letter_state,
        "error_type" => error_type,
    )
    .increment(1);

    let report_id =
        coordinator_wake_report_id_from_metadata(metadata).unwrap_or_else(new_report_id);
    if outcome != "completed" {
        record_report_id("agent_run_lifecycle", outcome);
    }
    write_system_event(
        db,
        if outcome == "failed" { "error" } else { "info" },
        "agent_run.coordinator_wake_terminal",
        json!({
            "event_name": "agent_run.coordinator_wake_terminal",
            "report_id": report_id,
            "wake_kind": wake_kind.to_string(),
            "parent_conversation_id": job.conversation_id.as_str(),
            "child_conversation_id": coordinator_wake_child_id_from_metadata(metadata),
            "child_batch_id": coordinator_wake_batch_id_from_metadata(metadata),
            "job_id": job.id.as_str(),
            "outcome": outcome,
            "retry_state": retry_state,
            "dead_letter_state": dead_letter_state,
            "error_type": error_type,
            "queue": QUEUE_CONVERSATION_TURN_JOBS,
        }),
    )
    .await;
    refresh_queue_metrics(db).await;
}

pub async fn refresh_queue_metrics(db: &SqlitePool) {
    if let Err(error) = refresh_queue_metrics_inner(db).await {
        tracing::warn!(
            target: "observability.agent_lifecycle",
            error = %error,
            "failed to refresh child batch/coordinator wake queue metrics"
        );
    }
}

async fn refresh_queue_metrics_inner(db: &SqlitePool) -> Result<()> {
    let (pending_jobs, running_jobs): (i64, i64) = sqlx::query_as(
        r#"
        SELECT
            COALESCE(SUM(CASE WHEN status = 'pending' THEN 1 ELSE 0 END), 0) AS pending_jobs,
            COALESCE(SUM(CASE WHEN status = 'running' THEN 1 ELSE 0 END), 0) AS running_jobs
        FROM conversation_turn_jobs
        "#,
    )
    .fetch_one(db)
    .await
    .context("load conversation turn queue depth")?;

    gauge!(METRIC_TASK_QUEUE_DEPTH, "queue" => QUEUE_CONVERSATION_TURN_JOBS)
        .set(pending_jobs as f64);
    gauge!(METRIC_TASK_ACTIVE, "worker_kind" => WORKER_AGENT_RUNNER).set(running_jobs as f64);

    let (pending_wakes, oldest_pending_created_at): (i64, Option<i64>) = sqlx::query_as(
        r#"
        SELECT COUNT(*) AS pending_wakes, MIN(created_at) AS oldest_pending_created_at
        FROM conversation_turn_jobs
        WHERE status = 'pending'
          AND message_metadata IS NOT NULL
          AND json_valid(message_metadata)
          AND json_extract(message_metadata, '$.origin') = 'agent_orchestrated'
          AND json_extract(message_metadata, '$.orchestration') = 'coordinator_child_completion_wake'
        "#,
    )
    .fetch_one(db)
    .await
    .context("load coordinator wake backlog")?;
    let oldest_age_seconds = oldest_pending_created_at
        .map(|created_at| Utc::now().timestamp().saturating_sub(created_at).max(0) as f64)
        .unwrap_or(0.0);
    gauge!(
        METRIC_COORDINATOR_WAKE_QUEUE_DEPTH,
        "wake_kind" => "all",
    )
    .set(pending_wakes as f64);
    gauge!(
        METRIC_COORDINATOR_WAKE_OLDEST_AGE_SECONDS,
        "wake_kind" => "all",
    )
    .set(oldest_age_seconds);

    let dead_letter_wakes: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)
        FROM conversation_turn_jobs
        WHERE status = 'failed'
          AND message_metadata IS NOT NULL
          AND json_valid(message_metadata)
          AND json_extract(message_metadata, '$.origin') = 'agent_orchestrated'
          AND json_extract(message_metadata, '$.orchestration') = 'coordinator_child_completion_wake'
        "#,
    )
    .fetch_one(db)
    .await
    .context("load coordinator wake dead-letter depth")?;
    gauge!(
        METRIC_COORDINATOR_WAKE_DEAD_LETTER_DEPTH,
        "wake_kind" => "all",
    )
    .set(dead_letter_wakes as f64);

    Ok(())
}

fn record_child_batch_metric(state: ChildBatchState) {
    let state_label: Cow<'static, str> = state.to_string().into();
    counter!(METRIC_CHILD_BATCHES_TOTAL, "state" => state_label).increment(1);
}

fn record_wake_metric(wake_kind: WakeKind, outcome: WakeOutcome) {
    let wake_kind_label: Cow<'static, str> = wake_kind.to_string().into();
    let outcome_label: Cow<'static, str> = outcome.to_string().into();
    counter!(
        METRIC_WAKE_EVENTS_TOTAL,
        "wake_kind" => wake_kind_label,
        "outcome" => outcome_label,
    )
    .increment(1);
}

fn record_report_id(surface: &str, outcome: &str) {
    let surface_label: Cow<'static, str> = surface.to_string().into();
    let outcome_label: Cow<'static, str> = outcome.to_string().into();
    counter!(
        METRIC_REPORT_IDS_TOTAL,
        "surface" => surface_label,
        "outcome" => outcome_label,
    )
    .increment(1);
}

async fn write_system_event(db: &SqlitePool, level: &str, event_name: &str, detail: Value) {
    let detail = detail.to_string();
    if let Err(error) = ticketing_system::system_logs::insert_log(
        db,
        level,
        COMPONENT,
        event_name,
        Some(&detail),
        None,
        None,
    )
    .await
    {
        tracing::warn!(
            target: "observability.agent_lifecycle",
            level,
            event_name,
            error = %error,
            "failed to write durable lifecycle event"
        );
    }
}

fn coordinator_wake_metadata_value(metadata: Option<&str>) -> Option<Value> {
    let value: Value = serde_json::from_str(metadata?).ok()?;
    if value.get("origin")?.as_str()? != "agent_orchestrated" {
        return None;
    }
    if value.get("orchestration")?.as_str()? != "coordinator_child_completion_wake" {
        return None;
    }
    Some(value)
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use ticketing_system::conversation_turn_jobs::ConversationTurnJobPayload;

    #[test]
    fn wake_kind_labels_are_stable() {
        let cases = [
            (WakeKind::SingleChild, "single_child"),
            (WakeKind::BatchCompletion, "batch_completion"),
            (WakeKind::CompletionRelay, "completion_relay"),
        ];

        for (kind, expected) in cases {
            assert_eq!(kind.to_string(), expected);
        }
    }

    #[test]
    fn wake_outcome_labels_are_stable() {
        let cases = [
            (WakeOutcome::Delayed, "delayed"),
            (WakeOutcome::Fired, "fired"),
            (WakeOutcome::Deduped, "deduped"),
            (WakeOutcome::Suppressed, "suppressed"),
            (WakeOutcome::Missed, "missed"),
        ];

        for (outcome, expected) in cases {
            assert_eq!(outcome.to_string(), expected);
        }
    }

    #[test]
    fn parses_child_batch_context_and_report_id() {
        let metadata = serde_json::json!({
            "origin": "agent_orchestrated",
            "orchestration": "child_initial_turn",
            "child_batch_id": "batch-1",
            "child_batch_size": 3,
            "child_batch_index": 2,
            "report_id": "af-report-test"
        })
        .to_string();

        let context = child_batch_context_from_metadata(Some(&metadata)).expect("context");

        assert_eq!(context.batch_id, "batch-1");
        assert_eq!(context.expected_count, 3);
        assert_eq!(context.child_index, Some(2));
        assert_eq!(context.report_id.as_deref(), Some("af-report-test"));
    }

    #[test]
    fn detects_batch_wake_metadata_without_using_ids_as_labels() {
        let metadata = serde_json::json!({
            "origin": "agent_orchestrated",
            "orchestration": "coordinator_child_completion_wake",
            "child_batch_id": "batch-1",
            "report_id": "af-report-test"
        })
        .to_string();

        assert_eq!(
            coordinator_wake_kind_from_metadata(Some(&metadata)),
            Some(WakeKind::BatchCompletion)
        );
        assert_eq!(
            coordinator_wake_report_id_from_metadata(Some(&metadata)).as_deref(),
            Some("af-report-test")
        );
    }

    #[tokio::test]
    async fn terminal_wake_event_records_dead_letter_state() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect sqlite");
        sqlx::query(
            r#"
            CREATE TABLE conversation_turn_jobs (
                id TEXT PRIMARY KEY,
                conversation_id TEXT NOT NULL,
                message_metadata TEXT,
                status TEXT NOT NULL,
                created_at INTEGER NOT NULL
            )
            "#,
        )
        .execute(&pool)
        .await
        .expect("create conversation_turn_jobs");
        sqlx::query(
            r#"
            CREATE TABLE system_logs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                level TEXT NOT NULL,
                component TEXT NOT NULL,
                message TEXT NOT NULL,
                detail TEXT,
                user_id TEXT,
                session_id TEXT,
                created_at INTEGER NOT NULL
            )
            "#,
        )
        .execute(&pool)
        .await
        .expect("create system_logs");

        let metadata = serde_json::json!({
            "origin": "agent_orchestrated",
            "orchestration": "coordinator_child_completion_wake",
            "child_batch_id": "batch-1",
            "report_id": "af-report-test"
        })
        .to_string();
        let job = ConversationTurnJob {
            id: "job-1".to_string(),
            conversation_id: "parent-1".to_string(),
            payload: ConversationTurnJobPayload {
                user_id: "user-1".to_string(),
                message: "wake".to_string(),
                agent_type: "full-access".to_string(),
                runtime: "codex-app-server".to_string(),
                prompt_name: "full-access".to_string(),
                working_dir: "/tmp".to_string(),
                prompt_vars: HashMap::new(),
                images_json: None,
                client_id: None,
                message_metadata: Some(metadata),
            },
            status: "running".to_string(),
            locked_by_generation_id: Some("runner-1".to_string()),
            error_message: None,
            created_at: 1,
            updated_at: 2,
            started_at: Some(2),
            completed_at: None,
        };

        record_coordinator_wake_terminal(&pool, &job, "failed", Some("usage limit reached")).await;

        let (message, detail): (String, String) = sqlx::query_as(
            "SELECT message, detail FROM system_logs WHERE component = ? ORDER BY id DESC LIMIT 1",
        )
        .bind(COMPONENT)
        .fetch_one(&pool)
        .await
        .expect("system log row");
        let detail: Value = serde_json::from_str(&detail).expect("detail json");

        assert_eq!(message, "agent_run.coordinator_wake_terminal");
        assert_eq!(detail["report_id"], "af-report-test");
        assert_eq!(detail["wake_kind"], "batch_completion");
        assert_eq!(detail["dead_letter_state"], "terminal_failed");
        assert_eq!(detail["error_type"], "usage_limit");
        assert_eq!(detail["job_id"], "job-1");
    }
}
