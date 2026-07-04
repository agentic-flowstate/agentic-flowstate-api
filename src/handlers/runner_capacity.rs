use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

pub use ticketing_system::runner_capacity::{admit_enqueue, build_snapshot, RunnerQueueAdmission};

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

#[cfg(test)]
mod tests {
    use super::*;
    use ticketing_system::runner_capacity::{
        admit_enqueue_from_snapshot, AgentJobCounts, AgentRunnerTurnCounts, BackpressureStatus,
        DbPoolStatus, HostLoadStatus, RunnerCapacityConfigStatus, RunnerCapacitySnapshot,
        RunningJobStats,
    };

    fn rejected_admission() -> RunnerQueueAdmission {
        let snapshot = RunnerCapacitySnapshot {
            jobs: AgentJobCounts {
                pending: 12,
                ..AgentJobCounts::default()
            },
            turns: AgentRunnerTurnCounts::default(),
            running_jobs: RunningJobStats {
                count: 0,
                average_age_seconds: 0.0,
                min_age_seconds: 0,
                max_age_seconds: 0,
            },
            runner: None,
            host: HostLoadStatus {
                load_1m: 1.0,
                cpu_count: 8,
                max_load: 12.0,
                max_load_per_core: 1.5,
            },
            db_pool: DbPoolStatus {
                size: 1,
                max_connections: 5,
                idle: 2,
                idle_reserve: Some(1),
                has_capacity: true,
            },
            config: RunnerCapacityConfigStatus {
                runner_kind: "agent-runner".to_string(),
                mode: "adaptive".to_string(),
                max_jobs: 12,
                max_pending_jobs: 12,
                claim_burst: Some(1),
                claim_interval_seconds: Some(10),
                db_idle_reserve: Some(1),
                max_load_per_core: Some(1.5),
                heartbeat_stale_seconds: 90,
                queue_admission_enabled: true,
                policy_source: "default".to_string(),
                policy_updated_at: 100,
            },
            backpressure: BackpressureStatus {
                state: "rejecting".to_string(),
                reason: "queue_pending_cap".to_string(),
                active_jobs: 0,
                pending_jobs: 12,
                max_jobs: 12,
                max_pending_jobs: 12,
                available_runner_slots: 12,
                available_queue_slots: 0,
                deferred_jobs: 12,
                rejected_jobs: 0,
                would_reject_new_job: true,
                message: "Runner queue pending cap reached".to_string(),
            },
            latest_sample: None,
            recent_admission_events: Vec::new(),
            recent_failures: Vec::new(),
            server_time: 100,
        };
        admit_enqueue_from_snapshot(snapshot, 1, "api_test")
    }

    #[test]
    fn queue_admission_rejection_response_uses_backpressure_payload() {
        let response = queue_admission_rejection_response(rejected_admission());
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }
}
