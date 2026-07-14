//! Continuously monitors every writable mounted volume and turns pressure
//! threshold transitions into durable incidents, APNs alerts, and Codex jobs.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;
use sqlx::{Connection, FromRow, SqlitePool};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use ticketing_system::conversation_turn_jobs::ConversationTurnJobPayload;
use ticketing_system::conversations::InitialChildTurnJobRequest;
use ticketing_system::models::{
    CreateChildConversationRequest, CreateConversationRequest, SystemLogIncidentStatus,
};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use uuid::Uuid;

use crate::agents::prompts::load_prompt;
use crate::agents::working_dir::resolve_working_dir;
use crate::agents::AgentType;
use crate::apns::ApnsService;
use crate::handlers::chat_stream::{encode_codex_options_for_job, ChatCodexOptions, ChatRuntime};

const OWNER_USER_ID: &str = "alex";
const ORGANIZATION: &str = "agentic-flowstate";
const COMPONENT: &str = "disk_pressure";
const DEFAULT_POLL_SECONDS: u64 = 5 * 60;
const MAX_POLL_SECONDS: u64 = 5 * 60;
const STARTUP_DELAY_SECONDS: u64 = 5;
const WARNING_THRESHOLD: f64 = 75.0;
const CRITICAL_THRESHOLD: f64 = 85.0;
const EMERGENCY_THRESHOLD: f64 = 95.0;
const RECOVERY_THRESHOLD: f64 = 70.0;
const PARENT_CONVERSATION_TYPE: &str = "disk-pressure-monitor";
const CHILD_CONVERSATION_TYPE: &str = "disk-pressure-diagnostic";

const METRIC_POLLS: &str = "disk_pressure_polls_total";
const METRIC_POLL_DURATION: &str = "disk_pressure_poll_duration_seconds";
const METRIC_RESPONSE_DURATION: &str = "disk_pressure_response_duration_seconds";
const METRIC_TRANSITIONS: &str = "disk_pressure_transitions_total";
const METRIC_NOTIFICATIONS: &str = "disk_pressure_notifications_total";
const METRIC_DIAGNOSTICS: &str = "disk_pressure_diagnostic_jobs_total";
const METRIC_VOLUME_COUNT: &str = "disk_pressure_writable_volumes";
const METRIC_MAX_USED_RATIO: &str = "disk_pressure_max_used_ratio";
const METRIC_ACTIVE_INCIDENTS: &str = "disk_pressure_active_incidents";

const VOLUME_STATE_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS disk_pressure_volume_state (
    volume_id TEXT PRIMARY KEY,
    volume_source TEXT NOT NULL,
    mount_path TEXT NOT NULL,
    filesystem_type TEXT NOT NULL,
    current_stage INTEGER NOT NULL DEFAULT 0
        CHECK(current_stage IN (0, 75, 85, 95)),
    cycle INTEGER NOT NULL DEFAULT 0 CHECK(cycle >= 0),
    action_id TEXT,
    action_kind TEXT CHECK(action_kind IN ('detected', 'escalated', 'recovered')),
    action_stage INTEGER CHECK(action_stage IN (0, 75, 85, 95)),
    incident_id TEXT,
    notification_status TEXT NOT NULL DEFAULT 'not_required'
        CHECK(notification_status IN ('pending', 'sent', 'failed', 'not_required')),
    diagnostic_status TEXT NOT NULL DEFAULT 'not_required'
        CHECK(diagnostic_status IN ('pending', 'enqueued', 'failed', 'not_required')),
    last_conversation_id TEXT,
    last_job_id TEXT,
    last_notification_error TEXT,
    last_diagnostic_error TEXT,
    total_bytes INTEGER NOT NULL,
    used_bytes INTEGER NOT NULL,
    available_bytes INTEGER NOT NULL,
    used_percent REAL NOT NULL,
    first_observed_at INTEGER NOT NULL,
    last_observed_at INTEGER NOT NULL,
    last_transition_at INTEGER,
    CHECK(
        (action_id IS NULL AND action_kind IS NULL AND action_stage IS NULL)
        OR
        (action_id IS NOT NULL AND action_kind IS NOT NULL AND action_stage IS NOT NULL)
    )
)
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(i64)]
enum PressureStage {
    Healthy = 0,
    Warning = 75,
    Critical = 85,
    Emergency = 95,
}

impl PressureStage {
    fn from_used_percent(used_percent: f64) -> Self {
        if used_percent >= EMERGENCY_THRESHOLD {
            Self::Emergency
        } else if used_percent >= CRITICAL_THRESHOLD {
            Self::Critical
        } else if used_percent >= WARNING_THRESHOLD {
            Self::Warning
        } else {
            Self::Healthy
        }
    }

    fn from_db(value: i64) -> Result<Self> {
        match value {
            0 => Ok(Self::Healthy),
            75 => Ok(Self::Warning),
            85 => Ok(Self::Critical),
            95 => Ok(Self::Emergency),
            other => bail!("invalid persisted disk pressure stage: {other}"),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Warning => "warning",
            Self::Critical => "critical",
            Self::Emergency => "emergency",
        }
    }

    fn threshold(self) -> i64 {
        self as i64
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransitionKind {
    Detected,
    Escalated,
    Recovered,
}

impl TransitionKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Detected => "detected",
            Self::Escalated => "escalated",
            Self::Recovered => "recovered",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "detected" => Ok(Self::Detected),
            "escalated" => Ok(Self::Escalated),
            "recovered" => Ok(Self::Recovered),
            other => bail!("invalid persisted disk pressure transition: {other}"),
        }
    }
}

#[derive(Debug, Clone)]
struct DiskPressureConfig {
    poll_interval: Duration,
}

impl DiskPressureConfig {
    fn from_env() -> Result<Self> {
        let seconds = match std::env::var("DISK_PRESSURE_POLL_SECONDS") {
            Ok(raw) => raw
                .parse::<u64>()
                .with_context(|| "DISK_PRESSURE_POLL_SECONDS must be an integer")?,
            Err(std::env::VarError::NotPresent) => DEFAULT_POLL_SECONDS,
            Err(error) => return Err(error).context("read DISK_PRESSURE_POLL_SECONDS"),
        };
        if seconds == 0 || seconds > MAX_POLL_SECONDS {
            bail!(
                "DISK_PRESSURE_POLL_SECONDS must be between 1 and {MAX_POLL_SECONDS}; got {seconds}"
            );
        }
        Ok(Self {
            poll_interval: Duration::from_secs(seconds),
        })
    }
}

#[derive(Debug, Clone)]
struct VolumeSample {
    volume_id: String,
    volume_source: String,
    mount_path: String,
    filesystem_type: String,
    total_bytes: i64,
    used_bytes: i64,
    available_bytes: i64,
    used_percent: f64,
}

#[derive(Debug, FromRow)]
struct StoredVolumeState {
    current_stage: i64,
    cycle: i64,
    action_id: Option<String>,
    action_kind: Option<String>,
    action_stage: Option<i64>,
    incident_id: Option<String>,
    notification_status: String,
    diagnostic_status: String,
    last_conversation_id: Option<String>,
}

#[derive(Debug, Clone)]
struct PendingAction {
    action_id: String,
    kind: TransitionKind,
    stage: PressureStage,
    cycle: i64,
    sample: VolumeSample,
    incident_id: Option<String>,
    notification_status: String,
    diagnostic_status: String,
    last_conversation_id: Option<String>,
}

#[derive(Debug)]
struct ObserveResult {
    action: Option<PendingAction>,
    transition_created: bool,
}

