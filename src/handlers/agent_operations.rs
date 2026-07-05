use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

pub async fn get_agent_operations_status(State(db): State<Arc<sqlx::SqlitePool>>) -> Response {
    match super::runner_capacity::build_snapshot(&db).await {
        Ok(status) => (StatusCode::OK, Json(status)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct AgentOperationsTrendsQuery {
    pub runner_kind: Option<String>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub window_seconds: Option<i64>,
}

pub async fn get_agent_operations_trends(
    State(db): State<Arc<sqlx::SqlitePool>>,
    Query(query): Query<AgentOperationsTrendsQuery>,
) -> Response {
    let filter = ticketing_system::runner_capacity::RunnerTrendFilter {
        runner_kind: query.runner_kind,
        since: query.since,
        until: query.until,
        window_seconds: query.window_seconds,
    };

    match ticketing_system::runner_capacity::load_trend_summary(&db, &filter).await {
        Ok(summary) => (StatusCode::OK, Json(summary)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}
