use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::SqlitePool;
use std::sync::Arc;
use tracing::{error, info};

use ticketing_system::{
    models::{
        CreatePipelineTemplateRequest, Pipeline, PipelineStep, PipelineStepStatus,
        PipelineTemplateStep,
    },
    pipelines, tickets,
};

use crate::pipeline_automation;

// ============================================================================
// Request/Response Types
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct ListTemplatesQuery {
    pub organization: Option<String>,
    pub epic_id: Option<String>,
    pub slice_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateTemplateRequest {
    pub template_id: String,
    pub name: String,
    pub description: Option<String>,
    pub organization: Option<String>,
    pub epic_id: Option<String>,
    pub slice_id: Option<String>,
    pub steps: Vec<PipelineTemplateStep>,
}

#[derive(Debug, Deserialize)]
pub struct SetPipelineRequest {
    /// Apply a template by ID
    pub template_id: Option<String>,
    /// OR provide a custom pipeline
    pub pipeline: Option<Pipeline>,
    /// Optional step input overrides (when using template)
    pub step_inputs: Option<std::collections::HashMap<String, serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
pub struct StartStepRequest {
    pub agent_run_id: String,
}

#[derive(Debug, Deserialize)]
pub struct CompleteStepRequest {
    pub outputs: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct FailStepRequest {
    pub error: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct RejectStepRequest {
    pub feedback: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PipelineResponse {
    pub pipeline: Pipeline,
}

#[derive(Debug, Serialize)]
pub struct StepResponse {
    pub step: PipelineStep,
    pub pipeline_status: Option<String>,
}

// ============================================================================
// Pipeline Template Handlers
// ============================================================================

/// GET /api/pipeline-templates
pub async fn list_templates(
    State(pool): State<Arc<SqlitePool>>,
    Query(params): Query<ListTemplatesQuery>,
) -> Response {
    match pipelines::list_templates(
        &pool,
        params.organization.as_deref(),
        params.epic_id.as_deref(),
        params.slice_id.as_deref(),
    )
    .await
    {
        Ok(templates) => (StatusCode::OK, Json(json!({ "templates": templates }))).into_response(),
        Err(e) => {
            error!("Failed to list pipeline templates: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to list templates: {}", e) })),
            )
                .into_response()
        }
    }
}

/// GET /api/pipeline-templates/:template_id
pub async fn get_template(
    State(pool): State<Arc<SqlitePool>>,
    Path(template_id): Path<String>,
) -> Response {
    match pipelines::get_template(&pool, &template_id).await {
        Ok(Some(template)) => (StatusCode::OK, Json(template)).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Template not found" })),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to get pipeline template: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to get template: {}", e) })),
            )
                .into_response()
        }
    }
}

/// POST /api/pipeline-templates
pub async fn create_template(
    State(pool): State<Arc<SqlitePool>>,
    Json(request): Json<CreateTemplateRequest>,
) -> Response {
    let req = CreatePipelineTemplateRequest {
        template_id: request.template_id,
        name: request.name,
        description: request.description,
        organization: request.organization,
        epic_id: request.epic_id,
        slice_id: request.slice_id,
        steps: request.steps,
    };

    match pipelines::create_template(&pool, req).await {
        Ok(template) => {
            info!("Created pipeline template: {}", template.template_id);
            (StatusCode::CREATED, Json(template)).into_response()
        }
        Err(e) => {
            error!("Failed to create pipeline template: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to create template: {}", e) })),
            )
                .into_response()
        }
    }
}

/// DELETE /api/pipeline-templates/:template_id
pub async fn delete_template(
    State(pool): State<Arc<SqlitePool>>,
    Path(template_id): Path<String>,
) -> Response {
    match pipelines::delete_template(&pool, &template_id).await {
        Ok(()) => {
            info!("Deleted pipeline template: {}", template_id);
            (StatusCode::OK, Json(json!({ "deleted": template_id }))).into_response()
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("not found") {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "Template not found" })),
                )
                    .into_response()
            } else {
                error!("Failed to delete pipeline template: {:?}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("Failed to delete template: {}", e) })),
                )
                    .into_response()
            }
        }
    }
}

// ============================================================================
// Ticket Pipeline Handlers
// ============================================================================