#[derive(Debug, Clone)]
struct DiagnosticJob {
    conversation_id: String,
    job_id: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct PollSummary {
    sampled_volumes: usize,
    transitions: usize,
    notifications_sent: usize,
    diagnostics_enqueued: usize,
    recoveries: usize,
    action_failures: usize,
}

#[async_trait]
trait PressureResponder: Send + Sync {
    async fn ensure_incident(&self, action: &PendingAction) -> Result<String>;
    async fn recover_incident(&self, action: &PendingAction, incident_id: &str) -> Result<()>;
    async fn enqueue_diagnostic(
        &self,
        action: &PendingAction,
        incident_id: &str,
    ) -> Result<DiagnosticJob>;
    async fn send_notification(
        &self,
        action: &PendingAction,
        incident_id: &str,
        conversation_id: Option<&str>,
    ) -> Result<()>;
}

struct DiskPressureMonitor {
    pool: Arc<SqlitePool>,
    responder: Arc<dyn PressureResponder>,
}

impl DiskPressureMonitor {
    fn new(pool: Arc<SqlitePool>, responder: Arc<dyn PressureResponder>) -> Self {
        Self { pool, responder }
    }

    async fn process_samples(&self, samples: Vec<VolumeSample>) -> Result<PollSummary> {
        let mut summary = PollSummary {
            sampled_volumes: samples.len(),
            ..PollSummary::default()
        };
        record_volume_gauges(&samples);

        for sample in samples {
            let observe = match self.observe_volume(sample.clone()).await {
                Ok(observe) => observe,
                Err(error) => {
                    summary.action_failures += 1;
                    tracing::error!(
                        component = COMPONENT,
                        operation = "disk_pressure.state_observe_failed",
                        volume_id = %sample.volume_id,
                        mount_path = %sample.mount_path,
                        error = %error,
                        "failed to persist disk pressure observation"
                    );
                    continue;
                }
            };

            if observe.transition_created {
                summary.transitions += 1;
                if let Some(action) = observe.action.as_ref() {
                    metrics::counter!(
                        METRIC_TRANSITIONS,
                        "kind" => action.kind.as_str(),
                        "stage" => action.stage.label()
                    )
                    .increment(1);
                }
            }

            let Some(action) = observe.action else {
                continue;
            };
            self.process_action(&action, &mut summary).await;
        }

        let active_incidents: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM disk_pressure_volume_state WHERE current_stage > 0",
        )
        .fetch_one(self.pool.as_ref())
        .await
        .context("count active disk pressure incidents")?;
        metrics::gauge!(METRIC_ACTIVE_INCIDENTS).set(active_incidents as f64);
        Ok(summary)
    }

    async fn process_action(&self, action: &PendingAction, summary: &mut PollSummary) {
        if action.kind == TransitionKind::Recovered {
            let Some(incident_id) = action.incident_id.as_deref() else {
                summary.action_failures += 1;
                self.record_action_failure(action, "incident_recovery", "missing incident id")
                    .await;
                return;
            };
            let started = Instant::now();
            let recovery_result = self.responder.recover_incident(action, incident_id).await;
            record_response_duration("recovery", &recovery_result, started);
            match recovery_result {
                Ok(()) => {
                    summary.recoveries += 1;
                    if let Err(error) = self.clear_action(&action.action_id).await {
                        summary.action_failures += 1;
                        tracing::error!(
                            component = COMPONENT,
                            operation = "disk_pressure.recovery_state_finalize_failed",
                            action_id = %action.action_id,
                            error = %error,
                            "failed to finalize disk pressure recovery state"
                        );
                    }
                }
                Err(error) => {
                    summary.action_failures += 1;
                    self.record_action_failure(action, "incident_recovery", &error.to_string())
                        .await;
                }
            }
            return;
        }

        let started = Instant::now();
        let incident_result = self.responder.ensure_incident(action).await;
        record_response_duration("incident", &incident_result, started);
        let incident_id = match incident_result {
            Ok(incident_id) => incident_id,
            Err(error) => {
                summary.action_failures += 1;
                self.record_action_failure(action, "incident_upsert", &error.to_string())
                    .await;
                return;
            }
        };
        if let Err(error) = self
            .store_incident_id(&action.action_id, &incident_id)
            .await
        {
            summary.action_failures += 1;
            self.record_action_failure(action, "incident_state_link", &error.to_string())
                .await;
            return;
        }

        let mut conversation_id = action.last_conversation_id.clone();
        if action.diagnostic_status == "pending" {
            let started = Instant::now();
            let diagnostic_result = self
                .responder
                .enqueue_diagnostic(action, &incident_id)
                .await;
            record_response_duration("diagnostic", &diagnostic_result, started);
            match diagnostic_result {
                Ok(job) => {
                    if let Err(error) = self.mark_diagnostic_enqueued(&action.action_id, &job).await
                    {
                        summary.action_failures += 1;
                        self.record_action_failure(
                            action,
                            "diagnostic_state_update",
                            &error.to_string(),
                        )
                        .await;
                        return;
                    }
                    conversation_id = Some(job.conversation_id);
                    summary.diagnostics_enqueued += 1;
                    metrics::counter!(METRIC_DIAGNOSTICS, "result" => "enqueued").increment(1);
                }
                Err(error) => {
                    let error_text = error.to_string();
                    if let Err(state_error) = self
                        .mark_diagnostic_failed(&action.action_id, &error_text)
                        .await
                    {
                        tracing::error!(
                            component = COMPONENT,
                            operation = "disk_pressure.diagnostic_failure_state_failed",
                            action_id = %action.action_id,
                            error = %state_error,
                            "failed to persist diagnostic enqueue failure"
                        );
                        return;
                    }
                    summary.action_failures += 1;
                    metrics::counter!(METRIC_DIAGNOSTICS, "result" => "failed").increment(1);
                    self.record_action_failure(action, "diagnostic_enqueue", &error_text)
                        .await;
                }
            }
        }

        if action.notification_status == "pending" {
            let started = Instant::now();
            let notification_result = self
                .responder
                .send_notification(action, &incident_id, conversation_id.as_deref())
                .await;
            record_response_duration("notification", &notification_result, started);
            match notification_result {
                Ok(()) => {
                    if let Err(error) = self.mark_notification_sent(&action.action_id).await {
                        summary.action_failures += 1;
                        self.record_action_failure(
                            action,
                            "notification_state_update",
                            &error.to_string(),
                        )
                        .await;
                        return;
                    }
                    summary.notifications_sent += 1;
                    metrics::counter!(METRIC_NOTIFICATIONS, "result" => "sent").increment(1);
                }
                Err(error) => {
                    let error_text = error.to_string();
                    if let Err(state_error) = self
                        .mark_notification_failed(&action.action_id, &error_text)
                        .await
                    {
                        tracing::error!(
                            component = COMPONENT,
                            operation = "disk_pressure.notification_failure_state_failed",
                            action_id = %action.action_id,
                            error = %state_error,
                            "failed to persist APNs notification failure"
                        );
                        return;
                    }
                    summary.action_failures += 1;
                    metrics::counter!(METRIC_NOTIFICATIONS, "result" => "failed").increment(1);
                    self.record_action_failure(action, "notification_send", &error_text)
                        .await;
                }
            }
        }

        if let Err(error) = self.clear_action(&action.action_id).await {
            summary.action_failures += 1;
            tracing::error!(
                component = COMPONENT,
                operation = "disk_pressure.action_finalize_failed",
                action_id = %action.action_id,
                error = %error,
                "failed to finalize disk pressure action"
            );
        }
    }

