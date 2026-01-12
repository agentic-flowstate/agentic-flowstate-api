use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use ticketing_system::{drafts, email_thread_tickets, CreateDraftRequest, EmailDraft, LinkThreadTicketRequest, SqlitePool, UpdateDraftRequest};

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
        ).await {
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

/// Send a draft via SES (POST /api/drafts/:id/send)
pub async fn send_draft(
    State(pool): State<Arc<SqlitePool>>,
    Path(id): Path<i64>,
) -> Result<Json<SendDraftResponse>, (StatusCode, String)> {
    use aws_sdk_sesv2::types::{Body, Content, Destination, EmailContent, Message};

    // Get the draft
    let draft = drafts::get_draft_by_id(&pool, id)
        .await
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    if draft.status != "draft" {
        return Err((StatusCode::BAD_REQUEST, "Draft has already been sent or discarded".to_string()));
    }

    // Load AWS config
    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .profile_name("ballotradar-shared")
        .region(aws_config::Region::new("us-east-1"))
        .load()
        .await;

    let ses_client = aws_sdk_sesv2::Client::new(&config);

    // Build destination
    let mut destination_builder = Destination::builder();
    // Parse to addresses (comma-separated)
    for to in draft.to_address.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
        destination_builder = destination_builder.to_addresses(to);
    }
    // Parse cc addresses if present
    if let Some(cc) = &draft.cc_address {
        for cc_addr in cc.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
            destination_builder = destination_builder.cc_addresses(cc_addr);
        }
    }
    let destination = destination_builder.build();

    // Build email body
    let body = Body::builder()
        .text(
            Content::builder()
                .data(&draft.body)
                .charset("UTF-8")
                .build()
                .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?
        )
        .html(
            Content::builder()
                .data(&format!("<pre style=\"font-family: sans-serif; white-space: pre-wrap;\">{}</pre>", draft.body))
                .charset("UTF-8")
                .build()
                .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?
        )
        .build();

    let subject = Content::builder()
        .data(&draft.subject)
        .charset("UTF-8")
        .build()
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    let message = Message::builder()
        .subject(subject)
        .body(body)
        .build();

    let email_content = EmailContent::builder()
        .simple(message)
        .build();

    let result = ses_client
        .send_email()
        .from_email_address(&draft.from_address)
        .destination(destination)
        .content(email_content)
        .send()
        .await
        .map_err(|e| {
            tracing::error!("SES send failed: {:?}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to send email: {}", e))
        })?;

    let message_id = result.message_id().unwrap_or("unknown").to_string();
    tracing::info!("Draft {} sent successfully, message_id: {}", id, message_id);

    // Mark draft as sent
    drafts::update_draft_status(&pool, id, "sent")
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Store in Sent folder
    let now = chrono::Utc::now().timestamp();
    let to_addresses: Vec<String> = draft.to_address
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let cc_addresses: Option<Vec<String>> = draft.cc_address.map(|cc| {
        cc.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    });

    // Save values for history logging before they get moved
    let history_to_address = draft.to_address.clone();
    let history_subject = draft.subject.clone();

    // Use message_id as thread_id for new conversations
    let thread_id = message_id.clone();

    let create_req = ticketing_system::CreateEmailRequest {
        message_id: message_id.clone(),
        mailbox: draft.from_address.clone(),
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
        ).await {
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