/// GET /api/tickets/:ticket_id/pipeline
pub async fn get_ticket_pipeline(
    State(pool): State<Arc<SqlitePool>>,
    Path(ticket_id): Path<String>,
) -> Response {
    match tickets::get_ticket_by_id(&pool, &ticket_id).await {
        Ok(Some(ticket)) => match ticket.pipeline {
            Some(pipeline) => (StatusCode::OK, Json(PipelineResponse { pipeline })).into_response(),
            None => (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Ticket has no pipeline" })),
            )
                .into_response(),
        },
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Ticket not found" })),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to get ticket pipeline: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to get pipeline: {}", e) })),
            )
                .into_response()
        }
    }
}

/// POST /api/tickets/:ticket_id/pipeline
pub async fn set_ticket_pipeline(
    State(pool): State<Arc<SqlitePool>>,
    Path(ticket_id): Path<String>,
    Json(request): Json<SetPipelineRequest>,
) -> Response {
    // First verify the ticket exists
    match tickets::get_ticket_by_id(&pool, &ticket_id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Ticket not found" })),
            )
                .into_response()
        }
        Err(e) => {
            error!("Failed to get ticket: {:?}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to get ticket: {}", e) })),
            )
                .into_response();
        }
    }

    // Resolve pipeline: from template or custom
    let pipeline = if let Some(template_id) = request.template_id {
        // Create from template
        match tickets::attach_pipeline_from_template(
            &pool,
            &ticket_id,
            &template_id,
            request.step_inputs.as_ref(),
        )
        .await
        {
            Ok(p) => p,
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("not found") {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(json!({ "error": "Template not found" })),
                    )
                        .into_response();
                }
                error!("Failed to attach pipeline from template: {:?}", e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("Failed to set pipeline: {}", e) })),
                )
                    .into_response();
            }
        }
    } else if let Some(pipeline) = request.pipeline {
        // Use custom pipeline
        if let Err(e) = tickets::update_ticket_pipeline(&pool, &ticket_id, Some(&pipeline)).await {
            error!("Failed to set custom pipeline: {:?}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to set pipeline: {}", e) })),
            )
                .into_response();
        }
        pipeline
    } else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Either template_id or pipeline must be provided" })),
        )
            .into_response();
    };

    info!("Set pipeline on ticket {}", ticket_id);
    (StatusCode::OK, Json(PipelineResponse { pipeline })).into_response()
}

/// DELETE /api/tickets/:ticket_id/pipeline
pub async fn delete_ticket_pipeline(
    State(pool): State<Arc<SqlitePool>>,
    Path(ticket_id): Path<String>,
) -> Response {
    // Verify ticket exists
    match tickets::get_ticket_by_id(&pool, &ticket_id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Ticket not found" })),
            )
                .into_response();
        }
        Err(e) => {
            error!("Failed to get ticket: {:?}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to get ticket: {}", e) })),
            )
                .into_response();
        }
    }

    if let Err(e) = tickets::update_ticket_pipeline(&pool, &ticket_id, None).await {
        error!("Failed to remove pipeline: {:?}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to remove pipeline: {}", e) })),
        )
            .into_response();
    }

    info!("Removed pipeline from ticket {}", ticket_id);
    (StatusCode::OK, Json(json!({ "deleted": true }))).into_response()
}

// ============================================================================
// Step Operation Handlers
// ============================================================================

/// Helper to get ticket and validate step exists
async fn get_ticket_and_step(
    pool: &SqlitePool,
    ticket_id: &str,
    step_id: &str,
) -> Result<(ticketing_system::models::Ticket, usize), Response> {
    let ticket = match tickets::get_ticket_by_id(pool, ticket_id).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Ticket not found" })),
            )
                .into_response())
        }
        Err(e) => {
            error!("Failed to get ticket: {:?}", e);
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to get ticket: {}", e) })),
            )
                .into_response());
        }
    };

    let pipeline = match &ticket.pipeline {
        Some(p) => p,
        None => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Ticket has no pipeline" })),
            )
                .into_response())
        }
    };

    let step_idx = match pipeline.steps.iter().position(|s| s.step_id == step_id) {
        Some(idx) => idx,
        None => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Step not found in pipeline" })),
            )
                .into_response())
        }
    };

    Ok((ticket, step_idx))
}

