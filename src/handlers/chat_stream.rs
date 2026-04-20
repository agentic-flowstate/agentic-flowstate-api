use async_stream::stream;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::stream::Stream;
use once_cell::sync::Lazy;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use ticketing_system::conversations;
use tokio::sync::{broadcast, mpsc, RwLock};
use tokio_stream::wrappers::ReceiverStream;

use serde::Deserialize;

use super::chat_client_manager::ChatClientManager;
use super::conversation_worker::WorkerMessage;
use super::conversation_worker_manager::WORKER_MANAGER;
use crate::agents::{AgentType, StreamEvent};
use crate::observability::streaming::{record_stream_event_emitted, DisconnectReason};

/// Image data attached to a chat message (base64-encoded)
#[derive(Debug, Clone, Deserialize)]
pub struct ChatImageData {
    /// Base64-encoded image data
    pub data: String,
    /// MIME type (e.g., "image/jpeg")
    pub mime_type: String,
}

/// Global broadcaster for live conversation events.
/// Reconnect streams subscribe here instead of polling SQLite.
static CONVERSATION_BROADCASTER: Lazy<RwLock<HashMap<String, broadcast::Sender<(i32, String)>>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

/// Get or create a broadcast sender for a conversation.
pub async fn get_broadcast_sender(conversation_id: &str) -> broadcast::Sender<(i32, String)> {
    {
        let map = CONVERSATION_BROADCASTER.read().await;
        if let Some(tx) = map.get(conversation_id) {
            return tx.clone();
        }
    }
    let mut map = CONVERSATION_BROADCASTER.write().await;
    // Double-check after acquiring write lock
    if let Some(tx) = map.get(conversation_id) {
        return tx.clone();
    }
    let (tx, _) = broadcast::channel(256);
    map.insert(conversation_id.to_string(), tx.clone());
    tx
}

/// Remove a broadcast channel when conversation completes.
pub async fn remove_broadcast_channel(conversation_id: &str) {
    let mut map = CONVERSATION_BROADCASTER.write().await;
    map.remove(conversation_id);
}

pub type SseStream = Sse<Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>>;

/// Configuration for a chat SSE endpoint
#[derive(Clone)]
pub struct ChatConfig {
    pub agent_type: AgentType,
    pub prompt_name: &'static str,
    pub working_dir: PathBuf,
    pub prompt_vars: HashMap<String, String>,
}

/// Start or continue a chat session via SSE.
///
/// Pushes the message to a ConversationWorker (one per conversation) which
/// processes messages sequentially, eliminating the dual-consumer race.
/// Returns an SSE stream that subscribes to the worker's broadcast.
pub fn chat(
    db: Arc<SqlitePool>,
    manager: Arc<ChatClientManager>,
    message: String,
    conversation_id: Option<String>,
    config: ChatConfig,
    user_id: String,
    images: Option<Vec<ChatImageData>>,
) -> SseStream {
    let (tx, rx) = mpsc::channel::<(i32, String)>(100);

    tokio::spawn(async move {
        let conv_id = match conversation_id {
            Some(id) => id,
            None => {
                tracing::error!("[CHAT] No conversation_id provided");
                if let Ok(json) = serde_json::to_string(&StreamEvent::Status {
                    status: "failed".to_string(),
                    message: Some("No conversation_id".to_string()),
                }) {
                    let _ = tx.send((0, json)).await;
                }
                return;
            }
        };

        // Subscribe to broadcast BEFORE pushing message to worker
        // so we don't miss any events the worker emits
        let broadcast_tx = get_broadcast_sender(&conv_id).await;
        let mut broadcast_rx = broadcast_tx.subscribe();
        drop(broadcast_tx);

        // Per-request completion channel: worker signals when THIS message
        // is done, preventing us from exiting on a prior message's terminal event
        let (completion_tx, mut completion_rx) = tokio::sync::oneshot::channel::<()>();

        // Get or create worker, push message to its queue
        let worker_tx = WORKER_MANAGER
            .get_or_create(conv_id.clone(), db, manager)
            .await;

        if worker_tx
            .send(WorkerMessage {
                user_id,
                message,
                config,
                images,
                completion_tx: Some(completion_tx),
            })
            .await
            .is_err()
        {
            tracing::error!("[CHAT] Worker channel closed for {}", conv_id);
            if let Ok(json) = serde_json::to_string(&StreamEvent::Status {
                status: "failed".to_string(),
                message: Some("Worker unavailable".to_string()),
            }) {
                let _ = tx.send((0, json)).await;
            }
            return;
        }

        // Forward broadcast events until worker signals THIS message is complete
        let timeout = tokio::time::sleep(Duration::from_secs(600));
        tokio::pin!(timeout);

        loop {
            tokio::select! {
                result = broadcast_rx.recv() => {
                    match result {
                        Ok((index, json)) => {
                            // Reset inactivity timeout
                            timeout.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(600));
                            let _ = tx.send((index, json)).await;
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            // Worker ended — broadcast channel dropped
                            break;
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!("[CHAT] SSE lagged by {} events for {}, catching up", n, conv_id);
                            // Continue — broadcast will deliver the next available event
                        }
                    }
                }
                _ = &mut completion_rx => {
                    // Worker signaled THIS message is done — drain remaining events
                    while let Ok((index, json)) = broadcast_rx.try_recv() {
                        let _ = tx.send((index, json)).await;
                    }
                    break;
                }
                _ = &mut timeout => {
                    tracing::warn!("[CHAT] SSE forwarding timed out for {}", conv_id);
                    break;
                }
            }
        }
    });

    create_sse_stream_raw(rx)
}

