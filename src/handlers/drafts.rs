use axum::{
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use ticketing_system::{
    drafts, email_thread_tickets, CreateDraftRequest, EmailDraft, LinkThreadTicketRequest,
    SqlitePool, UpdateDraftRequest,
};

use crate::auth_middleware::AuthenticatedUser;

#[derive(Debug, Deserialize)]
pub struct ListDraftsQuery {
    pub include_all: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct DraftListResponse {
    pub drafts: Vec<EmailDraft>,
    pub total: i64,
}

/// List drafts (GET /api/drafts)
pub async fn list_drafts(
    State(pool): State<Arc<SqlitePool>>,
    Query(params): Query<ListDraftsQuery>,
) -> Result<Json<DraftListResponse>, (StatusCode, String)> {
    let include_all = params.include_all.unwrap_or(false);

    let draft_list = drafts::list_drafts(&pool, include_all)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let total = draft_list.len() as i64;

    Ok(Json(DraftListResponse {
        drafts: draft_list,
        total,
    }))
}

/// Get single draft by ID (GET /api/drafts/:id)
pub async fn get_draft(
    State(pool): State<Arc<SqlitePool>>,
    Path(id): Path<i64>,
) -> Result<Json<EmailDraft>, (StatusCode, String)> {
    let draft = drafts::get_draft_by_id(&pool, id)
        .await
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    Ok(Json(draft))
}

/// Create a draft (POST /api/drafts)
pub async fn create_draft(
    State(pool): State<Arc<SqlitePool>>,
    Json(req): Json<CreateDraftRequest>,
) -> Result<(StatusCode, Json<EmailDraft>), (StatusCode, String)> {
    let draft = drafts::create_draft(&pool, &req)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Log draft creation to ticket history if associated with a ticket
    if let Some(ticket_id) = &draft.ticket_id {
        if let Err(e) = ticketing_system::ticket_history::log_draft_created(
            &pool,
            ticket_id,
            draft.id,
            &draft.to_address,
            &draft.subject,
        )
        .await
        {
            tracing::warn!("Failed to log draft creation to ticket history: {}", e);
        }
    }

    Ok((StatusCode::CREATED, Json(draft)))
}

/// Update a draft (PATCH /api/drafts/:id)
pub async fn update_draft(
    State(pool): State<Arc<SqlitePool>>,
    Path(id): Path<i64>,
    Json(req): Json<UpdateDraftRequest>,
) -> Result<Json<EmailDraft>, (StatusCode, String)> {
    let draft = drafts::update_draft(&pool, id, &req)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(draft))
}

#[derive(Debug, Deserialize)]
pub struct UpdateStatusRequest {
    pub status: String,
}

/// Update draft status (POST /api/drafts/:id/status)
pub async fn update_draft_status(
    State(pool): State<Arc<SqlitePool>>,
    Path(id): Path<i64>,
    Json(req): Json<UpdateStatusRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
    // Validate status
    if !["draft", "sent", "discarded"].contains(&req.status.as_str()) {
        return Err((StatusCode::BAD_REQUEST, "Invalid status".to_string()));
    }

    drafts::update_draft_status(&pool, id, &req.status)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Delete a draft (DELETE /api/drafts/:id)
pub async fn delete_draft(
    State(pool): State<Arc<SqlitePool>>,
    Path(id): Path<i64>,
) -> Result<StatusCode, (StatusCode, String)> {
    drafts::delete_draft(&pool, id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Send a draft (POST /api/drafts/:id/send)
pub async fn send_draft(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> Result<Json<SendDraftResponse>, (StatusCode, String)> {
    // Get the draft
    let draft = drafts::get_draft_by_id(&pool, id)
        .await
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    if draft.status != "draft" {
        return Err((
            StatusCode::BAD_REQUEST,
            "Draft has already been sent or discarded".to_string(),
        ));
    }

    let to_addresses: Vec<String> = draft
        .to_address
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect();
    let cc_addresses: Vec<String> = draft
        .cc_address
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect();

    let delivery = crate::email_delivery::send_outbound_email(
        &pool,
        &user.user_id,
        &crate::email_delivery::OutboundEmail {
            from: draft.from_address.clone(),
            to: to_addresses.clone(),
            cc: cc_addresses.clone(),
            bcc: vec![],
            subject: draft.subject.clone(),
            body_text: Some(draft.body.clone()),
            body_html: Some(format!(
                "<pre style=\"font-family: sans-serif; white-space: pre-wrap;\">{}</pre>",
                draft.body
            )),
            reply_to: None,
            in_reply_to: None,
        },
    )
    .await
    .map_err(|e| {
        tracing::error!("Draft send failed: {:?}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to send email: {}", e),
        )
    })?;

    let message_id = delivery.message_id;
    tracing::info!("Draft {} sent successfully, message_id: {}", id, message_id);

    // Mark draft as sent
    drafts::update_draft_status(&pool, id, "sent")
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Store in Sent folder
    let now = chrono::Utc::now().timestamp();
    let cc_addresses = if cc_addresses.is_empty() {
        None
    } else {
        Some(cc_addresses)
    };

    // Save values for history logging before they get moved
    let history_to_address = draft.to_address.clone();
    let history_subject = draft.subject.clone();

    // Use message_id as thread_id for new conversations
    let thread_id = message_id.clone();

    let create_req = ticketing_system::CreateEmailRequest {
        message_id: message_id.clone(),
        mailbox: delivery.source_mailbox,
        folder: "Sent".to_string(),
        from_address: draft.from_address.clone(),
        from_name: None,
        to_addresses,
        cc_addresses,
        subject: Some(draft.subject),
        body_text: Some(draft.body),
        body_html: None,
        received_at: now,
        thread_id: Some(thread_id.clone()),
        in_reply_to: None,
    };

    if let Err(e) = ticketing_system::emails::create_email(&pool, &create_req).await {
        tracing::warn!("Failed to store sent email in database: {}", e);
    }

    // Link thread to ticket if draft had a ticket_id
    if let Some(ticket_id) = &draft.ticket_id {
        let link_req = LinkThreadTicketRequest {
            thread_id: thread_id.clone(),
            ticket_id: ticket_id.clone(),
            epic_id: draft.epic_id.clone(),
            slice_id: draft.slice_id.clone(),
        };
        if let Err(e) = email_thread_tickets::link_thread_to_ticket(&pool, &link_req).await {
            tracing::warn!("Failed to link thread to ticket: {}", e);
        } else {
            tracing::info!("Linked thread {} to ticket {}", thread_id, ticket_id);
        }

        // Log email sent to ticket history
        if let Err(e) = ticketing_system::ticket_history::log_email_sent(
            &pool,
            ticket_id,
            id,
            &history_to_address,
            &history_subject,
            &message_id,
        )
        .await
        {
            tracing::warn!("Failed to log email sent to ticket history: {}", e);
        }
    }

    Ok(Json(SendDraftResponse {
        message_id,
        success: true,
    }))
}

#[derive(Debug, Serialize)]
pub struct SendDraftResponse {
    pub message_id: String,
    pub success: bool,
}