/// POST /api/tickets/:ticket_id/pipeline/steps/:step_id/start
pub async fn start_step(
    State(pool): State<Arc<SqlitePool>>,
    Path((ticket_id, step_id)): Path<(String, String)>,
    Json(request): Json<StartStepRequest>,
) -> Response {
    let (mut ticket, step_idx) = match get_ticket_and_step(&pool, &ticket_id, &step_id).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    let pipeline = ticket.pipeline.as_mut().unwrap();
    let step = &pipeline.steps[step_idx];

    // Validate state transition: can only start from Queued
    if step.status != PipelineStepStatus::Queued {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("Cannot start step in {:?} status, must be Queued", step.status)
            })),
        )
            .into_response();
    }

    pipelines::start_step(pipeline, &step_id, &request.agent_run_id);

    // Save updated pipeline
    if let Err(e) = tickets::update_ticket_pipeline(&pool, &ticket_id, Some(pipeline)).await {
        error!("Failed to update pipeline after start_step: {:?}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to update pipeline: {}", e) })),
        )
            .into_response();
    }

    let step = pipeline.steps[step_idx].clone();
    info!("Started step {} on ticket {}", step_id, ticket_id);
    (
        StatusCode::OK,
        Json(StepResponse {
            step,
            pipeline_status: pipeline.status.clone(),
        }),
    )
        .into_response()
}

/// POST /api/tickets/:ticket_id/pipeline/steps/:step_id/complete
pub async fn complete_step(
    State(pool): State<Arc<SqlitePool>>,
    Path((ticket_id, step_id)): Path<(String, String)>,
    Json(request): Json<CompleteStepRequest>,
) -> Response {
    let (mut ticket, step_idx) = match get_ticket_and_step(&pool, &ticket_id, &step_id).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    let pipeline = ticket.pipeline.as_mut().unwrap();
    let step = &pipeline.steps[step_idx];

    // Validate state transition: can complete from Running
    if step.status != PipelineStepStatus::Running {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("Cannot complete step in {:?} status, must be Running", step.status)
            })),
        )
            .into_response();
    }

    pipelines::complete_step(pipeline, &step_id, request.outputs);

    // Save updated pipeline
    if let Err(e) = tickets::update_ticket_pipeline(&pool, &ticket_id, Some(pipeline)).await {
        error!("Failed to update pipeline after complete_step: {:?}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to update pipeline: {}", e) })),
        )
            .into_response();
    }

    let step = pipeline.steps[step_idx].clone();
    info!("Completed step {} on ticket {}", step_id, ticket_id);

    // Trigger automation to process next step
    let pool_clone = pool.clone();
    let ticket_id_clone = ticket_id.clone();
    let step_id_clone = step_id.clone();
    tokio::spawn(async move {
        match pipeline_automation::process_next_step(&pool_clone, &ticket_id_clone, &step_id_clone, 0).await {
            Ok(result) => {
                info!("Pipeline automation result for ticket {}: {:?}", ticket_id_clone, result);
            }
            Err(e) => {
                error!("Pipeline automation failed for ticket {}: {:?}", ticket_id_clone, e);
            }
        }
    });

    (
        StatusCode::OK,
        Json(StepResponse {
            step,
            pipeline_status: pipeline.status.clone(),
        }),
    )
        .into_response()
}

/// POST /api/tickets/:ticket_id/pipeline/steps/:step_id/fail
pub async fn fail_step(
    State(pool): State<Arc<SqlitePool>>,
    Path((ticket_id, step_id)): Path<(String, String)>,
    Json(request): Json<FailStepRequest>,
) -> Response {
    let (mut ticket, step_idx) = match get_ticket_and_step(&pool, &ticket_id, &step_id).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    let pipeline = ticket.pipeline.as_mut().unwrap();
    let step = &pipeline.steps[step_idx];

    // Validate state transition: can fail from Running
    if step.status != PipelineStepStatus::Running {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("Cannot fail step in {:?} status, must be Running", step.status)
            })),
        )
            .into_response();
    }

    pipelines::fail_step(pipeline, &step_id, request.error);

    // Save updated pipeline
    if let Err(e) = tickets::update_ticket_pipeline(&pool, &ticket_id, Some(pipeline)).await {
        error!("Failed to update pipeline after fail_step: {:?}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to update pipeline: {}", e) })),
        )
            .into_response();
    }

    let step = pipeline.steps[step_idx].clone();
    info!("Failed step {} on ticket {}", step_id, ticket_id);
    (
        StatusCode::OK,
        Json(StepResponse {
            step,
            pipeline_status: pipeline.status.clone(),
        }),
    )
        .into_response()
}

