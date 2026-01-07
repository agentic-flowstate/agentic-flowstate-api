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
use tokio_stream::wrappers::ReceiverStream;
use async_stream::stream;
use sqlx::SqlitePool;

use crate::agents::{
    AgentExecutor, AgentType, AgentRun, AgentRunStatus,
    TicketContext, RunAgentRequest, RunAgentResponse, AgentRunsResponse, StreamEvent
};

/// POST /api/epics/:epic_id/slices/:slice_id/tickets/:ticket_id/agent-runs
///
/// Run an agent on a specific ticket.
pub async fn run_agent(
    Path((epic_id, slice_id, ticket_id)): Path<(String, String, String)>,
    State(db): State<Arc<SqlitePool>>,
    Json(req): Json<RunAgentRequest>,
) -> Result<Json<RunAgentResponse>, (StatusCode, String)> {
    // Get ticket details from database
    let ticket = ticketing_system::tickets::get_ticket(&db, &epic_id, &slice_id, &ticket_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Database error: {}", e)))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Ticket not found".to_string()))?;

    let context = TicketContext {
        epic_id: epic_id.clone(),
        slice_id: slice_id.clone(),
        ticket_id: ticket_id.clone(),
        title: ticket.title,
        intent: ticket.intent,
    };

    // Get previous output if chaining from a previous agent run
    let previous_output = if let Some(prev_session_id) = &req.previous_session_id {
        ticketing_system::agent_runs::get_agent_run(&db, prev_session_id)
            .await
            .ok()
            .flatten()
            .and_then(|r| r.output_summary)
    } else {
        None
    };

    // Create executor with working directory
    let working_dir = PathBuf::from("/Users/jarvisgpt/projects");
    let executor = AgentExecutor::new(working_dir);

    // Execute agent (no streaming channel for simple request/response)
    let agent_run = executor
        .execute(req.agent_type, context, previous_output, None)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Agent execution failed: {}", e)))?;

    // Store agent run in SQLite
    store_agent_run(&db, &agent_run)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to store agent run: {}", e)))?;

    Ok(Json(RunAgentResponse {
        session_id: agent_run.session_id,
        status: agent_run.status.as_str().to_string(),
    }))
}

