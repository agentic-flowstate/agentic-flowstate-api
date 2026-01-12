use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    pub limit: Option<i32>,
}

#[derive(Debug, Serialize)]
pub struct TicketHistoryResponse {
    pub events: Vec<ticketing_system::ticket_history::TicketHistoryEvent>,
}

/// GET /api/epics/:epic_id/slices/:slice_id/tickets/:ticket_id/history
///
/// Get history events for a specific ticket.
pub async fn get_ticket_history(
    Path((_epic_id, _slice_id, ticket_id)): Path<(String, String, String)>,
    Query(params): Query<HistoryQuery>,
    State(db): State<Arc<SqlitePool>>,
) -> Result<Json<TicketHistoryResponse>, (StatusCode, String)> {
    let events = if let Some(limit) = params.limit {
        ticketing_system::ticket_history::get_ticket_history_limited(&db, &ticket_id, limit)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to fetch history: {}", e)))?
    } else {
        ticketing_system::ticket_history::get_ticket_history(&db, &ticket_id)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to fetch history: {}", e)))?
    };

    Ok(Json(TicketHistoryResponse { events }))
}

/// GET /api/tickets/:ticket_id/history
///
/// Get history events for a ticket by ID only.
pub async fn get_ticket_history_by_id(
    Path(ticket_id): Path<String>,
    Query(params): Query<HistoryQuery>,
    State(db): State<Arc<SqlitePool>>,
) -> Result<Json<TicketHistoryResponse>, (StatusCode, String)> {
    let events = if let Some(limit) = params.limit {
        ticketing_system::ticket_history::get_ticket_history_limited(&db, &ticket_id, limit)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to fetch history: {}", e)))?
    } else {
        ticketing_system::ticket_history::get_ticket_history(&db, &ticket_id)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to fetch history: {}", e)))?
    };

    Ok(Json(TicketHistoryResponse { events }))
}
