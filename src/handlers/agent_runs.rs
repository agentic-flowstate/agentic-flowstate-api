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
    tracing::info!("=== STREAM_AGENT_RUN START ===");
    tracing::info!("Ticket: {}/{}/{}", epic_id, slice_id, ticket_id);
    tracing::info!("Agent type: {:?}", req.agent_type);

    let (tx, rx) = mpsc::channel::<StreamEvent>(100);

    // Get ticket details
    tracing::info!("Fetching ticket details...");
    let ticket_result = ticketing_system::tickets::get_ticket(&db, &epic_id, &slice_id, &ticket_id).await;
    tracing::info!("Ticket fetch result: {:?}", ticket_result.is_ok());
    let db_clone = db.clone();

    // Generate session_id upfront so we can store running state before execution
    let session_id = uuid::Uuid::new_v4().to_string();
    let started_at = chrono::Utc::now().to_rfc3339();
    tracing::info!("Generated session_id: {}", session_id);

    // Store agent run with "running" status BEFORE execution starts
    // This allows frontend to detect running agents on page refresh
    if let Ok(Some(ref ticket)) = ticket_result {
        tracing::info!("Creating initial 'running' record in DB...");
        let create_req = ticketing_system::CreateAgentRunRequest {
            session_id: session_id.clone(),
            epic_id: epic_id.clone(),
            slice_id: slice_id.clone(),
            ticket_id: ticket_id.clone(),
            agent_type: req.agent_type.as_str().to_string(),
            input_message: ticket.intent.clone(),
        };
        match ticketing_system::agent_runs::create_agent_run(&db, create_req).await {
            Ok(_) => tracing::info!("Successfully created 'running' record"),
            Err(e) => tracing::error!("Failed to store running agent state: {}", e),
        }
    }

    let session_id_clone = session_id.clone();
    let _agent_type_clone = req.agent_type.clone();

    // Spawn the agent execution in background
    tracing::info!("Spawning background task for agent execution...");
    tokio::spawn(async move {
        tracing::info!("[SPAWN] Background task started for session: {}", session_id_clone);
        match ticket_result {
            Ok(Some(ticket)) => {
                tracing::info!("[SPAWN] Ticket found: {}", ticket.title);
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
                tracing::info!("[SPAWN] Sending 'running' status to frontend...");
                let _ = tx.send(StreamEvent::Status {
                    status: "running".to_string(),
                    message: Some(format!("Agent started (session: {})", session_id_clone)),
                }).await;

                // Get previous output for chaining
                let previous_output = if let Some(prev_session_id) = &req.previous_session_id {
                    tracing::info!("[SPAWN] Fetching previous session output: {}", prev_session_id);
                    ticketing_system::agent_runs::get_agent_run(&db_clone, prev_session_id)
                        .await
                        .ok()
                        .flatten()
                        .and_then(|r| r.output_summary)
                } else {
                    None
                };

                tracing::info!("[SPAWN] Starting executor.execute()...");
                let agent_type_for_error = req.agent_type.clone();
                match executor.execute(req.agent_type, context, previous_output, Some(tx.clone())).await {
                    Ok(mut agent_run) => {
                        tracing::info!("[SPAWN] executor.execute() returned OK");
                        // Use our pre-generated session_id for consistency
                        agent_run.session_id = session_id_clone.clone();
                        tracing::info!("[SPAWN] Agent completed with status: {:?}", agent_run.status);
                        tracing::info!("[SPAWN] Output summary length: {:?}", agent_run.output_summary.as_ref().map(|s| s.len()));

                        // Store the completed run (updates the running record)
                        tracing::info!("[SPAWN] Calling store_agent_run to update status...");
                        match store_agent_run(&db_clone, &agent_run).await {
                            Ok(_) => tracing::info!("[SPAWN] Successfully stored agent run with status: {}", agent_run.status.as_str()),
                            Err(e) => tracing::error!("[SPAWN] Failed to store completed agent run: {}", e),
                        }

                        tracing::info!("[SPAWN] Sending final status to frontend...");
                        let _ = tx.send(StreamEvent::Status {
                            status: agent_run.status.as_str().to_string(),
                            message: Some("Agent completed".to_string()),
                        }).await;
                        tracing::info!("[SPAWN] Agent run complete, exiting spawn");
                    }
                    Err(e) => {
                        tracing::error!("[SPAWN] executor.execute() returned ERROR: {}", e);
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
                        tracing::info!("[SPAWN] Storing failed status...");
                        match ticketing_system::agent_runs::update_agent_run(&db_clone, &failed_run).await {
                            Ok(_) => tracing::info!("[SPAWN] Successfully stored failed status"),
                            Err(e) => tracing::error!("[SPAWN] Failed to store failed agent run: {}", e),
                        }
                        let _ = tx.send(StreamEvent::Status {
                            status: "failed".to_string(),
                            message: Some(format!("Agent failed: {}", e)),
                        }).await;
                    }
                }
            }
            Ok(None) => {
                tracing::error!("[SPAWN] Ticket not found!");
                let _ = tx.send(StreamEvent::Status {
                    status: "failed".to_string(),
                    message: Some("Ticket not found".to_string()),
                }).await;
            }
            Err(e) => {
                tracing::error!("[SPAWN] Database error fetching ticket: {}", e);
                let _ = tx.send(StreamEvent::Status {
                    status: "failed".to_string(),
                    message: Some(format!("Database error: {}", e)),
                }).await;
            }
        }
        tracing::info!("[SPAWN] Background task exiting");
    });

    // Convert channel to SSE stream with JSON serialization
    // Also store events to database for replay on reconnect
    tracing::info!("Setting up SSE stream for session: {}", session_id);
    let db_for_stream = db.clone();
    let session_id_for_stream = session_id.clone();
    let stream = stream! {
        tracing::info!("[STREAM] SSE stream started");
        let mut rx = ReceiverStream::new(rx);
        let mut event_index = 0i32;
        while let Some(event) = futures::StreamExt::next(&mut rx).await {
            let event_type = match &event {
                StreamEvent::Text { .. } => "text",
                StreamEvent::ToolUse { .. } => "tool_use",
                StreamEvent::ToolResult { .. } => "tool_result",
                StreamEvent::Thinking { .. } => "thinking",
                StreamEvent::Status { .. } => "status",
                StreamEvent::Result { .. } => "result",
                StreamEvent::ReplayComplete { .. } => "replay_complete",
            };
            tracing::debug!("[STREAM] Received event #{}: {}", event_index, event_type);

            match serde_json::to_string(&event) {
                Ok(json) => {
                    // Store event to database for replay
                    if let Err(e) = ticketing_system::agent_runs::store_event(
                        &db_for_stream,
                        &session_id_for_stream,
                        event_index,
                        event_type,
                        &json,
                    ).await {
                        tracing::warn!("[STREAM] Failed to store event #{}: {}", event_index, e);
                    }
                    event_index += 1;

                    yield Ok(Event::default().data(json));
                }
                Err(e) => {
                    tracing::error!("[STREAM] Failed to serialize event: {}", e);
                }
            }
        }
        tracing::info!("[STREAM] SSE stream ended after {} events", event_index);
    };

    tracing::info!("=== STREAM_AGENT_RUN returning SSE ===");
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
/// - Replays all stored events from the database
/// - Works for both running and completed agents
/// - For running agents, sends existing events then a status indicator
pub async fn reconnect_agent_stream(
    Path(session_id): Path<String>,
    State(db): State<Arc<SqlitePool>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let run_result = ticketing_system::agent_runs::get_agent_run(&db, &session_id).await;
    let events_result = ticketing_system::agent_runs::get_events(&db, &session_id).await;

    let stream = stream! {
        match run_result {
            Ok(Some(run)) => {
                // Always replay stored events first (for both running and completed agents)
                let mut event_count = 0usize;
                if let Ok(events) = events_result {
                    for db_event in events {
                        event_count += 1;
                        // The event_data is already serialized JSON, send it directly
                        yield Ok(Event::default().data(db_event.event_data));
                    }
                }

                // Send ReplayComplete event so frontend knows historical replay is done
                let replay_complete = StreamEvent::ReplayComplete {
                    total_events: event_count,
                    agent_status: run.status.clone(),
                };
                if let Ok(json) = serde_json::to_string(&replay_complete) {
                    yield Ok(Event::default().data(json));
                }

                if run.status == "running" {
                    // Agent is still running - send status indicator
                    // Frontend will show a loader for new events
                    let event = StreamEvent::Status {
                        status: "running".to_string(),
                        message: None,
                    };
                    if let Ok(json) = serde_json::to_string(&event) {
                        yield Ok(Event::default().data(json));
                    }
                } else {
                    // Agent finished
                    if event_count == 0 {
                        // Fallback: send stored output as text event if no events were stored
                        if let Some(output) = &run.output_summary {
                            let event = StreamEvent::Text { content: output.clone() };
                            if let Ok(json) = serde_json::to_string(&event) {
                                yield Ok(Event::default().data(json));
                            }
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
