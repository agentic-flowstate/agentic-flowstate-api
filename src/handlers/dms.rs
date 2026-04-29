use axum::{
    body::Body,
    extract::{Extension, Multipart, Path, Query, State},
    http::StatusCode,
    response::sse::{Event, KeepAlive, Sse},
    Json,
};
use futures::stream::Stream;
use serde::Deserialize;
use serde_json::json;
use std::collections::hash_map::DefaultHasher;
use std::convert::Infallible;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;
use ticketing_system::SqlitePool;

use crate::auth_middleware::AuthenticatedUser;

// ============================================================================
// Request/Query types
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct CreateDmBody {
    pub recipient_id: String,
}

#[derive(Debug, Deserialize)]
pub struct SendMessageBody {
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct ListMessagesQuery {
    pub limit: Option<i64>,
    pub before: Option<i64>,
}

// ============================================================================
// Handlers
// ============================================================================

/// GET /api/dms — list current user's DM conversations
pub async fn list_dms(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let dms = ticketing_system::dms::list_user_dms(&pool, &user.user_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "dms": dms })))
}

/// POST /api/dms — create or get DM conversation with recipient
pub async fn create_dm(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(body): Json<CreateDmBody>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, String)> {
    if body.recipient_id == user.user_id {
        return Err((StatusCode::BAD_REQUEST, "Cannot DM yourself".to_string()));
    }

    // Verify recipient exists
    let recipient = ticketing_system::users::get_user(&pool, &body.recipient_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if recipient.is_none() {
        return Err((
            StatusCode::NOT_FOUND,
            format!("User '{}' not found", body.recipient_id),
        ));
    }

    let dm = ticketing_system::dms::create_or_get_dm(&pool, &user.user_id, &body.recipient_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok((StatusCode::CREATED, Json(serde_json::to_value(dm).unwrap())))
}

/// GET /api/dms/:dm_id/messages — list messages in DM
pub async fn get_dm_messages(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(dm_id): Path<String>,
    Query(params): Query<ListMessagesQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    // Verify participant
    if !ticketing_system::dms::is_participant(&pool, &dm_id, &user.user_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    {
        return Err((
            StatusCode::FORBIDDEN,
            "Not a participant in this DM".to_string(),
        ));
    }

    let limit = params.limit.unwrap_or(50);
    let messages = ticketing_system::dms::list_messages(&pool, &dm_id, limit, params.before)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "messages": messages })))
}

/// POST /api/dms/:dm_id/messages — send a message (supports multipart for file attachments)
pub async fn send_dm_message(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(dm_id): Path<String>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, String)> {
    // Verify participant
    if !ticketing_system::dms::is_participant(&pool, &dm_id, &user.user_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    {
        return Err((
            StatusCode::FORBIDDEN,
            "Not a participant in this DM".to_string(),
        ));
    }

    let mut content = String::new();
    let mut files: Vec<(String, String, Vec<u8>)> = Vec::new(); // (filename, content_type, bytes)

    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "content" => {
                content = field.text().await.unwrap_or_default();
            }
            "files" => {
                let filename = field.file_name().unwrap_or("file").to_string();
                let content_type = field
                    .content_type()
                    .unwrap_or("application/octet-stream")
                    .to_string();
                let bytes = field.bytes().await.map_err(|e| {
                    (
                        StatusCode::BAD_REQUEST,
                        format!("Failed to read file: {}", e),
                    )
                })?;
                files.push((filename, content_type, bytes.to_vec()));
            }
            _ => {}
        }
    }

    if content.trim().is_empty() && files.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Message must have content or attachments".to_string(),
        ));
    }

    // If content is empty but files exist, use a placeholder
    if content.trim().is_empty() {
        content = String::new();
    }

    // Create the message
    let msg = ticketing_system::dms::send_message(&pool, &dm_id, &user.user_id, &content)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Save attachments
    let mut attachments = Vec::new();
    if !files.is_empty() {
        let base_dir = dirs::home_dir()
            .unwrap_or_default()
            .join(".agentic-flowstate")
            .join("dm_attachments")
            .join(&dm_id);
        tokio::fs::create_dir_all(&base_dir).await.map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to create attachment dir: {}", e),
            )
        })?;

        for (filename, content_type, bytes) in files {
            let att_id = format!("att-{}", uuid::Uuid::new_v4());
            // Sanitize filename: remove path separators, null bytes, CRLF, quotes
            let safe_filename: String = filename
                .chars()
                .filter(|c| {
                    *c != '/' && *c != '\\' && *c != '\0' && *c != '\r' && *c != '\n' && *c != '"'
                })
                .take(255)
                .collect();
            let safe_filename = if safe_filename.is_empty() {
                "file".to_string()
            } else {
                safe_filename
            };
            let stored_name = format!("{}_{}", att_id, safe_filename);
            let stored_path = base_dir.join(&stored_name);

            tokio::fs::write(&stored_path, &bytes).await.map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to write attachment: {}", e),
                )
            })?;

            let att = ticketing_system::dms::save_attachment(
                &pool,
                &msg.id,
                &safe_filename,
                &content_type,
                bytes.len() as i64,
                stored_path.to_str().unwrap_or(""),
            )
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

            attachments.push(att);
        }
    }

    // Build response with attachments
    let mut msg_value = serde_json::to_value(&msg).unwrap();
    if !attachments.is_empty() {
        msg_value["attachments"] = serde_json::to_value(&attachments).unwrap();
    }

    Ok((StatusCode::CREATED, Json(msg_value)))
}

