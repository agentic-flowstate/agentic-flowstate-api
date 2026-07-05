use axum::{
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use ticketing_system::{email_accounts, email_intake, emails, SqlitePool};

use crate::auth_middleware::AuthenticatedUser;
use crate::email_classifier;

async fn get_user_mailboxes(
    pool: &SqlitePool,
    user_id: &str,
) -> Result<Vec<String>, (StatusCode, String)> {
    let accounts = email_accounts::list_email_accounts_for_user(pool, user_id, true)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(accounts.into_iter().map(|a| a.email).collect())
}

async fn verify_mailbox_access(
    pool: &SqlitePool,
    user_id: &str,
    mailbox: &str,
) -> Result<(), (StatusCode, String)> {
    let is_admin = ticketing_system::system_logs::is_admin(pool, user_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if is_admin {
        return Ok(());
    }

    let has_access = email_accounts::user_has_email_access(pool, mailbox, user_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if has_access {
        Ok(())
    } else {
        Err((
            StatusCode::FORBIDDEN,
            format!("No access to mailbox: {mailbox}"),
        ))
    }
}

#[derive(Debug, Deserialize)]
pub struct IntakeListQuery {
    pub mailbox: Option<String>,
    pub status: Option<String>,
    pub item_type: Option<String>,
    pub risk_level: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct RunEmailIntakeRequest {
    pub email_id: Option<i64>,
    #[serde(default)]
    pub email_ids: Vec<i64>,
    pub mailbox: Option<String>,
    pub folder: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct RunEmailIntakeResponse {
    pub processed: usize,
    pub results: Vec<email_intake::EmailIntakeResult>,
}

#[derive(Debug, Deserialize)]
pub struct CreateExpectedResponseApiRequest {
    pub sent_email_id: Option<i64>,
    pub context_id: Option<String>,
    pub thread_id: Option<String>,
    pub mailbox: Option<String>,
    pub correspondent_email: Option<String>,
    pub subject: Option<String>,
    pub due_at: Option<i64>,
    pub due_in_days: Option<i64>,
    pub notes: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateContextApiRequest {
    pub title: String,
    pub context_type: Option<String>,
    pub organization: Option<String>,
    pub primary_mailbox: Option<String>,
    pub summary: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LinkThreadApiRequest {
    pub thread_id: String,
    pub mailbox: Option<String>,
    pub confidence: Option<f64>,
    pub link_reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct EmailNotificationIntentResponse {
    pub id: i64,
    pub email_id: i64,
    pub mailbox: String,
    pub thread_id: Option<String>,
    pub intent: String,
    pub reason: String,
    pub payload: Value,
    pub status: String,
    pub created_by: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, sqlx::FromRow)]
struct EmailNotificationIntentRow {
    id: i64,
    email_id: i64,
    mailbox: String,
    thread_id: Option<String>,
    intent: String,
    reason: String,
    payload_json: String,
    status: String,
    created_by: String,
    created_at: i64,
    updated_at: i64,
}

impl EmailNotificationIntentRow {
    fn into_response(self) -> Result<EmailNotificationIntentResponse, (StatusCode, String)> {
        let payload = serde_json::from_str(&self.payload_json).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Invalid notification intent payload: {e}"),
            )
        })?;
        Ok(EmailNotificationIntentResponse {
            id: self.id,
            email_id: self.email_id,
            mailbox: self.mailbox,
            thread_id: self.thread_id,
            intent: self.intent,
            reason: self.reason,
            payload,
            status: self.status,
            created_by: self.created_by,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

pub async fn list_email_attention_items(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Query(params): Query<IntakeListQuery>,
) -> Result<Json<Vec<email_intake::EmailAttentionItem>>, (StatusCode, String)> {
    let mailboxes = scoped_mailboxes(&pool, &user.user_id, params.mailbox.as_deref()).await?;
    let mut items = Vec::new();
    for mailbox in mailboxes {
        items.extend(
            email_intake::list_attention_items(
                &pool,
                Some(&mailbox),
                params.status.as_deref().or(Some("open")),
                params.item_type.as_deref(),
                params.limit.unwrap_or(100),
            )
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
        );
    }
    Ok(Json(items))
}

pub async fn list_email_notification_intents(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Query(params): Query<IntakeListQuery>,
) -> Result<Json<Vec<EmailNotificationIntentResponse>>, (StatusCode, String)> {
    email_classifier::ensure_email_classifier_schema(&pool)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mailboxes = scoped_mailboxes(&pool, &user.user_id, params.mailbox.as_deref()).await?;
    let mut intents = Vec::new();
    for mailbox in mailboxes {
        let rows = sqlx::query_as::<_, EmailNotificationIntentRow>(
            r#"
            SELECT id, email_id, mailbox, thread_id, intent, reason, payload_json,
                   status, created_by, created_at, updated_at
            FROM email_notification_intents
            WHERE mailbox = ?
              AND (? IS NULL OR status = ?)
            ORDER BY updated_at DESC, id DESC
            LIMIT ?
            "#,
        )
        .bind(&mailbox)
        .bind(params.status.as_deref())
        .bind(params.status.as_deref())
        .bind(params.limit.unwrap_or(100).clamp(1, 500))
        .fetch_all(&*pool)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

        for row in rows {
            intents.push(row.into_response()?);
        }
    }
    Ok(Json(intents))
}

pub async fn resolve_email_attention_item(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> Result<Json<email_intake::EmailAttentionItem>, (StatusCode, String)> {
    let item = sqlx::query_as::<_, email_intake::EmailAttentionItem>(
        r#"
        SELECT id, email_id, context_id, expected_response_id, mailbox, item_type,
               priority, status, title, detail, risk_level, created_by, created_at,
               updated_at, resolved_at
        FROM email_attention_items
        WHERE id = ?
        "#,
    )
    .bind(id)
    .fetch_one(&*pool)
    .await
    .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    verify_mailbox_access(&pool, &user.user_id, &item.mailbox).await?;

    let updated = email_intake::resolve_attention_item(&pool, id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(updated))
}

pub async fn list_email_security_scans(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Query(params): Query<IntakeListQuery>,
) -> Result<Json<Vec<email_intake::EmailSecurityScan>>, (StatusCode, String)> {
    let mailboxes = scoped_mailboxes(&pool, &user.user_id, params.mailbox.as_deref()).await?;
    let mut scans = Vec::new();
    for mailbox in mailboxes {
        scans.extend(
            email_intake::list_security_scans(
                &pool,
                Some(&mailbox),
                params.risk_level.as_deref(),
                params.limit.unwrap_or(100),
            )
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
        );
    }
    Ok(Json(scans))
}

pub async fn list_email_contexts(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Query(params): Query<IntakeListQuery>,
) -> Result<Json<Vec<email_intake::EmailContext>>, (StatusCode, String)> {
    let mailboxes = scoped_mailboxes(&pool, &user.user_id, params.mailbox.as_deref()).await?;
    let mut contexts = Vec::new();
    for mailbox in mailboxes {
        contexts.extend(
            email_intake::list_email_contexts(
                &pool,
                Some(&mailbox),
                params.status.as_deref(),
                params.limit.unwrap_or(100),
            )
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
        );
    }
    contexts.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    contexts.dedup_by(|a, b| a.context_id == b.context_id);
    Ok(Json(contexts))
}

pub async fn get_email_context(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(context_id): Path<String>,
) -> Result<Json<email_intake::EmailContextDetails>, (StatusCode, String)> {
    let details = email_intake::get_email_context_details(&pool, &context_id)
        .await
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    if let Some(mailbox) = &details.context.primary_mailbox {
        verify_mailbox_access(&pool, &user.user_id, mailbox).await?;
    }
    Ok(Json(details))
}

pub async fn create_email_context(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(req): Json<CreateContextApiRequest>,
) -> Result<Json<email_intake::EmailContext>, (StatusCode, String)> {
    if let Some(mailbox) = &req.primary_mailbox {
        verify_mailbox_access(&pool, &user.user_id, mailbox).await?;
    }
    let context = email_intake::create_email_context(
        &pool,
        &email_intake::CreateEmailContextRequest {
            title: req.title,
            context_type: req.context_type,
            organization: req.organization,
            primary_mailbox: req.primary_mailbox,
            summary: req.summary,
            created_by: Some(user.user_id),
        },
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(context))
}

pub async fn link_email_thread_to_context(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(context_id): Path<String>,
    Json(req): Json<LinkThreadApiRequest>,
) -> Result<Json<email_intake::EmailContextThread>, (StatusCode, String)> {
    if let Some(mailbox) = &req.mailbox {
        verify_mailbox_access(&pool, &user.user_id, mailbox).await?;
    }
    let link = email_intake::link_thread_to_context(
        &pool,
        &email_intake::LinkEmailThreadContextRequest {
            context_id,
            thread_id: req.thread_id,
            mailbox: req.mailbox,
            confidence: req.confidence,
            link_reason: req.link_reason,
            created_by: Some(user.user_id),
        },
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(link))
}

pub async fn list_expected_email_responses(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Query(params): Query<IntakeListQuery>,
) -> Result<Json<Vec<email_intake::EmailExpectedResponse>>, (StatusCode, String)> {
    let mailboxes = scoped_mailboxes(&pool, &user.user_id, params.mailbox.as_deref()).await?;
    let mut responses = Vec::new();
    for mailbox in mailboxes {
        responses.extend(
            email_intake::list_expected_responses(
                &pool,
                Some(&mailbox),
                params.status.as_deref(),
                params.limit.unwrap_or(100),
            )
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
        );
    }
    Ok(Json(responses))
}

pub async fn create_expected_email_response(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(req): Json<CreateExpectedResponseApiRequest>,
) -> Result<Json<email_intake::EmailExpectedResponse>, (StatusCode, String)> {
    if let Some(email_id) = req.sent_email_id {
        let email = emails::get_email_by_id(&pool, email_id)
            .await
            .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
        verify_mailbox_access(&pool, &user.user_id, &email.mailbox).await?;
    }
    if let Some(mailbox) = &req.mailbox {
        verify_mailbox_access(&pool, &user.user_id, mailbox).await?;
    }
    let due_at = expected_due_at(req.due_at, req.due_in_days)?;
    let response = email_intake::create_expected_response(
        &pool,
        &email_intake::CreateExpectedResponseRequest {
            sent_email_id: req.sent_email_id,
            context_id: req.context_id,
            thread_id: req.thread_id,
            mailbox: req.mailbox,
            correspondent_email: req.correspondent_email,
            subject: req.subject,
            due_at,
            notes: req.notes,
            created_by: Some(user.user_id),
        },
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(response))
}

pub async fn refresh_expected_email_responses(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<Json<Vec<email_intake::EmailAttentionItem>>, (StatusCode, String)> {
    let items = email_intake::refresh_expected_responses(&pool, &user.user_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let accessible = get_user_mailboxes(&pool, &user.user_id).await?;
    Ok(Json(
        items
            .into_iter()
            .filter(|item| accessible.contains(&item.mailbox))
            .collect(),
    ))
}

pub async fn run_email_intake(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(req): Json<RunEmailIntakeRequest>,
) -> Result<Json<RunEmailIntakeResponse>, (StatusCode, String)> {
    let mut ids = req.email_ids;
    if let Some(id) = req.email_id {
        ids.push(id);
    }

    let mut results = Vec::new();
    if ids.is_empty() {
        if let Some(mailbox) = &req.mailbox {
            verify_mailbox_access(&pool, &user.user_id, mailbox).await?;
        }
        let mailboxes = scoped_mailboxes(&pool, &user.user_id, req.mailbox.as_deref()).await?;
        for mailbox in mailboxes {
            results.extend(
                process_recent_emails(
                    &pool,
                    Some(&mailbox),
                    req.folder.as_deref(),
                    req.limit.unwrap_or(100),
                    &user.user_id,
                )
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
            );
        }
    } else {
        for id in ids {
            let email = emails::get_email_by_id(&pool, id)
                .await
                .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
            verify_mailbox_access(&pool, &user.user_id, &email.mailbox).await?;
            let result = email_intake::process_email_intake(&pool, id, &user.user_id)
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            if let Err(e) =
                email_classifier::enqueue_classifier_after_intake(&pool, &result, &user.user_id)
                    .await
            {
                tracing::warn!(
                    email_id = id,
                    error = ?e,
                    "Failed to enqueue email classifier after manual intake"
                );
            }
            results.push(result);
        }
    }

    Ok(Json(RunEmailIntakeResponse {
        processed: results.len(),
        results,
    }))
}

async fn process_recent_emails(
    pool: &SqlitePool,
    mailbox: Option<&str>,
    folder: Option<&str>,
    limit: i64,
    created_by: &str,
) -> anyhow::Result<Vec<email_intake::EmailIntakeResult>> {
    let limit = limit.clamp(1, 500);
    let folder = folder.unwrap_or("INBOX").trim();
    let emails = if let Some(mailbox) = mailbox {
        if is_all_email_folder_scope(folder) {
            emails::list_emails_for_mailbox_all_folders(pool, mailbox, limit, 0).await?
        } else {
            emails::list_emails(pool, mailbox, Some(folder), limit, 0).await?
        }
    } else if is_all_email_folder_scope(folder) {
        emails::list_all_emails(pool, limit, 0).await?
    } else {
        emails::list_emails_by_folder(pool, folder, limit, 0).await?
    };

    let mut results = Vec::new();
    for email in emails {
        let result = email_intake::process_email_intake(pool, email.id, created_by).await?;
        if let Err(e) =
            email_classifier::enqueue_classifier_after_intake(pool, &result, created_by).await
        {
            tracing::warn!(
                email_id = email.id,
                error = ?e,
                "Failed to enqueue email classifier after batch intake"
            );
        }
        results.push(result);
    }
    Ok(results)
}

fn is_all_email_folder_scope(folder: &str) -> bool {
    matches!(
        folder.trim().to_ascii_lowercase().as_str(),
        "*" | "all" | "__all__"
    )
}

async fn scoped_mailboxes(
    pool: &SqlitePool,
    user_id: &str,
    requested: Option<&str>,
) -> Result<Vec<String>, (StatusCode, String)> {
    if let Some(mailbox) = requested {
        verify_mailbox_access(pool, user_id, mailbox).await?;
        return Ok(vec![mailbox.to_string()]);
    }
    get_user_mailboxes(pool, user_id).await
}

fn expected_due_at(
    due_at: Option<i64>,
    due_in_days: Option<i64>,
) -> Result<i64, (StatusCode, String)> {
    let now = chrono::Utc::now().timestamp();
    if let Some(due_at) = due_at {
        if due_at <= now {
            return Err((
                StatusCode::BAD_REQUEST,
                "due_at must be in the future".to_string(),
            ));
        }
        return Ok(due_at);
    }
    if let Some(days) = due_in_days {
        if days <= 0 {
            return Err((
                StatusCode::BAD_REQUEST,
                "due_in_days must be positive".to_string(),
            ));
        }
        return Ok(now + days * 24 * 60 * 60);
    }
    Err((
        StatusCode::BAD_REQUEST,
        "due_at or due_in_days is required".to_string(),
    ))
}
