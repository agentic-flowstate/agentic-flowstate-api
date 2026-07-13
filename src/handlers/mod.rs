pub mod admin_logs;
pub mod admin_reload;
pub mod agent_operations;
pub mod agent_runs;
pub mod alex_voice;
pub mod anthropic_event_encoder;
pub mod app_diagnostics;
pub mod auth;
pub mod cad;
pub mod chat_client_manager;
pub mod chat_stream;
pub mod child_completion_status;
pub mod client_telemetry;
pub mod context_packets;
pub mod conversation_agent_chat;
pub mod conversation_child_agents;
pub mod conversation_handoff;
pub mod conversation_next_actions;
pub mod conversation_worker;
pub mod conversation_worker_manager;
pub mod conversations;
pub mod dailies;
pub mod daily_plan;
pub mod data_events;
pub mod debug_log;
pub mod device_tokens;
pub mod dms;
pub mod docs;
pub mod drafts;
pub mod email_accounts;
pub mod email_events;
pub mod email_intake;
pub mod email_thread_tickets;
pub mod emails;
pub mod epics;
pub mod focus;
pub mod full_access_chat;
pub mod health;
pub mod home_planner;
pub mod leitner_sync;
pub mod library;
pub mod log_incidents;
pub mod meeting_agent;
pub mod meeting_transcription;
pub mod meetings;
pub mod memberships;
pub mod ops_timeline;
pub mod outreach;
pub mod passkeys;
pub mod quick_commands;
pub mod quick_reference;
pub mod resume_cursor;
pub mod resume_snapshot;
pub mod resume_stream;
pub mod runner_capacity;
pub mod scoped_workspace_chat;
pub mod secret_references;
pub mod slices;
pub mod sse_keepalive;
pub mod storage;
pub mod ticket_history;
pub mod tickets;
pub mod title_generator;
pub mod transcripts;
pub mod tts;
pub mod unified_events;
pub mod usage;
pub mod voice_transcribe;
pub mod workspace_manager;

pub use agent_runs::*;
pub use alex_voice::*;
pub use chat_stream::codex_chat_options;
pub use context_packets::*;
pub use conversation_agent_chat::*;
pub use conversation_child_agents::*;
pub use conversation_next_actions::*;
pub use conversations::*;
pub use dailies::*;
pub use daily_plan::*;
pub use data_events::*;
pub use dms::*;
pub use docs::*;
pub use drafts::*;
pub use email_accounts::*;
pub use email_events::*;
pub use email_intake::*;
pub use email_thread_tickets::*;
pub use emails::*;
pub use epics::*;
pub use focus::*;
pub use full_access_chat::*;
pub use home_planner::*;
pub use library::*;
pub use meeting_agent::*;
pub use meeting_transcription::*;
pub use meetings::*;
pub use quick_reference::*;
pub use scoped_workspace_chat::*;
pub use secret_references::*;
pub use slices::*;
pub use ticket_history::*;
pub use tickets::*;
pub use transcripts::*;
pub use tts::*;
pub use unified_events::*;
pub use voice_transcribe::*;
pub use workspace_manager::*;

use axum::http::HeaderMap;

/// Extract organization from X-Organization header.
/// Returns empty string if header is missing (org-scoped routes are protected by
/// require_org_access middleware which rejects requests without the header).
pub fn get_organization(headers: &HeaderMap) -> String {
    headers
        .get("X-Organization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}
