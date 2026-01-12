use axum::response::sse::Event;
use futures::stream::Stream;
use std::convert::Infallible;
use sqlx::SqlitePool;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use async_stream::stream;

use crate::agents::StreamEvent;

/// Create an SSE stream from a channel receiver, storing events to database
pub fn create_sse_stream(
    db: SqlitePool,
    session_id: String,
    rx: mpsc::Receiver<StreamEvent>,
    initial_event_index: i32,
) -> impl Stream<Item = Result<Event, Infallible>> {
    stream! {
        tracing::info!("[STREAM] SSE stream started for session: {}", session_id);
        let mut rx = ReceiverStream::new(rx);
        let mut event_index = initial_event_index;

        while let Some(event) = futures::StreamExt::next(&mut rx).await {
            let event_type = get_event_type(&event);
            tracing::debug!("[STREAM] Received event #{}: {}", event_index, event_type);

            match serde_json::to_string(&event) {
                Ok(json) => {
                    if let Err(e) = ticketing_system::agent_runs::store_event(
                        &db,
                        &session_id,
                        event_index,
                        event_type,
                        &json,
                    ).await {
                        tracing::warn!("[STREAM] Failed to store event #{}: {}", event_index, e);
                    }
                    event_index += 1;
                    yield Ok(Event::default().data(json));
                }
                Err(e) => {
                    tracing::error!("[STREAM] Failed to serialize event: {}", e);
                }
            }
        }
        tracing::info!("[STREAM] SSE stream ended after {} events", event_index);
    }
}

/// Create an SSE stream for reconnection (replays stored events)
pub fn create_reconnect_stream(
    run: ticketing_system::AgentRun,
    events: Vec<ticketing_system::AgentRunEvent>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    stream! {
        let mut event_count = 0usize;
        let mut stored_text_len = 0usize;

        // Replay stored events
        for db_event in &events {
            event_count += 1;
            if db_event.event_type == "text" {
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&db_event.event_data) {
                    if let Some(content) = parsed.get("content").and_then(|c| c.as_str()) {
                        stored_text_len += content.len();
                    }
                }
            }
            yield Ok(Event::default().data(db_event.event_data.clone()));
        }

        // Send ReplayComplete event
        let replay_complete = StreamEvent::ReplayComplete {
            total_events: event_count,
            agent_status: run.status.clone(),
        };
        if let Ok(json) = serde_json::to_string(&replay_complete) {
            yield Ok(Event::default().data(json));
        }

        if run.status == "running" {
            let event = StreamEvent::Status {
                status: "running".to_string(),
                message: None,
            };
            if let Ok(json) = serde_json::to_string(&event) {
                yield Ok(Event::default().data(json));
            }
        } else {
            // Send output_summary if stored events don't have the full output
            if let Some(output) = &run.output_summary {
                if stored_text_len < output.len().saturating_sub(100) {
                    let event = StreamEvent::Text { content: output.clone() };
                    if let Ok(json) = serde_json::to_string(&event) {
                        yield Ok(Event::default().data(json));
                    }
                }
            }

            // Send final result event
            let result_event = StreamEvent::Result {
                session_id: run.session_id.clone(),
                status: run.status.clone(),
                is_error: run.status == "failed",
            };
            if let Ok(json) = serde_json::to_string(&result_event) {
                yield Ok(Event::default().data(json));
            }
        }
    }
}

/// Create error SSE stream for failed operations
pub fn create_error_stream(message: String) -> impl Stream<Item = Result<Event, Infallible>> {
    stream! {
        let event = StreamEvent::Status {
            status: "failed".to_string(),
            message: Some(message),
        };
        if let Ok(json) = serde_json::to_string(&event) {
            yield Ok(Event::default().data(json));
        }
    }
}

/// Get the event type string for a StreamEvent
pub fn get_event_type(event: &StreamEvent) -> &'static str {
    match event {
        StreamEvent::Text { .. } => "text",
        StreamEvent::ToolUse { .. } => "tool_use",
        StreamEvent::ToolResult { .. } => "tool_result",
        StreamEvent::Thinking { .. } => "thinking",
        StreamEvent::Status { .. } => "status",
        StreamEvent::Result { .. } => "result",
        StreamEvent::ReplayComplete { .. } => "replay_complete",
    }
}
