use axum::{
    extract::{Path, State},
    response::sse::{Event, KeepAlive, Sse},
    http::StatusCode,
    Json,
};
use futures::stream::Stream;
use std::convert::Infallible;
use std::sync::Arc;
use std::path::PathBuf;
use tokio::sync::mpsc;
use sqlx::SqlitePool;

use crate::agents::{
    AgentExecutor, AgentRun, AgentRunsResponse, StreamEvent,
    RunAgentRequest, RunAgentResponse, SendMessageRequest,
    resolve_working_dir,
};
use crate::pipeline_automation;
use super::{
    artifacts::write_artifact,
    context::{build_ticket_context, gather_agent_context},
    conversions::{db_run_to_api_run, store_agent_run},
    sse_helpers::{create_sse_stream, create_reconnect_stream, create_error_stream},
};

/// POST /api/epics/:epic_id/slices/:slice_id/tickets/:ticket_id/agent-runs
pub async fn run_agent(
    Path((epic_id, slice_id, ticket_id)): Path<(String, String, String)>,
    State(db): State<Arc<SqlitePool>>,
    Json(req): Json<RunAgentRequest>,
) -> Result<Json<RunAgentResponse>, (StatusCode, String)> {
    let ticket = ticketing_system::tickets::get_ticket_by_id(&db, &ticket_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Database error: {}", e)))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Ticket not found".to_string()))?;

    let context = build_ticket_context(&epic_id, &slice_id, &ticket_id, ticket.title, ticket.description.clone().unwrap_or_default());

    let (previous_output, selected_context, sender_info, blocked_by_context) = gather_agent_context(
        &db,
        &req.agent_type,
        &ticket_id,
        req.previous_session_id.as_deref(),
        &req.selected_session_ids,
        ticket.assignee.as_deref(),
    ).await;

    // Combine blocked_by context with previous output if both exist
    let combined_previous = match (blocked_by_context, previous_output) {
        (Some(blocked), Some(prev)) => Some(format!("{}\n\n{}", blocked, prev)),
        (Some(blocked), None) => Some(blocked),
        (None, Some(prev)) => Some(prev),
        (None, None) => None,
    };

    let working_dir = resolve_working_dir(&db, &req.agent_type, &ticket.organization)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to resolve working dir: {}", e)))?;
    let executor = AgentExecutor::new(working_dir);

    let agent_run = executor
        .execute(req.agent_type, context, combined_previous, selected_context, sender_info, None)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Agent execution failed: {}", e)))?;

    store_agent_run(&db, &agent_run)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to store agent run: {}", e)))?;

    // Write artifact to repository if agent completed successfully
    if agent_run.status == crate::agents::AgentRunStatus::Completed {
        if let Some(ref output) = agent_run.output_summary {
            if let Some(artifact_path) = write_artifact(
                &db,
                &ticket_id,
                agent_run.agent_type.as_str(),
                output,
            ).await {
                tracing::info!("Artifact written to {}", artifact_path);
            }
        }
    }

    Ok(Json(RunAgentResponse {
        session_id: agent_run.session_id,
        status: agent_run.status.as_str().to_string(),
    }))
}

/// GET /api/epics/:epic_id/slices/:slice_id/tickets/:ticket_id/agent-runs
pub async fn list_agent_runs(
    Path((epic_id, slice_id, ticket_id)): Path<(String, String, String)>,
    State(db): State<Arc<SqlitePool>>,
) -> Result<Json<AgentRunsResponse>, (StatusCode, String)> {
    let db_runs = ticketing_system::agent_runs::list_agent_runs(&db, &epic_id, &slice_id, &ticket_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to query agent runs: {}", e)))?;

    let runs: Vec<AgentRun> = db_runs.into_iter().map(db_run_to_api_run).collect();
    Ok(Json(AgentRunsResponse { runs }))
}

/// GET /api/agent-runs/:session_id
pub async fn get_agent_run(
    Path(session_id): Path<String>,
    State(db): State<Arc<SqlitePool>>,
) -> Result<Json<AgentRun>, (StatusCode, String)> {
    let db_run = ticketing_system::agent_runs::get_agent_run(&db, &session_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Database error: {}", e)))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Agent run not found".to_string()))?;

    Ok(Json(db_run_to_api_run(db_run)))
}

