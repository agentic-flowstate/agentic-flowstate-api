use axum::{
    extract::{Extension, State},
    http::HeaderMap,
    response::Response,
    Json,
};
use serde::Deserialize;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use super::chat_client_manager::ChatClientManager;
use super::chat_stream::{self, ChatConfig, ChatImageData, ChatRuntime};
use crate::agents::AgentType;
use crate::auth_middleware::AuthenticatedUser;

#[derive(Debug, Deserialize)]
pub struct MeetingAgentRequest {
    pub message: String,
    pub conversation_id: Option<String>,
    pub room_id: String,
    pub images: Option<Vec<ChatImageData>>,
}

/// Load meeting context (notes + transcript) for a given room
async fn load_meeting_context(db: &SqlitePool, room_id: &str) -> Option<String> {
    let meeting = match ticketing_system::meetings::get_meeting(db, room_id).await {
        Ok(Some(m)) => m,
        _ => return None,
    };

    let mut parts = Vec::new();

    // Meeting notes
    if let Some(ref notes) = meeting.meeting_notes {
        if !notes.is_empty() {
            parts.push(format!("<meeting_notes>\n{}\n</meeting_notes>", notes));
        }
    }

    // Transcript entries
    if let Some(ref session_id) = meeting.transcript_session_id {
        if let Ok(entries) = ticketing_system::transcripts::get_entries(db, session_id).await {
            if !entries.is_empty() {
                let transcript: Vec<String> = entries
                    .iter()
                    .map(|e| format!("[{}] {}: {}", e.timestamp, e.username, e.text))
                    .collect();
                parts.push(format!(
                    "<meeting_transcript>\n{}\n</meeting_transcript>",
                    transcript.join("\n")
                ));
            }
        }
    }

    // Metadata
    let title = meeting.title.as_deref().unwrap_or("Untitled Meeting");
    let created_at_str = meeting.created_at.to_string();
    let date = meeting.started_at.as_deref().unwrap_or(&created_at_str);
    parts.push(format!(
        "<meeting_metadata>\nTitle: {}\nRoom ID: {}\nDate: {}\nStatus: {}\n</meeting_metadata>",
        title, room_id, date, meeting.status
    ));

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

/// POST /api/meeting-agent/chat
pub async fn meeting_agent_chat(
    State(db): State<Arc<SqlitePool>>,
    State(manager): State<Arc<ChatClientManager>>,
    Extension(user): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Json(req): Json<MeetingAgentRequest>,
) -> Response {
    tracing::info!(
        "=== MEETING_AGENT_CHAT START === user={} room={}",
        user.user_id,
        req.room_id
    );

    let client_id = match chat_stream::extract_client_id(&headers) {
        Ok(v) => v,
        Err(e) => return chat_stream::malformed_idempotency_key_response(e),
    };

    let user_name = match ticketing_system::users::get_user(&db, &user.user_id).await {
        Ok(Some(u)) => u.name,
        _ => user.user_id.clone(),
    };

    let mut prompt_vars = HashMap::new();
    prompt_vars.insert("USER_NAME".to_string(), user_name);

    let config = ChatConfig {
        agent_type: AgentType::MeetingAgent,
        runtime: ChatRuntime::CodexExec,
        prompt_name: "meeting-agent",
        working_dir: PathBuf::from("/Users/jarvisgpt/projects"),
        prompt_vars,
    };

    // For new conversations, prepend meeting context.
    // For resumed conversations, context is already in history.
    let message = if let Some(ref conv_id) = req.conversation_id {
        let has_session = ticketing_system::conversations::get_conversation(&db, conv_id, false)
            .await
            .ok()
            .flatten()
            .and_then(|c| c.session_id)
            .is_some();

        if has_session {
            req.message
        } else if let Some(ctx) = load_meeting_context(&db, &req.room_id).await {
            format!("{}\n\n{}", ctx, req.message)
        } else {
            req.message
        }
    } else if let Some(ctx) = load_meeting_context(&db, &req.room_id).await {
        format!("{}\n\n{}", ctx, req.message)
    } else {
        req.message
    };

    chat_stream::chat(
        db,
        manager,
        message,
        req.conversation_id,
        config,
        user.user_id,
        req.images,
        client_id,
    )
    .await
}
