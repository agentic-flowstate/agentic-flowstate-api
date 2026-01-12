use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use ticketing_system::{emails, Email, SqlitePool};

#[derive(Debug, Deserialize)]
pub struct ListEmailsQuery {
    pub mailbox: Option<String>,
    pub folder: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct EmailListResponse {
    pub emails: Vec<Email>,
    pub total: i64,
    pub unread: i64,
}

/// List emails (GET /api/emails)
pub async fn list_emails(
    State(pool): State<Arc<SqlitePool>>,
    Query(params): Query<ListEmailsQuery>,
) -> Result<Json<EmailListResponse>, (StatusCode, String)> {
    let limit = params.limit.unwrap_or(50);
    let offset = params.offset.unwrap_or(0);

    let (email_list, total, unread) = if let Some(mailbox) = &params.mailbox {
        let folder = params.folder.as_deref();
        let list = emails::list_emails(&pool, mailbox, folder, limit, offset)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let total = emails::count_emails(&pool, mailbox, folder)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let unread = emails::count_unread_emails(&pool, mailbox)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        (list, total, unread)
    } else {
        // List all emails across all mailboxes
        let list = emails::list_all_emails(&pool, limit, offset)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        // For unified inbox, count is the list length (simplified)
        let total = list.len() as i64;
        let unread = list.iter().filter(|e| !e.is_read).count() as i64;
        (list, total, unread)
    };

    Ok(Json(EmailListResponse {
        emails: email_list,
        total,
        unread,
    }))
}

/// Get single email by ID (GET /api/emails/:id)
pub async fn get_email(
    State(pool): State<Arc<SqlitePool>>,
    Path(id): Path<i64>,
) -> Result<Json<Email>, (StatusCode, String)> {
    let email = emails::get_email_by_id(&pool, id)
        .await
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    Ok(Json(email))
}

#[derive(Debug, Deserialize)]
pub struct UpdateEmailRequest {
    pub is_read: Option<bool>,
    pub is_starred: Option<bool>,
}

/// Update email (PATCH /api/emails/:id)
pub async fn update_email(
    State(pool): State<Arc<SqlitePool>>,
    Path(id): Path<i64>,
    Json(req): Json<UpdateEmailRequest>,
) -> Result<Json<Email>, (StatusCode, String)> {
    if let Some(is_read) = req.is_read {
        emails::mark_email_read(&pool, id, is_read)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }

    if let Some(is_starred) = req.is_starred {
        emails::mark_email_starred(&pool, id, is_starred)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }

    let email = emails::get_email_by_id(&pool, id)
        .await
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    Ok(Json(email))
}

/// Delete email (DELETE /api/emails/:id)
pub async fn delete_email(
    State(pool): State<Arc<SqlitePool>>,
    Path(id): Path<i64>,
) -> Result<StatusCode, (StatusCode, String)> {
    emails::delete_email(&pool, id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Serialize)]
pub struct EmailStatsResponse {
    pub mailboxes: Vec<MailboxStats>,
}

#[derive(Debug, Serialize)]
pub struct MailboxStats {
    pub mailbox: String,
    pub total: i64,
    pub unread: i64,
}

/// Get email stats (GET /api/emails/stats)
pub async fn get_email_stats(
    State(pool): State<Arc<SqlitePool>>,
) -> Result<Json<EmailStatsResponse>, (StatusCode, String)> {
    // For now, hardcode the mailbox - could query distinct mailboxes later
    let mailboxes = vec!["jakeGreene@ballotradar.com".to_string()];

    let mut stats = Vec::new();
    for mailbox in mailboxes {
        let total = emails::count_emails(&pool, &mailbox, None)
            .await
            .unwrap_or(0);
        let unread = emails::count_unread_emails(&pool, &mailbox)
            .await
            .unwrap_or(0);

        stats.push(MailboxStats {
            mailbox,
            total,
            unread,
        });
    }

    Ok(Json(EmailStatsResponse { mailboxes: stats }))
}

#[derive(Debug, Deserialize)]
pub struct SendEmailRequest {
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub bcc: Vec<String>,
    pub subject: String,
    pub body_text: Option<String>,
    pub body_html: Option<String>,
    #[serde(default = "default_from_address")]
    pub from: String,
    pub reply_to: Option<String>,
}

fn default_from_address() -> String {
    "jakeGreene@ballotradar.com".to_string()
}

#[derive(Debug, Serialize)]
pub struct SendEmailResponse {
    pub message_id: String,
    pub success: bool,
}

/// Send email via SES and store in Sent folder (POST /api/emails/send)
pub async fn send_email(
    State(pool): State<Arc<SqlitePool>>,
    Json(req): Json<SendEmailRequest>,
) -> Result<Json<SendEmailResponse>, (StatusCode, String)> {
    use aws_sdk_sesv2::types::{Body, Content, Destination, EmailContent, Message};

    // Load AWS config with ballotradar-shared profile
    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .profile_name("ballotradar-shared")
        .region(aws_config::Region::new("us-east-1"))
        .load()
        .await;

    let ses_client = aws_sdk_sesv2::Client::new(&config);

    // Build destination
    let mut destination_builder = Destination::builder();
    for to in &req.to {
        destination_builder = destination_builder.to_addresses(to);
    }
    for cc in &req.cc {
        destination_builder = destination_builder.cc_addresses(cc);
    }
    for bcc in &req.bcc {
        destination_builder = destination_builder.bcc_addresses(bcc);
    }
    let destination = destination_builder.build();

    // Build email body
    let mut body_builder = Body::builder();
    if let Some(text) = &req.body_text {
        body_builder = body_builder.text(
            Content::builder()
                .data(text)
                .charset("UTF-8")
                .build()
                .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?
        );
    }
    if let Some(html) = &req.body_html {
        body_builder = body_builder.html(
            Content::builder()
                .data(html)
                .charset("UTF-8")
                .build()
                .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?
        );
    }
    let body = body_builder.build();

    // Build message
    let subject = Content::builder()
        .data(&req.subject)
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

    // Build and send request
    let mut send_request = ses_client
        .send_email()
        .from_email_address(&req.from)
        .destination(destination)
        .content(email_content);

    if let Some(reply_to) = &req.reply_to {
        send_request = send_request.reply_to_addresses(reply_to);
    }

    let result = send_request
        .send()
        .await
        .map_err(|e| {
            tracing::error!("SES send failed: {:?}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to send email: {}", e))
        })?;

    let message_id = result.message_id().unwrap_or("unknown").to_string();
    tracing::info!("Email sent successfully, message_id: {}", message_id);

    // Store in Sent folder
    let now = chrono::Utc::now().timestamp();
    let create_req = ticketing_system::CreateEmailRequest {
        message_id: message_id.clone(),
        mailbox: req.from.clone(),
        folder: "Sent".to_string(),
        from_address: req.from.clone(),
        from_name: None,
        to_addresses: req.to.clone(),
        cc_addresses: if req.cc.is_empty() { None } else { Some(req.cc.clone()) },
        subject: Some(req.subject.clone()),
        body_text: req.body_text.clone(),
        body_html: req.body_html.clone(),
        received_at: now,
        thread_id: None,
        in_reply_to: None,
    };

    if let Err(e) = emails::create_email(&pool, &create_req).await {
        tracing::warn!("Failed to store sent email in database: {}", e);
        // Don't fail the request - email was sent successfully
    }

    Ok(Json(SendEmailResponse {
        message_id,
        success: true,
    }))
}