/// POST /api/dms/:dm_id/messages/json — send a text-only message (JSON body, no files)
pub async fn send_dm_message_json(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(dm_id): Path<String>,
    Json(body): Json<SendMessageBody>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, String)> {
    if body.content.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Message content cannot be empty".to_string(),
        ));
    }

    // Verify participant
    if !ticketing_system::dms::is_participant(&pool, &dm_id, &user.user_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    {
        return Err((
            StatusCode::FORBIDDEN,
            "Not a participant in this DM".to_string(),
        ));
    }

    let msg = ticketing_system::dms::send_message(&pool, &dm_id, &user.user_id, &body.content)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok((
        StatusCode::CREATED,
        Json(serde_json::to_value(msg).unwrap()),
    ))
}

/// GET /api/dms/:dm_id/attachments/:attachment_id — download an attachment
pub async fn download_dm_attachment(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path((dm_id, attachment_id)): Path<(String, String)>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    // Verify participant
    if !ticketing_system::dms::is_participant(&pool, &dm_id, &user.user_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    {
        return Err((
            StatusCode::FORBIDDEN,
            "Not a participant in this DM".to_string(),
        ));
    }

    let (att, stored_path) = ticketing_system::dms::get_attachment(&pool, &attachment_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Attachment not found".to_string()))?;

    // Verify attachment belongs to this DM (not just any DM the user is in)
    let att_dm_id = ticketing_system::dms::get_dm_id_for_message(&pool, &att.message_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                "Attachment message not found".to_string(),
            )
        })?;
    if att_dm_id != dm_id {
        return Err((
            StatusCode::FORBIDDEN,
            "Attachment does not belong to this DM".to_string(),
        ));
    }

    let file_bytes = tokio::fs::read(&stored_path).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to read file: {}", e),
        )
    })?;

    // Sanitize filename for Content-Disposition header (prevent CRLF injection)
    let safe_name: String = att
        .filename
        .chars()
        .filter(|c| *c != '\r' && *c != '\n' && *c != '"')
        .collect();

    let response = axum::response::Response::builder()
        .header("Content-Type", &att.content_type)
        .header(
            "Content-Disposition",
            format!("inline; filename=\"{}\"", safe_name),
        )
        .header("Content-Length", file_bytes.len().to_string())
        .body(Body::from(file_bytes))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(response)
}

/// POST /api/dms/:dm_id/read — mark DM as read
pub async fn mark_dm_read(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(dm_id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    if !ticketing_system::dms::is_participant(&pool, &dm_id, &user.user_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    {
        return Err((
            StatusCode::FORBIDDEN,
            "Not a participant in this DM".to_string(),
        ));
    }

    ticketing_system::dms::mark_read(&pool, &dm_id, &user.user_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

/// GET /api/dms/contacts — list all users (for recipient picker)
pub async fn list_contacts(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let all_users = ticketing_system::users::list_users(&pool)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Filter out current user, return just id + name
    let contacts: Vec<serde_json::Value> = all_users
        .into_iter()
        .filter(|u| u.user_id != user.user_id)
        .map(|u| json!({ "user_id": u.user_id, "name": u.name }))
        .collect();

    Ok(Json(json!({ "contacts": contacts })))
}

// ============================================================================
// SSE Subscribe
// ============================================================================

/// GET /api/dms/subscribe — SSE for real-time DM updates
pub async fn subscribe_dms(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let user_id = user.user_id.clone();

    let stream = async_stream::stream! {
        let mut last_hash: u64 = 0;

        loop {
            if let Ok(dms) = ticketing_system::dms::list_user_dms(&pool, &user_id).await {
                let hash = hash_dms(&dms);
                if hash != last_hash {
                    last_hash = hash;
                    let event = json!({ "type": "dms", "dms": dms });
                    if let Ok(json) = serde_json::to_string(&event) {
                        yield Ok(Event::default().data(json));
                    }
                }
            }

            // Was 2s — reduced to save battery/radio on mobile
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    };

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(30))
            .text("ping"),
    )
}

fn hash_dms(dms: &[ticketing_system::DmConversation]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for dm in dms {
        dm.id.hash(&mut hasher);
        dm.updated_at.hash(&mut hasher);
        dm.unread_count.hash(&mut hasher);
        if let Some(ref msg) = dm.last_message {
            msg.hash(&mut hasher);
        }
    }
    dms.len().hash(&mut hasher);
    hasher.finish()
}
