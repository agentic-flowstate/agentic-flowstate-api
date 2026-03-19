pub mod auth;
pub mod epics;
pub mod slices;
pub mod tickets;
pub mod agent_runs;
pub mod emails;
pub mod email_accounts;
pub mod transcripts;
pub mod drafts;
pub mod email_thread_tickets;
pub mod ticket_history;
pub mod chat_stream;
pub mod chat_client_manager;
pub mod workspace_manager;
pub mod conversations;
pub mod data_events;
pub mod email_events;
pub mod meetings;
pub mod meeting_transcription;
pub mod home_planner;
pub mod full_access_chat;
pub mod meeting_agent;
pub mod daily_plan;
pub mod project_workload;
pub mod docs;
pub mod library;
pub mod memberships;
pub mod tts;
pub mod dms;
pub mod admin_logs;
pub mod admin_reload;
pub mod client_telemetry;
pub mod debug_log;
pub mod voice_transcribe;
pub mod device_tokens;

pub use epics::*;
pub use slices::*;
pub use tickets::*;
pub use agent_runs::*;
pub use emails::*;
pub use email_accounts::*;
pub use transcripts::*;
pub use drafts::*;
pub use email_thread_tickets::*;
pub use ticket_history::*;
pub use workspace_manager::*;
pub use conversations::*;
pub use data_events::*;
pub use email_events::*;
pub use meetings::*;
pub use meeting_transcription::*;
pub use home_planner::*;
pub use full_access_chat::*;
pub use meeting_agent::*;
pub use daily_plan::*;
pub use project_workload::*;
pub use docs::*;
pub use library::*;
pub use tts::*;
pub use dms::*;
pub use voice_transcribe::*;

use axum::http::HeaderMap;

/// Extract organization from X-Organization header, defaulting to "telemetryops"
pub fn get_organization(headers: &HeaderMap) -> String {
    headers.get("X-Organization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("telemetryops")
        .to_string()
}