    async fn observe_volume(&self, sample: VolumeSample) -> Result<ObserveResult> {
        let now = Utc::now().timestamp();
        let mut connection = self.pool.acquire().await?;
        let mut tx = connection
            .begin_with("BEGIN IMMEDIATE")
            .await
            .context("begin disk pressure observation transaction")?;

        let stored = sqlx::query_as::<_, StoredVolumeState>(
            r#"
            SELECT current_stage, cycle, action_id, action_kind, action_stage,
                   incident_id, notification_status, diagnostic_status,
                   last_conversation_id
            FROM disk_pressure_volume_state
            WHERE volume_id = ?
            "#,
        )
        .bind(&sample.volume_id)
        .fetch_optional(&mut *tx)
        .await?;

        let stored = match stored {
            Some(stored) => stored,
            None => {
                sqlx::query(
                    r#"
                    INSERT INTO disk_pressure_volume_state (
                        volume_id, volume_source, mount_path, filesystem_type,
                        current_stage, cycle, notification_status, diagnostic_status,
                        total_bytes, used_bytes, available_bytes, used_percent,
                        first_observed_at, last_observed_at
                    ) VALUES (?, ?, ?, ?, 0, 0, 'not_required', 'not_required', ?, ?, ?, ?, ?, ?)
                    "#,
                )
                .bind(&sample.volume_id)
                .bind(&sample.volume_source)
                .bind(&sample.mount_path)
                .bind(&sample.filesystem_type)
                .bind(sample.total_bytes)
                .bind(sample.used_bytes)
                .bind(sample.available_bytes)
                .bind(sample.used_percent)
                .bind(now)
                .bind(now)
                .execute(&mut *tx)
                .await?;
                StoredVolumeState {
                    current_stage: 0,
                    cycle: 0,
                    action_id: None,
                    action_kind: None,
                    action_stage: None,
                    incident_id: None,
                    notification_status: "not_required".to_string(),
                    diagnostic_status: "not_required".to_string(),
                    last_conversation_id: None,
                }
            }
        };

        sqlx::query(
            r#"
            UPDATE disk_pressure_volume_state
            SET volume_source = ?, mount_path = ?, filesystem_type = ?,
                total_bytes = ?, used_bytes = ?, available_bytes = ?,
                used_percent = ?, last_observed_at = ?
            WHERE volume_id = ?
            "#,
        )
        .bind(&sample.volume_source)
        .bind(&sample.mount_path)
        .bind(&sample.filesystem_type)
        .bind(sample.total_bytes)
        .bind(sample.used_bytes)
        .bind(sample.available_bytes)
        .bind(sample.used_percent)
        .bind(now)
        .bind(&sample.volume_id)
        .execute(&mut *tx)
        .await?;

        if let (Some(action_id), Some(kind), Some(action_stage)) = (
            stored.action_id.clone(),
            stored.action_kind.as_deref(),
            stored.action_stage,
        ) {
            let action = PendingAction {
                action_id,
                kind: TransitionKind::parse(kind)?,
                stage: PressureStage::from_db(action_stage)?,
                cycle: stored.cycle,
                sample,
                incident_id: stored.incident_id,
                notification_status: stored.notification_status,
                diagnostic_status: stored.diagnostic_status,
                last_conversation_id: stored.last_conversation_id,
            };
            tx.commit().await?;
            return Ok(ObserveResult {
                action: Some(action),
                transition_created: false,
            });
        }

        let current_stage = PressureStage::from_db(stored.current_stage)?;
        let observed_stage = PressureStage::from_used_percent(sample.used_percent);
        let transition = if current_stage == PressureStage::Healthy
            && observed_stage > PressureStage::Healthy
        {
            Some((TransitionKind::Detected, observed_stage, stored.cycle + 1))
        } else if current_stage > PressureStage::Healthy && sample.used_percent < RECOVERY_THRESHOLD
        {
            Some((
                TransitionKind::Recovered,
                PressureStage::Healthy,
                stored.cycle,
            ))
        } else if current_stage > PressureStage::Healthy && observed_stage > current_stage {
            Some((TransitionKind::Escalated, observed_stage, stored.cycle))
        } else {
            None
        };

        let Some((kind, stage, cycle)) = transition else {
            tx.commit().await?;
            return Ok(ObserveResult {
                action: None,
                transition_created: false,
            });
        };

        let action_id = stable_action_id(&sample.volume_id, cycle, kind, stage);
        let (notification_status, diagnostic_status) = if kind == TransitionKind::Recovered {
            ("not_required", "not_required")
        } else {
            ("pending", "pending")
        };
        sqlx::query(
            r#"
            UPDATE disk_pressure_volume_state
            SET current_stage = ?, cycle = ?, action_id = ?, action_kind = ?,
                action_stage = ?, notification_status = ?, diagnostic_status = ?,
                last_notification_error = NULL, last_diagnostic_error = NULL,
                last_transition_at = ?
            WHERE volume_id = ?
            "#,
        )
        .bind(stage.threshold())
        .bind(cycle)
        .bind(&action_id)
        .bind(kind.as_str())
        .bind(stage.threshold())
        .bind(notification_status)
        .bind(diagnostic_status)
        .bind(now)
        .bind(&sample.volume_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        tracing::warn!(
            component = COMPONENT,
            operation = "disk_pressure.transition_claimed",
            action_id = %action_id,
            volume_id = %sample.volume_id,
            volume_source = %sample.volume_source,
            mount_path = %sample.mount_path,
            transition = kind.as_str(),
            stage = stage.label(),
            used_percent = sample.used_percent,
            cycle,
            "claimed durable disk pressure transition"
        );
        Ok(ObserveResult {
            action: Some(PendingAction {
                action_id,
                kind,
                stage,
                cycle,
                sample,
                incident_id: stored.incident_id,
                notification_status: notification_status.to_string(),
                diagnostic_status: diagnostic_status.to_string(),
                last_conversation_id: stored.last_conversation_id,
            }),
            transition_created: true,
        })
    }

    async fn store_incident_id(&self, action_id: &str, incident_id: &str) -> Result<()> {
        update_action_row(
            self.pool.as_ref(),
            action_id,
            "incident_id = ?",
            incident_id,
        )
        .await
    }

    async fn mark_diagnostic_enqueued(&self, action_id: &str, job: &DiagnosticJob) -> Result<()> {
        let result = sqlx::query(
            r#"
            UPDATE disk_pressure_volume_state
            SET diagnostic_status = 'enqueued', last_conversation_id = ?,
                last_job_id = ?, last_diagnostic_error = NULL
            WHERE action_id = ? AND diagnostic_status = 'pending'
            "#,
        )
        .bind(&job.conversation_id)
        .bind(&job.job_id)
        .bind(action_id)
        .execute(self.pool.as_ref())
        .await?;
        ensure_action_row_updated(result.rows_affected(), action_id, "diagnostic enqueued")
    }

    async fn mark_diagnostic_failed(&self, action_id: &str, error: &str) -> Result<()> {
        let result = sqlx::query(
            r#"
            UPDATE disk_pressure_volume_state
            SET diagnostic_status = 'failed', last_diagnostic_error = ?
            WHERE action_id = ? AND diagnostic_status = 'pending'
            "#,
        )
        .bind(truncate_error(error))
        .bind(action_id)
        .execute(self.pool.as_ref())
        .await?;
        ensure_action_row_updated(result.rows_affected(), action_id, "diagnostic failed")
    }

    async fn mark_notification_sent(&self, action_id: &str) -> Result<()> {
        let result = sqlx::query(
            r#"
            UPDATE disk_pressure_volume_state
            SET notification_status = 'sent', last_notification_error = NULL
            WHERE action_id = ? AND notification_status = 'pending'
            "#,
        )
        .bind(action_id)
        .execute(self.pool.as_ref())
        .await?;
        ensure_action_row_updated(result.rows_affected(), action_id, "notification sent")
    }

    async fn mark_notification_failed(&self, action_id: &str, error: &str) -> Result<()> {
        let result = sqlx::query(
            r#"
            UPDATE disk_pressure_volume_state
            SET notification_status = 'failed', last_notification_error = ?
            WHERE action_id = ? AND notification_status = 'pending'
            "#,
        )
        .bind(truncate_error(error))
        .bind(action_id)
        .execute(self.pool.as_ref())
        .await?;
        ensure_action_row_updated(result.rows_affected(), action_id, "notification failed")
    }

    async fn clear_action(&self, action_id: &str) -> Result<()> {
        let result = sqlx::query(
            r#"
            UPDATE disk_pressure_volume_state
            SET action_id = NULL, action_kind = NULL, action_stage = NULL,
                notification_status = 'not_required', diagnostic_status = 'not_required'
            WHERE action_id = ?
              AND notification_status IN ('sent', 'failed', 'not_required')
              AND diagnostic_status IN ('enqueued', 'failed', 'not_required')
            "#,
        )
        .bind(action_id)
        .execute(self.pool.as_ref())
        .await?;
        ensure_action_row_updated(result.rows_affected(), action_id, "action finalized")
    }

    async fn record_action_failure(&self, action: &PendingAction, operation: &str, error: &str) {
        let detail = json!({
            "action_id": action.action_id,
            "volume_id": action.sample.volume_id,
            "incident_id": action.incident_id,
            "transition": action.kind.as_str(),
            "stage": action.stage.label(),
            "operation": operation,
            "error": truncate_error(error),
        })
        .to_string();
        if let Err(log_error) = ticketing_system::system_logs::insert_log(
            self.pool.as_ref(),
            "error",
            COMPONENT,
            "Disk pressure response action failed",
            Some(&detail),
            Some(OWNER_USER_ID),
            None,
        )
        .await
        {
            tracing::error!(
                component = COMPONENT,
                operation = "disk_pressure.failure_system_log_failed",
                action_id = %action.action_id,
                failed_operation = operation,
                error = %error,
                system_log_error = %log_error,
                "disk pressure action failed and its durable system log could not be written"
            );
        } else {
            tracing::error!(
                component = COMPONENT,
                operation = "disk_pressure.action_failed",
                action_id = %action.action_id,
                volume_id = %action.sample.volume_id,
                failed_operation = operation,
                error = %error,
                "disk pressure response action failed"
            );
        }
    }
}

async fn update_action_row(
    pool: &SqlitePool,
    action_id: &str,
    assignment: &str,
    value: &str,
) -> Result<()> {
    let sql = format!("UPDATE disk_pressure_volume_state SET {assignment} WHERE action_id = ?");
    let result = sqlx::query(&sql)
        .bind(value)
        .bind(action_id)
        .execute(pool)
        .await?;
    ensure_action_row_updated(result.rows_affected(), action_id, "action metadata updated")
}

fn ensure_action_row_updated(rows: u64, action_id: &str, operation: &str) -> Result<()> {
    if rows == 1 {
        Ok(())
    } else {
        bail!("disk pressure {operation} affected {rows} rows for action {action_id}")
    }
}

fn stable_action_id(
    volume_id: &str,
    cycle: i64,
    kind: TransitionKind,
    stage: PressureStage,
) -> String {
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!(
            "agentic-flowstate://disk-pressure/{volume_id}/{cycle}/{}/{stage}",
            kind.as_str(),
            stage = stage.threshold()
        )
        .as_bytes(),
    )
    .to_string()
}

