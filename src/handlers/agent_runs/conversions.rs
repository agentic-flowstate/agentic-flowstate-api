use sqlx::SqlitePool;
use crate::agents::{AgentRun, AgentRunStatus};

/// Store an agent run to the database
pub async fn store_agent_run(db: &SqlitePool, run: &AgentRun) -> anyhow::Result<()> {
    let db_run = ticketing_system::AgentRun {
        session_id: run.session_id.clone(),
        epic_id: run.epic_id.clone(),
        slice_id: run.slice_id.clone(),
        ticket_id: run.ticket_id.clone(),
        agent_type: run.agent_type.as_str().to_string(),
        status: run.status.as_str().to_string(),
        started_at: run.started_at.clone(),
        completed_at: run.completed_at.clone(),
        input_message: run.input_message.clone(),
        output_summary: run.output_summary.clone(),
    };

    ticketing_system::agent_runs::update_agent_run(db, &db_run).await
}

/// Convert a database agent run to API agent run
pub fn db_run_to_api_run(db_run: ticketing_system::AgentRun) -> AgentRun {
    let email_output = if db_run.agent_type == "email" {
        db_run.output_summary.as_ref().and_then(|s| crate::agents::EmailOutput::parse(s))
    } else {
        None
    };

    AgentRun {
        session_id: db_run.session_id,
        ticket_id: db_run.ticket_id,
        epic_id: db_run.epic_id,
        slice_id: db_run.slice_id,
        agent_type: db_run.agent_type,
        status: parse_agent_status(&db_run.status),
        started_at: db_run.started_at,
        completed_at: db_run.completed_at,
        input_message: db_run.input_message,
        output_summary: db_run.output_summary,
        email_output,
    }
}

/// Parse agent status string to enum
pub fn parse_agent_status(s: &str) -> AgentRunStatus {
    match s {
        "running" => AgentRunStatus::Running,
        "completed" => AgentRunStatus::Completed,
        "failed" => AgentRunStatus::Failed,
        "cancelled" => AgentRunStatus::Cancelled,
        _ => AgentRunStatus::Completed,
    }
}
