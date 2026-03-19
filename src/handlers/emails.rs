use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use ticketing_system::{emails, Email, EmailAttachment, EmailThread, SqlitePool};

/// Sanitize HTML email body to prevent XSS
fn sanitize_email_html(html: &str) -> String {
    ammonia::Builder::default()
        .add_tags(&[
            "div", "span", "p", "br", "a", "b", "i", "u", "strong", "em",
            "h1", "h2", "h3", "h4", "h5", "h6", "ul", "ol", "li",
            "table", "thead", "tbody", "tfoot", "tr", "td", "th", "caption", "colgroup", "col",
            "blockquote", "pre", "code", "img", "hr", "sup", "sub", "small",
            "dl", "dt", "dd", "figure", "figcaption", "abbr", "cite",
            "center", "font",
        ])
        .add_tag_attributes("a", &["href", "title", "target"])
        .add_tag_attributes("img", &["src", "alt", "width", "height", "style"])
        .add_tag_attributes("td", &["colspan", "rowspan", "style", "align", "valign", "width"])
        .add_tag_attributes("th", &["colspan", "rowspan", "style", "align", "valign", "width"])
        .add_tag_attributes("table", &["style", "width", "cellpadding", "cellspacing", "border"])
        .add_tag_attributes("tr", &["style"])
        .add_tag_attributes("div", &["style", "class"])
        .add_tag_attributes("span", &["style", "class"])
        .add_tag_attributes("p", &["style"])
        .add_tag_attributes("font", &["color", "size", "face"])
        .add_tag_attributes("col", &["width", "style"])
        .url_schemes(std::collections::HashSet::from(["http", "https", "mailto", "cid"]))
        .link_rel(Some("noopener noreferrer"))
        .clean(html)
        .to_string()
}

/// Apply HTML sanitization to an Email struct
fn sanitize_email(mut email: Email) -> Email {
    if let Some(ref html) = email.body_html {
        email.body_html = Some(sanitize_email_html(html));
    }
    email
}

fn sanitize_emails(emails: Vec<Email>) -> Vec<Email> {
    emails.into_iter().map(sanitize_email).collect()
}

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
    } else if let Some(folder) = &params.folder {
        // Filter by folder across all mailboxes
        let list = emails::list_emails_by_folder(&pool, folder, limit, offset)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let total = list.len() as i64;
        let unread = list.iter().filter(|e| !e.is_read).count() as i64;
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
        emails: sanitize_emails(email_list),
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

    Ok(Json(sanitize_email(email)))
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
    // Query distinct mailboxes from stored emails
    let rows = sqlx::query_as::<_, (String,)>("SELECT DISTINCT mailbox FROM emails")
        .fetch_all(pool.as_ref())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mailboxes: Vec<String> = rows.into_iter().map(|(m,)| m).collect();

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
    pub from: String,
    pub reply_to: Option<String>,
    /// Set when replying — the Message-ID of the email being replied to
    pub in_reply_to: Option<String>,
    /// Thread ID for grouping in conversation view
    pub thread_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SendEmailResponse {
    pub message_id: String,
    pub success: bool,
}

/// Send email via SES and store in Sent folder (POST /api/emails/send)
///
/// When `in_reply_to` is set, constructs raw MIME with In-Reply-To and References
/// headers so mail clients thread the conversation properly.
pub async fn send_email(
    State(pool): State<Arc<SqlitePool>>,
    Json(req): Json<SendEmailRequest>,
) -> Result<Json<SendEmailResponse>, (StatusCode, String)> {
    // Load AWS config with ballotradar-shared profile
    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .profile_name("ballotradar-shared")
        .region(aws_config::Region::new("us-east-1"))
        .load()
        .await;

    let ses_client = aws_sdk_sesv2::Client::new(&config);

    let result = if req.in_reply_to.is_some() {
        // Reply/forward: use raw MIME to set In-Reply-To and References headers
        send_raw_email(&ses_client, &req).await?
    } else {
        // New email: use simple SES API
        send_simple_email(&ses_client, &req).await?
    };

    let message_id = result;
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
        thread_id: req.thread_id.clone(),
        in_reply_to: req.in_reply_to.clone(),
    };

    if let Err(e) = emails::create_email(&pool, &create_req).await {
        tracing::warn!("Failed to store sent email in database: {}", e);
    }

    Ok(Json(SendEmailResponse {
        message_id,
        success: true,
    }))
}

/// Send via SES Simple API (no threading headers needed)
async fn send_simple_email(
    ses_client: &aws_sdk_sesv2::Client,
    req: &SendEmailRequest,
) -> Result<String, (StatusCode, String)> {
    use aws_sdk_sesv2::types::{Body, Content, Destination, EmailContent, Message};

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

    let mut body_builder = Body::builder();
    if let Some(text) = &req.body_text {
        body_builder = body_builder.text(
            Content::builder().data(text).charset("UTF-8").build()
                .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?
        );
    }
    if let Some(html) = &req.body_html {
        body_builder = body_builder.html(
            Content::builder().data(html).charset("UTF-8").build()
                .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?
        );
    }

    let message = Message::builder()
        .subject(Content::builder().data(&req.subject).charset("UTF-8").build()
            .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?)
        .body(body_builder.build())
        .build();

    let email_content = EmailContent::builder().simple(message).build();

    let mut send_request = ses_client
        .send_email()
        .from_email_address(&req.from)
        .destination(destination_builder.build())
        .content(email_content);

    if let Some(reply_to) = &req.reply_to {
        send_request = send_request.reply_to_addresses(reply_to);
    }

    let result = send_request.send().await.map_err(|e| {
        tracing::error!("SES send failed: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to send email: {}", e))
    })?;

    Ok(result.message_id().unwrap_or("unknown").to_string())
}