fn truncate_error(error: &str) -> String {
    const MAX_CHARS: usize = 500;
    if error.chars().count() <= MAX_CHARS {
        return error.to_string();
    }
    let mut value = error.chars().take(MAX_CHARS).collect::<String>();
    value.push_str("...");
    value
}

struct ProductionResponder {
    pool: Arc<SqlitePool>,
}

impl ProductionResponder {
    fn new(pool: Arc<SqlitePool>) -> Self {
        Self { pool }
    }

    async fn existing_incident_for_action(&self, action_id: &str) -> Result<Option<String>> {
        sqlx::query_scalar(
            r#"
            SELECT occurrence.incident_id
            FROM system_logs AS logs
            JOIN system_log_incident_occurrences AS occurrence ON occurrence.log_id = logs.id
            WHERE json_valid(logs.detail)
              AND json_extract(logs.detail, '$.action_id') = ?
            ORDER BY logs.id DESC
            LIMIT 1
            "#,
        )
        .bind(action_id)
        .fetch_optional(self.pool.as_ref())
        .await
        .context("find existing incident occurrence for disk pressure action")
    }

    async fn update_incident_sample(
        &self,
        incident_id: &str,
        level: &str,
        detail: &str,
    ) -> Result<()> {
        let now = Utc::now().timestamp();
        sqlx::query(
            r#"
            UPDATE system_log_incidents
            SET level = ?, sample_detail = ?, owner_agent = 'full-access', updated_at = ?
            WHERE incident_id = ?
            "#,
        )
        .bind(level)
        .bind(detail)
        .bind(now)
        .bind(incident_id)
        .execute(self.pool.as_ref())
        .await?;
        ticketing_system::system_logs::set_system_log_incident_status(
            self.pool.as_ref(),
            incident_id,
            SystemLogIncidentStatus::Investigating,
        )
        .await?;
        Ok(())
    }

    async fn existing_diagnostic_job(&self, action_id: &str) -> Result<Option<DiagnosticJob>> {
        let row = sqlx::query_as::<_, (String, String)>(
            r#"
            SELECT id, conversation_id
            FROM conversation_turn_jobs
            WHERE message_metadata IS NOT NULL
              AND json_valid(message_metadata)
              AND json_extract(message_metadata, '$.disk_pressure_action_id') = ?
            ORDER BY created_at ASC
            LIMIT 1
            "#,
        )
        .bind(action_id)
        .fetch_optional(self.pool.as_ref())
        .await
        .context("find existing disk pressure diagnostic job")?;
        Ok(row.map(|(job_id, conversation_id)| DiagnosticJob {
            conversation_id,
            job_id,
        }))
    }

    async fn ensure_parent_conversation(&self) -> Result<String> {
        if let Some(parent_id) = sqlx::query_scalar::<_, String>(
            r#"
            SELECT id
            FROM conversations
            WHERE user_id = ? AND organization = ?
              AND conversation_type = ? AND parent_conversation_id IS NULL
              AND status IN ('open', 'waiting')
            ORDER BY started_at ASC
            LIMIT 1
            "#,
        )
        .bind(OWNER_USER_ID)
        .bind(ORGANIZATION)
        .bind(PARENT_CONVERSATION_TYPE)
        .fetch_optional(self.pool.as_ref())
        .await?
        {
            return Ok(parent_id);
        }

        let parent = ticketing_system::conversations::create_conversation(
            self.pool.as_ref(),
            CreateConversationRequest {
                user_id: OWNER_USER_ID.to_string(),
                organization: ORGANIZATION.to_string(),
                title: "Disk Pressure Monitor".to_string(),
                session_id: None,
                agent: Some("full-access".to_string()),
                conversation_type: Some(PARENT_CONVERSATION_TYPE.to_string()),
                parent_conversation_id: None,
                conversation_role: Some("standard".to_string()),
                child_sort_order: None,
            },
        )
        .await
        .context("create disk pressure monitor parent conversation")?;
        Ok(parent.id)
    }

