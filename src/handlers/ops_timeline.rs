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
pub struct OpsTimelineQuery {
    pub source: Option<String>,
    pub event_type: Option<String>,
    #[serde(rename = "type")]
    pub type_filter: Option<String>,
    pub severity: Option<String>,
    pub component: Option<String>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub search: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// GET /api/admin/ops/timeline
pub async fn list_ops_timeline(
    State(pool): State<Arc<SqlitePool>>,
    Query(query): Query<OpsTimelineQuery>,
) -> Response {
    let filter = ticketing_system::ops_timeline::OpsTimelineFilter {
        source: query.source,
        event_type: query.event_type.or(query.type_filter),
        severity: query.severity,
        component: query.component,
        since: query.since,
        until: query.until,
        search: query.search,
        limit: query.limit,
        offset: query.offset,
    };

    match ticketing_system::ops_timeline::list_ops_timeline(&pool, &filter).await {
        Ok(page) => (StatusCode::OK, Json(page)).into_response(),
        Err(e) => {
            tracing::error!(
                target: "agentic_api::ops_timeline",
                event = "ops_timeline.list_failed",
                error = %e,
                "failed to list ops timeline"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Failed to list ops timeline"})),
            )
                .into_response()
        }
    }
}