/// Create SSE stream from raw (index, json) broadcast tuples.
fn create_sse_stream_raw(rx: mpsc::Receiver<(i32, String)>) -> SseStream {
    let stream = stream! {
        let mut rx = ReceiverStream::new(rx);
        while let Some((index, json)) = futures::StreamExt::next(&mut rx).await {
            yield Ok(Event::default().id(index.to_string()).data(json));
        }
    };
    Sse::new(Box::pin(stream) as Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(30))
                .text("ping"),
        )
}

/// Create an SSE stream that replays stored events for a conversation, then
/// tails the event log while the agent is still running.
pub fn create_conversation_reconnect_stream(
    db: Arc<SqlitePool>,
    conversation_id: String,
    events: Vec<ticketing_system::ConversationEvent>,
    checkpoint_status: String,
) -> impl Stream<Item = Result<Event, Infallible>> {
    stream! {
        let mut event_count = 0usize;
        let mut last_event_index: i32 = -1;

        // Phase 1: Replay stored events.
        // load_event_payload_str materializes blob-offloaded payloads
        // (T-E184E642 / v46 event_blobs table) back into raw JSON. Inline
        // events round-trip as a cheap clone. Callers must NEVER ship
        // `event.event_data` directly — for offloaded events that value
        // is the `{"$blob":...}` sentinel, which would break SSE parsers.
        for db_event in &events {
            event_count += 1;
            last_event_index = db_event.event_index;
            let payload = match conversations::load_event_payload_str(&db, db_event).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(
                        "[RECONNECT] Failed to materialize payload for event {}/{}: {}",
                        db_event.conversation_id, db_event.event_index, e
                    );
                    continue;
                }
            };
            record_stream_event_emitted(&conversation_id, payload.len());
            yield Ok(Event::default()
                .id(db_event.event_index.to_string())
                .data(payload));
        }

        // Send replay_complete
        let replay = StreamEvent::ReplayComplete {
            total_events: event_count,
            agent_status: checkpoint_status.clone(),
        };
        if let Ok(json) = serde_json::to_string(&replay) {
            yield Ok(Event::default().data(json));
        }

        // Phase 2: If agent is still running, subscribe to live broadcast
        if checkpoint_status == "running" || checkpoint_status == "pending" {
            let broadcast_tx = get_broadcast_sender(&conversation_id).await;
            let mut broadcast_rx = broadcast_tx.subscribe();
            // Drop our clone of the sender so we don't keep the channel alive
            drop(broadcast_tx);

            let timeout = tokio::time::sleep(Duration::from_secs(600));
            tokio::pin!(timeout);

            loop {
                tokio::select! {
                    result = broadcast_rx.recv() => {
                        match result {
                            Ok((event_index, event_data)) => {
                                if event_index > last_event_index {
                                    last_event_index = event_index;
                                    // Reset inactivity timeout on each received event
                                    timeout.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(600));
                                    record_stream_event_emitted(&conversation_id, event_data.len());
                                    yield Ok(Event::default()
                                        .id(event_index.to_string())
                                        .data(event_data));
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(n)) => {
                                tracing::warn!("[RECONNECT] Broadcast lagged by {} events for {}, falling back to DB", n, conversation_id);
                                // Catch up from DB — materialize blob payloads.
                                if let Ok(missed) = conversations::get_events_after(&db, &conversation_id, last_event_index).await {
                                    for ev in &missed {
                                        last_event_index = ev.event_index;
                                        let payload = match conversations::load_event_payload_str(&db, ev).await {
                                            Ok(s) => s,
                                            Err(e) => {
                                                tracing::error!(
                                                    "[RECONNECT] Failed to materialize payload for event {}/{}: {}",
                                                    ev.conversation_id, ev.event_index, e
                                                );
                                                continue;
                                            }
                                        };
                                        record_stream_event_emitted(&conversation_id, payload.len());
                                        yield Ok(Event::default()
                                            .id(ev.event_index.to_string())
                                            .data(payload));
                                    }
                                }
                            }
                            Err(broadcast::error::RecvError::Closed) => {
                                // Persister finished — agent is done.
                                // Fetch any remaining events from DB and materialize blob payloads.
                                if let Ok(final_events) = conversations::get_events_after(&db, &conversation_id, last_event_index).await {
                                    for ev in &final_events {
                                        let payload = match conversations::load_event_payload_str(&db, ev).await {
                                            Ok(s) => s,
                                            Err(e) => {
                                                tracing::error!(
                                                    "[RECONNECT] Failed to materialize payload for event {}/{}: {}",
                                                    ev.conversation_id, ev.event_index, e
                                                );
                                                continue;
                                            }
                                        };
                                        record_stream_event_emitted(&conversation_id, payload.len());
                                        yield Ok(Event::default()
                                            .id(ev.event_index.to_string())
                                            .data(payload));
                                    }
                                }
                                let done = StreamEvent::Status {
                                    status: "completed".to_string(),
                                    message: None,
                                };
                                if let Ok(json) = serde_json::to_string(&done) {
                                    yield Ok(Event::default().data(json));
                                }
                                // Yielded close reason: worker finished cleanly.
                                let _ = DisconnectReason::Normal;
                                break;
                            }
                        }
                    }
                    _ = &mut timeout => {
                        tracing::warn!("[CHAT] Reconnect stream for {} timed out after 10 minutes", conversation_id);
                        let timeout_event = StreamEvent::Status {
                            status: "timeout".to_string(),
                            message: Some("Agent appears stuck — no activity for 10 minutes".to_string()),
                        };
                        if let Ok(json) = serde_json::to_string(&timeout_event) {
                            yield Ok(Event::default().data(json));
                        }
                        // Yielded close reason: idle timeout.
                        let _ = DisconnectReason::ServerIdleTimeout;
                        break;
                    }
                }
            }
        }
    }
}