    fn action_detail(action: &PendingAction, event: &str) -> String {
        json!({
            "event": event,
            "action_id": action.action_id,
            "volume_id": action.sample.volume_id,
            "volume_source": action.sample.volume_source,
            "mount_path": action.sample.mount_path,
            "filesystem_type": action.sample.filesystem_type,
            "transition": action.kind.as_str(),
            "stage": action.stage.label(),
            "stage_threshold_percent": action.stage.threshold(),
            "used_percent": action.sample.used_percent,
            "total_bytes": action.sample.total_bytes,
            "used_bytes": action.sample.used_bytes,
            "available_bytes": action.sample.available_bytes,
            "cycle": action.cycle,
            "automatic_cleanup": false,
        })
        .to_string()
    }
}

#[async_trait]
impl PressureResponder for ProductionResponder {
    async fn ensure_incident(&self, action: &PendingAction) -> Result<String> {
        let detail = Self::action_detail(action, action.kind.as_str());
        let level = match action.stage {
            PressureStage::Warning => "error",
            PressureStage::Critical | PressureStage::Emergency => "critical",
            PressureStage::Healthy => bail!("cannot create a pressure incident at healthy stage"),
        };

        if let Some(incident_id) = self.existing_incident_for_action(&action.action_id).await? {
            self.update_incident_sample(&incident_id, level, &detail)
                .await?;
            return Ok(incident_id);
        }

        let log_id = ticketing_system::system_logs::insert_log_returning_id(
            self.pool.as_ref(),
            level,
            COMPONENT,
            "Writable volume disk pressure detected",
            Some(&detail),
            Some(OWNER_USER_ID),
            None,
        )
        .await
        .context("write disk pressure transition system log")?;
        let incident = ticketing_system::system_logs::upsert_incident_for_error_log(
            self.pool.as_ref(),
            log_id,
            &["volume_id"],
        )
        .await
        .context("upsert durable disk pressure system incident")?;
        self.update_incident_sample(&incident.incident_id, level, &detail)
            .await?;

        tracing::warn!(
            component = COMPONENT,
            operation = "disk_pressure.incident_upserted",
            incident_id = %incident.incident_id,
            action_id = %action.action_id,
            volume_id = %action.sample.volume_id,
            stage = action.stage.label(),
            used_percent = action.sample.used_percent,
            "created or updated durable disk pressure incident"
        );
        Ok(incident.incident_id)
    }

    async fn recover_incident(&self, action: &PendingAction, incident_id: &str) -> Result<()> {
        let detail = Self::action_detail(action, "recovered");
        let already_logged: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(*) FROM system_logs
            WHERE component = ? AND json_valid(detail)
              AND json_extract(detail, '$.action_id') = ?
              AND json_extract(detail, '$.event') = 'recovered'
            "#,
        )
        .bind(COMPONENT)
        .bind(&action.action_id)
        .fetch_one(self.pool.as_ref())
        .await?;
        if already_logged == 0 {
            ticketing_system::system_logs::insert_log(
                self.pool.as_ref(),
                "info",
                COMPONENT,
                "Writable volume disk pressure recovered",
                Some(&detail),
                Some(OWNER_USER_ID),
                None,
            )
            .await?;
        }
        sqlx::query(
            "UPDATE system_log_incidents SET sample_detail = ?, updated_at = ? WHERE incident_id = ?",
        )
        .bind(&detail)
        .bind(Utc::now().timestamp())
        .bind(incident_id)
        .execute(self.pool.as_ref())
        .await?;
        ticketing_system::system_logs::set_system_log_incident_status(
            self.pool.as_ref(),
            incident_id,
            SystemLogIncidentStatus::Fixed,
        )
        .await?;

        tracing::info!(
            component = COMPONENT,
            operation = "disk_pressure.recovered",
            incident_id,
            action_id = %action.action_id,
            volume_id = %action.sample.volume_id,
            used_percent = action.sample.used_percent,
            recovery_threshold = RECOVERY_THRESHOLD,
            "disk pressure incident recovered and volume re-armed"
        );
        Ok(())
    }

    async fn enqueue_diagnostic(
        &self,
        action: &PendingAction,
        incident_id: &str,
    ) -> Result<DiagnosticJob> {
        if let Some(existing) = self.existing_diagnostic_job(&action.action_id).await? {
            return Ok(existing);
        }

        let admission = ticketing_system::runner_capacity::admit_enqueue(
            self.pool.as_ref(),
            1,
            "disk_pressure_monitor",
        )
        .await
        .context("inspect runner capacity for disk pressure diagnostic")?;
        if !admission.accepted {
            bail!("runner queue rejected disk pressure diagnostic: {admission:?}");
        }
        if !ticketing_system::system_logs::is_admin(self.pool.as_ref(), OWNER_USER_ID).await? {
            bail!("user {OWNER_USER_ID} is not authorized for full-access diagnostics");
        }

        let parent_id = self.ensure_parent_conversation().await?;
        let agent_type = AgentType::FullAccess;
        let working_dir = resolve_working_dir(self.pool.as_ref(), &agent_type, ORGANIZATION)
            .await
            .context("resolve full-access diagnostic working directory")?;
        let message = load_prompt(
            "disk-pressure-diagnostic",
            HashMap::from([
                ("INCIDENT_ID".to_string(), incident_id.to_string()),
                ("ACTION_ID".to_string(), action.action_id.clone()),
                ("VOLUME_ID".to_string(), action.sample.volume_id.clone()),
                (
                    "VOLUME_SOURCE".to_string(),
                    action.sample.volume_source.clone(),
                ),
                ("MOUNT_PATH".to_string(), action.sample.mount_path.clone()),
                (
                    "FILESYSTEM_TYPE".to_string(),
                    action.sample.filesystem_type.clone(),
                ),
                (
                    "PRESSURE_STAGE".to_string(),
                    action.stage.label().to_string(),
                ),
                (
                    "USED_PERCENT".to_string(),
                    format!("{:.2}", action.sample.used_percent),
                ),
                (
                    "TOTAL_BYTES".to_string(),
                    action.sample.total_bytes.to_string(),
                ),
                (
                    "USED_BYTES".to_string(),
                    action.sample.used_bytes.to_string(),
                ),
                (
                    "AVAILABLE_BYTES".to_string(),
                    action.sample.available_bytes.to_string(),
                ),
            ]),
        )?;
        let agents_md = std::fs::read_to_string("/Users/jarvisgpt/projects/AGENTS.md")
            .context("read AGENTS.md for disk pressure full-access diagnostic")?;
        let prompt_vars = encode_codex_options_for_job(
            HashMap::from([
                ("USER_ID".to_string(), OWNER_USER_ID.to_string()),
                ("AGENTS_MD".to_string(), agents_md),
                ("CONTEXT".to_string(), String::new()),
            ]),
            &ChatCodexOptions::default_for_agent(&agent_type),
        );
        let metadata = json!({
            "origin": "agent_orchestrated",
            "orchestrated_by": "disk_pressure_monitor",
            "orchestration": "child_initial_turn",
            "agent": "full-access",
            "disk_pressure_action_id": action.action_id,
            "disk_pressure_incident_id": incident_id,
            "volume_id": action.sample.volume_id,
            "report_id": format!("disk-pressure-{}", action.action_id),
        })
        .to_string();
        let created =
            ticketing_system::conversations::create_child_conversations_with_initial_turn_jobs(
                self.pool.as_ref(),
                OWNER_USER_ID,
                &parent_id,
                vec![CreateChildConversationRequest {
                    title: format!(
                        "{} disk pressure at {}% — {}",
                        action.stage.label(),
                        action.stage.threshold(),
                        action.sample.mount_path
                    ),
                    agent: Some("full-access".to_string()),
                    conversation_type: Some(CHILD_CONVERSATION_TYPE.to_string()),
                    child_sort_order: None,
                }],
                vec![InitialChildTurnJobRequest {
                    child_index: 0,
                    payload: ConversationTurnJobPayload {
                        user_id: OWNER_USER_ID.to_string(),
                        message,
                        agent_type: "full-access".to_string(),
                        runtime: ChatRuntime::CodexAppServer.as_job_runtime().to_string(),
                        prompt_name: "full-access".to_string(),
                        working_dir: working_dir.to_string_lossy().into_owned(),
                        prompt_vars,
                        images_json: None,
                        client_id: Some(format!("disk-pressure:{}", action.action_id)),
                        message_metadata: Some(metadata),
                    },
                }],
            )
            .await
            .context("create durable disk pressure diagnostic child job")?;
        let queued = created
            .queued_jobs
            .into_iter()
            .next()
            .context("disk pressure child was created without a queued job")?;
        crate::handlers::conversations::publish_conversation_run_status(
            self.pool.as_ref(),
            &queued.child_conversation_id,
        )
        .await?;

        tracing::info!(
            component = COMPONENT,
            operation = "disk_pressure.diagnostic_enqueued",
            incident_id,
            action_id = %action.action_id,
            volume_id = %action.sample.volume_id,
            conversation_id = %queued.child_conversation_id,
            job_id = %queued.job_id,
            runtime = "codex-app-server",
            agent = "full-access",
            "enqueued subscription-backed Codex disk pressure diagnostic"
        );
        Ok(DiagnosticJob {
            conversation_id: queued.child_conversation_id,
            job_id: queued.job_id,
        })
    }

    async fn send_notification(
        &self,
        action: &PendingAction,
        incident_id: &str,
        conversation_id: Option<&str>,
    ) -> Result<()> {
        let apns = ApnsService::global()
            .context("APNs alert service is unavailable for disk pressure notification")?;
        let title = match action.stage {
            PressureStage::Warning => "Disk space warning",
            PressureStage::Critical => "Disk space critical",
            PressureStage::Emergency => "Disk space emergency",
            PressureStage::Healthy => bail!("cannot notify for a healthy disk pressure stage"),
        };
        let diagnostic_text = if conversation_id.is_some() {
            "A Codex diagnostic is queued."
        } else {
            "The diagnostic enqueue failed; the incident is recorded."
        };
        let body = format!(
            "{} is {:.1}% used at {}. {}",
            action.sample.volume_source,
            action.sample.used_percent,
            action.sample.mount_path,
            diagnostic_text
        );
        let report = apns
            .send_disk_pressure_notification_to_user(
                self.pool.as_ref(),
                OWNER_USER_ID,
                title,
                &body,
                conversation_id,
                &action.action_id,
            )
            .await
            .map_err(anyhow::Error::msg)?;
        tracing::info!(
            component = COMPONENT,
            operation = "disk_pressure.notification_sent",
            incident_id,
            action_id = %action.action_id,
            volume_id = %action.sample.volume_id,
            registered_devices = report.registered_devices,
            delivered_devices = report.delivered_devices,
            "sent disk pressure APNs alert to Alex's registered devices"
        );
        Ok(())
    }
}