/// Send via SES Raw API with In-Reply-To and References headers for threading
async fn send_raw_email(
    ses_client: &aws_sdk_sesv2::Client,
    req: &SendEmailRequest,
) -> Result<String, (StatusCode, String)> {
    use aws_sdk_sesv2::types::{EmailContent, RawMessage};
    use aws_sdk_sesv2::primitives::Blob;
    use mail_builder::MessageBuilder;

    let mut builder = MessageBuilder::new();
    builder = builder.from(req.from.as_str());

    for to in &req.to {
        builder = builder.to(to.as_str());
    }
    for cc in &req.cc {
        builder = builder.cc(cc.as_str());
    }
    for bcc in &req.bcc {
        builder = builder.bcc(bcc.as_str());
    }

    builder = builder.subject(&req.subject);

    if let Some(in_reply_to) = &req.in_reply_to {
        builder = builder.in_reply_to(in_reply_to.as_str());
        // References = In-Reply-To for single-level threading
        builder = builder.header("References", mail_builder::headers::raw::Raw::new(in_reply_to.as_str()));
    }

    // Build body: multipart/alternative if both text and html, otherwise single part
    match (&req.body_text, &req.body_html) {
        (Some(text), Some(html)) => {
            builder = builder.text_body(text);
            builder = builder.html_body(html);
        }
        (Some(text), None) => {
            builder = builder.text_body(text);
        }
        (None, Some(html)) => {
            builder = builder.html_body(html);
        }
        (None, None) => {
            builder = builder.text_body("");
        }
    }

    let raw_bytes = builder.write_to_vec()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to build MIME: {}", e)))?;

    let raw_message = RawMessage::builder()
        .data(Blob::new(raw_bytes))
        .build()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let email_content = EmailContent::builder()
        .raw(raw_message)
        .build();

    let result = ses_client
        .send_email()
        .content(email_content)
        .send()
        .await
        .map_err(|e| {
            tracing::error!("SES raw send failed: {:?}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to send email: {}", e))
        })?;

    Ok(result.message_id().unwrap_or("unknown").to_string())
}

// ============================================================================
// Search endpoint
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct SearchEmailsQuery {
    pub q: String,
    pub mailbox: Option<String>,
    pub folder: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// Search emails via FTS5 (GET /api/emails/search)
pub async fn search_emails(
    State(pool): State<Arc<SqlitePool>>,
    Query(params): Query<SearchEmailsQuery>,
) -> Result<Json<EmailListResponse>, (StatusCode, String)> {
    if params.q.trim().is_empty() {
        return Ok(Json(EmailListResponse {
            emails: vec![],
            total: 0,
            unread: 0,
        }));
    }

    let limit = params.limit.unwrap_or(50);
    let offset = params.offset.unwrap_or(0);
    let mailbox = params.mailbox.as_deref()
        .and_then(|m| if m == "all" { None } else { Some(m) });

    let results = emails::search_emails(
        &pool,
        &params.q,
        mailbox,
        params.folder.as_deref(),
        limit,
        offset,
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let total = results.len() as i64;
    let unread = results.iter().filter(|e| !e.is_read).count() as i64;

    Ok(Json(EmailListResponse {
        emails: sanitize_emails(results),
        total,
        unread,
    }))
}

// ============================================================================
// Thread endpoints
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct ListThreadsQuery {
    pub mailbox: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct ThreadListResponse {
    pub threads: Vec<EmailThread>,
}

/// List email threads (GET /api/emails/threads)
pub async fn list_threads(
    State(pool): State<Arc<SqlitePool>>,
    Query(params): Query<ListThreadsQuery>,
) -> Result<Json<ThreadListResponse>, (StatusCode, String)> {
    let limit = params.limit.unwrap_or(50);
    let offset = params.offset.unwrap_or(0);
    let mailbox = params.mailbox.as_deref()
        .and_then(|m| if m == "all" { None } else { Some(m) });

    let threads = emails::list_threads(&pool, mailbox, limit, offset)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(ThreadListResponse { threads }))
}

/// Get all emails in a thread (GET /api/emails/threads/:thread_id)
pub async fn get_thread(
    State(pool): State<Arc<SqlitePool>>,
    Path(thread_id): Path<String>,
) -> Result<Json<Vec<Email>>, (StatusCode, String)> {
    let thread_emails = emails::get_thread_emails(&pool, &thread_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(sanitize_emails(thread_emails)))
}

// ============================================================================
// Attachment endpoints
// ============================================================================

/// List attachments for an email (GET /api/emails/:id/attachments)
pub async fn list_attachments(
    State(pool): State<Arc<SqlitePool>>,
    Path(email_id): Path<i64>,
) -> Result<Json<Vec<EmailAttachment>>, (StatusCode, String)> {
    let attachments = emails::list_attachments(&pool, email_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(attachments))
}

/// Download an attachment (GET /api/emails/attachments/:attachment_id)
pub async fn download_attachment(
    State(pool): State<Arc<SqlitePool>>,
    Path(attachment_id): Path<i64>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    use axum::body::Body;
    use axum::response::Response;

    let attachment = emails::get_attachment(&pool, attachment_id)
        .await
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    let stored_path = attachment.stored_path
        .ok_or((StatusCode::NOT_FOUND, "Attachment file not stored".to_string()))?;

    let file_bytes = tokio::fs::read(&stored_path)
        .await
        .map_err(|e| (StatusCode::NOT_FOUND, format!("File not found: {}", e)))?;

    let response = Response::builder()
        .header("Content-Type", &attachment.content_type)
        .header("Content-Disposition", format!("attachment; filename=\"{}\"", attachment.filename))
        .header("Content-Length", file_bytes.len().to_string())
        .body(Body::from(file_bytes))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(response)
}
