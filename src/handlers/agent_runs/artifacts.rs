use sqlx::SqlitePool;
use std::path::PathBuf;
use tokio::fs;
use chrono::Utc;

/// Write agent output to repository as a markdown artifact
/// Returns the relative artifact path if successful
///
/// Artifacts go to the org's documentation repo (repo_type='documentation')
/// Falls back to 'research' repo if no documentation repo exists
pub async fn write_artifact(
    db: &SqlitePool,
    ticket_id: &str,
    agent_type: &str,
    output_summary: &str,
) -> Option<String> {
    // Get the ticket
    let ticket = ticketing_system::tickets::get_ticket_by_id(db, ticket_id)
        .await
        .ok()
        .flatten()?;

    // Try documentation repo first, fall back to research repo
    let repo = match ticketing_system::repositories::get_repository_by_org_and_type(
        db,
        &ticket.organization,
        "documentation",
    )
    .await
    {
        Ok(Some(r)) => r,
        _ => {
            // Fallback to research repo
            match ticketing_system::repositories::get_repository_by_org_and_type(
                db,
                &ticket.organization,
                "research",
            )
            .await
            {
                Ok(Some(r)) => r,
                Ok(None) => {
                    tracing::warn!(
                        "No documentation or research repo found for org '{}', skipping artifact write",
                        ticket.organization
                    );
                    return None;
                }
                Err(e) => {
                    tracing::error!("Failed to lookup repo: {}", e);
                    return None;
                }
            }
        }
    };

    let local_path = repo.local_path.as_ref()?;

    // Determine artifact directory based on agent type
    let artifact_dir = match agent_type {
        "research" | "exa-research" | "research-synthesis" | "competitive-research" | "vendor-research" | "technical-research" => {
            "docs/research"
        }
        "planning" => "docs/planning",
        "evaluation" => "docs/evaluation",
        _ => "docs/agent-output",
    };

    // Build full path
    let repo_path = PathBuf::from(local_path);
    let output_dir = repo_path.join(artifact_dir);

    // Create directory if it doesn't exist
    if let Err(e) = fs::create_dir_all(&output_dir).await {
        tracing::error!("Failed to create artifact directory {:?}: {}", output_dir, e);
        return None;
    }

    // Generate filename: {ticket_id}-{agent_type}.md to prevent overwrites
    let filename = format!("{}-{}.md", ticket_id, agent_type);
    let file_path = output_dir.join(&filename);
    let relative_path = format!("{}/{}", artifact_dir, filename);

    // Build markdown content with frontmatter
    let now = Utc::now().to_rfc3339();
    let content = format!(
        r#"---
ticket_id: {}
agent_type: {}
generated_at: {}
title: {}
---

# {}

{}
"#,
        ticket_id,
        agent_type,
        now,
        ticket.title,
        ticket.title,
        output_summary
    );

    // Write the file
    if let Err(e) = fs::write(&file_path, &content).await {
        tracing::error!("Failed to write artifact to {:?}: {}", file_path, e);
        return None;
    }

    tracing::info!(
        "Wrote artifact for ticket {} to {:?}",
        ticket_id,
        file_path
    );

    // Update ticket's artifact_path
    if let Ok(Some(mut updated_ticket)) = ticketing_system::tickets::get_ticket_by_id(db, ticket_id).await {
        updated_ticket.artifact_path = Some(relative_path.clone());
        if let Err(e) = ticketing_system::tickets::update_ticket(db, &updated_ticket).await {
            tracing::warn!("Failed to update ticket artifact_path: {}", e);
        }
    }

    Some(relative_path)
}
