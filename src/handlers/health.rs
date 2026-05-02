use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Global readiness flag. Set to `true` after full initialization
/// (DB pool, listeners, background tasks). Set to `false` immediately
/// on shutdown signal. Used by /health/ready for readiness probes.
static READY: AtomicBool = AtomicBool::new(false);

/// Mark the service as ready to accept requests.
/// Called once after all initialization is complete.
pub fn set_ready() {
    READY.store(true, Ordering::SeqCst);
    tracing::info!("[HEALTH] Service marked as READY");
}

/// Mark the service as not ready (shutting down).
/// Called immediately when shutdown signal is received.
pub fn set_not_ready() {
    READY.store(false, Ordering::SeqCst);
    tracing::info!("[HEALTH] Service marked as NOT READY (shutting down)");
}

/// Check if the service is currently ready.
pub fn is_ready() -> bool {
    READY.load(Ordering::SeqCst)
}

/// GET /health/ready — readiness probe.
///
/// Returns 200 when the service is fully initialized and accepting requests.
/// Returns 503 during startup or shutdown.
///
/// Use this after a restart to confirm the service is actually back up:
///   Poll GET /health/ready every 500ms, max 30 retries (15s).
pub async fn ready() -> Response {
    if is_ready() {
        (
            StatusCode::OK,
            Json(json!({
                "status": "ready"
            })),
        )
            .into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "status": "starting"
            })),
        )
            .into_response()
    }
}

/// GET /health/ready with active agent count — for the iOS app to know
/// whether a restart would interrupt work.
pub async fn ready_with_agents(State(db): State<Arc<sqlx::SqlitePool>>) -> Response {
    let ready = is_ready();
    let active_agents = ticketing_system::restart_queue::count_active_work(&db)
        .await
        .ok();
    let active_agent_runs = active_agents
        .as_ref()
        .map(|w| w.agent_run_count)
        .unwrap_or(0);
    let active_runner_turns = active_agents
        .as_ref()
        .map(|w| w.runner_turn_count)
        .unwrap_or(0);
    let active_checkpoints = active_agents
        .as_ref()
        .map(|w| w.checkpoint_count)
        .unwrap_or(0);
    let active_total = active_agents.as_ref().map(|w| w.total).unwrap_or(0);

    let status_code = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let status_str = if ready { "ready" } else { "starting" };

    (
        status_code,
        Json(json!({
            "status": status_str,
            "active_agent_runs": active_agent_runs,
            "active_runner_turns": active_runner_turns,
            "active_checkpoints": active_checkpoints,
            "active_work_total": active_total
        })),
    )
        .into_response()
}