/// POST /api/tickets/:ticket_id/pipeline/steps/:step_id/approve
pub async fn approve_step(
    State(pool): State<Arc<SqlitePool>>,
    Path((ticket_id, step_id)): Path<(String, String)>,
) -> Response {
    let (mut ticket, step_idx) = match get_ticket_and_step(&pool, &ticket_id, &step_id).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    let pipeline = ticket.pipeline.as_mut().unwrap();
    let step = &pipeline.steps[step_idx];

    // Validate state transition: can only approve from AwaitingApproval
    if step.status != PipelineStepStatus::AwaitingApproval {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("Cannot approve step in {:?} status, must be AwaitingApproval", step.status)
            })),
        )
            .into_response();
    }

    pipelines::approve_step(pipeline, &step_id);

    // Save updated pipeline
    if let Err(e) = tickets::update_ticket_pipeline(&pool, &ticket_id, Some(pipeline)).await {
        error!("Failed to update pipeline after approve_step: {:?}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to update pipeline: {}", e) })),
        )
            .into_response();
    }

    let step = pipeline.steps[step_idx].clone();
    info!("Approved step {} on ticket {}", step_id, ticket_id);

    // Trigger automation to execute the approved step
    let pool_clone = pool.clone();
    let ticket_id_clone = ticket_id.clone();
    let step_id_clone = step_id.clone();
    tokio::spawn(async move {
        match pipeline_automation::execute_approved_step(&pool_clone, &ticket_id_clone, &step_id_clone).await {
            Ok(result) => {
                info!("Approved step execution result for ticket {}: {:?}", ticket_id_clone, result);
            }
            Err(e) => {
                error!("Approved step execution failed for ticket {}: {:?}", ticket_id_clone, e);
            }
        }
    });

    (
        StatusCode::OK,
        Json(StepResponse {
            step,
            pipeline_status: pipeline.status.clone(),
        }),
    )
        .into_response()
}

/// POST /api/tickets/:ticket_id/pipeline/steps/:step_id/reject
pub async fn reject_step(
    State(pool): State<Arc<SqlitePool>>,
    Path((ticket_id, step_id)): Path<(String, String)>,
    Json(request): Json<RejectStepRequest>,
) -> Response {
    let (mut ticket, step_idx) = match get_ticket_and_step(&pool, &ticket_id, &step_id).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    let pipeline = ticket.pipeline.as_mut().unwrap();
    let step = &pipeline.steps[step_idx];

    // Validate state transition: can only reject from AwaitingApproval
    if step.status != PipelineStepStatus::AwaitingApproval {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("Cannot reject step in {:?} status, must be AwaitingApproval", step.status)
            })),
        )
            .into_response();
    }

    // Mark step as failed with rejection feedback
    let error = request
        .feedback
        .map(|f| json!({ "rejected": true, "feedback": f }))
        .unwrap_or_else(|| json!({ "rejected": true }));

    pipelines::fail_step(pipeline, &step_id, Some(error));

    // Save updated pipeline
    if let Err(e) = tickets::update_ticket_pipeline(&pool, &ticket_id, Some(pipeline)).await {
        error!("Failed to update pipeline after reject_step: {:?}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to update pipeline: {}", e) })),
        )
            .into_response();
    }

    let step = pipeline.steps[step_idx].clone();
    info!("Rejected step {} on ticket {}", step_id, ticket_id);
    (
        StatusCode::OK,
        Json(StepResponse {
            step,
            pipeline_status: pipeline.status.clone(),
        }),
    )
        .into_response()
}

