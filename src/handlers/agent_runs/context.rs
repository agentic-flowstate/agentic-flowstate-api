use crate::agents::{AgentType, TicketContext};
use anyhow::{bail, Context, Result};
use sqlx::{Row, SqlitePool};
use ticketing_system::retrieval::{
    gather_context, ContextPacketItem, ContextPacketResponse, GatherContextRequest,
    RetrievalRequest,
};

const MAX_BLOCKED_BY_CONTEXT_TICKETS: usize = 5;
const BLOCKED_BY_CONTEXT_MAX_RESULTS: usize = 8;
const BLOCKED_BY_CONTEXT_MAX_ITEMS: usize = 3;
const BLOCKED_BY_CONTEXT_TOKEN_BUDGET: usize = 1_200;
const AGENT_RUN_CONTEXT_MAX_RESULTS: usize = 12;
const AGENT_RUN_CONTEXT_MAX_ITEMS: usize = 6;
const AGENT_RUN_CONTEXT_TOKEN_BUDGET: usize = 2_400;

#[derive(Debug, Clone)]
struct AgentRunArtifactRef {
    artifact_id: String,
    title: String,
    agent_run_id: String,
}

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

/// Build bounded context from blocked_by tickets through persisted context packets.
pub async fn build_blocked_by_context(db: &SqlitePool, ticket_id: &str) -> Result<Option<String>> {
    let ticket = ticketing_system::tickets::get_ticket_by_id(db, ticket_id)
        .await
        .context("load ticket for blocked_by context")?
        .context("ticket not found for blocked_by context")?;

    let Some(blocked_by) = ticket.blocked_by.as_ref() else {
        return Ok(None);
    };
    if blocked_by.is_empty() {
        return Ok(None);
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
            Ok(packet) => context_sections.push(format_context_packet_section(
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
                return Err(e).with_context(|| {
                    format!(
                        "assemble artifact-memory packet for blocker {blocker_id} of {ticket_id}"
                    )
                });
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
        Ok(None)
    } else {
        Ok(Some(format!(
            "# Prior Research from Dependency Tickets\n\n\
            The following bounded artifact-memory context packets were gathered from tickets this work depends on.\n\n\
            {}",
            context_sections.join("\n\n---\n\n")
        )))
    }
}

async fn build_agent_run_context_packet(
    db: &SqlitePool,
    ticket: &ticketing_system::Ticket,
    label: &str,
    session_ids: &[String],
) -> Result<Option<String>> {
    if session_ids.is_empty() {
        return Ok(None);
    }

    let artifacts = load_agent_run_artifacts(db, ticket, session_ids).await?;
    if artifacts.is_empty() {
        return Ok(None);
    }

    let artifact_handles = artifacts
        .iter()
        .map(|artifact| artifact.artifact_id.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let query_text = format!(
        "{label} for ticket {} from agent-run artifact handles: {artifact_handles}",
        ticket.ticket_id
    );
    let actor_id = format!("api-agent-run-context:{}", ticket.ticket_id);
    let request = GatherContextRequest {
        retrieval: RetrievalRequest {
            organization: ticket.organization.clone(),
            query_text,
            actor_type: "agent".to_string(),
            actor_id: actor_id.clone(),
            tool_name: "gather_context".to_string(),
            work_summary: Some(format!(
                "{label} for {} using artifact-memory packets",
                ticket.ticket_id
            )),
            ticket_id: Some(ticket.ticket_id.clone()),
            repository: ticket.repository.clone(),
            max_results: Some(AGENT_RUN_CONTEXT_MAX_RESULTS),
            max_selected: Some(AGENT_RUN_CONTEXT_MAX_ITEMS),
            token_budget: Some(AGENT_RUN_CONTEXT_TOKEN_BUDGET),
        },
        created_by: actor_id,
        created_by_agent: Some("agent-run-context".to_string()),
        max_items: Some(AGENT_RUN_CONTEXT_MAX_ITEMS),
        token_budget: Some(AGENT_RUN_CONTEXT_TOKEN_BUDGET),
    };

    let packet = gather_context(db, request)
        .await
        .with_context(|| format!("assemble artifact-memory packet for {label}"))?;
    if packet.items.is_empty() {
        bail!(
            "artifact-memory packet {} for {label} had no selected items",
            packet.packet_id
        );
    }

    let artifact_list = artifacts
        .iter()
        .map(|artifact| {
            format!(
                "- `{}` from run `{}`: {}",
                artifact.artifact_id, artifact.agent_run_id, artifact.title
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(Some(format!(
        "{}\n\nSource agent-run artifacts:\n{}",
        format_context_packet_section(label, &ticket.title, &packet),
        artifact_list
    )))
}

async fn load_agent_run_artifacts(
    db: &SqlitePool,
    ticket: &ticketing_system::Ticket,
    session_ids: &[String],
) -> Result<Vec<AgentRunArtifactRef>> {
    let mut artifacts = Vec::new();
    for session_id in session_ids {
        let rows = sqlx::query(
            r#"
            SELECT artifact_id, title, agent_run_id
            FROM artifacts
            WHERE organization = ?
              AND ticket_id = ?
              AND agent_run_id = ?
              AND lifecycle_status = 'active'
              AND visibility IN ('organization', 'system')
            ORDER BY created_at DESC
            "#,
        )
        .bind(&ticket.organization)
        .bind(&ticket.ticket_id)
        .bind(session_id)
        .fetch_all(db)
        .await
        .with_context(|| format!("load artifacts for agent run {session_id}"))?;

        if rows.is_empty() {
            bail!(
                "no active artifact-memory artifact found for prior agent run `{}` on ticket `{}`",
                session_id,
                ticket.ticket_id
            );
        }

        for row in rows {
            artifacts.push(AgentRunArtifactRef {
                artifact_id: row.get("artifact_id"),
                title: row.get("title"),
                agent_run_id: row.get("agent_run_id"),
            });
        }
    }
    Ok(artifacts)
}

fn format_context_packet_section(
    context_id: &str,
    context_title: &str,
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
        "## {context_id}: {context_title}\n\n\
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
    ticket: &ticketing_system::Ticket,
    previous_session_id: Option<&str>,
    selected_session_ids: &[String],
    assignee: Option<&str>,
) -> Result<(
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
)> {
    let previous_output = if let Some(prev_id) = previous_session_id {
        build_agent_run_context_packet(
            db,
            ticket,
            "Prior Agent Run Context",
            &[prev_id.to_string()],
        )
        .await?
    } else {
        None
    };

    let selected_context = build_agent_run_context_packet(
        db,
        ticket,
        "Selected Agent Run Context",
        selected_session_ids,
    )
    .await?;

    let sender_info = if *agent_type == AgentType::Email {
        get_sender_info(db, assignee).await
    } else {
        None
    };

    // Auto-fetch context from blocked_by tickets
    let blocked_by_context = build_blocked_by_context(db, &ticket.ticket_id).await?;

    Ok((
        previous_output,
        selected_context,
        sender_info,
        blocked_by_context,
    ))
}