/// POST /api/epics/:epic_id/slices/:slice_id/tickets/:ticket_id/agent-runs/stream
pub async fn stream_agent_run(
    Path((epic_id, slice_id, ticket_id)): Path<(String, String, String)>,
    State(db): State<Arc<SqlitePool>>,
    Json(req): Json<RunAgentRequest>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    tracing::info!("=== STREAM_AGENT_RUN START ===");
    tracing::info!("Ticket: {}/{}/{}", epic_id, slice_id, ticket_id);

    let (tx, rx) = mpsc::channel::<StreamEvent>(100);
    let ticket_result = ticketing_system::tickets::get_ticket_by_id(&db, &ticket_id).await;
    let db_clone = db.clone();

    // Generate session_id upfront
    let session_id = uuid::Uuid::new_v4().to_string();
    let started_at = chrono::Utc::now().to_rfc3339();

    // Store agent run with "running" status before execution
    if let Ok(Some(ref ticket)) = ticket_result {
        let create_req = ticketing_system::CreateAgentRunRequest {
            session_id: session_id.clone(),
            epic_id: epic_id.clone(),
            slice_id: slice_id.clone(),
            ticket_id: ticket_id.clone(),
            agent_type: req.agent_type.as_str().to_string(),
            input_message: ticket.description.clone().unwrap_or_default(),
        };
        if let Err(e) = ticketing_system::agent_runs::create_agent_run(&db, create_req).await {
            tracing::error!("Failed to store running agent state: {}", e);
        }
    }

    let session_id_clone = session_id.clone();

    // Spawn agent execution in background
    let custom_input_message = req.custom_input_message.clone();
    let step_id = req.step_id.clone();
    tokio::spawn(async move {
        match ticket_result {
            Ok(Some(ticket)) => {
                // If step_id is provided, transition the pipeline step to Running
                if let Some(ref sid) = step_id {
                    if let Ok(Some(t)) = ticketing_system::tickets::get_ticket_by_id(&db_clone, &ticket_id).await {
                        if let Some(mut pipeline) = t.pipeline {
                            if let Some(step) = pipeline.steps.iter().find(|s| s.step_id == *sid) {
                                let step_status = step.status.clone();
                                match step_status {
                                    ticketing_system::models::PipelineStepStatus::AwaitingApproval => {
                                        // Approve + start in one shot
                                        ticketing_system::pipelines::approve_step(&mut pipeline, sid);
                                        ticketing_system::pipelines::start_step(&mut pipeline, sid, &session_id_clone);
                                        if let Err(e) = ticketing_system::tickets::update_ticket_pipeline(&db_clone, &ticket_id, Some(&pipeline)).await {
                                            tracing::error!("Failed to transition step {} to running: {}", sid, e);
                                        } else {
                                            tracing::info!("Pipeline step {} transitioned AwaitingApproval → Running for ticket {}", sid, ticket_id);
                                        }
                                    }
                                    ticketing_system::models::PipelineStepStatus::Queued => {
                                        ticketing_system::pipelines::start_step(&mut pipeline, sid, &session_id_clone);
                                        if let Err(e) = ticketing_system::tickets::update_ticket_pipeline(&db_clone, &ticket_id, Some(&pipeline)).await {
                                            tracing::error!("Failed to transition step {} to running: {}", sid, e);
                                        } else {
                                            tracing::info!("Pipeline step {} transitioned Queued → Running for ticket {}", sid, ticket_id);
                                        }
                                    }
                                    other => {
                                        tracing::error!("Cannot start step {} in {:?} status", sid, other);
                                        let _ = tx.send(StreamEvent::Status {
                                            status: "failed".to_string(),
                                            message: Some(format!("Step {} is in {:?} status, cannot start", sid, other)),
                                        }).await;
                                        return;
                                    }
                                }
                            } else {
                                tracing::error!("Step {} not found in pipeline for ticket {}", sid, ticket_id);
                                let _ = tx.send(StreamEvent::Status {
                                    status: "failed".to_string(),
                                    message: Some(format!("Step {} not found in pipeline", sid)),
                                }).await;
                                return;
                            }
                        }
                    }
                }

                // For ticket-assistant, use custom_input_message as the intent with ticket context
                let intent = if req.agent_type == crate::agents::AgentType::TicketAssistant {
                    if let Some(ref question) = custom_input_message {
                        format!(
                            "{}\n\nUser's Question: {}",
                            ticket.description.clone().unwrap_or_default(),
                            question
                        )
                    } else {
                        ticket.description.clone().unwrap_or_default()
                    }
                } else {
                    ticket.description.clone().unwrap_or_default()
                };

                let context = build_ticket_context(
                    &epic_id, &slice_id, &ticket_id, ticket.title, intent
                );

                let working_dir = match resolve_working_dir(&db_clone, &req.agent_type, &ticket.organization).await {
                    Ok(wd) => wd,
                    Err(e) => {
                        let _ = tx.send(StreamEvent::Status {
                            status: "failed".to_string(),
                            message: Some(format!("Failed to resolve working dir: {}", e)),
                        }).await;
                        return;
                    }
                };
                let executor = AgentExecutor::new(working_dir);

                let _ = tx.send(StreamEvent::Status {
                    status: "running".to_string(),
                    message: Some(format!("Agent started (session: {})", session_id_clone)),
                }).await;

                let (previous_output, selected_context, sender_info, blocked_by_context) = gather_agent_context(
                    &db_clone,
                    &req.agent_type,
                    &ticket_id,
                    req.previous_session_id.as_deref(),
                    &req.selected_session_ids,
                    ticket.assignee.as_deref(),
                ).await;

                // Combine blocked_by context with previous output if both exist
                let combined_previous = match (blocked_by_context, previous_output) {
                    (Some(blocked), Some(prev)) => Some(format!("{}\n\n{}", blocked, prev)),
                    (Some(blocked), None) => Some(blocked),
                    (None, Some(prev)) => Some(prev),
                    (None, None) => None,
                };

                let agent_type_for_error = req.agent_type.clone();

                match executor.execute(req.agent_type, context, combined_previous, selected_context, sender_info, Some(tx.clone())).await {
                    Ok(mut agent_run) => {
                        agent_run.session_id = session_id_clone.clone();

                        if let Err(e) = store_agent_run(&db_clone, &agent_run).await {
                            tracing::error!("Failed to store completed agent run: {}", e);
                        }

                        if let Err(e) = ticketing_system::ticket_history::log_agent_run_completed(
                            &db_clone, &ticket_id, &agent_run.session_id,
                            agent_run.agent_type.as_str(), agent_run.status.as_str(),
                        ).await {
                            tracing::warn!("Failed to log agent run to ticket history: {}", e);
                        }

                        // Pipeline step management: use explicit step_id if provided
                        if let Some(ref sid) = step_id {
                            let outputs = agent_run.output_summary.as_ref().map(|s| serde_json::json!({ "summary": s }));
                            match pipeline_automation::advance_pipeline_after_step(
                                &db_clone, &ticket_id, sid, true, outputs
                            ).await {
                                Ok(result) => {
                                    tracing::info!("Pipeline advance result for ticket {}: {:?}", ticket_id, result);
                                }
                                Err(e) => {
                                    tracing::error!("Failed to advance pipeline for ticket {}: {}", ticket_id, e);
                                }
                            }
                        }
                        // When step_id is None: ad-hoc agent run, no pipeline changes

                        // Write artifact to repository if agent completed successfully
                        if agent_run.status == crate::agents::AgentRunStatus::Completed {
                            if let Some(ref output) = agent_run.output_summary {
                                if let Some(artifact_path) = write_artifact(
                                    &db_clone,
                                    &ticket_id,
                                    agent_run.agent_type.as_str(),
                                    output,
                                ).await {
                                    tracing::info!("Artifact written to {}", artifact_path);
                                }
                            }
                        }

                        let _ = tx.send(StreamEvent::Status {
                            status: agent_run.status.as_str().to_string(),
                            message: Some("Agent completed".to_string()),
                        }).await;
                    }
                    Err(e) => {
                        let failed_run = ticketing_system::AgentRun {
                            session_id: session_id_clone.clone(),
                            ticket_id: ticket_id.clone(),
                            epic_id: epic_id.clone(),
                            slice_id: slice_id.clone(),
                            agent_type: agent_type_for_error.as_str().to_string(),
                            status: "failed".to_string(),
                            started_at,
                            completed_at: Some(chrono::Utc::now().to_rfc3339()),
                            input_message: String::new(),
                            output_summary: Some(format!("Agent failed: {}", e)),
                        };

                        let _ = ticketing_system::agent_runs::update_agent_run(&db_clone, &failed_run).await;
                        let _ = ticketing_system::ticket_history::log_agent_run_completed(
                            &db_clone, &ticket_id, &session_id_clone, agent_type_for_error.as_str(), "failed",
                        ).await;

                        // Pipeline step failure: use explicit step_id if provided
                        if let Some(ref sid) = step_id {
                            match pipeline_automation::advance_pipeline_after_step(
                                &db_clone, &ticket_id, sid, false,
                                Some(serde_json::json!({ "error": e.to_string() })),
                            ).await {
                                Ok(result) => {
                                    tracing::info!("Pipeline failure result for ticket {}: {:?}", ticket_id, result);
                                }
                                Err(e) => {
                                    tracing::error!("Failed to update pipeline failure for ticket {}: {}", ticket_id, e);
                                }
                            }
                        }
                        // When step_id is None: ad-hoc agent run, no pipeline changes

                        let _ = tx.send(StreamEvent::Status {
                            status: "failed".to_string(),
                            message: Some(format!("Agent failed: {}", e)),
                        }).await;
                    }
                }
            }
            Ok(None) => {
                let _ = tx.send(StreamEvent::Status {
                    status: "failed".to_string(),
                    message: Some("Ticket not found".to_string()),
                }).await;
            }
            Err(e) => {
                let _ = tx.send(StreamEvent::Status {
                    status: "failed".to_string(),
                    message: Some(format!("Database error: {}", e)),
                }).await;
            }
        }
    });

    let stream = create_sse_stream((*db).clone(), session_id, rx, 0);
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// GET /api/epics/:epic_id/slices/:slice_id/tickets/:ticket_id/agent-runs/active
pub async fn get_active_agent_run(
    Path((epic_id, slice_id, ticket_id)): Path<(String, String, String)>,
    State(db): State<Arc<SqlitePool>>,
) -> Result<Json<AgentRun>, (StatusCode, String)> {
    let db_run = ticketing_system::agent_runs::get_active_agent_run(&db, &epic_id, &slice_id, &ticket_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to query agent runs: {}", e)))?;

    match db_run {
        Some(run) => Ok(Json(db_run_to_api_run(run))),
        None => Err((StatusCode::NOT_FOUND, "No active agent run".to_string())),
    }
}

/// GET /api/agent-runs/:session_id/stream
pub async fn reconnect_agent_stream(
    Path(session_id): Path<String>,
    State(db): State<Arc<SqlitePool>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let run_result = ticketing_system::agent_runs::get_agent_run(&db, &session_id).await;
    let events_result = ticketing_system::agent_runs::get_events(&db, &session_id).await;

    let stream: Box<dyn Stream<Item = Result<Event, Infallible>> + Send + Unpin> = match run_result {
        Ok(Some(run)) => {
            let events = events_result.unwrap_or_default();
            Box::new(Box::pin(create_reconnect_stream(run, events)))
        }
        Ok(None) => Box::new(Box::pin(create_error_stream("Agent run not found".to_string()))),
        Err(e) => Box::new(Box::pin(create_error_stream(format!("Database error: {}", e)))),
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// POST /api/agent-runs/:session_id/message
pub async fn send_message_to_agent(
    Path(session_id): Path<String>,
    State(db): State<Arc<SqlitePool>>,
    Json(req): Json<SendMessageRequest>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    tracing::info!("=== SEND_MESSAGE_TO_AGENT START ===");

    let (tx, rx) = mpsc::channel::<StreamEvent>(100);
    let session_id_clone = session_id.clone();
    let db_clone = db.clone();

    tokio::spawn(async move {
        match ticketing_system::agent_runs::get_agent_run(&db_clone, &session_id_clone).await {
            Ok(Some(run)) => {
                // Resolve working dir from the original agent run's context
                let working_dir = if let Ok(Some(ticket)) = ticketing_system::tickets::get_ticket_by_id(&db_clone, &run.ticket_id).await {
                    if let Ok(agent_type) = serde_json::from_str::<crate::agents::AgentType>(&format!("\"{}\"", run.agent_type)) {
                        resolve_working_dir(&db_clone, &agent_type, &ticket.organization).await.unwrap_or_else(|_| PathBuf::from("/Users/jarvisgpt/projects"))
                    } else {
                        PathBuf::from("/Users/jarvisgpt/projects")
                    }
                } else {
                    PathBuf::from("/Users/jarvisgpt/projects")
                };
                let executor = AgentExecutor::new(working_dir);

                let _ = tx.send(StreamEvent::Status {
                    status: "running".to_string(),
                    message: Some("Processing follow-up message...".to_string()),
                }).await;

                match executor.resume(&session_id_clone, &req.message, Some(tx.clone())).await {
                    Ok(_) => {
                        let _ = tx.send(StreamEvent::Status {
                            status: "completed".to_string(),
                            message: Some("Message processed successfully".to_string()),
                        }).await;
                    }
                    Err(e) => {
                        let _ = tx.send(StreamEvent::Status {
                            status: "failed".to_string(),
                            message: Some(format!("Failed to process message: {}", e)),
                        }).await;
                    }
                }
            }
            Ok(None) => {
                let _ = tx.send(StreamEvent::Status {
                    status: "failed".to_string(),
                    message: Some("Session not found".to_string()),
                }).await;
            }
            Err(e) => {
                let _ = tx.send(StreamEvent::Status {
                    status: "failed".to_string(),
                    message: Some(format!("Database error: {}", e)),
                }).await;
            }
        }
    });

    // Get current max event index
    let initial_index = match ticketing_system::agent_runs::get_events(&db, &session_id).await {
        Ok(events) => events.len() as i32,
        Err(_) => 0,
    };

    let stream = create_sse_stream((*db).clone(), session_id, rx, initial_index);
    Sse::new(stream).keep_alive(KeepAlive::default())
}
