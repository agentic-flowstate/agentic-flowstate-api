//! Pipeline Automation Engine
//!
//! Drives pipeline execution by automatically triggering next steps when:
//! - An auto step completes successfully
//! - A manual step is approved
//!
//! The engine:
//! - Spawns agent runs for auto steps
//! - Marks manual steps as awaiting_approval
//! - Updates ticket status on pipeline completion

use anyhow::Result;
use sqlx::SqlitePool;
use tracing::{error, info, warn};

use ticketing_system::{
    models::{ExecutionType, PipelineStepStatus, Ticket},
    pipelines, tickets,
};

use crate::agents::{AgentExecutor, AgentType, TicketContext, resolve_working_dir};

/// Maximum depth of chained auto-steps to prevent infinite loops
const MAX_AUTO_CHAIN_DEPTH: u32 = 10;

/// Result of advancing a pipeline after a step completes
#[derive(Debug)]
pub enum PipelineAdvanceResult {
    /// Pipeline completed (all steps done)
    PipelineDone { completed: bool },
    /// Next auto step was spawned in background
    NextAutoStepSpawned { step_id: String, session_id: String },
    /// Next step is manual, marked as awaiting approval
    NextStepAwaitingApproval { step_id: String },
    /// No next step to process
    NoNextStep,
    /// Step or pipeline not found
    NotFound { reason: String },
}

/// Advance a pipeline after a step completes or fails.
///
/// This is the single reusable function called by both the streaming handler
/// and the background auto-step executor after an agent finishes.
///
/// It:
/// 1. Re-reads the ticket pipeline from DB (fresh state)
/// 2. If success: calls `pipelines::complete_step()`, saves to DB
/// 3. If failure: calls `pipelines::fail_step()`, saves to DB, returns early
/// 4. Checks `pipeline.is_complete()` → updates ticket status to "completed" if done
/// 5. Finds next step → if auto, spawns background agent; if manual, marks awaiting approval
pub async fn advance_pipeline_after_step(
    pool: &SqlitePool,
    ticket_id: &str,
    step_id: &str,
    success: bool,
    outputs: Option<serde_json::Value>,
) -> Result<PipelineAdvanceResult> {
    // Re-read ticket to get fresh pipeline state
    let ticket = match tickets::get_ticket_by_id(pool, ticket_id).await? {
        Some(t) => t,
        None => return Ok(PipelineAdvanceResult::NotFound { reason: format!("Ticket not found: {}", ticket_id) }),
    };

    let mut pipeline = match ticket.pipeline.clone() {
        Some(p) => p,
        None => return Ok(PipelineAdvanceResult::NotFound { reason: "Pipeline not found on ticket".to_string() }),
    };

    // Find the step
    let step_idx = match pipeline.steps.iter().position(|s| s.step_id == step_id) {
        Some(idx) => idx,
        None => return Ok(PipelineAdvanceResult::NotFound { reason: format!("Step not found: {}", step_id) }),
    };

    if !success {
        // Mark step as failed
        pipelines::fail_step(&mut pipeline, step_id, outputs);
        tickets::update_ticket_pipeline(pool, ticket_id, Some(&pipeline)).await?;
        info!("Pipeline step {} failed for ticket {}", step_id, ticket_id);
        return Ok(PipelineAdvanceResult::PipelineDone { completed: false });
    }

    // Mark step as completed
    pipelines::complete_step(&mut pipeline, step_id, outputs);
    tickets::update_ticket_pipeline(pool, ticket_id, Some(&pipeline)).await?;
    info!("Pipeline step {} completed for ticket {}", step_id, ticket_id);

    // Check if pipeline is complete
    if pipeline.is_complete() {
        if !pipeline.has_failed() {
            info!("Pipeline completed successfully for ticket {}, updating status to 'completed'", ticket_id);
            if let Err(e) = tickets::update_ticket_status(
                pool,
                &ticket.organization,
                &ticket.epic_id,
                &ticket.slice_id,
                ticket_id,
                "completed",
            ).await {
                error!("Failed to update ticket status to completed: {}", e);
            }
            return Ok(PipelineAdvanceResult::PipelineDone { completed: true });
        }
        return Ok(PipelineAdvanceResult::PipelineDone { completed: false });
    }

    // Find next step
    let next_idx = step_idx + 1;
    if next_idx >= pipeline.steps.len() {
        return Ok(PipelineAdvanceResult::NoNextStep);
    }

    let next_step = &pipeline.steps[next_idx];
    if next_step.status != PipelineStepStatus::Queued {
        return Ok(PipelineAdvanceResult::NoNextStep);
    }

    let next_step_id = next_step.step_id.clone();

    match next_step.execution_type {
        ExecutionType::Auto => {
            // Spawn agent for auto step (background, non-streaming)
            match spawn_agent_for_step(pool, &ticket, next_idx, 0).await? {
                PipelineProgressResult::AgentSpawned { step_id, session_id } => {
                    Ok(PipelineAdvanceResult::NextAutoStepSpawned { step_id, session_id })
                }
                _ => Ok(PipelineAdvanceResult::NoNextStep),
            }
        }
        ExecutionType::Manual => {
            // Mark as awaiting approval
            pipelines::await_approval(&mut pipeline, &next_step_id);
            tickets::update_ticket_pipeline(pool, ticket_id, Some(&pipeline)).await?;
            info!("Pipeline step {} marked as awaiting approval for ticket {}", next_step_id, ticket_id);
            Ok(PipelineAdvanceResult::NextStepAwaitingApproval { step_id: next_step_id })
        }
    }
}