/// POST /api/tickets/:ticket_id/pipeline/steps/:step_id/retry
/// Retry a failed or skipped step: resets it to queued, un-skips downstream steps,
/// cleans up old agent runs/events, and auto-starts the step.
pub async fn retry_step(
    State(pool): State<Arc<SqlitePool>>,
    Path((ticket_id, step_id)): Path<(String, String)>,
) -> Response {
    let (mut ticket, step_idx) = match get_ticket_and_step(&pool, &ticket_id, &step_id).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    let pipeline = ticket.pipeline.as_mut().unwrap();
    let step = &pipeline.steps[step_idx];

    // Validate state: can only retry failed or skipped steps
    if step.status != PipelineStepStatus::Failed && step.status != PipelineStepStatus::Skipped {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("Cannot retry step in {:?} status, must be Failed or Skipped", step.status)
            })),
        )
            .into_response();
    }

    let agent_type = step.agent_type.clone();

    // Reset the step and un-skip downstream steps
    if !pipelines::retry_step(pipeline, &step_id) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "Failed to reset step" })),
        )
            .into_response();
    }

    // Save updated pipeline
    if let Err(e) = tickets::update_ticket_pipeline(&pool, &ticket_id, Some(pipeline)).await {
        error!("Failed to update pipeline after retry_step: {:?}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to update pipeline: {}", e) })),
        )
            .into_response();
    }

    // Clean up old agent runs and events for this step's agent type
    match ticketing_system::agent_runs::delete_runs_for_ticket_agent(&pool, &ticket_id, &agent_type).await {
        Ok(count) if count > 0 => {
            info!("Cleaned up {} old agent run(s) for retry of {} on ticket {}", count, agent_type, ticket_id);
        }
        Err(e) => {
            error!("Failed to clean up old agent runs for retry: {:?}", e);
            // Non-fatal, continue with retry
        }
        _ => {}
    }

    info!("Retrying step {} on ticket {}", step_id, ticket_id);

    // Auto-start the retried step via pipeline automation
    let pool_clone = pool.clone();
    let ticket_id_clone = ticket_id.clone();
    let step_id_clone = step_id.clone();
    let session_id = match pipeline_automation::start_step_execution(&pool_clone, &ticket_id_clone, &step_id_clone).await {
        Ok(pipeline_automation::PipelineProgressResult::AgentSpawned { session_id, .. }) => {
            Some(session_id)
        }
        Ok(pipeline_automation::PipelineProgressResult::AwaitingApproval { .. }) => {
            None
        }
        Ok(other) => {
            info!("Retry step result: {:?}", other);
            None
        }
        Err(e) => {
            error!("Failed to auto-start retried step: {:?}", e);
            // Step is reset to queued, user can start it manually
            None
        }
    };

    // Re-read ticket to get the latest pipeline state after automation
    let (step, pipeline_status) = match tickets::get_ticket_by_id(&pool, &ticket_id).await {
        Ok(Some(t)) if t.pipeline.is_some() => {
            let p = t.pipeline.unwrap();
            let s = p.steps.get(step_idx).cloned();
            (s, p.status)
        }
        _ => (None, None)
    };
    let step = step.unwrap_or_else(|| {
        ticket.pipeline.as_ref().unwrap().steps[step_idx].clone()
    });
    let pipeline_status = pipeline_status.or_else(|| {
        ticket.pipeline.as_ref().unwrap().status.clone()
    });

    (
        StatusCode::OK,
        Json(json!({
            "step": step,
            "pipeline_status": pipeline_status,
            "session_id": session_id,
            "retried": true
        })),
    )
        .into_response()
}

// ============================================================================
// Agent Run Details Handler
// ============================================================================

