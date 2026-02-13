pub mod auth;
pub mod epics;
pub mod slices;
pub mod tickets;
pub mod agent_runs;
pub mod emails;
pub mod transcripts;
pub mod drafts;
pub mod email_thread_tickets;
pub mod ticket_history;
pub mod chat_stream;
pub mod workspace_manager;
pub mod conversations;
pub mod pipeline_templates;
pub mod pipeline_steps;
pub mod data_events;
pub mod meetings;
pub mod meeting_transcription;
pub mod life_planner;
pub mod daily_plan;
pub mod project_workload;

pub use epics::*;
pub use slices::*;
pub use tickets::*;
pub use agent_runs::*;
pub use emails::*;
pub use transcripts::*;
pub use drafts::*;
pub use email_thread_tickets::*;
pub use ticket_history::*;
pub use workspace_manager::*;
pub use conversations::*;
pub use pipeline_templates::*;
pub use pipeline_steps::*;
pub use data_events::*;
pub use meetings::*;
pub use meeting_transcription::*;
pub use life_planner::*;
pub use daily_plan::*;
pub use project_workload::*;

use axum::http::HeaderMap;

/// Extract organization from X-Organization header, defaulting to "telemetryops"
pub fn get_organization(headers: &HeaderMap) -> String {
    headers.get("X-Organization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("telemetryops")
        .to_string()
}