/// Result of processing pipeline progression
#[derive(Debug)]
pub enum PipelineProgressResult {
    /// No next step to process
    NoNextStep,
    /// Next step is manual, marked as awaiting approval
    AwaitingApproval { step_id: String },
    /// Next step is auto, agent spawned
    AgentSpawned { step_id: String, session_id: String },
    /// Pipeline completed (all steps done)
    PipelineCompleted,
    /// Pipeline failed
    PipelineFailed { reason: String },
    /// Max chain depth reached (safety limit)
    MaxDepthReached,
}

/// Check if there's a next step and process it according to its execution type.
/// This is the main entry point called after step completion or approval.
///
/// Returns information about what action was taken.
pub async fn process_next_step(
    pool: &SqlitePool,
    ticket_id: &str,
    current_step_id: &str,
    depth: u32,
) -> Result<PipelineProgressResult> {
    // Safety check to prevent infinite loops
    if depth >= MAX_AUTO_CHAIN_DEPTH {
        warn!(
            "Pipeline automation: max chain depth {} reached for ticket {}",
            MAX_AUTO_CHAIN_DEPTH, ticket_id
        );
        return Ok(PipelineProgressResult::MaxDepthReached);
    }

    // Get the ticket with its pipeline
    let ticket = tickets::get_ticket_by_id(pool, ticket_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Ticket not found: {}", ticket_id))?;

    let pipeline = match &ticket.pipeline {
        Some(p) => p,
        None => return Ok(PipelineProgressResult::NoNextStep),
    };

    // Check if pipeline is already complete or failed
    if pipeline.is_complete() {
        return handle_pipeline_completion(pool, &ticket).await;
    }

    // Find the current step index
    let current_idx = pipeline
        .steps
        .iter()
        .position(|s| s.step_id == current_step_id);

    let current_idx = match current_idx {
        Some(idx) => idx,
        None => return Ok(PipelineProgressResult::NoNextStep),
    };

    // Get the next step (if any)
    let next_idx = current_idx + 1;
    if next_idx >= pipeline.steps.len() {
        // No more steps - check completion
        return handle_pipeline_completion(pool, &ticket).await;
    }

    let next_step = &pipeline.steps[next_idx];

    // Only process if the next step is still queued
    if next_step.status != PipelineStepStatus::Queued {
        info!(
            "Next step {} is not queued (status: {:?}), skipping",
            next_step.step_id, next_step.status
        );
        return Ok(PipelineProgressResult::NoNextStep);
    }

    match next_step.execution_type {
        ExecutionType::Auto => {
            // Spawn agent for auto step
            spawn_agent_for_step(pool, &ticket, next_idx, depth).await
        }
        ExecutionType::Manual => {
            // Mark as awaiting approval
            mark_step_awaiting_approval(pool, &ticket, next_idx).await
        }
    }
}

/// Mark a step as awaiting approval
async fn mark_step_awaiting_approval(
    pool: &SqlitePool,
    ticket: &Ticket,
    step_idx: usize,
) -> Result<PipelineProgressResult> {
    let mut pipeline = ticket.pipeline.clone().unwrap();
    let step_id = pipeline.steps[step_idx].step_id.clone();

    pipelines::await_approval(&mut pipeline, &step_id);

    tickets::update_ticket_pipeline(pool, &ticket.ticket_id, Some(&pipeline)).await?;

    info!(
        "Pipeline step {} marked as awaiting approval for ticket {}",
        step_id, ticket.ticket_id
    );

    Ok(PipelineProgressResult::AwaitingApproval { step_id })
}

/// Spawn an agent for an auto step
async fn spawn_agent_for_step(
    pool: &SqlitePool,
    ticket: &Ticket,
    step_idx: usize,
    depth: u32,
) -> Result<PipelineProgressResult> {
    let mut pipeline = ticket.pipeline.clone().unwrap();
    let step = &pipeline.steps[step_idx];
    let step_id = step.step_id.clone();
    let agent_type_str = step.agent_type.clone();

    // Parse agent type
    let agent_type: AgentType = match serde_json::from_str(&format!("\"{}\"", agent_type_str)) {
        Ok(at) => at,
        Err(e) => {
            error!(
                "Unknown agent type '{}' for step {}: {}",
                agent_type_str, step_id, e
            );
            // Mark step as failed
            pipelines::fail_step(
                &mut pipeline,
                &step_id,
                Some(serde_json::json!({
                    "error": format!("Unknown agent type: {}", agent_type_str)
                })),
            );
            tickets::update_ticket_pipeline(pool, &ticket.ticket_id, Some(&pipeline)).await?;
            return Ok(PipelineProgressResult::PipelineFailed {
                reason: format!("Unknown agent type: {}", agent_type_str),
            });
        }
    };

    // Generate session ID for the agent run
    let session_id = uuid::Uuid::new_v4().to_string();

    // Mark step as started
    pipelines::start_step(&mut pipeline, &step_id, &session_id);
    tickets::update_ticket_pipeline(pool, &ticket.ticket_id, Some(&pipeline)).await?;

    info!(
        "Starting auto step {} with agent {} for ticket {} (session: {})",
        step_id, agent_type_str, ticket.ticket_id, session_id
    );

    // Create agent run record
    let create_req = ticketing_system::CreateAgentRunRequest {
        session_id: session_id.clone(),
        epic_id: ticket.epic_id.clone(),
        slice_id: ticket.slice_id.clone(),
        ticket_id: ticket.ticket_id.clone(),
        agent_type: agent_type_str.clone(),
        input_message: ticket.description.clone().unwrap_or_default(),
    };
    ticketing_system::agent_runs::create_agent_run(pool, create_req).await?;

    // Spawn agent execution in background
    let pool_clone = pool.clone();
    let ticket_id = ticket.ticket_id.clone();
    let epic_id = ticket.epic_id.clone();
    let slice_id = ticket.slice_id.clone();
    let organization = ticket.organization.clone();
    let title = ticket.title.clone();
    let description = ticket.description.clone().unwrap_or_default();
    let step_id_clone = step_id.clone();
    let session_id_clone = session_id.clone();

    tokio::spawn(async move {
        let result = execute_agent_for_step(
            &pool_clone,
            &ticket_id,
            &epic_id,
            &slice_id,
            &organization,
            &title,
            &description,
            &step_id_clone,
            &session_id_clone,
            agent_type,
            depth,
        )
        .await;

        if let Err(e) = result {
            error!(
                "Agent execution failed for step {} on ticket {}: {}",
                step_id_clone, ticket_id, e
            );
        }
    });

    Ok(PipelineProgressResult::AgentSpawned { step_id, session_id })
}

/// Execute an agent and handle completion/failure.
/// This runs in a loop to handle chained auto-steps without async recursion.
async fn execute_agent_for_step(
    pool: &SqlitePool,
    ticket_id: &str,
    epic_id: &str,
    slice_id: &str,
    organization: &str,
    title: &str,
    intent: &str,
    initial_step_id: &str,
    initial_session_id: &str,
    initial_agent_type: AgentType,
    initial_depth: u32,
) -> Result<()> {
    let mut working_dir = resolve_working_dir(pool, &initial_agent_type, organization).await?;

    // Track current step info for the loop
    let mut current_step_id = initial_step_id.to_string();
    let mut current_session_id = initial_session_id.to_string();
    let mut current_agent_type = initial_agent_type;
    let mut depth = initial_depth;

    // Track previous step output for chaining between auto-steps
    let mut previous_step_output: Option<String> = {
        // Check if there's a completed step before the initial step
        let ticket = tickets::get_ticket_by_id(pool, ticket_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Ticket not found: {}", ticket_id))?;
        if let Some(pipeline) = &ticket.pipeline {
            if let Some(current_idx) = pipeline.steps.iter().position(|s| s.step_id == initial_step_id) {
                if current_idx > 0 {
                    pipeline.steps[current_idx - 1]
                        .outputs
                        .as_ref()
                        .and_then(|o| o.get("summary"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        }
    };

    loop {
        // Safety check
        if depth >= MAX_AUTO_CHAIN_DEPTH {
            warn!(
                "Pipeline automation: max chain depth {} reached for ticket {}",
                MAX_AUTO_CHAIN_DEPTH, ticket_id
            );
            break;
        }

        let executor = AgentExecutor::new(working_dir.clone());

        let context = TicketContext {
            epic_id: epic_id.to_string(),
            slice_id: slice_id.to_string(),
            ticket_id: ticket_id.to_string(),
            title: title.to_string(),
            intent: intent.to_string(),
        };

        // Execute agent (no streaming for automated runs)
        // Pass previous step output for chaining (e.g., research output → synthesis agent)
        let result = executor
            .execute(current_agent_type.clone(), context, previous_step_output.clone(), None, None, None)
            .await;

        // Get current pipeline state
        let ticket = tickets::get_ticket_by_id(pool, ticket_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Ticket not found: {}", ticket_id))?;

        let mut pipeline = ticket
            .pipeline
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Pipeline not found on ticket"))?;

        match result {
            Ok(agent_run) => {
                // Update agent run record
                let db_run = ticketing_system::AgentRun {
                    session_id: current_session_id.clone(),
                    ticket_id: ticket_id.to_string(),
                    epic_id: epic_id.to_string(),
                    slice_id: slice_id.to_string(),
                    agent_type: current_agent_type.as_str().to_string(),
                    status: agent_run.status.as_str().to_string(),
                    started_at: agent_run.started_at.clone(),
                    completed_at: agent_run.completed_at.clone(),
                    input_message: agent_run.input_message.clone(),
                    output_summary: agent_run.output_summary.clone(),
                };
                ticketing_system::agent_runs::update_agent_run(pool, &db_run).await?;

                // Capture output for next step in chain
                previous_step_output = agent_run.output_summary.clone();

                // Create outputs JSON from agent run
                let outputs = agent_run.output_summary.map(|s| serde_json::json!({ "summary": s }));

                // Mark step as completed
                pipelines::complete_step(&mut pipeline, &current_step_id, outputs);
                tickets::update_ticket_pipeline(pool, ticket_id, Some(&pipeline)).await?;

                info!(
                    "Auto step {} completed successfully for ticket {}",
                    current_step_id, ticket_id
                );

                // Log to ticket history
                if let Err(e) = ticketing_system::ticket_history::log_agent_run_completed(
                    pool,
                    ticket_id,
                    &current_session_id,
                    current_agent_type.as_str(),
                    "completed",
                )
                .await
                {
                    warn!("Failed to log agent run to history: {}", e);
                }

                // Find current step index
                let current_idx = pipeline
                    .steps
                    .iter()
                    .position(|s| s.step_id == current_step_id);

                let current_idx = match current_idx {
                    Some(idx) => idx,
                    None => break,
                };

                // Check if pipeline is complete
                if pipeline.is_complete() {
                    // Handle pipeline completion
                    if !pipeline.has_failed() {
                        info!(
                            "Pipeline completed successfully for ticket {}, updating status to 'completed'",
                            ticket_id
                        );
                        if let Err(e) = tickets::update_ticket_status(
                            pool,
                            &ticket.organization,
                            epic_id,
                            slice_id,
                            ticket_id,
                            "completed",
                        )
                        .await
                        {
                            error!("Failed to update ticket status to completed: {}", e);
                        }
                    }
                    break;
                }

                // Get next step
                let next_idx = current_idx + 1;
                if next_idx >= pipeline.steps.len() {
                    break;
                }

                let next_step = &pipeline.steps[next_idx];

                // Only process if next step is queued
                if next_step.status != PipelineStepStatus::Queued {
                    break;
                }

                // Clone values we need before any mutable borrows
                let next_step_id = next_step.step_id.clone();
                let next_agent_type_str = next_step.agent_type.clone();
                let next_execution_type = next_step.execution_type.clone();

                match next_execution_type {
                    ExecutionType::Auto => {
                        // Set up for next iteration — re-resolve working_dir for new agent type
                        current_agent_type = match serde_json::from_str(&format!("\"{}\"", next_agent_type_str)) {
                            Ok(at) => at,
                            Err(e) => {
                                error!(
                                    "Unknown agent type '{}' for step {}: {}",
                                    next_agent_type_str, next_step_id, e
                                );
                                pipelines::fail_step(
                                    &mut pipeline,
                                    &next_step_id,
                                    Some(serde_json::json!({
                                        "error": format!("Unknown agent type: {}", next_agent_type_str)
                                    })),
                                );
                                tickets::update_ticket_pipeline(pool, ticket_id, Some(&pipeline)).await?;
                                break;
                            }
                        };

                        // Re-resolve working dir for the new agent type
                        working_dir = resolve_working_dir(pool, &current_agent_type, organization).await?;

                        // Generate new session ID and mark step as started
                        current_session_id = uuid::Uuid::new_v4().to_string();
                        current_step_id = next_step_id;

                        // Re-read pipeline since we need to update it
                        let ticket = tickets::get_ticket_by_id(pool, ticket_id)
                            .await?
                            .ok_or_else(|| anyhow::anyhow!("Ticket not found: {}", ticket_id))?;
                        let mut pipeline = ticket.pipeline.unwrap();

                        pipelines::start_step(&mut pipeline, &current_step_id, &current_session_id);
                        tickets::update_ticket_pipeline(pool, ticket_id, Some(&pipeline)).await?;

                        // Create agent run record
                        let create_req = ticketing_system::CreateAgentRunRequest {
                            session_id: current_session_id.clone(),
                            epic_id: epic_id.to_string(),
                            slice_id: slice_id.to_string(),
                            ticket_id: ticket_id.to_string(),
                            agent_type: current_agent_type.as_str().to_string(),
                            input_message: intent.to_string(),
                        };
                        ticketing_system::agent_runs::create_agent_run(pool, create_req).await?;

                        info!(
                            "Starting chained auto step {} with agent {} for ticket {} (session: {})",
                            current_step_id, current_agent_type.as_str(), ticket_id, current_session_id
                        );

                        depth += 1;
                        // Continue to next iteration
                    }
                    ExecutionType::Manual => {
                        // Mark as awaiting approval and stop the loop
                        pipelines::await_approval(&mut pipeline, &next_step_id);
                        tickets::update_ticket_pipeline(pool, ticket_id, Some(&pipeline)).await?;
                        info!(
                            "Pipeline step {} marked as awaiting approval for ticket {}",
                            next_step_id, ticket_id
                        );
                        break;
                    }
                }
            }
            Err(e) => {
                // Update agent run as failed
                let now = chrono::Utc::now().to_rfc3339();
                let db_run = ticketing_system::AgentRun {
                    session_id: current_session_id.clone(),
                    ticket_id: ticket_id.to_string(),
                    epic_id: epic_id.to_string(),
                    slice_id: slice_id.to_string(),
                    agent_type: current_agent_type.as_str().to_string(),
                    status: "failed".to_string(),
                    started_at: now.clone(),
                    completed_at: Some(now),
                    input_message: intent.to_string(),
                    output_summary: Some(format!("Agent failed: {}", e)),
                };
                ticketing_system::agent_runs::update_agent_run(pool, &db_run).await?;

                // Mark step as failed
                pipelines::fail_step(
                    &mut pipeline,
                    &current_step_id,
                    Some(serde_json::json!({ "error": e.to_string() })),
                );
                tickets::update_ticket_pipeline(pool, ticket_id, Some(&pipeline)).await?;

                error!(
                    "Auto step {} failed for ticket {}: {}",
                    current_step_id, ticket_id, e
                );

                // Log to ticket history
                if let Err(e) = ticketing_system::ticket_history::log_agent_run_completed(
                    pool,
                    ticket_id,
                    &current_session_id,
                    current_agent_type.as_str(),
                    "failed",
                )
                .await
                {
                    warn!("Failed to log agent run to history: {}", e);
                }

                // Do NOT continue on failure - pipeline halts
                break;
            }
        }
    }

    Ok(())
}

/// Handle pipeline completion - update ticket status
async fn handle_pipeline_completion(
    pool: &SqlitePool,
    ticket: &Ticket,
) -> Result<PipelineProgressResult> {
    let pipeline = ticket.pipeline.as_ref().unwrap();

    if pipeline.has_failed() {
        info!(
            "Pipeline failed for ticket {}, setting status to 'pipeline_failed'",
            ticket.ticket_id
        );
        // Optionally update ticket status to indicate pipeline failure
        // We don't change to "completed" since it failed
        return Ok(PipelineProgressResult::PipelineFailed {
            reason: "One or more steps failed".to_string(),
        });
    }

    if pipeline.is_complete() && !pipeline.has_failed() {
        info!(
            "Pipeline completed successfully for ticket {}, updating status to 'completed'",
            ticket.ticket_id
        );

        // Update ticket status to completed
        if let Err(e) = tickets::update_ticket_status(
            pool,
            &ticket.organization,
            &ticket.epic_id,
            &ticket.slice_id,
            &ticket.ticket_id,
            "completed",
        )
        .await
        {
            error!(
                "Failed to update ticket status to completed: {}",
                e
            );
        }

        return Ok(PipelineProgressResult::PipelineCompleted);
    }

    Ok(PipelineProgressResult::NoNextStep)
}

/// Start executing a specific step (used for both first step and approved manual steps).
/// For auto steps: spawns the agent immediately.
/// For manual steps that are Queued: marks as awaiting approval.
/// For manual steps that were just approved (Queued after approval): spawns the agent.
pub async fn start_step_execution(
    pool: &SqlitePool,
    ticket_id: &str,
    step_id: &str,
) -> Result<PipelineProgressResult> {
    let ticket = tickets::get_ticket_by_id(pool, ticket_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Ticket not found: {}", ticket_id))?;

    let pipeline = match &ticket.pipeline {
        Some(p) => p,
        None => return Ok(PipelineProgressResult::NoNextStep),
    };

    let step_idx = pipeline
        .steps
        .iter()
        .position(|s| s.step_id == step_id)
        .ok_or_else(|| anyhow::anyhow!("Step not found: {}", step_id))?;

    let step = &pipeline.steps[step_idx];

    // Only process if step is in Queued status
    if step.status != PipelineStepStatus::Queued {
        info!(
            "Step {} is not queued (status: {:?}), skipping execution",
            step_id, step.status
        );
        return Ok(PipelineProgressResult::NoNextStep);
    }

    // Handle based on execution type
    match step.execution_type {
        ExecutionType::Auto => {
            // Spawn agent for auto step
            spawn_agent_for_step(pool, &ticket, step_idx, 0).await
        }
        ExecutionType::Manual => {
            // Mark as awaiting approval
            mark_step_awaiting_approval(pool, &ticket, step_idx).await
        }
    }
}

