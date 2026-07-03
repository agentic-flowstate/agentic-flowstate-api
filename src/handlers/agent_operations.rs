use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use serde_json::json;
use sqlx::Row;
use std::sync::Arc;

const DEFAULT_ADAPTIVE_MAX_CONCURRENCY: i64 = 12;
const DEFAULT_ADAPTIVE_MAX_LOAD_PER_CORE: f64 = 1.5;

#[derive(Debug, Serialize)]
pub struct AgentOperationsStatus {
    pub jobs: AgentJobCounts,
    pub running_jobs: RunningJobStats,
    pub runner: Option<RunnerGenerationStatus>,
    pub host: HostLoadStatus,
    pub backpressure: BackpressureStatus,
}

#[derive(Debug, Default, Serialize)]
pub struct AgentJobCounts {
    pub pending: i64,
    pub running: i64,
    pub completed: i64,
    pub failed: i64,
    pub cancelled: i64,
}

#[derive(Debug, Serialize)]
pub struct RunningJobStats {
    pub count: i64,
    pub average_age_seconds: f64,
    pub min_age_seconds: i64,
    pub max_age_seconds: i64,
}

#[derive(Debug, Serialize)]
pub struct RunnerGenerationStatus {
    pub generation_id: String,
    pub status: String,
    pub active_turn_count: i64,
    pub pid: i64,
    pub last_heartbeat_at: i64,
    pub heartbeat_age_seconds: i64,
}

#[derive(Debug, Serialize)]
pub struct HostLoadStatus {
    pub load_1m: f64,
    pub cpu_count: usize,
    pub max_load: f64,
    pub max_load_per_core: f64,
}

#[derive(Debug, Serialize)]
pub struct BackpressureStatus {
    pub state: &'static str,
    pub reason: &'static str,
    pub active_jobs: i64,
    pub max_jobs: i64,
}

pub async fn get_agent_operations_status(State(db): State<Arc<sqlx::SqlitePool>>) -> Response {
    match build_agent_operations_status(&db).await {
        Ok(status) => (StatusCode::OK, Json(status)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn build_agent_operations_status(
    db: &sqlx::SqlitePool,
) -> anyhow::Result<AgentOperationsStatus> {
    let jobs = load_job_counts(db).await?;
    let running_jobs = load_running_job_stats(db).await?;
    let runner = load_runner_generation(db).await?;
    let host = current_host_load()?;
    let active_jobs = runner
        .as_ref()
        .map(|generation| generation.active_turn_count)
        .unwrap_or(jobs.running);
    let backpressure = classify_backpressure(active_jobs, &host);

    Ok(AgentOperationsStatus {
        jobs,
        running_jobs,
        runner,
        host,
        backpressure,
    })
}

async fn load_job_counts(db: &sqlx::SqlitePool) -> anyhow::Result<AgentJobCounts> {
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

async fn load_running_job_stats(db: &sqlx::SqlitePool) -> anyhow::Result<RunningJobStats> {
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

async fn load_runner_generation(
    db: &sqlx::SqlitePool,
) -> anyhow::Result<Option<RunnerGenerationStatus>> {
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
        WHERE runner_kind = 'agent-runner'
        ORDER BY last_heartbeat_at DESC
        LIMIT 1
        "#,
    )
    .fetch_optional(db)
    .await?;

    Ok(row.map(|row| RunnerGenerationStatus {
        generation_id: row.get("generation_id"),
        status: row.get("status"),
        active_turn_count: row.get("active_turn_count"),
        pid: row.get("pid"),
        last_heartbeat_at: row.get("last_heartbeat_at"),
        heartbeat_age_seconds: row.get("heartbeat_age_seconds"),
    }))
}

fn current_host_load() -> anyhow::Result<HostLoadStatus> {
    let cpu_count = std::thread::available_parallelism()?.get();
    let mut loads = [0.0_f64; 3];
    let samples = unsafe { libc::getloadavg(loads.as_mut_ptr(), 1) };
    if samples != 1 {
        anyhow::bail!("failed to read system load average");
    }

    Ok(HostLoadStatus {
        load_1m: loads[0],
        cpu_count,
        max_load: cpu_count as f64 * DEFAULT_ADAPTIVE_MAX_LOAD_PER_CORE,
        max_load_per_core: DEFAULT_ADAPTIVE_MAX_LOAD_PER_CORE,
    })
}

fn classify_backpressure(active_jobs: i64, host: &HostLoadStatus) -> BackpressureStatus {
    if host.load_1m >= host.max_load {
        return BackpressureStatus {
            state: "blocked",
            reason: "host_load",
            active_jobs,
            max_jobs: DEFAULT_ADAPTIVE_MAX_CONCURRENCY,
        };
    }

    if active_jobs >= DEFAULT_ADAPTIVE_MAX_CONCURRENCY {
        return BackpressureStatus {
            state: "blocked",
            reason: "concurrency_cap",
            active_jobs,
            max_jobs: DEFAULT_ADAPTIVE_MAX_CONCURRENCY,
        };
    }

    BackpressureStatus {
        state: "accepting",
        reason: "capacity_available",
        active_jobs,
        max_jobs: DEFAULT_ADAPTIVE_MAX_CONCURRENCY,
    }
}
