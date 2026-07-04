use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use serde_json::json;
use sqlx::{Row, SqlitePool};

const RUNNER_KIND: &str = "agent-runner";
const RUNNER_HEARTBEAT_STALE_SECONDS: i64 = 90;
const DEFAULT_ADAPTIVE_MAX_CONCURRENCY: i64 = 12;
const DEFAULT_ADAPTIVE_CLAIM_BURST: i64 = 1;
const DEFAULT_ADAPTIVE_DB_IDLE_RESERVE: usize = 1;
const DEFAULT_ADAPTIVE_MAX_LOAD_PER_CORE_MILLIS: u32 = 1500;
const DEFAULT_ADAPTIVE_CLAIM_INTERVAL_SECONDS: u64 = 10;

#[derive(Debug, Clone, Serialize)]
pub struct RunnerCapacitySnapshot {
    pub jobs: AgentJobCounts,
    pub turns: AgentRunnerTurnCounts,
    pub running_jobs: RunningJobStats,
    pub runner: Option<RunnerGenerationStatus>,
    pub host: HostLoadStatus,
    pub db_pool: DbPoolStatus,
    pub config: RunnerCapacityConfigStatus,
    pub backpressure: BackpressureStatus,
    pub recent_failures: Vec<RunnerFailureSummary>,
    pub server_time: i64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AgentJobCounts {
    pub pending: i64,
    pub running: i64,
    pub completed: i64,
    pub failed: i64,
    pub cancelled: i64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AgentRunnerTurnCounts {
    pub queued: i64,
    pub running: i64,
    pub completed: i64,
    pub failed: i64,
    pub cancelled: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunningJobStats {
    pub count: i64,
    pub average_age_seconds: f64,
    pub min_age_seconds: i64,
    pub max_age_seconds: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunnerGenerationStatus {
    pub generation_id: String,
    pub status: String,
    pub active_turn_count: i64,
    pub pid: i64,
    pub last_heartbeat_at: i64,
    pub heartbeat_age_seconds: i64,
    pub heartbeat_stale: bool,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct HostLoadStatus {
    pub load_1m: f64,
    pub cpu_count: usize,
    pub max_load: f64,
    pub max_load_per_core: f64,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct DbPoolStatus {
    pub size: u32,
    pub max_connections: u32,
    pub idle: usize,
    pub idle_reserve: Option<usize>,
    pub has_capacity: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunnerCapacityConfigStatus {
    pub mode: String,
    pub max_jobs: i64,
    pub max_pending_jobs: i64,
    pub claim_burst: Option<i64>,
    pub claim_interval_seconds: Option<u64>,
    pub db_idle_reserve: Option<usize>,
    pub max_load_per_core: Option<f64>,
    pub heartbeat_stale_seconds: i64,
    pub queue_admission_enabled: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackpressureStatus {
    pub state: &'static str,
    pub reason: &'static str,
    pub active_jobs: i64,
    pub pending_jobs: i64,
    pub max_jobs: i64,
    pub max_pending_jobs: i64,
    pub available_runner_slots: i64,
    pub available_queue_slots: i64,
    pub deferred_jobs: i64,
    pub rejected_jobs: i64,
    pub would_reject_new_job: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunnerFailureSummary {
    pub source: &'static str,
    pub category: &'static str,
    pub conversation_id: String,
    pub job_id: Option<String>,
    pub event_index: Option<i64>,
    pub occurred_at: i64,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunnerQueueAdmission {
    pub accepted: bool,
    pub context: String,
    pub reason: &'static str,
    pub requested_jobs: i64,
    pub accepted_jobs: i64,
    pub rejected_jobs: i64,
    pub snapshot: RunnerCapacitySnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunnerConcurrencyConfig {
    Fixed {
        max_jobs: i64,
    },
    Adaptive {
        max_jobs: i64,
        claim_burst: i64,
        db_idle_reserve: usize,
        max_load_per_core_millis: u32,
        claim_interval_seconds: u64,
    },
}

impl RunnerConcurrencyConfig {
    fn default_adaptive() -> Self {
        Self::Adaptive {
            max_jobs: DEFAULT_ADAPTIVE_MAX_CONCURRENCY,
            claim_burst: DEFAULT_ADAPTIVE_CLAIM_BURST,
            db_idle_reserve: DEFAULT_ADAPTIVE_DB_IDLE_RESERVE,
            max_load_per_core_millis: DEFAULT_ADAPTIVE_MAX_LOAD_PER_CORE_MILLIS,
            claim_interval_seconds: DEFAULT_ADAPTIVE_CLAIM_INTERVAL_SECONDS,
        }
    }

    fn max_jobs(self) -> i64 {
        match self {
            Self::Fixed { max_jobs } | Self::Adaptive { max_jobs, .. } => max_jobs,
        }
    }

    fn db_idle_reserve(self) -> Option<usize> {
        match self {
            Self::Adaptive {
                db_idle_reserve, ..
            } => Some(db_idle_reserve),
            Self::Fixed { .. } => None,
        }
    }

    fn max_load_per_core(self) -> Option<f64> {
        match self {
            Self::Adaptive {
                max_load_per_core_millis,
                ..
            } => Some(f64::from(max_load_per_core_millis) / 1000.0),
            Self::Fixed { .. } => None,
        }
    }

    fn status(self, max_pending_jobs: i64) -> RunnerCapacityConfigStatus {
        match self {
            Self::Fixed { max_jobs } => RunnerCapacityConfigStatus {
                mode: "fixed".to_string(),
                max_jobs,
                max_pending_jobs,
                claim_burst: None,
                claim_interval_seconds: None,
                db_idle_reserve: None,
                max_load_per_core: None,
                heartbeat_stale_seconds: RUNNER_HEARTBEAT_STALE_SECONDS,
                queue_admission_enabled: true,
            },
            Self::Adaptive {
                max_jobs,
                claim_burst,
                db_idle_reserve,
                max_load_per_core_millis,
                claim_interval_seconds,
            } => RunnerCapacityConfigStatus {
                mode: "adaptive".to_string(),
                max_jobs,
                max_pending_jobs,
                claim_burst: Some(claim_burst),
                claim_interval_seconds: Some(claim_interval_seconds),
                db_idle_reserve: Some(db_idle_reserve),
                max_load_per_core: Some(f64::from(max_load_per_core_millis) / 1000.0),
                heartbeat_stale_seconds: RUNNER_HEARTBEAT_STALE_SECONDS,
                queue_admission_enabled: true,
            },
        }
    }
}

pub async fn build_snapshot(db: &SqlitePool) -> anyhow::Result<RunnerCapacitySnapshot> {
    let config = runner_concurrency_config()?;
    let max_pending_jobs = runner_queue_max_pending(config.max_jobs())?;
    let jobs = load_job_counts(db).await?;
    let turns = load_turn_counts(db).await?;
    let running_jobs = load_running_job_stats(db).await?;
    let runner = load_runner_generation(db).await?;
    let host = current_host_load(config)?;
    let db_pool = db_pool_status(db, config.db_idle_reserve());
    let config_status = config.status(max_pending_jobs);
    let active_jobs = active_job_count(&jobs, &turns, runner.as_ref());
    let backpressure = classify_backpressure(
        &config,
        max_pending_jobs,
        active_jobs,
        jobs.pending,
        runner.as_ref(),
        &host,
        &db_pool,
    );
    let recent_failures = load_recent_failures(db).await?;

    Ok(RunnerCapacitySnapshot {
        jobs,
        turns,
        running_jobs,
        runner,
        host,
        db_pool,
        config: config_status,
        backpressure,
        recent_failures,
        server_time: chrono::Utc::now().timestamp(),
    })
}

pub async fn admit_enqueue(
    db: &SqlitePool,
    requested_jobs: usize,
    context: &str,
) -> anyhow::Result<RunnerQueueAdmission> {
    let snapshot = build_snapshot(db).await?;
    Ok(admit_enqueue_from_snapshot(
        snapshot,
        requested_jobs,
        context,
    ))
}

pub fn queue_admission_rejection_response(admission: RunnerQueueAdmission) -> Response {
    let status = if admission.reason == "runner_unavailable" {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::TOO_MANY_REQUESTS
    };
    (
        status,
        Json(json!({
            "error": "runner_queue_backpressure",
            "message": admission.snapshot.backpressure.message,
            "admission": admission,
        })),
    )
        .into_response()
}

fn admit_enqueue_from_snapshot(
    snapshot: RunnerCapacitySnapshot,
    requested_jobs: usize,
    context: &str,
) -> RunnerQueueAdmission {
    let requested_jobs = i64::try_from(requested_jobs).unwrap_or(i64::MAX);
    let backpressure = &snapshot.backpressure;
    let reason = if requested_jobs <= 0 {
        "no_jobs_requested"
    } else if matches!(
        backpressure.reason,
        "runner_unavailable"
            | "runner_draining"
            | "runner_heartbeat_stale"
            | "host_load"
            | "db_pool"
    ) {
        backpressure.reason
    } else if requested_jobs > snapshot.config.max_jobs {
        "batch_exceeds_capacity"
    } else if snapshot.jobs.pending.saturating_add(requested_jobs)
        > snapshot.config.max_pending_jobs
    {
        "queue_pending_cap"
    } else {
        "capacity_available"
    };
    let accepted = reason == "capacity_available" || reason == "no_jobs_requested";

    RunnerQueueAdmission {
        accepted,
        context: context.to_string(),
        reason,
        requested_jobs,
        accepted_jobs: if accepted { requested_jobs } else { 0 },
        rejected_jobs: if accepted { 0 } else { requested_jobs },
        snapshot,
    }
}

fn active_job_count(
    jobs: &AgentJobCounts,
    turns: &AgentRunnerTurnCounts,
    runner: Option<&RunnerGenerationStatus>,
) -> i64 {
    let runner_active = runner
        .filter(|runner| !runner.heartbeat_stale)
        .map(|runner| runner.active_turn_count)
        .unwrap_or(0);
    runner_active
        .max(jobs.running)
        .max(turns.queued + turns.running)
}

fn classify_backpressure(
    config: &RunnerConcurrencyConfig,
    max_pending_jobs: i64,
    active_jobs: i64,
    pending_jobs: i64,
    runner: Option<&RunnerGenerationStatus>,
    host: &HostLoadStatus,
    db_pool: &DbPoolStatus,
) -> BackpressureStatus {
    let max_jobs = config.max_jobs();
    let available_runner_slots = max_jobs.saturating_sub(active_jobs);
    let available_queue_slots = max_pending_jobs.saturating_sub(pending_jobs);

    let (state, reason, would_reject_new_job, message) = if runner.is_none() {
        (
            "rejecting",
            "runner_unavailable",
            true,
            "No agent-runner generation is registered; refusing to add more queued work."
                .to_string(),
        )
    } else if runner.is_some_and(|runner| runner.heartbeat_stale) {
        (
            "rejecting",
            "runner_heartbeat_stale",
            true,
            "Latest agent-runner heartbeat is stale; refusing to add more queued work.".to_string(),
        )
    } else if runner.is_some_and(|runner| runner.status != "accepting") {
        (
            "rejecting",
            "runner_draining",
            true,
            "Latest agent-runner generation is not accepting new work.".to_string(),
        )
    } else if matches!(config, RunnerConcurrencyConfig::Adaptive { .. })
        && host.load_1m >= host.max_load
    {
        (
            "rejecting",
            "host_load",
            true,
            format!(
                "Host load is above runner threshold ({:.2} >= {:.2}); refusing more queued work.",
                host.load_1m, host.max_load
            ),
        )
    } else if matches!(config, RunnerConcurrencyConfig::Adaptive { .. }) && !db_pool.has_capacity {
        (
            "rejecting",
            "db_pool",
            true,
            "SQLite pool does not have the configured idle reserve; refusing more queued work."
                .to_string(),
        )
    } else if pending_jobs >= max_pending_jobs {
        (
            "rejecting",
            "queue_pending_cap",
            true,
            format!(
                "Runner queue pending cap reached ({pending_jobs}/{max_pending_jobs}); wait for jobs to drain."
            ),
        )
    } else if active_jobs >= max_jobs {
        (
            "deferred",
            "concurrency_cap",
            false,
            format!(
                "Runner is at concurrency cap ({active_jobs}/{max_jobs}); new accepted work will stay pending."
            ),
        )
    } else if pending_jobs > 0 {
        (
            "deferred",
            "pending_backlog",
            false,
            format!(
                "Runner has {pending_jobs} pending job(s); capacity remains for a bounded enqueue."
            ),
        )
    } else {
        (
            "accepting",
            "capacity_available",
            false,
            format!(
                "Runner has capacity: active={active_jobs}/{max_jobs}, pending={pending_jobs}/{max_pending_jobs}."
            ),
        )
    };

    BackpressureStatus {
        state,
        reason,
        active_jobs,
        pending_jobs,
        max_jobs,
        max_pending_jobs,
        available_runner_slots,
        available_queue_slots,
        deferred_jobs: pending_jobs,
        rejected_jobs: 0,
        would_reject_new_job,
        message,
    }
}

async fn load_job_counts(db: &SqlitePool) -> anyhow::Result<AgentJobCounts> {
    let rows = sqlx::query(
        r#"
        SELECT status, COUNT(*) AS count
        FROM conversation_turn_jobs
        GROUP BY status
        "#,
    )
    .fetch_all(db)
    .await?;

    let mut counts = AgentJobCounts::default();
    for row in rows {
        let status: String = row.get("status");
        let count: i64 = row.get("count");
        match status.as_str() {
            "pending" => counts.pending = count,
            "running" => counts.running = count,
            "completed" => counts.completed = count,
            "failed" => counts.failed = count,
            "cancelled" => counts.cancelled = count,
            _ => {}
        }
    }

    Ok(counts)
}

async fn load_turn_counts(db: &SqlitePool) -> anyhow::Result<AgentRunnerTurnCounts> {
    let rows = sqlx::query(
        r#"
        SELECT status, COUNT(*) AS count
        FROM agent_runner_turns
        GROUP BY status
        "#,
    )
    .fetch_all(db)
    .await?;

    let mut counts = AgentRunnerTurnCounts::default();
    for row in rows {
        let status: String = row.get("status");
        let count: i64 = row.get("count");
        match status.as_str() {
            "queued" => counts.queued = count,
            "running" => counts.running = count,
            "completed" => counts.completed = count,
            "failed" => counts.failed = count,
            "cancelled" => counts.cancelled = count,
            _ => {}
        }
    }

    Ok(counts)
}

async fn load_running_job_stats(db: &SqlitePool) -> anyhow::Result<RunningJobStats> {
    let row = sqlx::query(
        r#"
        SELECT
          COUNT(*) AS count,
          COALESCE(AVG(strftime('%s', 'now') - started_at), 0.0) AS average_age_seconds,
          COALESCE(MIN(strftime('%s', 'now') - started_at), 0) AS min_age_seconds,
          COALESCE(MAX(strftime('%s', 'now') - started_at), 0) AS max_age_seconds
        FROM conversation_turn_jobs
        WHERE status = 'running' AND started_at IS NOT NULL
        "#,
    )
    .fetch_one(db)
    .await?;

    Ok(RunningJobStats {
        count: row.get("count"),
        average_age_seconds: row.get("average_age_seconds"),
        min_age_seconds: row.get("min_age_seconds"),
        max_age_seconds: row.get("max_age_seconds"),
    })
}

async fn load_runner_generation(db: &SqlitePool) -> anyhow::Result<Option<RunnerGenerationStatus>> {
    let row = sqlx::query(
        r#"
        SELECT
          generation_id,
          status,
          active_turn_count,
          pid,
          last_heartbeat_at,
          strftime('%s', 'now') - last_heartbeat_at AS heartbeat_age_seconds
        FROM agent_runner_generations
        WHERE runner_kind = ?
        ORDER BY last_heartbeat_at DESC
        LIMIT 1
        "#,
    )
    .bind(RUNNER_KIND)
    .fetch_optional(db)
    .await?;

    Ok(row.map(|row| {
        let heartbeat_age_seconds = row.get("heartbeat_age_seconds");
        RunnerGenerationStatus {
            generation_id: row.get("generation_id"),
            status: row.get("status"),
            active_turn_count: row.get("active_turn_count"),
            pid: row.get("pid"),
            last_heartbeat_at: row.get("last_heartbeat_at"),
            heartbeat_age_seconds,
            heartbeat_stale: heartbeat_age_seconds > RUNNER_HEARTBEAT_STALE_SECONDS,
        }
    }))
}

fn current_host_load(config: RunnerConcurrencyConfig) -> anyhow::Result<HostLoadStatus> {
    let cpu_count = std::thread::available_parallelism()?.get();
    let mut loads = [0.0_f64; 3];
    let samples = unsafe { libc::getloadavg(loads.as_mut_ptr(), 1) };
    if samples != 1 {
        anyhow::bail!("failed to read system load average");
    }

    let max_load_per_core = config
        .max_load_per_core()
        .unwrap_or(f64::from(DEFAULT_ADAPTIVE_MAX_LOAD_PER_CORE_MILLIS) / 1000.0);
    Ok(HostLoadStatus {
        load_1m: loads[0],
        cpu_count,
        max_load: cpu_count as f64 * max_load_per_core,
        max_load_per_core,
    })
}

fn db_pool_status(db: &SqlitePool, idle_reserve: Option<usize>) -> DbPoolStatus {
    let size = db.size();
    let max_connections = db.options().get_max_connections();
    let idle = db.num_idle();
    let has_capacity = idle_reserve
        .map(|reserve| size < max_connections || idle > reserve)
        .unwrap_or(true);
    DbPoolStatus {
        size,
        max_connections,
        idle,
        idle_reserve,
        has_capacity,
    }
}

async fn load_recent_failures(db: &SqlitePool) -> anyhow::Result<Vec<RunnerFailureSummary>> {
    let mut failures = load_recent_job_failures(db).await?;
    failures.extend(load_recent_error_events(db).await?);
    failures.sort_by(|left, right| right.occurred_at.cmp(&left.occurred_at));
    failures.truncate(10);
    Ok(failures)
}

async fn load_recent_job_failures(db: &SqlitePool) -> anyhow::Result<Vec<RunnerFailureSummary>> {
    let rows = sqlx::query(
        r#"
        SELECT
          id,
          conversation_id,
          error_message,
          COALESCE(completed_at, updated_at, created_at) AS occurred_at
        FROM conversation_turn_jobs
        WHERE status = 'failed'
          AND error_message IS NOT NULL
          AND trim(error_message) != ''
        ORDER BY occurred_at DESC
        LIMIT 10
        "#,
    )
    .fetch_all(db)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| {
            let message: String = row.get("error_message");
            RunnerFailureSummary {
                source: "conversation_turn_jobs",
                category: classify_failure_message(&message),
                conversation_id: row.get("conversation_id"),
                job_id: Some(row.get("id")),
                event_index: None,
                occurred_at: row.get("occurred_at"),
                message,
            }
        })
        .collect())
}

async fn load_recent_error_events(db: &SqlitePool) -> anyhow::Result<Vec<RunnerFailureSummary>> {
    let rows = sqlx::query(
        r#"
        SELECT conversation_id, event_index, event_data, created_at
        FROM conversation_events
        WHERE event_type = 'error'
        ORDER BY created_at DESC
        LIMIT 25
        "#,
    )
    .fetch_all(db)
    .await?;

    let mut failures = Vec::new();
    for row in rows {
        let event_data: String = row.get("event_data");
        let Some(message) = error_message_from_event_data(&event_data) else {
            continue;
        };
        failures.push(RunnerFailureSummary {
            source: "conversation_events",
            category: classify_failure_message(&message),
            conversation_id: row.get("conversation_id"),
            job_id: None,
            event_index: Some(row.get("event_index")),
            occurred_at: row.get("created_at"),
            message,
        });
    }

    Ok(failures)
}

fn error_message_from_event_data(event_data: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(event_data).ok()?;
    value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| value.get("message").and_then(serde_json::Value::as_str))
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .map(ToOwned::to_owned)
}

fn classify_failure_message(message: &str) -> &'static str {
    let lower = message.to_ascii_lowercase();
    if lower.contains("usage limit")
        || lower.contains("rate limit")
        || lower.contains("limit reached")
        || lower.contains("too many requests")
        || lower.contains("quota")
        || lower.contains("credits")
    {
        return "usage_limit";
    }
    if lower.contains("failed to start codex")
        || lower.contains("failed to start codex app-server")
        || lower.contains("closed stdout before")
        || lower.contains("failed reading codex app-server")
        || lower.contains("failed parsing codex app-server")
        || lower.contains("initialize failed")
        || lower.contains("thread/start failed")
        || lower.contains("turn/start failed")
        || lower.contains("spawn")
        || lower.contains("os error")
    {
        return "process_start";
    }
    "runtime_error"
}

fn runner_concurrency_config() -> anyhow::Result<RunnerConcurrencyConfig> {
    let mut config =
        parse_runner_concurrency(std::env::var("AGENTIC_RUNNER_CONCURRENCY").ok().as_deref())?;
    if let RunnerConcurrencyConfig::Adaptive {
        max_jobs,
        claim_burst,
        db_idle_reserve,
        max_load_per_core_millis,
        claim_interval_seconds,
    } = &mut config
    {
        *max_jobs = parse_optional_i64_env("AGENTIC_RUNNER_ADAPTIVE_MAX_JOBS", *max_jobs)?;
        *claim_burst = parse_optional_i64_env("AGENTIC_RUNNER_ADAPTIVE_CLAIM_BURST", *claim_burst)?;
        *db_idle_reserve =
            parse_optional_usize_env("AGENTIC_RUNNER_ADAPTIVE_DB_IDLE_RESERVE", *db_idle_reserve)?;
        *max_load_per_core_millis = parse_optional_load_per_core_env(
            "AGENTIC_RUNNER_ADAPTIVE_MAX_LOAD_PER_CORE",
            *max_load_per_core_millis,
        )?;
        *claim_interval_seconds = parse_optional_u64_env(
            "AGENTIC_RUNNER_ADAPTIVE_CLAIM_INTERVAL_SECONDS",
            *claim_interval_seconds,
        )?;
    }

    Ok(config)
}

fn runner_queue_max_pending(default: i64) -> anyhow::Result<i64> {
    parse_optional_i64_env("AGENTIC_RUNNER_QUEUE_MAX_PENDING", default)
}

fn parse_runner_concurrency(value: Option<&str>) -> anyhow::Result<RunnerConcurrencyConfig> {
    let Some(raw) = value else {
        return Ok(RunnerConcurrencyConfig::default_adaptive());
    };

    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(RunnerConcurrencyConfig::default_adaptive());
    }

    if matches!(
        trimmed.to_ascii_lowercase().as_str(),
        "adaptive" | "unlimited"
    ) {
        return Ok(RunnerConcurrencyConfig::default_adaptive());
    }

    let max_jobs = trimmed.parse::<i64>().map_err(|e| {
        anyhow::anyhow!(
            "AGENTIC_RUNNER_CONCURRENCY must be a positive integer, 'adaptive', or 'unlimited': {e}"
        )
    })?;
    if max_jobs <= 0 {
        anyhow::bail!("AGENTIC_RUNNER_CONCURRENCY must be greater than zero");
    }
    Ok(RunnerConcurrencyConfig::Fixed { max_jobs })
}

fn parse_optional_i64_env(name: &str, default: i64) -> anyhow::Result<i64> {
    let Some(raw) = optional_env(name)? else {
        return Ok(default);
    };

    let value = raw
        .parse::<i64>()
        .map_err(|e| anyhow::anyhow!("{name} must be a positive integer: {e}"))?;
    if value <= 0 {
        anyhow::bail!("{name} must be greater than zero");
    }
    Ok(value)
}

fn parse_optional_usize_env(name: &str, default: usize) -> anyhow::Result<usize> {
    let Some(raw) = optional_env(name)? else {
        return Ok(default);
    };

    let value = raw
        .parse::<usize>()
        .map_err(|e| anyhow::anyhow!("{name} must be a positive integer: {e}"))?;
    if value == 0 {
        anyhow::bail!("{name} must be greater than zero");
    }
    Ok(value)
}

fn parse_optional_u64_env(name: &str, default: u64) -> anyhow::Result<u64> {
    let Some(raw) = optional_env(name)? else {
        return Ok(default);
    };

    let value = raw
        .parse::<u64>()
        .map_err(|e| anyhow::anyhow!("{name} must be a positive integer: {e}"))?;
    if value == 0 {
        anyhow::bail!("{name} must be greater than zero");
    }
    Ok(value)
}

fn parse_optional_load_per_core_env(name: &str, default: u32) -> anyhow::Result<u32> {
    let Some(raw) = optional_env(name)? else {
        return Ok(default);
    };

    parse_load_per_core_millis(&raw).map_err(|e| {
        anyhow::anyhow!("{name} must be a positive number, such as 1.5 for 150% of CPU cores: {e}")
    })
}

fn optional_env(name: &str) -> anyhow::Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed.to_string()))
            }
        }
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(e) => Err(anyhow::anyhow!("failed to read {name}: {e}")),
    }
}