/// GET /api/epics/:epic_id/slices/:slice_id/tickets/:ticket_id/agent-runs
///
/// List all agent runs for a ticket.
pub async fn list_agent_runs(
    Path((epic_id, slice_id, ticket_id)): Path<(String, String, String)>,
    State(db): State<Arc<SqlitePool>>,
) -> Result<Json<AgentRunsResponse>, (StatusCode, String)> {
    let db_runs = ticketing_system::agent_runs::list_agent_runs(&db, &epic_id, &slice_id, &ticket_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to query agent runs: {}", e)))?;

    // Convert to API types
    let runs: Vec<AgentRun> = db_runs.into_iter().map(db_run_to_api_run).collect();

    Ok(Json(AgentRunsResponse { runs }))
}

/// GET /api/agent-runs/:session_id
///
/// Get a specific agent run by session ID.
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
///
/// SSE endpoint for streaming agent output in real-time.
/// Sends structured JSON events for text, tool_use, tool_result, etc.
pub async fn stream_agent_run(
    Path((epic_id, slice_id, ticket_id)): Path<(String, String, String)>,
    State(db): State<Arc<SqlitePool>>,
    Json(req): Json<RunAgentRequest>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (tx, rx) = mpsc::channel::<StreamEvent>(100);

    // Get ticket details
    let ticket_result = ticketing_system::tickets::get_ticket(&db, &epic_id, &slice_id, &ticket_id).await;
    let db_clone = db.clone();

    // Generate session_id upfront so we can store running state before execution
    let session_id = uuid::Uuid::new_v4().to_string();
    let started_at = chrono::Utc::now().to_rfc3339();

    // Store agent run with "running" status BEFORE execution starts
    // This allows frontend to detect running agents on page refresh
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
    let agent_type_clone = req.agent_type.clone();

    // Spawn the agent execution in background
    tokio::spawn(async move {
        match ticket_result {
            Ok(Some(ticket)) => {
                let context = TicketContext {
                    epic_id: epic_id.clone(),
                    slice_id: slice_id.clone(),
                    ticket_id: ticket_id.clone(),
                    title: ticket.title,
                    intent: ticket.intent.clone(),
                };

                let working_dir = PathBuf::from("/Users/jarvisgpt/projects");
                let executor = AgentExecutor::new(working_dir);

                // Send status update with session_id
                let _ = tx.send(StreamEvent::Status {
                    status: "running".to_string(),
                    message: Some(format!("Agent started (session: {})", session_id_clone)),
                }).await;

                // Get previous output for chaining
                let previous_output = if let Some(prev_session_id) = &req.previous_session_id {
                    ticketing_system::agent_runs::get_agent_run(&db_clone, prev_session_id)
                        .await
                        .ok()
                        .flatten()
                        .and_then(|r| r.output_summary)
                } else {
                    None
                };

                let agent_type_for_error = req.agent_type.clone();
                match executor.execute(req.agent_type, context, previous_output, Some(tx.clone())).await {
                    Ok(mut agent_run) => {
                        // Use our pre-generated session_id for consistency
                        agent_run.session_id = session_id_clone.clone();
                        // Store the completed run (updates the running record)
                        let _ = store_agent_run(&db_clone, &agent_run).await;
                        let _ = tx.send(StreamEvent::Status {
                            status: agent_run.status.as_str().to_string(),
                            message: Some("Agent completed".to_string()),
                        }).await;
                    }
                    Err(e) => {
                        // Update the running record to failed status
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

    // Convert channel to SSE stream with JSON serialization
    let stream = stream! {
        let mut rx = ReceiverStream::new(rx);
        while let Some(event) = futures::StreamExt::next(&mut rx).await {
            match serde_json::to_string(&event) {
                Ok(json) => yield Ok(Event::default().data(json)),
                Err(e) => {
                    tracing::error!("Failed to serialize event: {}", e);
                }
            }
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// GET /api/epics/:epic_id/slices/:slice_id/tickets/:ticket_id/agent-runs/active
///
/// Check if there's currently a running agent for this ticket.
/// Returns the active agent run if found, or 404 if none running.
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
///
/// Reconnect endpoint for agent runs.
/// - If agent completed/failed: replays stored events as SSE
/// - If agent still running: returns status indicating it's running (output not available)
pub async fn reconnect_agent_stream(
    Path(session_id): Path<String>,
    State(db): State<Arc<SqlitePool>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let run_result = ticketing_system::agent_runs::get_agent_run(&db, &session_id).await;
    let events_result = ticketing_system::agent_runs::get_events(&db, &session_id).await;

    let stream = stream! {
        match run_result {
            Ok(Some(run)) => {
                if run.status == "running" {
                    // Agent is still running - we can't reconnect to live stream
                    // Just send status indicating it's running
                    let event = StreamEvent::Status {
                        status: "running".to_string(),
                        message: Some("Agent is still running. Output will be available when complete.".to_string()),
                    };
                    if let Ok(json) = serde_json::to_string(&event) {
                        yield Ok(Event::default().data(json));
                    }
                } else {
                    // Agent finished - replay stored events if available
                    if let Ok(events) = events_result {
                        for db_event in events {
                            // The event_data is already serialized JSON, send it directly
                            yield Ok(Event::default().data(db_event.event_data));
                        }
                    } else if let Some(output) = &run.output_summary {
                        // Fallback: send stored output as text event
                        let event = StreamEvent::Text { content: output.clone() };
                        if let Ok(json) = serde_json::to_string(&event) {
                            yield Ok(Event::default().data(json));
                        }
                    }

                    // Send final result event
                    let result_event = StreamEvent::Result {
                        session_id: run.session_id.clone(),
                        status: run.status.clone(),
                        is_error: run.status == "failed",
                    };
                    if let Ok(json) = serde_json::to_string(&result_event) {
                        yield Ok(Event::default().data(json));
                    }
                }
            }
            Ok(None) => {
                let event = StreamEvent::Status {
                    status: "failed".to_string(),
                    message: Some("Agent run not found".to_string()),
                };
                if let Ok(json) = serde_json::to_string(&event) {
                    yield Ok(Event::default().data(json));
                }
            }
            Err(e) => {
                let event = StreamEvent::Status {
                    status: "failed".to_string(),
                    message: Some(format!("Database error: {}", e)),
                };
                if let Ok(json) = serde_json::to_string(&event) {
                    yield Ok(Event::default().data(json));
                }
            }
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}

// Helper functions

async fn store_agent_run(db: &SqlitePool, run: &AgentRun) -> anyhow::Result<()> {
    let db_run = ticketing_system::AgentRun {
        session_id: run.session_id.clone(),
        epic_id: run.epic_id.clone(),
        slice_id: run.slice_id.clone(),
        ticket_id: run.ticket_id.clone(),
        agent_type: run.agent_type.as_str().to_string(),
        status: run.status.as_str().to_string(),
        started_at: run.started_at.clone(),
        completed_at: run.completed_at.clone(),
        input_message: run.input_message.clone(),
        output_summary: run.output_summary.clone(),
    };

    ticketing_system::agent_runs::update_agent_run(db, &db_run).await
}

fn db_run_to_api_run(db_run: ticketing_system::AgentRun) -> AgentRun {
    AgentRun {
        session_id: db_run.session_id,
        ticket_id: db_run.ticket_id,
        epic_id: db_run.epic_id,
        slice_id: db_run.slice_id,
        agent_type: parse_agent_type(&db_run.agent_type),
        status: parse_agent_status(&db_run.status),
        started_at: db_run.started_at,
        completed_at: db_run.completed_at,
        input_message: db_run.input_message,
        output_summary: db_run.output_summary,
    }
}

fn parse_agent_type(s: &str) -> AgentType {
    match s {
        "research" => AgentType::Research,
        "planning" => AgentType::Planning,
        "execution" => AgentType::Execution,
        "evaluation" => AgentType::Evaluation,
        _ => AgentType::Research,
    }
}

fn parse_agent_status(s: &str) -> AgentRunStatus {
    match s {
        "running" => AgentRunStatus::Running,
        "completed" => AgentRunStatus::Completed,
        "failed" => AgentRunStatus::Failed,
        "cancelled" => AgentRunStatus::Cancelled,
        _ => AgentRunStatus::Completed,
    }
}