pub async fn spawn_disk_pressure_monitor(
    pool: Arc<SqlitePool>,
    token: CancellationToken,
) -> Result<()> {
    let config = DiskPressureConfig::from_env()?;
    ensure_schema(pool.as_ref()).await?;
    ticketing_system::system_logs::ensure_incident_schema(pool.as_ref()).await?;
    if ApnsService::global().is_none() {
        bail!(
            "Automatic disk pressure response requires APNS_ALERT_ENABLED=true and a configured APNs alert service"
        );
    }
    if !ticketing_system::system_logs::is_admin(pool.as_ref(), OWNER_USER_ID).await? {
        bail!(
            "Automatic disk pressure response requires '{OWNER_USER_ID}' to be an admin for full-access Codex diagnostics"
        );
    }

    let responder: Arc<dyn PressureResponder> = Arc::new(ProductionResponder::new(pool.clone()));
    let monitor = Arc::new(DiskPressureMonitor::new(pool, responder));
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(STARTUP_DELAY_SECONDS)).await;
        let mut interval = tokio::time::interval(config.poll_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tracing::info!(
            component = COMPONENT,
            operation = "disk_pressure.monitor_started",
            poll_seconds = config.poll_interval.as_secs(),
            warning_threshold = WARNING_THRESHOLD,
            critical_threshold = CRITICAL_THRESHOLD,
            emergency_threshold = EMERGENCY_THRESHOLD,
            recovery_threshold = RECOVERY_THRESHOLD,
            "automatic disk pressure monitor started"
        );
        loop {
            tokio::select! {
                _ = token.cancelled() => {
                    tracing::info!(
                        component = COMPONENT,
                        operation = "disk_pressure.monitor_stopped",
                        "automatic disk pressure monitor stopped"
                    );
                    break;
                }
                _ = interval.tick() => {
                    run_poll(monitor.clone()).await;
                }
            }
        }
    });
    Ok(())
}

async fn run_poll(monitor: Arc<DiskPressureMonitor>) {
    let started = Instant::now();
    let span = tracing::info_span!(
        "disk_pressure.poll",
        component = COMPONENT,
        operation = "disk_pressure.poll"
    );
    async move {
        let samples = match sample_writable_volumes() {
            Ok(samples) => samples,
            Err(error) => {
                metrics::counter!(METRIC_POLLS, "result" => "sampling_failed").increment(1);
                metrics::histogram!(METRIC_POLL_DURATION, "result" => "sampling_failed")
                    .record(started.elapsed().as_secs_f64());
                tracing::error!(
                    component = COMPONENT,
                    operation = "disk_pressure.volume_sampling_failed",
                    error = %error,
                    "failed to sample writable mounted volumes"
                );
                return;
            }
        };
        match monitor.process_samples(samples).await {
            Ok(summary) => {
                let result = if summary.action_failures == 0 {
                    "success"
                } else {
                    "degraded"
                };
                metrics::counter!(METRIC_POLLS, "result" => result).increment(1);
                metrics::histogram!(METRIC_POLL_DURATION, "result" => result)
                    .record(started.elapsed().as_secs_f64());
                tracing::info!(
                    component = COMPONENT,
                    operation = "disk_pressure.poll_completed",
                    result,
                    sampled_volumes = summary.sampled_volumes,
                    transitions = summary.transitions,
                    notifications_sent = summary.notifications_sent,
                    diagnostics_enqueued = summary.diagnostics_enqueued,
                    recoveries = summary.recoveries,
                    action_failures = summary.action_failures,
                    duration_ms = started.elapsed().as_millis() as u64,
                    "disk pressure poll completed"
                );
            }
            Err(error) => {
                metrics::counter!(METRIC_POLLS, "result" => "failed").increment(1);
                metrics::histogram!(METRIC_POLL_DURATION, "result" => "failed")
                    .record(started.elapsed().as_secs_f64());
                tracing::error!(
                    component = COMPONENT,
                    operation = "disk_pressure.poll_failed",
                    error = %error,
                    duration_ms = started.elapsed().as_millis() as u64,
                    "disk pressure poll failed"
                );
            }
        }
    }
    .instrument(span)
    .await;
}

async fn ensure_schema(pool: &SqlitePool) -> Result<()> {
    sqlx::query(VOLUME_STATE_SCHEMA)
        .execute(pool)
        .await
        .context("ensure disk pressure volume state schema")?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_disk_pressure_active ON disk_pressure_volume_state(current_stage) WHERE current_stage > 0",
    )
    .execute(pool)
    .await
    .context("ensure disk pressure active-volume index")?;
    sqlx::query(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_disk_pressure_action ON disk_pressure_volume_state(action_id) WHERE action_id IS NOT NULL",
    )
    .execute(pool)
    .await
    .context("ensure disk pressure action index")?;
    Ok(())
}

