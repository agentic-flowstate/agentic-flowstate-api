use axum::{
    extract::{Extension, Path, Query, State},
    http::{header, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    Json,
};
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use ticketing_system::{
    checkpoints, conversations, AddMessageRequest, Conversation, ConversationMessage,
    CreateConversationRequest, SqlitePool, UpdateConversationRequest,
};

use super::chat_client_manager::ChatClientManager;
use super::chat_stream::get_broadcast_sender;
use super::conversation_worker_manager::WORKER_MANAGER;
use crate::auth_middleware::AuthenticatedUser;
use crate::observability::streaming::{
    record_cursor_expired, record_stream_closed, record_stream_opened, DisconnectReason,
};

#[derive(Debug, Deserialize)]
pub struct ListConversationsQuery {
    pub organization: Option<String>,
    pub agent: Option<String>,
    /// Comma-separated status filter (e.g., "open,waiting"). Default: "open,waiting"
    pub status: Option<String>,
    pub limit: Option<i64>,
    pub updated_since: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ConversationListResponse {
    pub conversations: Vec<Conversation>,
    pub total: i64,
}

/// List conversations (GET /api/conversations)
pub async fn list_conversations(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Query(params): Query<ListConversationsQuery>,
) -> Result<Json<ConversationListResponse>, (StatusCode, String)> {
    let list = conversations::list_conversations(
        &pool,
        params.organization.as_deref(),
        Some(&user.user_id),
        params.agent.as_deref(),
        params.status.as_deref(),
        params.limit,
        params.updated_since.as_deref(),
    )
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
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
) -> Result<Json<Conversation>, (StatusCode, String)> {
    let conv = conversations::get_conversation(&pool, &id, true)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Conversation not found".to_string()))?;

    if conv.user_id != user.user_id {
        return Err((StatusCode::NOT_FOUND, "Conversation not found".to_string()));
    }

    Ok(Json(conv))
}

/// Create a conversation (POST /api/conversations)
pub async fn create_conversation(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(mut req): Json<CreateConversationRequest>,
) -> Result<(StatusCode, Json<Conversation>), (StatusCode, String)> {
    req.user_id = user.user_id;
    let conv = conversations::create_conversation(&pool, req)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok((StatusCode::CREATED, Json(conv)))
}

/// Update a conversation (PATCH /api/conversations/:id)
pub async fn update_conversation(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
    Json(req): Json<UpdateConversationRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
    conversations::update_conversation(&pool, &user.user_id, &id, req)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Set conversation to waiting (POST /api/conversations/:id/wait)
pub async fn wait_conversation(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    conversations::wait_conversation(&pool, &user.user_id, &id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Activate a conversation — move from waiting/archived back to open (POST /api/conversations/:id/activate)
pub async fn activate_conversation(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    conversations::activate_conversation(&pool, &user.user_id, &id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Archive a conversation (DELETE /api/conversations/:id)
/// Sets status='archived' and archived_at timestamp. Conversation data is preserved.
pub async fn delete_conversation(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    conversations::archive_conversation(&pool, &user.user_id, &id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Cancel a running conversation's agent (POST /api/conversations/:id/cancel)
pub async fn cancel_conversation(
    State(pool): State<Arc<SqlitePool>>,
    State(manager): State<Arc<ChatClientManager>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    // Verify conversation belongs to user
    let conv = conversations::get_conversation(&pool, &id, false)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Conversation not found".to_string()))?;
    if conv.user_id != user.user_id {
        return Err((StatusCode::NOT_FOUND, "Conversation not found".to_string()));
    }

    manager.mark_cancelled_turn(&id).await;
    let worker_exists = WORKER_MANAGER.has_worker(&id).await;

    // Try to interrupt the running agent
    match manager.interrupt(&id).await {
        Ok(true) => {
            tracing::info!("[CANCEL] Interrupted agent for conversation {}", id);
            // Broadcast cancelled status so SSE clients get notified
            let event = crate::agents::StreamEvent::Status {
                status: "cancelled".to_string(),
                message: Some("Cancelled by user".to_string()),
            };
            if let Ok(json) = serde_json::to_string(&event) {
                let broadcast_tx = get_broadcast_sender(&id).await;
                let _ = broadcast_tx.send((-1, json));
            }
        }
        Ok(false) => {
            if worker_exists {
                tracing::info!(
                    "[CANCEL] Marked queued turn cancelled for conversation {}",
                    id
                );
            } else {
                tracing::info!(
                    "[CANCEL] No active or queued turn for conversation {}, nothing to cancel",
                    id
                );
                let _ = manager.consume_cancelled_turn(&id).await;
            }
        }
        Err(e) => {
            tracing::warn!("[CANCEL] Interrupt failed for {}: {}", id, e);
            // Don't fail the request — the agent might have already finished
        }
    }

    Ok(StatusCode::OK)
}

/// Add a message to a conversation (POST /api/conversations/:id/messages)
pub async fn add_message(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
    Json(req): Json<AddMessageRequest>,
) -> Result<(StatusCode, Json<ConversationMessage>), (StatusCode, String)> {
    // Verify conversation exists and belongs to user
    let conv = conversations::get_conversation(&pool, &id, false)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Conversation not found".to_string()))?;
    if conv.user_id != user.user_id {
        return Err((StatusCode::NOT_FOUND, "Conversation not found".to_string()));
    }

    let msg = conversations::add_message(&pool, &id, req)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok((StatusCode::CREATED, Json(msg)))
}

#[derive(Debug, Deserialize)]
pub struct UpdateMessageRequest {
    pub content: String,
}

/// Update a message (PATCH /api/conversations/:id/messages/:message_id)
pub async fn update_message(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path((conv_id, message_id)): Path<(String, String)>,
    Json(req): Json<UpdateMessageRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
    // Verify conversation exists and belongs to user
    let conv = conversations::get_conversation(&pool, &conv_id, false)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Conversation not found".to_string()))?;
    if conv.user_id != user.user_id {
        return Err((StatusCode::NOT_FOUND, "Conversation not found".to_string()));
    }

    conversations::update_message(&pool, &message_id, &req.content)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Query params for `GET /api/conversations/:id/messages`. Clients that only
/// render a recent window pass a small `limit` to avoid downloading the full
/// conversation on cold start. Omitted `limit` = all messages.
///
/// `before` is a `message_index` cursor: when set, returns up to `limit`
/// messages strictly older than that index (chronological). Used by the
/// iOS app for pull-to-load-older pagination — pass the smallest known
/// `message_index` to load the previous page of older history.
#[derive(Debug, Deserialize)]
pub struct ListMessagesQuery {
    pub limit: Option<i64>,
    pub before: Option<i64>,
}

/// List messages for a conversation (GET /api/conversations/:id/messages)
pub async fn list_messages(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
    Query(params): Query<ListMessagesQuery>,
) -> Result<Json<Vec<ConversationMessage>>, (StatusCode, String)> {
    // Verify conversation exists and belongs to user
    let conv = conversations::get_conversation(&pool, &id, false)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Conversation not found".to_string()))?;
    if conv.user_id != user.user_id {
        return Err((StatusCode::NOT_FOUND, "Conversation not found".to_string()));
    }

    let messages = conversations::list_messages(&pool, &id, params.limit, params.before)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(messages))
}

/// Get checkpoint status for a conversation (GET /api/conversations/:id/checkpoint)
/// Returns whether an agent is actively processing this conversation.
///
/// On top of the basic `status/tool_call_count/updated_at` used by the
/// background-task path, this returns a richer snapshot for the iOS chat UI
/// to hydrate optimistic state on conversation re-entry:
///
///   * `last_event_index` — latest SSE event id persisted. Client uses this
///     as the `starting_after` cursor instead of whatever stale cursor it
///     had cached locally.
///   * `server_time` — wall-clock time on the server. Clients diff this
///     against `updated_at` to decide whether to show a "catching up…" pill
///     or discard the checkpoint as stale.
///   * `recent_events` — every event since the most recent terminator
///     (result / status=completed / cancelled / failed / timeout), minus
///     heartbeats. Capped at 200. Feeds through the same parseEvent pipeline
///     the SSE stream uses, so the UI can show the current tool card and
///     partial text BEFORE the SSE handshake completes. See
///     `conversations::get_active_run_events` for the selection logic.
pub async fn get_conversation_checkpoint(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
) -> Result<Json<ConversationCheckpointResponse>, (StatusCode, String)> {
    // Verify conversation belongs to user
    let conv = conversations::get_conversation(&pool, &id, false)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Conversation not found".to_string()))?;
    if conv.user_id != user.user_id {
        return Err((StatusCode::NOT_FOUND, "Conversation not found".to_string()));
    }

    let checkpoint = ticketing_system::checkpoints::get_checkpoint(&pool, &id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let last_event_index = conversations::get_max_event_index(&pool, &id)
        .await
        .unwrap_or(-1);

    let server_time = chrono::Utc::now().timestamp();

    // Only bother shipping recent events when the agent is actually running
    // (or just finished) — no point paying the query cost for idle chats
    // where the client has all the history via /messages.
    let include_active = matches!(
        checkpoint.as_ref().map(|c| c.status.as_str()),
        Some("running") | Some("pending")
    );

    let recent_events: Vec<RecentEvent> = if include_active {
        let raw = conversations::get_active_run_events(&pool, &id, 200)
            .await
            .unwrap_or_default();
        // Materialize payloads via load_event_payload_str so blob-offloaded
        // events (T-E184E642) surface the real JSON to the client, not the
        // `{"$blob":...}` sentinel. Inline rows round-trip as a cheap clone.
        let mut out = Vec::with_capacity(raw.len());
        for e in raw {
            let event_data = match conversations::load_event_payload_str(&pool, &e).await {
                Ok(s) => s,
                Err(err) => {
                    tracing::error!(
                        "Failed to materialize payload for event {}/{}: {}",
                        e.conversation_id,
                        e.event_index,
                        err
                    );
                    continue;
                }
            };
            out.push(RecentEvent {
                event_index: e.event_index,
                event_type: e.event_type,
                event_data,
                created_at: e.created_at,
            });
        }
        out
    } else {
        Vec::new()
    };

    match checkpoint {
        Some(cp) => Ok(Json(ConversationCheckpointResponse {
            status: cp.status,
            tool_call_count: cp.tool_call_count,
            updated_at: cp.updated_at,
            last_event_index,
            server_time,
            recent_events,
        })),
        None => Ok(Json(ConversationCheckpointResponse {
            status: "none".to_string(),
            tool_call_count: 0,
            updated_at: 0,
            last_event_index,
            server_time,
            recent_events,
        })),
    }
}

#[derive(Debug, Serialize)]
pub struct ConversationCheckpointResponse {
    pub status: String,
    pub tool_call_count: i32,
    pub updated_at: i64,
    /// Max event_index persisted to conversation_events. -1 if none.
    /// Clients use this as the `starting_after` cursor on SSE reconnect.
    pub last_event_index: i32,
    /// Server wall-clock time at response generation, for client staleness checks.
    pub server_time: i64,
    /// Snapshot of events in the currently-running turn, ordered by event_index.
    /// Empty when the agent isn't running. Heartbeats are filtered.
    pub recent_events: Vec<RecentEvent>,
}

#[derive(Debug, Serialize)]
pub struct RecentEvent {
    pub event_index: i32,
    pub event_type: String,
    /// Raw JSON string (same shape the SSE `data:` field carries). Ship it
    /// as-is so the client can reuse its SSE parser with zero divergence.
    pub event_data: String,
    pub created_at: i64,
}

/// Query parameters for the reconnect SSE endpoint.
#[derive(Debug, Deserialize)]
pub struct ReconnectQuery {
    /// If provided, only replay events after this index (cursor-based resume).
    pub starting_after: Option<i32>,
}

/// GET /api/conversations/:id/stream?starting_after=N
/// SSE reconnection endpoint: replays stored events (optionally from cursor), then tails live events while agent is running.
pub async fn reconnect_conversation_stream(
    Path(id): Path<String>,
    Query(query): Query<ReconnectQuery>,
    State(db): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Sse<Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>> {
    // Verify the authenticated user owns this conversation
    let conv = conversations::get_conversation(&db, &id, false).await;
    match conv {
        Ok(Some(c)) if c.user_id == user.user_id => {}
        _ => {
            // Not found or not owned — return an empty stream that closes immediately
            let empty = futures::stream::empty();
            return Sse::new(
                Box::pin(empty) as Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>
            )
            .keep_alive(
                KeepAlive::new()
                    .interval(Duration::from_secs(30))
                    .text("ping"),
            );
        }
    }

    let checkpoint_status = match checkpoints::get_checkpoint(&db, &id).await {
        Ok(Some(cp)) => cp.status,
        _ => "none".to_string(),
    };

    // Resume vs cold-start: a reconnect with `starting_after=N` is the
    // resume arm of the stream-open metric; a reconnect without it is a
    // cold start (the client is re-subscribing from the top).
    let resume = query.starting_after.is_some();

    let events = if let Some(after) = query.starting_after {
        conversations::get_events_after(&db, &id, after)
            .await
            .unwrap_or_default()
    } else {
        conversations::get_events(&db, &id)
            .await
            .unwrap_or_default()
    };

    // Cursor-expired 410 detection: the client passed `starting_after=N`
    // but either (a) the allocator has already moved past N and no event
    // at index N+1 is retained in the log (first returned event has an
    // index well above N+1), or (b) the client's cursor is negative.
    // We fold this into the normal SSE response (no 410 frame on the
    // wire — iOS re-syncs via full message fetch) and only emit the
    // metric so operators can chart the rate. Feature retention work
    // (T-future) will convert this into an actual HTTP 410.
    let mut cursor_expired = false;
    if let Some(after) = query.starting_after {
        let oldest_retained = match conversations::get_events(&db, &id).await {
            Ok(ref all) if !all.is_empty() => Some(all[0].event_index),
            _ => None,
        };
        if let Some(oldest) = oldest_retained {
            if after < 0 || oldest > after + 1 {
                record_cursor_expired(&id, after, oldest);
                cursor_expired = true;
            }
        } else if after < 0 {
            // No events retained AND negative cursor — still expired.
            record_cursor_expired(&id, after, 0);
            cursor_expired = true;
        }
    }

    // Record the stream-opened metric BEFORE we hand the body off so the
    // gauge/counter reflect the connection immediately, not after the
    // first event flushes. The matching close is emitted by the
    // StreamCloseGuard wrapper below when the stream is dropped.
    record_stream_opened(&id, &user.user_id, resume);

    let inner = super::chat_stream::create_conversation_reconnect_stream(
        db,
        id.clone(),
        events,
        checkpoint_status,
    );

    // Box-pin the inner stream so the drop-guard wrapper has a
    // concrete `Unpin`-able handle. The underlying async_stream
    // `AsyncStream` is not itself `Unpin`, so we must pin on the heap.
    let inner_boxed: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> =
        Box::pin(inner);

    // Drop-guard adapter: wraps the inner stream, records close when the
    // stream terminates (natural end-of-stream OR client disconnect —
    // axum drops the stream on both paths). If the cursor was detected
    // as expired above, pre-seed the close reason so the close metric
    // fires with `reason="cursor_expired"` instead of the default
    // `client_disconnect`.
    let mut guarded = StreamCloseGuard::new(id, inner_boxed);
    if cursor_expired {
        guarded.reason = DisconnectReason::CursorExpired;
    }

    Sse::new(Box::pin(guarded) as Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(30))
                .text("ping"),
        )
}

/// RAII stream wrapper that records the matching `stream_closed_total`
/// counter + `stream_duration_ms` histogram observation when the stream
/// is dropped.
///
/// The close reason is inferred from whether the underlying stream ever
/// ran to completion (`Normal`) or was dropped mid-flight
/// (`ClientDisconnect`). More specific reasons — idle timeouts or
/// cursor-expired rejections — are emitted at the call site and
/// suppress the generic close recorded here via `set_reason`.
struct StreamCloseGuard {
    conversation_id: String,
    opened_at: std::time::Instant,
    reason: DisconnectReason,
    inner: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>,
    closed: bool,
}

impl StreamCloseGuard {
    fn new(
        conversation_id: String,
        inner: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>,
    ) -> Self {
        Self {
            conversation_id,
            opened_at: std::time::Instant::now(),
            reason: DisconnectReason::ClientDisconnect,
            inner,
            closed: false,
        }
    }
}

impl Stream for StreamCloseGuard {
    type Item = Result<Event, Infallible>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let poll = self.inner.as_mut().poll_next(cx);
        if matches!(poll, std::task::Poll::Ready(None)) {
            // Natural end-of-stream: server drained everything and the
            // generator yielded Poll::Ready(None). Mark the close as
            // normal so the drop handler emits `reason="normal"`.
            self.reason = DisconnectReason::Normal;
        }
        poll
    }
}

impl Drop for StreamCloseGuard {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        let duration_ms = self.opened_at.elapsed().as_millis().min(u64::MAX as u128) as u64;
        record_stream_closed(&self.conversation_id, duration_ms, self.reason);
    }
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
    Extension(user): Extension<AuthenticatedUser>,
    Query(params): Query<ListConversationsQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let user_id = user.user_id.clone();
    let stream = async_stream::stream! {
        // Track the last update time we've seen
        let mut last_sync_hash: u64 = 0;

        loop {
            // Get current conversations for this user
            match conversations::list_conversations(&pool, params.organization.as_deref(), Some(&user_id), params.agent.as_deref(), params.status.as_deref(), None, None).await {
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

            // Poll every 10 seconds (was 2s — reduced to save battery/radio on mobile)
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    };

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(30))
            .text("ping"),
    )
}

/// GET /api/chat-images/:conversation_id/:filename
/// Serve an image attachment from the chat-images directory.
pub async fn get_chat_image(
    Path((conversation_id, filename)): Path<(String, String)>,
) -> impl IntoResponse {
    let path = dirs::home_dir()
        .unwrap_or_default()
        .join(".agentic-flowstate/chat-images")
        .join(&conversation_id)
        .join(&filename);

    match tokio::fs::read(&path).await {
        Ok(data) => {
            let mime = if filename.ends_with(".png") {
                "image/png"
            } else if filename.ends_with(".gif") {
                "image/gif"
            } else if filename.ends_with(".webp") {
                "image/webp"
            } else if filename.ends_with(".heic") {
                "image/heic"
            } else {
                "image/jpeg"
            };
            ([(header::CONTENT_TYPE, mime)], data).into_response()
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}
