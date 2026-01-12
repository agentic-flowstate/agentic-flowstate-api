use axum::{
    extract::{Path, Query, State},
    response::sse::{Event, KeepAlive, Sse},
    http::StatusCode,
    Json,
};
use futures::stream::Stream;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use sqlx::SqlitePool;
use serde::{Deserialize, Serialize};

use ticketing_system::{
    TranscriptSession, TranscriptEntry,
    CreateTranscriptSessionRequest, CreateTranscriptEntryRequest,
};

#[derive(Debug, Serialize)]
pub struct TranscriptSessionsResponse {
    pub sessions: Vec<TranscriptSession>,
}

#[derive(Debug, Serialize)]
pub struct TranscriptEntriesResponse {
    pub entries: Vec<TranscriptEntry>,
    pub session: TranscriptSession,
}

#[derive(Debug, Deserialize)]
pub struct ListSessionsQuery {
    pub active_only: Option<bool>,
}

/// SSE event for transcript streaming
#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum TranscriptStreamEvent {
    #[serde(rename = "entry")]
    Entry(TranscriptEntry),
    #[serde(rename = "session_ended")]
    SessionEnded { session_id: String },
}

/// GET /api/transcripts
/// List all transcript sessions
pub async fn list_sessions(
    State(db): State<Arc<SqlitePool>>,
    Query(query): Query<ListSessionsQuery>,
) -> Result<Json<TranscriptSessionsResponse>, (StatusCode, String)> {
    let active_only = query.active_only.unwrap_or(false);

    let sessions = ticketing_system::transcripts::list_sessions(&db, active_only)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Database error: {}", e)))?;

    Ok(Json(TranscriptSessionsResponse { sessions }))
}

/// GET /api/transcripts/:session_id
/// Get a specific transcript session with all entries
pub async fn get_session(
    Path(session_id): Path<String>,
    State(db): State<Arc<SqlitePool>>,
) -> Result<Json<TranscriptEntriesResponse>, (StatusCode, String)> {
    let session = ticketing_system::transcripts::get_session(&db, &session_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Database error: {}", e)))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Session not found".to_string()))?;

    let entries = ticketing_system::transcripts::get_entries(&db, &session_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Database error: {}", e)))?;

    Ok(Json(TranscriptEntriesResponse { entries, session }))
}

/// POST /api/transcripts
/// Create a new transcript session
pub async fn create_session(
    State(db): State<Arc<SqlitePool>>,
    Json(req): Json<CreateTranscriptSessionRequest>,
) -> Result<Json<TranscriptSession>, (StatusCode, String)> {
    let session = ticketing_system::transcripts::create_session(&db, req)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Database error: {}", e)))?;

    Ok(Json(session))
}

/// POST /api/transcripts/:session_id/end
/// End a transcript session
pub async fn end_session(
    Path(session_id): Path<String>,
    State(db): State<Arc<SqlitePool>>,
) -> Result<StatusCode, (StatusCode, String)> {
    ticketing_system::transcripts::end_session(&db, &session_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Database error: {}", e)))?;

    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/transcripts/:session_id/entries
/// Add a transcript entry
pub async fn add_entry(
    Path(session_id): Path<String>,
    State(db): State<Arc<SqlitePool>>,
    Json(mut req): Json<CreateTranscriptEntryRequest>,
) -> Result<Json<TranscriptEntry>, (StatusCode, String)> {
    // Ensure session_id matches
    req.session_id = session_id;

    let entry = ticketing_system::transcripts::add_entry(&db, req)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Database error: {}", e)))?;

    Ok(Json(entry))
}

/// GET /api/transcripts/:session_id/stream
/// SSE endpoint for live transcript updates
pub async fn stream_session(
    Path(session_id): Path<String>,
    State(db): State<Arc<SqlitePool>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = async_stream::stream! {
        let mut last_id: i64 = 0;

        // First, send all existing entries
        match ticketing_system::transcripts::get_entries(&db, &session_id).await {
            Ok(entries) => {
                for entry in entries {
                    last_id = entry.id;
                    let event = TranscriptStreamEvent::Entry(entry);
                    if let Ok(json) = serde_json::to_string(&event) {
                        yield Ok(Event::default().data(json));
                    }
                }
            }
            Err(e) => {
                tracing::error!("Failed to get initial entries: {}", e);
            }
        }

        // Poll for new entries every 500ms
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;

            // Check if session is still active
            match ticketing_system::transcripts::get_session(&db, &session_id).await {
                Ok(Some(session)) => {
                    if !session.is_active {
                        let event = TranscriptStreamEvent::SessionEnded {
                            session_id: session_id.clone(),
                        };
                        if let Ok(json) = serde_json::to_string(&event) {
                            yield Ok(Event::default().data(json));
                        }
                        break;
                    }
                }
                Ok(None) => {
                    tracing::warn!("Session {} not found", session_id);
                    break;
                }
                Err(e) => {
                    tracing::error!("Failed to check session status: {}", e);
                }
            }

            // Get new entries
            match ticketing_system::transcripts::get_entries_after(&db, &session_id, last_id).await {
                Ok(entries) => {
                    for entry in entries {
                        last_id = entry.id;
                        let event = TranscriptStreamEvent::Entry(entry);
                        if let Ok(json) = serde_json::to_string(&event) {
                            yield Ok(Event::default().data(json));
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to get new entries: {}", e);
                }
            }
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}