fn record_volume_gauges(samples: &[VolumeSample]) {
    let mut counts = [0usize; 4];
    let mut max_used_ratio = 0.0f64;
    for sample in samples {
        max_used_ratio = max_used_ratio.max((sample.used_percent / 100.0).clamp(0.0, 1.0));
        match PressureStage::from_used_percent(sample.used_percent) {
            PressureStage::Healthy => counts[0] += 1,
            PressureStage::Warning => counts[1] += 1,
            PressureStage::Critical => counts[2] += 1,
            PressureStage::Emergency => counts[3] += 1,
        }
    }
    metrics::gauge!(METRIC_VOLUME_COUNT, "stage" => "healthy").set(counts[0] as f64);
    metrics::gauge!(METRIC_VOLUME_COUNT, "stage" => "warning").set(counts[1] as f64);
    metrics::gauge!(METRIC_VOLUME_COUNT, "stage" => "critical").set(counts[2] as f64);
    metrics::gauge!(METRIC_VOLUME_COUNT, "stage" => "emergency").set(counts[3] as f64);
    metrics::gauge!(METRIC_MAX_USED_RATIO).set(max_used_ratio);
}

fn record_response_duration<T, E>(action: &'static str, result: &Result<T, E>, started: Instant) {
    let result_label = if result.is_ok() { "success" } else { "failed" };
    metrics::histogram!(
        METRIC_RESPONSE_DURATION,
        "action" => action,
        "result" => result_label
    )
    .record(started.elapsed().as_secs_f64());
}

