use anyhow::{bail, Result};
use sqlx::SqlitePool;
use std::path::PathBuf;

use super::AgentType;

const DEFAULT_WORKING_DIR: &str = "/Users/jarvisgpt/projects";

/// Resolve the working directory for an agent execution.
///
/// If the agent config has a `working_dir` template (e.g. `{{ORG_REPO:documentation}}`),
/// resolves it using the ticket's organization and the repository registry.
/// If no `working_dir` is configured, returns the default projects directory.
pub async fn resolve_working_dir(
    pool: &SqlitePool,
    agent_type: &AgentType,
    organization: &str,
) -> Result<PathBuf> {
    let template = match agent_type.working_dir_template() {
        Some(t) => t,
        None => return Ok(PathBuf::from(DEFAULT_WORKING_DIR)),
    };

    // Check for {{ORG_REPO:type}} pattern
    if let Some(repo_type) = template
        .strip_prefix("{{ORG_REPO:")
        .and_then(|s| s.strip_suffix("}}"))
    {
        let repo = ticketing_system::repositories::get_repository_by_org_and_type(
            pool,
            organization,
            repo_type,
        )
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "No '{}' repository registered for org '{}'. Register one first.",
                repo_type,
                organization
            )
        })?;

        let local_path = repo.local_path.ok_or_else(|| {
            anyhow::anyhow!(
                "Repository '{}' for org '{}' has no local_path configured",
                repo_type,
                organization
            )
        })?;

        return Ok(PathBuf::from(local_path));
    }

    // Treat as literal path — verify it's not empty
    if template.is_empty() {
        bail!("Agent '{}' has empty working_dir configured", agent_type.as_str());
    }

    Ok(PathBuf::from(template))
}
