use sqlx::SqlitePool;
use crate::agents::{AgentType, TicketContext};

/// Build ticket context for agent execution
pub fn build_ticket_context(
    epic_id: &str,
    slice_id: &str,
    ticket_id: &str,
    title: String,
    intent: String,
) -> TicketContext {
    TicketContext {
        epic_id: epic_id.to_string(),
        slice_id: slice_id.to_string(),
        ticket_id: ticket_id.to_string(),
        title,
        intent,
    }
}

/// Get previous output from a prior agent run for chaining
pub async fn get_previous_output(db: &SqlitePool, session_id: &str) -> Option<String> {
    ticketing_system::agent_runs::get_agent_run(db, session_id)
        .await
        .ok()
        .flatten()
        .and_then(|r| r.output_summary)
}

/// Build selected context from multiple session IDs (for email agent)
pub async fn build_selected_context(db: &SqlitePool, session_ids: &[String]) -> Option<String> {
    if session_ids.is_empty() {
        return None;
    }

    let mut context_parts = Vec::new();
    for session_id in session_ids {
        if let Ok(Some(run)) = ticketing_system::agent_runs::get_agent_run(db, session_id).await {
            if let Some(output) = run.output_summary {
                context_parts.push(format!(
                    "### {} Agent Output ({})\n{}",
                    run.agent_type, run.session_id, output
                ));
            }
        }
    }

    if context_parts.is_empty() {
        None
    } else {
        Some(context_parts.join("\n\n---\n\n"))
    }
}

/// Look up sender information from ticket assignee
pub async fn get_sender_info(db: &SqlitePool, assignee: Option<&str>) -> Option<String> {
    let assignee = assignee?;

    let user = ticketing_system::users::get_user_by_name(db, assignee)
        .await
        .ok()
        .flatten()?;

    let mut parts = vec![format!("Name: {}", user.name)];

    if let Some(title) = &user.title {
        parts.push(format!("Title: {}", title));
    }
    if let Some(org) = &user.organization {
        parts.push(format!("Organization: {}", org));
    }
    if let Some(email) = &user.email {
        parts.push(format!("Email: {}", email));
    }
    if let Some(phone) = &user.phone {
        parts.push(format!("Phone: {}", phone));
    }

    Some(parts.join("\n"))
}

/// Get all context for agent execution
pub async fn gather_agent_context(
    db: &SqlitePool,
    agent_type: &AgentType,
    previous_session_id: Option<&str>,
    selected_session_ids: &[String],
    assignee: Option<&str>,
) -> (Option<String>, Option<String>, Option<String>) {
    let previous_output = if let Some(prev_id) = previous_session_id {
        get_previous_output(db, prev_id).await
    } else {
        None
    };

    let selected_context = build_selected_context(db, selected_session_ids).await;

    let sender_info = if *agent_type == AgentType::Email {
        get_sender_info(db, assignee).await
    } else {
        None
    };

    (previous_output, selected_context, sender_info)
}