#[cfg(target_os = "macos")]
fn sample_writable_volumes() -> Result<Vec<VolumeSample>> {
    use std::ffi::CStr;

    let mut mounts: *mut libc::statfs = std::ptr::null_mut();
    let count = unsafe { libc::getmntinfo(&mut mounts, libc::MNT_NOWAIT) };
    if count <= 0 || mounts.is_null() {
        return Err(std::io::Error::last_os_error())
            .context("getmntinfo returned no mounted filesystems");
    }
    let entries = unsafe { std::slice::from_raw_parts(mounts, count as usize) };
    let mut samples = Vec::new();
    for entry in entries {
        if (entry.f_flags as u64 & libc::MNT_RDONLY as u64) != 0 || entry.f_blocks == 0 {
            continue;
        }
        let block_size = entry.f_bsize as u128;
        if block_size == 0 {
            continue;
        }
        let total_blocks = entry.f_blocks as u128;
        let free_blocks = entry.f_bfree as u128;
        let available_blocks = entry.f_bavail as u128;
        let used_blocks = total_blocks.saturating_sub(free_blocks);
        let total_bytes = clamp_bytes(total_blocks.saturating_mul(block_size));
        let used_bytes = clamp_bytes(used_blocks.saturating_mul(block_size));
        let available_bytes = clamp_bytes(available_blocks.saturating_mul(block_size));
        let used_percent = (used_blocks as f64 / total_blocks as f64) * 100.0;
        let volume_source = unsafe { CStr::from_ptr(entry.f_mntfromname.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        let mount_path = unsafe { CStr::from_ptr(entry.f_mntonname.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        let filesystem_type = unsafe { CStr::from_ptr(entry.f_fstypename.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        // libc intentionally keeps fsid_t's two-word payload private, but
        // Darwin defines it as exactly two i32 values. Read that stable
        // kernel filesystem identity without depending on the private field
        // name used by a particular libc crate release.
        let fsid = unsafe { std::ptr::read(std::ptr::addr_of!(entry.f_fsid).cast::<[i32; 2]>()) };
        let volume_id = format!("macos-fsid:{:08x}:{:08x}", fsid[0] as u32, fsid[1] as u32);
        samples.push(VolumeSample {
            volume_id,
            volume_source,
            mount_path,
            filesystem_type,
            total_bytes,
            used_bytes,
            available_bytes,
            used_percent,
        });
    }
    samples.sort_by(|a, b| a.volume_id.cmp(&b.volume_id));
    // One filesystem can be exposed through more than one mount path. The
    // pressure contract is per stable volume identity, so process it once.
    samples.dedup_by(|left, right| left.volume_id == right.volume_id);
    Ok(samples)
}

#[cfg(not(target_os = "macos"))]
fn sample_writable_volumes() -> Result<Vec<VolumeSample>> {
    bail!("automatic writable-volume disk pressure sampling is supported only on macOS")
}

#[cfg(target_os = "macos")]
fn clamp_bytes(bytes: u128) -> i64 {
    bytes.min(i64::MAX as u128) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;
    use tokio::sync::Mutex;

    #[derive(Debug, Default)]
    struct MockState {
        incident_actions: Vec<String>,
        notification_actions: Vec<String>,
        diagnostic_actions: Vec<String>,
        recovery_actions: Vec<String>,
        fail_diagnostic: bool,
    }

    #[derive(Debug, Default)]
    struct MockResponder {
        state: Mutex<MockState>,
    }

    impl MockResponder {
        async fn counts(&self) -> (usize, usize, usize, usize) {
            let state = self.state.lock().await;
            (
                state.incident_actions.len(),
                state.notification_actions.len(),
                state.diagnostic_actions.len(),
                state.recovery_actions.len(),
            )
        }

        async fn fail_diagnostics(&self) {
            self.state.lock().await.fail_diagnostic = true;
        }
    }

    #[async_trait]
    impl PressureResponder for MockResponder {
        async fn ensure_incident(&self, action: &PendingAction) -> Result<String> {
            self.state
                .lock()
                .await
                .incident_actions
                .push(action.action_id.clone());
            Ok(format!("incident-{}", action.sample.volume_id))
        }

        async fn recover_incident(&self, action: &PendingAction, _incident_id: &str) -> Result<()> {
            self.state
                .lock()
                .await
                .recovery_actions
                .push(action.action_id.clone());
            Ok(())
        }

        async fn enqueue_diagnostic(
            &self,
            action: &PendingAction,
            _incident_id: &str,
        ) -> Result<DiagnosticJob> {
            let mut state = self.state.lock().await;
            state.diagnostic_actions.push(action.action_id.clone());
            if state.fail_diagnostic {
                bail!("injected diagnostic enqueue failure");
            }
            Ok(DiagnosticJob {
                conversation_id: format!("conversation-{}", action.action_id),
                job_id: format!("job-{}", action.action_id),
            })
        }

        async fn send_notification(
            &self,
            action: &PendingAction,
            _incident_id: &str,
            _conversation_id: Option<&str>,
        ) -> Result<()> {
            self.state
                .lock()
                .await
                .notification_actions
                .push(action.action_id.clone());
            Ok(())
        }
    }

    async fn test_monitor() -> (DiskPressureMonitor, Arc<MockResponder>) {
        let pool = Arc::new(
            SqlitePoolOptions::new()
                .max_connections(1)
                .connect("sqlite::memory:")
                .await
                .unwrap(),
        );
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
        .execute(pool.as_ref())
        .await
        .unwrap();
        ensure_schema(pool.as_ref()).await.unwrap();
        let responder = Arc::new(MockResponder::default());
        let monitor = DiskPressureMonitor::new(pool, responder.clone());
        (monitor, responder)
    }

    fn sample(volume_id: &str, used_percent: f64) -> VolumeSample {
        let total_bytes = 1_000_000i64;
        let used_bytes = (used_percent / 100.0 * total_bytes as f64) as i64;
        VolumeSample {
            volume_id: volume_id.to_string(),
            volume_source: format!("/dev/{volume_id}"),
            mount_path: format!("/Volumes/{volume_id}"),
            filesystem_type: "apfs".to_string(),
            total_bytes,
            used_bytes,
            available_bytes: total_bytes - used_bytes,
            used_percent,
        }
    }

    fn pending_action(
        action_id: &str,
        kind: TransitionKind,
        stage: PressureStage,
        used_percent: f64,
        incident_id: Option<&str>,
    ) -> PendingAction {
        PendingAction {
            action_id: action_id.to_string(),
            kind,
            stage,
            cycle: 1,
            sample: sample("disk-a", used_percent),
            incident_id: incident_id.map(str::to_string),
            notification_status: "pending".to_string(),
            diagnostic_status: "pending".to_string(),
            last_conversation_id: None,
        }
    }

    async fn current_stage(monitor: &DiskPressureMonitor, volume_id: &str) -> i64 {
        sqlx::query_scalar(
            "SELECT current_stage FROM disk_pressure_volume_state WHERE volume_id = ?",
        )
        .bind(volume_id)
        .fetch_one(monitor.pool.as_ref())
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn threshold_crossing_triggers_one_incident_push_and_diagnostic() {
        let (monitor, responder) = test_monitor().await;
        let healthy = monitor
            .process_samples(vec![sample("disk-a", 74.9)])
            .await
            .unwrap();
        assert_eq!(healthy.transitions, 0);

        let crossed = monitor
            .process_samples(vec![sample("disk-a", 75.0)])
            .await
            .unwrap();
        assert_eq!(crossed.transitions, 1);
        assert_eq!(crossed.notifications_sent, 1);
        assert_eq!(crossed.diagnostics_enqueued, 1);
        assert_eq!(responder.counts().await, (1, 1, 1, 0));
        assert_eq!(current_stage(&monitor, "disk-a").await, 75);
    }

    #[tokio::test]
    async fn repeated_polls_are_deduplicated_at_the_same_stage() {
        let (monitor, responder) = test_monitor().await;
        monitor
            .process_samples(vec![sample("disk-a", 76.0)])
            .await
            .unwrap();
        for used in [77.0, 80.0, 84.99] {
            let summary = monitor
                .process_samples(vec![sample("disk-a", used)])
                .await
                .unwrap();
            assert_eq!(summary.transitions, 0);
        }
        assert_eq!(responder.counts().await, (1, 1, 1, 0));
    }

    #[tokio::test]
    async fn higher_thresholds_each_allow_one_escalation_action() {
        let (monitor, responder) = test_monitor().await;
        for used in [75.0, 85.0, 90.0, 95.0, 99.0] {
            monitor
                .process_samples(vec![sample("disk-a", used)])
                .await
                .unwrap();
        }
        assert_eq!(responder.counts().await, (3, 3, 3, 0));
        assert_eq!(current_stage(&monitor, "disk-a").await, 95);
    }

    #[tokio::test]
    async fn recovery_uses_hysteresis_and_rearms_future_pressure() {
        let (monitor, responder) = test_monitor().await;
        monitor
            .process_samples(vec![sample("disk-a", 76.0)])
            .await
            .unwrap();
        monitor
            .process_samples(vec![sample("disk-a", 72.0)])
            .await
            .unwrap();
        assert_eq!(current_stage(&monitor, "disk-a").await, 75);
        monitor
            .process_samples(vec![sample("disk-a", 69.9)])
            .await
            .unwrap();
        assert_eq!(current_stage(&monitor, "disk-a").await, 0);
        monitor
            .process_samples(vec![sample("disk-a", 76.0)])
            .await
            .unwrap();
        assert_eq!(responder.counts().await, (2, 2, 2, 1));
        assert_eq!(current_stage(&monitor, "disk-a").await, 75);
    }

    #[tokio::test]
    async fn multiple_writable_volumes_are_tracked_independently() {
        let (monitor, responder) = test_monitor().await;
        let summary = monitor
            .process_samples(vec![sample("disk-a", 76.0), sample("disk-b", 86.0)])
            .await
            .unwrap();
        assert_eq!(summary.sampled_volumes, 2);
        assert_eq!(summary.transitions, 2);
        assert_eq!(responder.counts().await, (2, 2, 2, 0));
        assert_eq!(current_stage(&monitor, "disk-a").await, 75);
        assert_eq!(current_stage(&monitor, "disk-b").await, 85);
    }

    #[tokio::test]
    async fn diagnostic_enqueue_failure_is_durable_and_not_retried_same_stage() {
        let (monitor, responder) = test_monitor().await;
        responder.fail_diagnostics().await;
        let first = monitor
            .process_samples(vec![sample("disk-a", 76.0)])
            .await
            .unwrap();
        assert_eq!(first.action_failures, 1);
        assert_eq!(first.notifications_sent, 1);
        assert_eq!(first.diagnostics_enqueued, 0);

        monitor
            .process_samples(vec![sample("disk-a", 80.0)])
            .await
            .unwrap();
        assert_eq!(responder.counts().await, (1, 1, 1, 0));
        let failure_logs: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM system_logs WHERE component = 'disk_pressure' AND message = 'Disk pressure response action failed'",
        )
        .fetch_one(monitor.pool.as_ref())
        .await
        .unwrap();
        assert_eq!(failure_logs, 1);
    }

    #[tokio::test]
    async fn production_incident_is_updated_on_escalation_and_fixed_on_recovery() {
        let (monitor, _) = test_monitor().await;
        sqlx::query("CREATE TABLE tickets (ticket_id TEXT PRIMARY KEY)")
            .execute(monitor.pool.as_ref())
            .await
            .unwrap();
        sqlx::query("CREATE TABLE conversations (id TEXT PRIMARY KEY)")
            .execute(monitor.pool.as_ref())
            .await
            .unwrap();
        ticketing_system::system_logs::ensure_incident_schema(monitor.pool.as_ref())
            .await
            .unwrap();
        let responder = ProductionResponder::new(monitor.pool.clone());
        let detected = pending_action(
            "action-detected",
            TransitionKind::Detected,
            PressureStage::Warning,
            76.0,
            None,
        );
        let incident_id = responder.ensure_incident(&detected).await.unwrap();
        let escalated = pending_action(
            "action-escalated",
            TransitionKind::Escalated,
            PressureStage::Critical,
            86.0,
            Some(&incident_id),
        );
        let escalated_incident_id = responder.ensure_incident(&escalated).await.unwrap();
        assert_eq!(escalated_incident_id, incident_id);

        let incident = ticketing_system::system_logs::get_system_log_incident(
            monitor.pool.as_ref(),
            &incident_id,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(incident.status, SystemLogIncidentStatus::Investigating);
        assert_eq!(incident.occurrence_count, 2);
        assert_eq!(incident.level, "critical");

        let recovered = pending_action(
            "action-recovered",
            TransitionKind::Recovered,
            PressureStage::Healthy,
            69.0,
            Some(&incident_id),
        );
        responder
            .recover_incident(&recovered, &incident_id)
            .await
            .unwrap();
        let incident = ticketing_system::system_logs::get_system_log_incident(
            monitor.pool.as_ref(),
            &incident_id,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(incident.status, SystemLogIncidentStatus::Fixed);
        let recovery_logs: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM system_logs WHERE component = 'disk_pressure' AND message = 'Writable volume disk pressure recovered'",
        )
        .fetch_one(monitor.pool.as_ref())
        .await
        .unwrap();
        assert_eq!(recovery_logs, 1);
    }
}
