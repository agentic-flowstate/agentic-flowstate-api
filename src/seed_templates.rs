//! Seed default pipeline templates on startup
//!
//! Creates standard pipeline templates if they don't already exist.

use sqlx::SqlitePool;
use ticketing_system::models::{CreatePipelineTemplateRequest, ExecutionType, PipelineTemplateStep};
use ticketing_system::pipelines;
use tracing::{info, warn};

/// Seed default pipeline templates into the database.
/// Skips templates that already exist.
pub async fn seed_default_templates(pool: &SqlitePool) -> anyhow::Result<()> {
    let templates = get_default_templates();

    for template_req in templates {
        let template_id = template_req.template_id.clone();

        // Check if template already exists
        match pipelines::get_template(pool, &template_id).await {
            Ok(Some(_)) => {
                info!("Pipeline template '{}' already exists, skipping", template_id);
            }
            Ok(None) => {
                // Create the template
                match pipelines::create_template(pool, template_req).await {
                    Ok(_) => {
                        info!("Created pipeline template: {}", template_id);
                    }
                    Err(e) => {
                        warn!("Failed to create pipeline template '{}': {:?}", template_id, e);
                    }
                }
            }
            Err(e) => {
                warn!("Failed to check pipeline template '{}': {:?}", template_id, e);
            }
        }
    }

    Ok(())
}

/// Get the default pipeline templates
fn get_default_templates() -> Vec<CreatePipelineTemplateRequest> {
    vec![
        // Standard dev workflow
        CreatePipelineTemplateRequest {
            template_id: "standard-dev".to_string(),
            name: "Standard Development".to_string(),
            description: Some(
                "Research, plan review, execute, and evaluate. Best for most feature work."
                    .to_string(),
            ),
            organization: None, // Global template
            epic_id: None,
            slice_id: None,
            steps: vec![
                PipelineTemplateStep {
                    step_id: "research".to_string(),
                    agent_type: "technical-research".to_string(),
                    execution_type: ExecutionType::Auto,
                    name: Some("Research codebase".to_string()),
                    default_inputs: None,
                },
                PipelineTemplateStep {
                    step_id: "plan".to_string(),
                    agent_type: "planning".to_string(),
                    execution_type: ExecutionType::Manual,
                    name: Some("Review implementation plan".to_string()),
                    default_inputs: None,
                },
                PipelineTemplateStep {
                    step_id: "execute".to_string(),
                    agent_type: "execution".to_string(),
                    execution_type: ExecutionType::Auto,
                    name: Some("Implement changes".to_string()),
                    default_inputs: None,
                },
                PipelineTemplateStep {
                    step_id: "evaluate".to_string(),
                    agent_type: "evaluation".to_string(),
                    execution_type: ExecutionType::Auto,
                    name: Some("Verify implementation".to_string()),
                    default_inputs: None,
                },
            ],
        },
        // Research only
        CreatePipelineTemplateRequest {
            template_id: "research-only".to_string(),
            name: "Research Only".to_string(),
            description: Some(
                "Deep dive research followed by findings review. For investigation tasks."
                    .to_string(),
            ),
            organization: None,
            epic_id: None,
            slice_id: None,
            steps: vec![
                PipelineTemplateStep {
                    step_id: "research".to_string(),
                    agent_type: "technical-research".to_string(),
                    execution_type: ExecutionType::Auto,
                    name: Some("Deep research".to_string()),
                    default_inputs: None,
                },
                PipelineTemplateStep {
                    step_id: "review".to_string(),
                    agent_type: "planning".to_string(),
                    execution_type: ExecutionType::Manual,
                    name: Some("Review findings".to_string()),
                    default_inputs: None,
                },
            ],
        },
        // Quick fix
        CreatePipelineTemplateRequest {
            template_id: "quick-fix".to_string(),
            name: "Quick Fix".to_string(),
            description: Some(
                "Execute and verify. For simple bug fixes or small changes.".to_string(),
            ),
            organization: None,
            epic_id: None,
            slice_id: None,
            steps: vec![
                PipelineTemplateStep {
                    step_id: "execute".to_string(),
                    agent_type: "execution".to_string(),
                    execution_type: ExecutionType::Auto,
                    name: Some("Make fix".to_string()),
                    default_inputs: None,
                },
                PipelineTemplateStep {
                    step_id: "evaluate".to_string(),
                    agent_type: "evaluation".to_string(),
                    execution_type: ExecutionType::Auto,
                    name: Some("Verify fix".to_string()),
                    default_inputs: None,
                },
            ],
        },
        // Full review (extra gates for risky changes)
        CreatePipelineTemplateRequest {
            template_id: "full-review".to_string(),
            name: "Full Review".to_string(),
            description: Some(
                "Research, plan review, execute, human review. For risky or complex changes."
                    .to_string(),
            ),
            organization: None,
            epic_id: None,
            slice_id: None,
            steps: vec![
                PipelineTemplateStep {
                    step_id: "research".to_string(),
                    agent_type: "technical-research".to_string(),
                    execution_type: ExecutionType::Auto,
                    name: Some("Understand impact".to_string()),
                    default_inputs: None,
                },
                PipelineTemplateStep {
                    step_id: "plan".to_string(),
                    agent_type: "planning".to_string(),
                    execution_type: ExecutionType::Manual,
                    name: Some("Review plan".to_string()),
                    default_inputs: None,
                },
                PipelineTemplateStep {
                    step_id: "execute".to_string(),
                    agent_type: "execution".to_string(),
                    execution_type: ExecutionType::Auto,
                    name: Some("Implement".to_string()),
                    default_inputs: None,
                },
                PipelineTemplateStep {
                    step_id: "evaluate".to_string(),
                    agent_type: "evaluation".to_string(),
                    execution_type: ExecutionType::Manual,
                    name: Some("Human review of changes".to_string()),
                    default_inputs: None,
                },
            ],
        },
        // Vendor evaluation
        CreatePipelineTemplateRequest {
            template_id: "vendor-eval".to_string(),
            name: "Vendor Evaluation".to_string(),
            description: Some(
                "Research vendors, compare options, review recommendations.".to_string(),
            ),
            organization: None,
            epic_id: None,
            slice_id: None,
            steps: vec![
                PipelineTemplateStep {
                    step_id: "vendor-research".to_string(),
                    agent_type: "vendor-research".to_string(),
                    execution_type: ExecutionType::Auto,
                    name: Some("Research vendors".to_string()),
                    default_inputs: None,
                },
                PipelineTemplateStep {
                    step_id: "competitive".to_string(),
                    agent_type: "competitive-research".to_string(),
                    execution_type: ExecutionType::Auto,
                    name: Some("Compare options".to_string()),
                    default_inputs: None,
                },
                PipelineTemplateStep {
                    step_id: "review".to_string(),
                    agent_type: "planning".to_string(),
                    execution_type: ExecutionType::Manual,
                    name: Some("Review recommendations".to_string()),
                    default_inputs: None,
                },
            ],
        },
    ]
}