fn parse_load_per_core_millis(raw: &str) -> anyhow::Result<u32> {
    let value = raw
        .parse::<f64>()
        .map_err(|e| anyhow::anyhow!("load threshold must be numeric: {e}"))?;
    if !value.is_finite() || value <= 0.0 {
        anyhow::bail!("load threshold must be greater than zero");
    }
    if value > 1000.0 {
        anyhow::bail!("load threshold is implausibly high");
    }
    Ok((value * 1000.0).round() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accepting_runner() -> RunnerGenerationStatus {
        RunnerGenerationStatus {
            generation_id: "runner-test".to_string(),
            status: "accepting".to_string(),
            active_turn_count: 0,
            pid: 1,
            last_heartbeat_at: 100,
            heartbeat_age_seconds: 1,
            heartbeat_stale: false,
        }
    }

    fn host_under_limit() -> HostLoadStatus {
        HostLoadStatus {
            load_1m: 1.0,
            cpu_count: 8,
            max_load: 12.0,
            max_load_per_core: 1.5,
        }
    }

    fn db_with_capacity() -> DbPoolStatus {
        DbPoolStatus {
            size: 1,
            max_connections: 5,
            idle: 2,
            idle_reserve: Some(1),
            has_capacity: true,
        }
    }

    fn snapshot_with_pending(pending: i64) -> RunnerCapacitySnapshot {
        let config = RunnerConcurrencyConfig::Adaptive {
            max_jobs: 12,
            claim_burst: 1,
            db_idle_reserve: 1,
            max_load_per_core_millis: 1500,
            claim_interval_seconds: 10,
        };
        let jobs = AgentJobCounts {
            pending,
            ..AgentJobCounts::default()
        };
        let turns = AgentRunnerTurnCounts::default();
        let runner = Some(accepting_runner());
        let host = host_under_limit();
        let db_pool = db_with_capacity();
        let backpressure = classify_backpressure(
            &config,
            12,
            active_job_count(&jobs, &turns, runner.as_ref()),
            jobs.pending,
            runner.as_ref(),
            &host,
            &db_pool,
        );

        RunnerCapacitySnapshot {
            jobs,
            turns,
            running_jobs: RunningJobStats {
                count: 0,
                average_age_seconds: 0.0,
                min_age_seconds: 0,
                max_age_seconds: 0,
            },
            runner,
            host,
            db_pool,
            config: config.status(12),
            backpressure,
            recent_failures: Vec::new(),
            server_time: 100,
        }
    }

    #[test]
    fn default_runner_concurrency_is_adaptive() {
        assert_eq!(
            parse_runner_concurrency(None).unwrap(),
            RunnerConcurrencyConfig::default_adaptive()
        );
        assert!(matches!(
            parse_runner_concurrency(Some("unlimited")).unwrap(),
            RunnerConcurrencyConfig::Adaptive { .. }
        ));
    }

    #[test]
    fn fixed_runner_concurrency_parses_positive_limit() {
        assert_eq!(
            parse_runner_concurrency(Some("4")).unwrap(),
            RunnerConcurrencyConfig::Fixed { max_jobs: 4 }
        );
        assert!(parse_runner_concurrency(Some("0")).is_err());
    }

    #[test]
    fn backpressure_rejects_when_pending_cap_reached() {
        let config = RunnerConcurrencyConfig::default_adaptive();
        let status = classify_backpressure(
            &config,
            12,
            3,
            12,
            Some(&accepting_runner()),
            &host_under_limit(),
            &db_with_capacity(),
        );

        assert_eq!(status.state, "rejecting");
        assert_eq!(status.reason, "queue_pending_cap");
        assert!(status.would_reject_new_job);
    }

    #[test]
    fn enqueue_admission_rejects_oversized_child_batch() {
        let admission = admit_enqueue_from_snapshot(snapshot_with_pending(0), 51, "child_batch");

        assert!(!admission.accepted);
        assert_eq!(admission.reason, "batch_exceeds_capacity");
        assert_eq!(admission.rejected_jobs, 51);
    }

    #[test]
    fn failure_classifier_separates_usage_and_startup_failures() {
        assert_eq!(
            classify_failure_message("usage limit reached; try again later"),
            "usage_limit"
        );
        assert_eq!(
            classify_failure_message("Failed to start Codex: spawn os error 35"),
            "process_start"
        );
    }
}
