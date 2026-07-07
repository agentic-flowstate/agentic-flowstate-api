use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;
use sqlx::SqlitePool;
use std::sync::Arc;
use ticketing_system::SystemLogIncidentStatus;

#[derive(Debug, Deserialize)]
pub struct IncidentQuery {
    pub status: Option<String>,
    pub limit: Option<i64>,
}

pub async fn list_log_incidents(
    State(pool): State<Arc<SqlitePool>>,
    Query(query): Query<IncidentQuery>,
) -> Response {
    let status = match parse_status(query.status.as_deref()) {
        Ok(status) => status,
        Err(message) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response()
        }
    };
    let limit = query.limit.unwrap_or(100).clamp(1, 1000);

    if let Err(error) =
        ticketing_system::system_logs::refresh_fixed_system_log_incidents(pool.as_ref()).await
    {
        tracing::warn!(
            target: "agentic_api::log_incidents",
            error = %error,
            "failed to refresh fixed incident statuses before list"
        );
    }

    match ticketing_system::system_logs::list_system_log_incidents_with_runtime_status(
        pool.as_ref(),
        status,
        limit,
    )
    .await
    {
        Ok(incidents) => (StatusCode::OK, Json(json!(incidents))).into_response(),
        Err(error) => {
            tracing::error!(
                target: "agentic_api::log_incidents",
                error = %error,
                "failed to list log incidents"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to list log incidents" })),
            )
                .into_response()
        }
    }
}

pub async fn run_log_incident_triage(State(pool): State<Arc<SqlitePool>>) -> Response {
    match crate::log_incident_triage::run_once(pool).await {
        Ok(summary) => (StatusCode::OK, Json(json!(summary))).into_response(),
        Err(error) => {
            tracing::error!(
                target: "agentic_api::log_incidents",
                error = %error,
                "manual log incident triage failed"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Log incident triage failed" })),
            )
                .into_response()
        }
    }
}

pub async fn get_log_incident_triage_status(State(pool): State<Arc<SqlitePool>>) -> Response {
    match ticketing_system::system_logs::get_system_log_incident_scan_state(
        pool.as_ref(),
        crate::log_incident_triage::SCANNER_KEY,
        crate::log_incident_triage::SCANNER_STALE_AFTER_SECONDS,
    )
    .await
    {
        Ok(Some(status)) => (StatusCode::OK, Json(json!(status))).into_response(),
        Ok(None) => (
            StatusCode::OK,
            Json(json!({
                "scanner_key": crate::log_incident_triage::SCANNER_KEY,
                "last_log_id": 0,
                "updated_at": null,
                "updated_at_iso": null,
                "age_seconds": null,
                "stale": true
            })),
        )
            .into_response(),
        Err(error) => {
            tracing::error!(
                target: "agentic_api::log_incidents",
                error = %error,
                "failed to load log incident triage status"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to load log incident triage status" })),
            )
                .into_response()
        }
    }
}

fn parse_status(value: Option<&str>) -> Result<Option<SystemLogIncidentStatus>, String> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    SystemLogIncidentStatus::try_from(value)
        .map(Some)
        .map_err(|_| {
            "status must be one of: unaddressed, investigating, fixed, ignored".to_string()
        })
}