/// GET /api/tickets/:ticket_id/pipeline/steps/:step_id/agent-run
pub async fn get_step_agent_run(
    State(pool): State<Arc<SqlitePool>>,
    Path((ticket_id, step_id)): Path<(String, String)>,
) -> Response {
    let (ticket, step_idx) = match get_ticket_and_step(&pool, &ticket_id, &step_id).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    let pipeline = ticket.pipeline.as_ref().unwrap();
    let step = &pipeline.steps[step_idx];

    let agent_run_id = match &step.agent_run_id {
        Some(id) => id,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Step has no associated agent run" })),
            )
                .into_response()
        }
    };

    // Get the agent run
    match ticketing_system::agent_runs::get_agent_run(&pool, agent_run_id).await {
        Ok(Some(run)) => {
            // Also get the events for full history
            match ticketing_system::agent_runs::get_events(&pool, agent_run_id).await {
                Ok(events) => (
                    StatusCode::OK,
                    Json(json!({
                        "agent_run": run,
                        "events": events
                    })),
                )
                    .into_response(),
                Err(e) => {
                    error!("Failed to get agent run events: {:?}", e);
                    // Return agent run without events
                    (StatusCode::OK, Json(json!({ "agent_run": run }))).into_response()
                }
            }
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Agent run not found" })),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to get agent run: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to get agent run: {}", e) })),
            )
                .into_response()
        }
    }
}

// ============================================================================
// Pipeline Execution Handler
// ============================================================================

/// Response for pipeline run endpoint
#[derive(Debug, Serialize)]
pub struct RunPipelineResponse {
    pub started: bool,
    pub first_step_id: Option<String>,
    pub session_id: Option<String>,
    pub message: String,
}

/// POST /api/tickets/:ticket_id/pipeline/run
/// Start executing a pipeline from the first step.
/// If the first step is auto, it will be executed immediately.
/// If the first step is manual, it will be marked as awaiting approval.
pub async fn run_pipeline(
    State(pool): State<Arc<SqlitePool>>,
    Path(ticket_id): Path<String>,
) -> Response {
    // Get ticket and pipeline
    let ticket = match tickets::get_ticket_by_id(&pool, &ticket_id).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Ticket not found" })),
            )
                .into_response();
        }
        Err(e) => {
            error!("Failed to get ticket: {:?}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to get ticket: {}", e) })),
            )
                .into_response();
        }
    };

    let pipeline = match &ticket.pipeline {
        Some(p) => p,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Ticket has no pipeline" })),
            )
                .into_response();
        }
    };

    // Check if pipeline is already running or complete
    if let Some(status) = &pipeline.status {
        if status == "running" {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Pipeline is already running" })),
            )
                .into_response();
        }
        if status == "completed" {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Pipeline is already completed" })),
            )
                .into_response();
        }
    }

    // Check if there's a first step to execute
    if pipeline.steps.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Pipeline has no steps" })),
        )
            .into_response();
    }

    let first_step = &pipeline.steps[0];

    // Only start if the first step is queued
    if first_step.status != PipelineStepStatus::Queued {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("First step is not queued (status: {:?})", first_step.status)
            })),
        )
            .into_response();
    }

    let first_step_id = first_step.step_id.clone();

    // Trigger automation for the first step
    // We use a dummy "previous step" since there is none - the automation will find the first queued step
    let pool_clone = pool.clone();
    let ticket_id_clone = ticket_id.clone();
    let first_step_id_clone = first_step_id.clone();

    // For the first step, we use start_step_execution which handles both auto and manual steps
    let result = match pipeline_automation::start_step_execution(&pool_clone, &ticket_id_clone, &first_step_id_clone).await {
        Ok(result) => result,
        Err(e) => {
            error!("Failed to start pipeline for ticket {}: {:?}", ticket_id, e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to start pipeline: {}", e) })),
            )
                .into_response();
        }
    };

    let (session_id, message) = match result {
        pipeline_automation::PipelineProgressResult::AgentSpawned { step_id, session_id } => {
            (Some(session_id), format!("Started auto step: {}", step_id))
        }
        pipeline_automation::PipelineProgressResult::AwaitingApproval { step_id } => {
            (None, format!("Step {} is awaiting approval", step_id))
        }
        pipeline_automation::PipelineProgressResult::PipelineCompleted => {
            (None, "Pipeline completed".to_string())
        }
        pipeline_automation::PipelineProgressResult::PipelineFailed { reason } => {
            (None, format!("Pipeline failed: {}", reason))
        }
        other => {
            (None, format!("Unexpected result: {:?}", other))
        }
    };

    info!("Started pipeline for ticket {}: {}", ticket_id, message);

    (
        StatusCode::OK,
        Json(RunPipelineResponse {
            started: true,
            first_step_id: Some(first_step_id),
            session_id,
            message,
        }),
    )
        .into_response()
}
