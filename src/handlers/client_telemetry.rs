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

use crate::observability::contracts;
use ticketing_system::models::ClientEventInput;

/// POST /api/telemetry/events — accepts a JSON array of client events (max 1000)
pub async fn ingest_events(
    State(pool): State<Arc<SqlitePool>>,
    Json(events): Json<Vec<ClientEventInput>>,
) -> Response {
    if events.is_empty() {
        return (StatusCode::OK, Json(json!({"accepted": 0}))).into_response();
    }
    if events.len() > 1000 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Maximum 1000 events per request"})),
        )
            .into_response();
    }
    if let Err(error) = contracts::validate_client_events(&events) {
        tracing::warn!(
            target: "observability.contracts",
            event = "client_telemetry.rejected",
            registry_version = contracts::registry_version(),
            event_index = error.event_index,
            reason = %error.reason,
            "client telemetry batch rejected by observability guardrail"
        );
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "Client telemetry rejected by observability guardrail",
                "event_index": error.event_index,
                "reason": error.reason,
                "registry_version": contracts::registry_version()
            })),
        )
            .into_response();
    }

    match ticketing_system::client_events::insert_client_events(&pool, &events).await {
        Ok(accepted) => (StatusCode::OK, Json(json!({"accepted": accepted}))).into_response(),
        Err(e) => {
            tracing::error!("Failed to ingest client events: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Failed to ingest events"})),
            )
                .into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ClientEventsQuery {
    pub level: Option<String>,
    pub component: Option<String>,
    pub user_id: Option<String>,
    pub event_type: Option<String>,
    pub since: Option<i64>,
    pub search: Option<String>,
    pub limit: Option<i64>,
}

/// GET /api/admin/client-events — query client events with filters (admin-only)
pub async fn list_client_events(
    State(pool): State<Arc<SqlitePool>>,
    Query(query): Query<ClientEventsQuery>,
) -> Response {
    let limit = query.limit.unwrap_or(200).min(1000);
    match ticketing_system::client_events::list_client_events(
        &pool,
        query.level.as_deref(),
        query.component.as_deref(),
        query.user_id.as_deref(),
        query.event_type.as_deref(),
        query.since,
        query.search.as_deref(),
        limit,
    )
    .await
    {
        Ok(events) => (StatusCode::OK, Json(json!(events))).into_response(),
        Err(e) => {
            tracing::error!("Failed to list client events: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Failed to list client events"})),
            )
                .into_response()
        }
    }
}
