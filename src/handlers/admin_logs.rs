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

#[derive(Debug, Deserialize)]
pub struct LogsQuery {
    pub level: Option<String>,
    pub component: Option<String>,
    pub search: Option<String>,
    pub since: Option<i64>,
    pub limit: Option<i64>,
}

/// GET /api/admin/logs
pub async fn list_logs(
    State(pool): State<Arc<SqlitePool>>,
    Query(query): Query<LogsQuery>,
) -> Response {
    let limit = query.limit.unwrap_or(200).min(1000);
    match ticketing_system::system_logs::list_logs(
        &pool,
        query.level.as_deref(),
        query.component.as_deref(),
        query.search.as_deref(),
        query.since,
        limit,
    )
    .await
    {
        Ok(logs) => (StatusCode::OK, Json(json!(logs))).into_response(),
        Err(e) => {
            tracing::error!("Failed to list system logs: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Failed to list system logs"})),
            )
                .into_response()
        }
    }
}

/// GET /api/admin/check — returns 200 if caller is admin (guards via middleware)
pub async fn check_admin() -> Response {
    (StatusCode::OK, Json(json!({"is_admin": true}))).into_response()
}
