use crate::agents::{AgentType, TicketContext};
use sqlx::SqlitePool;
use ticketing_system::retrieval::{
    gather_context, ContextPacketItem, ContextPacketResponse, GatherContextRequest,
    RetrievalRequest,
};

const MAX_BLOCKED_BY_CONTEXT_TICKETS: usize = 5;
const BLOCKED_BY_CONTEXT_MAX_RESULTS: usize = 8;
const BLOCKED_BY_CONTEXT_MAX_ITEMS: usize = 3;
const BLOCKED_BY_CONTEXT_TOKEN_BUDGET: usize = 1_200;

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

/// Build bounded context from blocked_by tickets through persisted context packets.
pub async fn build_blocked_by_context(db: &SqlitePool, ticket_id: &str) -> Option<String> {
    let ticket = ticketing_system::tickets::get_ticket_by_id(db, ticket_id)
        .await
        .ok()
        .flatten()?;

    let blocked_by = ticket.blocked_by.as_ref()?;
    if blocked_by.is_empty() {
        return None;
    }

    let mut context_sections = Vec::new();
    let mut skipped = Vec::new();
    let actor_id = format!("api-agent-run-context:{ticket_id}");

    for blocker_id in blocked_by.iter().take(MAX_BLOCKED_BY_CONTEXT_TICKETS) {
        let blocker_ticket = match ticketing_system::tickets::get_ticket_by_id(db, blocker_id).await
        {
            Ok(Some(blocker_ticket)) if blocker_ticket.organization == ticket.organization => {
                blocker_ticket
            }
            Ok(Some(_)) | Ok(None) => {
                skipped.push(format!("{blocker_id} (not visible in organization)"));
                continue;
            }
            Err(e) => {
                tracing::error!(
                    "Failed to fetch blocker ticket {} for {}: {:?}",
                    blocker_id,
                    ticket_id,
                    e
                );
                skipped.push(format!("{blocker_id} (ticket lookup failed)"));
                continue;
            }
        };

        let query_text = format!(
            "Dependency context for blocker ticket {blocker_id}: {}",
            blocker_ticket.title
        );
        let request = GatherContextRequest {
            retrieval: RetrievalRequest {
                organization: ticket.organization.clone(),
                query_text,
                actor_type: "agent".to_string(),
                actor_id: actor_id.clone(),
                tool_name: "gather_context".to_string(),
                work_summary: Some(format!(
                    "Bounded dependency context for {ticket_id} from blocker {blocker_id}"
                )),
                ticket_id: Some(blocker_id.to_string()),
                repository: blocker_ticket.repository.clone(),
                max_results: Some(BLOCKED_BY_CONTEXT_MAX_RESULTS),
                max_selected: Some(BLOCKED_BY_CONTEXT_MAX_ITEMS),
                token_budget: Some(BLOCKED_BY_CONTEXT_TOKEN_BUDGET),
            },
            created_by: actor_id.clone(),
            created_by_agent: Some("agent-run-context".to_string()),
            max_items: Some(BLOCKED_BY_CONTEXT_MAX_ITEMS),
            token_budget: Some(BLOCKED_BY_CONTEXT_TOKEN_BUDGET),
        };

        match gather_context(db, request).await {
            Ok(packet) => context_sections.push(format_blocker_packet_section(
                blocker_id,
                &blocker_ticket.title,
                &packet,
            )),
            Err(e) => {
                tracing::error!(
                    "Failed to gather dependency context for {} from blocker {}: {:?}",
                    ticket_id,
                    blocker_id,
                    e
                );
                context_sections.push(format!(
                    "## {blocker_id}: {}\n\nContext packet retrieval failed for this dependency.",
                    blocker_ticket.title
                ));
            }
        }
    }

    if blocked_by.len() > MAX_BLOCKED_BY_CONTEXT_TICKETS {
        skipped.push(format!(
            "{} additional blocker ticket(s) beyond the bounded context cap",
            blocked_by.len() - MAX_BLOCKED_BY_CONTEXT_TICKETS
        ));
    }

    if !skipped.is_empty() {
        context_sections.push(format!(
            "## Dependency Context Warnings\n\n{}",
            skipped
                .iter()
                .map(|warning| format!("- {warning}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }

    if context_sections.is_empty() {
        None
    } else {
        Some(format!(
            "# Prior Research from Dependency Tickets\n\n\
            The following bounded artifact-memory context packets were gathered from tickets this work depends on.\n\n\
            {}",
            context_sections.join("\n\n---\n\n")
        ))
    }
}

fn format_blocker_packet_section(
    blocker_id: &str,
    blocker_title: &str,
    packet: &ContextPacketResponse,
) -> String {
    let warnings = if packet.warnings.is_empty() {
        String::new()
    } else {
        format!("\nWarnings: {}", packet.warnings.join(", "))
    };
    let items = packet
        .items
        .iter()
        .map(format_packet_item)
        .collect::<Vec<_>>()
        .join("\n\n");
    format!(
        "## {blocker_id}: {blocker_title}\n\n\
         Context packet: `{}`\n\
         Retrieval: `{}`\n\
         Token count: {} / {}{}\n\n\
         {}",
        packet.packet_id,
        packet.retrieval_id,
        packet.token_count,
        packet.token_budget,
        warnings,
        items
    )
}

fn format_packet_item(item: &ContextPacketItem) -> String {
    let citation = item.citation_label.as_deref().unwrap_or("[uncited]");
    let mut sources = Vec::new();
    if let Some(artifact_id) = item.artifact_id.as_deref() {
        sources.push(format!("artifact `{artifact_id}`"));
    }
    if let Some(chunk_id) = item.chunk_id.as_deref() {
        sources.push(format!("chunk `{chunk_id}`"));
    }
    if let Some(ticket_id) = item.ticket_id.as_deref() {
        sources.push(format!("ticket `{ticket_id}`"));
    }
    if let Some(document_id) = item.document_id.as_deref() {
        sources.push(format!("document `{document_id}`"));
    }
    let source_suffix = if sources.is_empty() {
        String::new()
    } else {
        format!(" ({})", sources.join(", "))
    };
    let text = item.included_text.as_deref().unwrap_or("");
    format!(
        "- {citation} {}{}: {}",
        item.relevance_reason, source_suffix, text
    )
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
/// Returns: (previous_output, selected_context, sender_info, blocked_by_context)
pub async fn gather_agent_context(
    db: &SqlitePool,
    agent_type: &AgentType,
    ticket_id: &str,
    previous_session_id: Option<&str>,
    selected_session_ids: &[String],
    assignee: Option<&str>,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
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

    // Auto-fetch context from blocked_by tickets
    let blocked_by_context = build_blocked_by_context(db, ticket_id).await;

    (
        previous_output,
        selected_context,
        sender_info,
        blocked_by_context,
    )
}
