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
    RunAgentRequest, RunAgentResponse, SendMessageRequest, HookConfig,
};
use super::{
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

    let context = build_ticket_context(&epic_id, &slice_id, &ticket_id, ticket.title, ticket.intent.clone());

    let (previous_output, selected_context, sender_info) = gather_agent_context(
        &db,
        &req.agent_type,
        req.previous_session_id.as_deref(),
        &req.selected_session_ids,
        ticket.assignee.as_deref(),
    ).await;

    let working_dir = PathBuf::from("/Users/jarvisgpt/projects");
    let executor = AgentExecutor::new(working_dir);

    let agent_run = executor
        .execute(req.agent_type, context, previous_output, selected_context, sender_info, None, None)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Agent execution failed: {}", e)))?;

    store_agent_run(&db, &agent_run)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to store agent run: {}", e)))?;

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
            input_message: ticket.intent.clone(),
        };
        if let Err(e) = ticketing_system::agent_runs::create_agent_run(&db, create_req).await {
            tracing::error!("Failed to store running agent state: {}", e);
        }
    }

    let session_id_clone = session_id.clone();

    // Spawn agent execution in background
    tokio::spawn(async move {
        match ticket_result {
            Ok(Some(ticket)) => {
                let context = build_ticket_context(
                    &epic_id, &slice_id, &ticket_id, ticket.title, ticket.intent.clone()
                );

                let working_dir = PathBuf::from("/Users/jarvisgpt/projects");
                let executor = AgentExecutor::new(working_dir);

                let _ = tx.send(StreamEvent::Status {
                    status: "running".to_string(),
                    message: Some(format!("Agent started (session: {})", session_id_clone)),
                }).await;

                let (previous_output, selected_context, sender_info) = gather_agent_context(
                    &db_clone,
                    &req.agent_type,
                    req.previous_session_id.as_deref(),
                    &req.selected_session_ids,
                    ticket.assignee.as_deref(),
                ).await;

                let agent_type_for_error = req.agent_type.clone();

                // Create hook config with db connection for direct tool result storage
                // Start event index at 10000 to avoid collision with normal stream events
                let hook_config = HookConfig {
                    db: (*db_clone).clone(),
                    session_id: session_id_clone.clone(),
                    event_index_start: 10000,
                };

                match executor.execute(req.agent_type, context, previous_output, selected_context, sender_info, Some(tx.clone()), Some(hook_config)).await {
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
            Ok(Some(_run)) => {
                let working_dir = PathBuf::from("/Users/jarvisgpt/projects");
                let executor = AgentExecutor::new(working_dir);

                let _ = tx.send(StreamEvent::Status {
                    status: "running".to_string(),
                    message: Some("Processing follow-up message...".to_string()),
                }).await;

                // Create hook config for direct tool result storage
                let hook_config = HookConfig {
                    db: (*db_clone).clone(),
                    session_id: session_id_clone.clone(),
                    event_index_start: 10000,
                };

                match executor.resume(&session_id_clone, &req.message, Some(tx.clone()), Some(hook_config)).await {
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
