use sqlx::SqlitePool;

/// Write agent output as an inline DB artifact.
/// Returns the artifact_id if successful.
pub async fn write_artifact(
    db: &SqlitePool,
    ticket_id: &str,
    agent_type: &str,
    output_summary: &str,
    agent_run_id: Option<&str>,
) -> Option<String> {
    let ticket = ticketing_system::tickets::get_ticket_by_id(db, ticket_id)
        .await
        .ok()
        .flatten()?;

    // Map agent_type to artifact_type
    let artifact_type = match agent_type {
        "research"
        | "exa-research"
        | "research-synthesis"
        | "competitive-research"
        | "vendor-research"
        | "technical-research"
        | "codebase-research" => "research",
        "planning" => "planning",
        "evaluation" => "evaluation",
        "doc-drafter" => "spec",
        _ => "agent-output",
    };

    let req = ticketing_system::CreateArtifactRequest {
        title: format!("{} — {}", ticket.title, agent_type),
        content: output_summary.to_string(),
        artifact_type: artifact_type.to_string(),
        created_by: agent_type.to_string(),
        source_step_id: None,
        organization: ticket.organization.clone(),
        epic_id: Some(ticket.epic_id.clone()),
        slice_id: Some(ticket.slice_id.clone()),
        ticket_id: Some(ticket_id.to_string()),
        agent_run_id: agent_run_id.map(String::from),
        owner_agent: None,
        produced_by_agent: Some(agent_type.to_string()),
        source_uri: None,
        source_conversation_id: None,
        source_message_id: None,
        source_document_id: None,
        repository: ticket.repository.clone(),
        metadata: None,
    };

    match ticketing_system::artifacts::create_artifact(db, req).await {
        Ok(artifact) => {
            tracing::info!(
                "Created artifact {} for ticket {} (type: {})",
                artifact.artifact_id,
                ticket_id,
                artifact_type,
            );
            Some(artifact.artifact_id)
        }
        Err(e) => {
            tracing::error!("Failed to create artifact for ticket {}: {}", ticket_id, e);
            None
        }
    }
}
