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
        // Standard dev workflow: research → plan → execute → evaluate
        CreatePipelineTemplateRequest {
            template_id: "standard-dev".to_string(),
            name: "Standard Development".to_string(),
            description: Some(
                "Research, plan review, execute, and evaluate. For code implementation work."
                    .to_string(),
            ),
            organization: None,
            epic_id: None,
            slice_id: None,
            steps: vec![
                PipelineTemplateStep {
                    step_id: "research".to_string(),
                    agent_type: "exa-research".to_string(),
                    execution_type: ExecutionType::Auto,
                    name: Some("Research".to_string()),
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
        // Human task (no automation, manual completion)
        CreatePipelineTemplateRequest {
            template_id: "human-task".to_string(),
            name: "Human Task".to_string(),
            description: Some(
                "Manual human task with no agent automation. Mark complete when done.".to_string(),
            ),
            organization: None,
            epic_id: None,
            slice_id: None,
            steps: vec![
                PipelineTemplateStep {
                    step_id: "execute".to_string(),
                    agent_type: "human".to_string(),
                    execution_type: ExecutionType::Manual,
                    name: Some("Complete task".to_string()),
                    default_inputs: None,
                },
            ],
        },
        // Deep research with ticket creation
        CreatePipelineTemplateRequest {
            template_id: "exa-research".to_string(),
            name: "Deep Research".to_string(),
            description: Some(
                "Web and codebase research and follow-up ticket creation.".to_string(),
            ),
            organization: None,
            epic_id: None,
            slice_id: None,
            steps: vec![
                PipelineTemplateStep {
                    step_id: "exa-research".to_string(),
                    agent_type: "exa-research".to_string(),
                    execution_type: ExecutionType::Auto,
                    name: Some("Research".to_string()),
                    default_inputs: None,
                },
                PipelineTemplateStep {
                    step_id: "plan-tickets".to_string(),
                    agent_type: "ticket-planner".to_string(),
                    execution_type: ExecutionType::Auto,
                    name: Some("Plan follow-up tickets".to_string()),
                    default_inputs: None,
                },
                PipelineTemplateStep {
                    step_id: "create-tickets".to_string(),
                    agent_type: "ticket-creator".to_string(),
                    execution_type: ExecutionType::Manual,
                    name: Some("Create follow-up tickets".to_string()),
                    default_inputs: None,
                },
            ],
        },
        // Research only: single research step, no follow-up tickets
        CreatePipelineTemplateRequest {
            template_id: "research-only".to_string(),
            name: "Research Only".to_string(),
            description: Some(
                "Research a topic and produce findings. No follow-up ticket creation.".to_string(),
            ),
            organization: None,
            epic_id: None,
            slice_id: None,
            steps: vec![
                PipelineTemplateStep {
                    step_id: "exa-research".to_string(),
                    agent_type: "exa-research".to_string(),
                    execution_type: ExecutionType::Auto,
                    name: Some("Research".to_string()),
                    default_inputs: None,
                },
            ],
        },
        // Document drafting: research → draft (drafter does its own structured extraction)
        CreatePipelineTemplateRequest {
            template_id: "doc-drafting".to_string(),
            name: "Document Drafting".to_string(),
            description: Some(
                "Research a topic, then draft a policy/procedure/training document with evidence traceability."
                    .to_string(),
            ),
            organization: None,
            epic_id: None,
            slice_id: None,
            steps: vec![
                PipelineTemplateStep {
                    step_id: "research".to_string(),
                    agent_type: "exa-research".to_string(),
                    execution_type: ExecutionType::Auto,
                    name: Some("Research".to_string()),
                    default_inputs: None,
                },
                PipelineTemplateStep {
                    step_id: "draft".to_string(),
                    agent_type: "doc-drafter".to_string(),
                    execution_type: ExecutionType::Manual,
                    name: Some("Draft document".to_string()),
                    default_inputs: None,
                },
            ],
        },
    ]
}
