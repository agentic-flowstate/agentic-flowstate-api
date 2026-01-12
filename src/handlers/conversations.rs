use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::sse::{Event, KeepAlive, Sse},
    Json,
};
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use ticketing_system::{
    conversations, AddMessageRequest, Conversation, ConversationMessage,
    CreateConversationRequest, SqlitePool, UpdateConversationRequest,
};

#[derive(Debug, Deserialize)]
pub struct ListConversationsQuery {
    pub organization: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ConversationListResponse {
    pub conversations: Vec<Conversation>,
    pub total: i64,
}

/// List conversations (GET /api/conversations)
pub async fn list_conversations(
    State(pool): State<Arc<SqlitePool>>,
    Query(params): Query<ListConversationsQuery>,
) -> Result<Json<ConversationListResponse>, (StatusCode, String)> {
    let list = conversations::list_conversations(&pool, params.organization.as_deref())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let total = list.len() as i64;

    Ok(Json(ConversationListResponse {
        conversations: list,
        total,
    }))
}

/// Get single conversation by ID (GET /api/conversations/:id)
pub async fn get_conversation(
    State(pool): State<Arc<SqlitePool>>,
    Path(id): Path<String>,
) -> Result<Json<Conversation>, (StatusCode, String)> {
    let conv = conversations::get_conversation(&pool, &id, true)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Conversation not found".to_string()))?;

    Ok(Json(conv))
}

/// Create a conversation (POST /api/conversations)
pub async fn create_conversation(
    State(pool): State<Arc<SqlitePool>>,
    Json(req): Json<CreateConversationRequest>,
) -> Result<(StatusCode, Json<Conversation>), (StatusCode, String)> {
    let conv = conversations::create_conversation(&pool, req)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok((StatusCode::CREATED, Json(conv)))
}

/// Update a conversation (PATCH /api/conversations/:id)
pub async fn update_conversation(
    State(pool): State<Arc<SqlitePool>>,
    Path(id): Path<String>,
    Json(req): Json<UpdateConversationRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
    conversations::update_conversation(&pool, &id, req)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Delete a conversation (DELETE /api/conversations/:id)
pub async fn delete_conversation(
    State(pool): State<Arc<SqlitePool>>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    conversations::delete_conversation(&pool, &id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Add a message to a conversation (POST /api/conversations/:id/messages)
pub async fn add_message(
    State(pool): State<Arc<SqlitePool>>,
    Path(id): Path<String>,
    Json(req): Json<AddMessageRequest>,
) -> Result<(StatusCode, Json<ConversationMessage>), (StatusCode, String)> {
    // Verify conversation exists
    let _ = conversations::get_conversation(&pool, &id, false)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Conversation not found".to_string()))?;

    let msg = conversations::add_message(&pool, &id, req)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok((StatusCode::CREATED, Json(msg)))
}

#[derive(Debug, Deserialize)]
pub struct UpdateMessageRequest {
    pub content: String,
    pub tool_uses: Option<Vec<ticketing_system::ToolUse>>,
}

/// Update a message (PATCH /api/conversations/:id/messages/:message_id)
pub async fn update_message(
    State(pool): State<Arc<SqlitePool>>,
    Path((conv_id, message_id)): Path<(String, String)>,
    Json(req): Json<UpdateMessageRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
    // Verify conversation exists
    let _ = conversations::get_conversation(&pool, &conv_id, false)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Conversation not found".to_string()))?;

    conversations::update_message(&pool, &message_id, &req.content, req.tool_uses.as_deref())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

/// List messages for a conversation (GET /api/conversations/:id/messages)
pub async fn list_messages(
    State(pool): State<Arc<SqlitePool>>,
    Path(id): Path<String>,
) -> Result<Json<Vec<ConversationMessage>>, (StatusCode, String)> {
    // Verify conversation exists
    let _ = conversations::get_conversation(&pool, &id, false)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Conversation not found".to_string()))?;

    let messages = conversations::list_messages(&pool, &id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(messages))
}

/// SSE event types for conversation updates
#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum ConversationStreamEvent {
    /// Full list of conversations (sent on connect and when changes detected)
    #[serde(rename = "sync")]
    Sync {
        conversations: Vec<Conversation>,
        updated_at: i64,
    },
}

/// GET /api/conversations/subscribe
/// SSE endpoint for real-time conversation list updates
pub async fn subscribe_conversations(
    State(pool): State<Arc<SqlitePool>>,
    Query(params): Query<ListConversationsQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = async_stream::stream! {
        // Track the last update time we've seen
        let mut last_sync_hash: u64 = 0;

        loop {
            // Get current conversations
            match conversations::list_conversations(&pool, params.organization.as_deref()).await {
                Ok(convs) => {
                    // Simple change detection: hash the updated_at timestamps
                    use std::hash::{Hash, Hasher};
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    for conv in &convs {
                        conv.updated_at.hash(&mut hasher);
                        conv.id.hash(&mut hasher);
                    }
                    convs.len().hash(&mut hasher);
                    let current_hash = hasher.finish();

                    // Only send if changed
                    if current_hash != last_sync_hash {
                        last_sync_hash = current_hash;
                        let event = ConversationStreamEvent::Sync {
                            conversations: convs,
                            updated_at: chrono::Utc::now().timestamp(),
                        };
                        if let Ok(json) = serde_json::to_string(&event) {
                            yield Ok(Event::default().data(json));
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to list conversations for SSE: {}", e);
                }
            }

            // Poll every 2 seconds
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    };

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping")
    )
}
