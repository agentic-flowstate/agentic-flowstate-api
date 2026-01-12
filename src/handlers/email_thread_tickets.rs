use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::Serialize;
use std::sync::Arc;
use ticketing_system::{email_thread_tickets, EmailThreadTicket, LinkThreadTicketRequest, SqlitePool};

#[derive(Debug, Serialize)]
pub struct ThreadTicketsResponse {
    pub thread_id: String,
    pub tickets: Vec<EmailThreadTicket>,
}

/// Get all tickets linked to a thread (GET /api/email-threads/:thread_id/tickets)
pub async fn get_tickets_for_thread(
    State(pool): State<Arc<SqlitePool>>,
    Path(thread_id): Path<String>,
) -> Result<Json<ThreadTicketsResponse>, (StatusCode, String)> {
    let tickets = email_thread_tickets::get_tickets_for_thread(&pool, &thread_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(ThreadTicketsResponse {
        thread_id,
        tickets,
    }))
}

/// Link a thread to a ticket (POST /api/email-threads/:thread_id/tickets)
pub async fn link_thread_to_ticket(
    State(pool): State<Arc<SqlitePool>>,
    Path(thread_id): Path<String>,
    Json(body): Json<LinkTicketBody>,
) -> Result<(StatusCode, Json<EmailThreadTicket>), (StatusCode, String)> {
    let req = LinkThreadTicketRequest {
        thread_id,
        ticket_id: body.ticket_id,
        epic_id: body.epic_id,
        slice_id: body.slice_id,
    };

    let link = email_thread_tickets::link_thread_to_ticket(&pool, &req)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok((StatusCode::CREATED, Json(link)))
}

#[derive(Debug, serde::Deserialize)]
pub struct LinkTicketBody {
    pub ticket_id: String,
    pub epic_id: Option<String>,
    pub slice_id: Option<String>,
}

/// Unlink a thread from a ticket (DELETE /api/email-threads/:thread_id/tickets/:ticket_id)
pub async fn unlink_thread_from_ticket(
    State(pool): State<Arc<SqlitePool>>,
    Path((thread_id, ticket_id)): Path<(String, String)>,
) -> Result<StatusCode, (StatusCode, String)> {
    email_thread_tickets::unlink_thread_from_ticket(&pool, &thread_id, &ticket_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}
