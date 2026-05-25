use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::sse::{Event, KeepAlive, Sse},
    Extension, Json,
};
use futures::stream::Stream;
use sqlx::SqlitePool;
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::mpsc;

use super::{
    artifacts::write_artifact,
    context::{build_ticket_context, gather_agent_context},
    conversions::{db_run_to_api_run, store_agent_run},
    sse_helpers::{
        create_error_stream, create_reconnect_stream, create_sse_stream, spawn_event_persister,
    },
};
use crate::agents::{
    executor::run_codex_agent_turn, resolve_working_dir, AgentExecutor, AgentRun,
    AgentRunsResponse, AgentType, RunAgentRequest, RunAgentResponse, SendMessageRequest,
    StreamEvent,
};
use crate::auth_middleware::AuthenticatedUser;
use crate::handlers::chat_client_manager::ChatClientManager;
use crate::handlers::get_organization;

async fn load_authorized_ticket(
    db: &SqlitePool,
    headers: &HeaderMap,
    epic_id: &str,
    slice_id: &str,
    ticket_id: &str,
) -> Result<ticketing_system::Ticket, (StatusCode, String)> {
    let organization = get_organization(headers);
    let ticket = ticketing_system::tickets::get_ticket_by_id(db, ticket_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Database error: {}", e),
            )
        })?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Ticket not found".to_string()))?;

    if ticket.organization != organization
        || ticket.epic_id != epic_id
        || ticket.slice_id != slice_id
    {
        return Err((StatusCode::NOT_FOUND, "Ticket not found".to_string()));
    }

    Ok(ticket)
}

async fn agent_run_visible_to_user(
    db: &SqlitePool,
    user_id: &str,
    run: &ticketing_system::AgentRun,
) -> Result<bool, String> {
    if ticketing_system::system_logs::is_admin(db, user_id)
        .await
        .map_err(|e| e.to_string())?
    {
        return Ok(true);
    }

    let organization = if let Some(org) = run.organization.as_deref() {
        Some(org.to_string())
    } else if let Some(ticket_id) = run.ticket_id.as_deref() {
        ticketing_system::tickets::get_ticket_by_id(db, ticket_id)
            .await
            .map_err(|e| e.to_string())?
            .map(|ticket| ticket.organization)
    } else {
        None
    };

    match organization {
        Some(org) => ticketing_system::memberships::check_membership(db, user_id, &org)
            .await
            .map_err(|e| e.to_string()),
        None => Ok(false),
    }
}

/// POST /api/epics/:epic_id/slices/:slice_id/tickets/:ticket_id/agent-runs
pub async fn run_agent(
    Path((epic_id, slice_id, ticket_id)): Path<(String, String, String)>,
    State(db): State<Arc<SqlitePool>>,
    headers: HeaderMap,
    Json(req): Json<RunAgentRequest>,
) -> Result<Json<RunAgentResponse>, (StatusCode, String)> {
    let ticket = load_authorized_ticket(&db, &headers, &epic_id, &slice_id, &ticket_id).await?;

    let context = build_ticket_context(
        &epic_id,
        &slice_id,
        &ticket_id,
        ticket.title,
        ticket.description.clone().unwrap_or_default(),
    );

    let (previous_output, selected_context, sender_info, blocked_by_context) =
        gather_agent_context(
            &db,
            &req.agent_type,
            &ticket_id,
            req.previous_session_id.as_deref(),
            &req.selected_session_ids,
            ticket.assignee.as_deref(),
        )
        .await;

    // Combine blocked_by context with previous output if both exist
    let combined_previous = match (blocked_by_context, previous_output) {
        (Some(blocked), Some(prev)) => Some(format!("{}\n\n{}", blocked, prev)),
        (Some(blocked), None) => Some(blocked),
        (None, Some(prev)) => Some(prev),
        (None, None) => None,
    };

    let working_dir = resolve_working_dir(&db, &req.agent_type, &ticket.organization)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to resolve working dir: {}", e),
            )
        })?;
    let executor = AgentExecutor::new(working_dir);

    let agent_run = executor
        .execute(
            req.agent_type,
            context,
            combined_previous,
            selected_context,
            sender_info,
            None,
        )
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Agent execution failed: {}", e),
            )
        })?;

    store_agent_run(&db, &agent_run).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to store agent run: {}", e),
        )
    })?;

    // Write artifact to DB if agent completed successfully
    if agent_run.status == crate::agents::AgentRunStatus::Completed {
        if let Some(ref output) = agent_run.output_summary {
            if let Some(artifact_id) = write_artifact(
                &db,
                &ticket_id,
                agent_run.agent_type.as_str(),
                output,
                Some(&agent_run.session_id),
            )
            .await
            {
                tracing::info!("Artifact created: {}", artifact_id);
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
    headers: HeaderMap,
) -> Result<Json<AgentRunsResponse>, (StatusCode, String)> {
    load_authorized_ticket(&db, &headers, &epic_id, &slice_id, &ticket_id).await?;

    let db_runs =
        ticketing_system::agent_runs::list_agent_runs(&db, &epic_id, &slice_id, &ticket_id)
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to query agent runs: {}", e),
                )
            })?;

    let runs: Vec<AgentRun> = db_runs.into_iter().map(db_run_to_api_run).collect();
    Ok(Json(AgentRunsResponse { runs }))
}

/// GET /api/agent-runs/:session_id
pub async fn get_agent_run(
    Path(session_id): Path<String>,
    State(db): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<Json<AgentRun>, (StatusCode, String)> {
    let db_run = ticketing_system::agent_runs::get_agent_run(&db, &session_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Database error: {}", e),
            )
        })?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Agent run not found".to_string()))?;

    if !agent_run_visible_to_user(&db, &user.user_id, &db_run)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
    {
        return Err((StatusCode::NOT_FOUND, "Agent run not found".to_string()));
    }

    Ok(Json(db_run_to_api_run(db_run)))
}

/// POST /api/epics/:epic_id/slices/:slice_id/tickets/:ticket_id/agent-runs/stream
pub async fn stream_agent_run(
    Path((epic_id, slice_id, ticket_id)): Path<(String, String, String)>,
    State(db): State<Arc<SqlitePool>>,
    State(_client_manager): State<Arc<ChatClientManager>>,
    headers: HeaderMap,
    Json(req): Json<RunAgentRequest>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    tracing::info!("=== STREAM_AGENT_RUN START ===");
    tracing::info!("Ticket: {}/{}/{}", epic_id, slice_id, ticket_id);

    let (tx, rx) = mpsc::channel::<StreamEvent>(100);
    let ticket_result = load_authorized_ticket(&db, &headers, &epic_id, &slice_id, &ticket_id)
        .await
        .map(Some)
        .map_err(|(_, message)| message);
    let db_clone = db.clone();

    // Generate session_id upfront
    let session_id = uuid::Uuid::new_v4().to_string();
    let started_at = chrono::Utc::now().to_rfc3339();

    // Store agent run with "running" status before execution
    if let Ok(Some(ref ticket)) = ticket_result {
        let create_req = ticketing_system::CreateAgentRunRequest {
            session_id: session_id.clone(),
            organization: Some(ticket.organization.clone()),
            epic_id: Some(epic_id.clone()),
            slice_id: Some(slice_id.clone()),
            ticket_id: Some(ticket_id.clone()),
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
    tokio::spawn(async move {
        match ticket_result {
            Ok(Some(ticket)) => {
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

                let context =
                    build_ticket_context(&epic_id, &slice_id, &ticket_id, ticket.title, intent);

                let working_dir =
                    match resolve_working_dir(&db_clone, &req.agent_type, &ticket.organization)
                        .await
                    {
                        Ok(wd) => wd,
                        Err(e) => {
                            let _ = tx
                                .send(StreamEvent::Status {
                                    status: "failed".to_string(),
                                    message: Some(format!("Failed to resolve working dir: {}", e)),
                                })
                                .await;
                            return;
                        }
                    };
                let executor = AgentExecutor::new(working_dir);

                let _ = tx
                    .send(StreamEvent::Status {
                        status: "running".to_string(),
                        message: Some(format!("Agent started (session: {})", session_id_clone)),
                    })
                    .await;

                let (previous_output, selected_context, sender_info, blocked_by_context) =
                    gather_agent_context(
                        &db_clone,
                        &req.agent_type,
                        &ticket_id,
                        req.previous_session_id.as_deref(),
                        &req.selected_session_ids,
                        ticket.assignee.as_deref(),
                    )
                    .await;

                // Combine blocked_by context with previous output if both exist
                let combined_previous = match (blocked_by_context, previous_output) {
                    (Some(blocked), Some(prev)) => Some(format!("{}\n\n{}", blocked, prev)),
                    (Some(blocked), None) => Some(blocked),
                    (None, Some(prev)) => Some(prev),
                    (None, None) => None,
                };

                let agent_type_for_error = req.agent_type.clone();

                match executor
                    .execute(
                        req.agent_type,
                        context,
                        combined_previous,
                        selected_context,
                        sender_info,
                        Some(tx.clone()),
                    )
                    .await
                {
                    Ok(mut agent_run) => {
                        agent_run.session_id = session_id_clone.clone();

                        tracing::info!(
                            "Storing agent run: session={}, status={:?}",
                            agent_run.session_id,
                            agent_run.status
                        );
                        if let Err(e) = store_agent_run(&db_clone, &agent_run).await {
                            tracing::error!("Failed to store completed agent run: {}", e);
                            crate::system_log_helper::log_event(
                                &db_clone,
                                "error",
                                "agent",
                                &format!("Failed to store agent run: {}", e),
                                Some(&format!(
                                    "session={}, status={:?}",
                                    agent_run.session_id, agent_run.status
                                )),
                                None,
                                Some(&agent_run.session_id),
                            )
                            .await;
                        }

                        if let Err(e) = ticketing_system::ticket_history::log_agent_run_completed(
                            &db_clone,
                            &ticket_id,
                            &agent_run.session_id,
                            agent_run.agent_type.as_str(),
                            agent_run.status.as_str(),
                        )
                        .await
                        {
                            tracing::warn!("Failed to log agent run to ticket history: {}", e);
                        }

                        // Write artifact to DB if agent completed successfully
                        if agent_run.status == crate::agents::AgentRunStatus::Completed {
                            if let Some(ref output) = agent_run.output_summary {
                                if let Some(artifact_id) = write_artifact(
                                    &db_clone,
                                    &ticket_id,
                                    agent_run.agent_type.as_str(),
                                    output,
                                    Some(&session_id_clone),
                                )
                                .await
                                {
                                    tracing::info!("Artifact created: {}", artifact_id);
                                }
                            }
                        }

                        let _ = tx
                            .send(StreamEvent::Status {
                                status: agent_run.status.as_str().to_string(),
                                message: Some("Agent completed".to_string()),
                            })
                            .await;
                    }
                    Err(e) => {
                        crate::system_log_helper::log_event(
                            &db_clone,
                            "error",
                            "agent",
                            &format!("Agent execution failed: {}", e),
                            Some(&format!(
                                "ticket={}, agent_type={}",
                                ticket_id,
                                agent_type_for_error.as_str()
                            )),
                            None,
                            Some(&session_id_clone),
                        )
                        .await;

                        let failed_run = ticketing_system::AgentRun {
                            session_id: session_id_clone.clone(),
                            organization: None,
                            ticket_id: Some(ticket_id.clone()),
                            epic_id: Some(epic_id.clone()),
                            slice_id: Some(slice_id.clone()),
                            agent_type: agent_type_for_error.as_str().to_string(),
                            status: "failed".to_string(),
                            started_at,
                            completed_at: Some(chrono::Utc::now().to_rfc3339()),
                            input_message: String::new(),
                            output_summary: Some(format!("Agent failed: {}", e)),
                            tool_call_count: 0,
                            cc_session_id: None,
                        };

                        let _ =
                            ticketing_system::agent_runs::update_agent_run(&db_clone, &failed_run)
                                .await;
                        let _ = ticketing_system::ticket_history::log_agent_run_completed(
                            &db_clone,
                            &ticket_id,
                            &session_id_clone,
                            agent_type_for_error.as_str(),
                            "failed",
                        )
                        .await;

                        let _ = tx
                            .send(StreamEvent::Status {
                                status: "failed".to_string(),
                                message: Some(format!("Agent failed: {}", e)),
                            })
                            .await;
                    }
                }
            }
            Ok(None) => {
                let _ = tx
                    .send(StreamEvent::Status {
                        status: "failed".to_string(),
                        message: Some("Ticket not found".to_string()),
                    })
                    .await;
            }
            Err(e) => {
                let _ = tx
                    .send(StreamEvent::Status {
                        status: "failed".to_string(),
                        message: Some(format!("Database error: {}", e)),
                    })
                    .await;
            }
        }
    });

    // Events are stored by the persister even if the client disconnects
    let sse_rx = spawn_event_persister((*db).clone(), session_id, rx, 0);
    let stream = create_sse_stream(sse_rx);
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// GET /api/epics/:epic_id/slices/:slice_id/tickets/:ticket_id/agent-runs/active
pub async fn get_active_agent_run(
    Path((epic_id, slice_id, ticket_id)): Path<(String, String, String)>,
    State(db): State<Arc<SqlitePool>>,
    headers: HeaderMap,
) -> Result<Json<AgentRun>, (StatusCode, String)> {
    load_authorized_ticket(&db, &headers, &epic_id, &slice_id, &ticket_id).await?;

    let db_run =
        ticketing_system::agent_runs::get_active_agent_run(&db, &epic_id, &slice_id, &ticket_id)
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to query agent runs: {}", e),
                )
            })?;

    match db_run {
        Some(run) => Ok(Json(db_run_to_api_run(run))),
        None => Err((StatusCode::NOT_FOUND, "No active agent run".to_string())),
    }
}

/// GET /api/agent-runs/:session_id/stream
pub async fn reconnect_agent_stream(
    Path(session_id): Path<String>,
    State(db): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let run_result = ticketing_system::agent_runs::get_agent_run(&db, &session_id).await;
    let events_result = ticketing_system::agent_runs::get_events(&db, &session_id).await;

    let stream: Box<dyn Stream<Item = Result<Event, Infallible>> + Send + Unpin> = match run_result
    {
        Ok(Some(run)) => match agent_run_visible_to_user(&db, &user.user_id, &run).await {
            Ok(true) => {
                let events = events_result.unwrap_or_default();
                Box::new(Box::pin(create_reconnect_stream(run, events)))
            }
            Ok(false) => Box::new(Box::pin(create_error_stream(
                "Agent run not found".to_string(),
            ))),
            Err(e) => Box::new(Box::pin(create_error_stream(format!(
                "Database error: {}",
                e
            )))),
        },
        Ok(None) => Box::new(Box::pin(create_error_stream(
            "Agent run not found".to_string(),
        ))),
        Err(e) => Box::new(Box::pin(create_error_stream(format!(
            "Database error: {}",
            e
        )))),
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// POST /api/agent-runs/:session_id/message
///
/// Sends a follow-up message by resuming the persisted Codex app-server thread for
/// the agent run.
pub async fn send_message_to_agent(
    Path(session_id): Path<String>,
    State(db): State<Arc<SqlitePool>>,
    State(_client_manager): State<Arc<ChatClientManager>>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(req): Json<SendMessageRequest>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    tracing::info!("=== SEND_MESSAGE_TO_AGENT START === session={}", session_id);

    let (tx, rx) = mpsc::channel::<StreamEvent>(100);
    let session_id_clone = session_id.clone();
    let db_clone = db.clone();
    let user_id = user.user_id.clone();

    tokio::spawn(async move {
        let db = db_clone;

        let db_run = match ticketing_system::agent_runs::get_agent_run(&db, &session_id_clone).await
        {
            Ok(Some(run)) => run,
            Ok(None) => {
                let _ = tx
                    .send(StreamEvent::Status {
                        status: "failed".to_string(),
                        message: Some("Agent run not found in database.".to_string()),
                    })
                    .await;
                return;
            }
            Err(e) => {
                let _ = tx
                    .send(StreamEvent::Status {
                        status: "failed".to_string(),
                        message: Some(format!("Database error: {}", e)),
                    })
                    .await;
                return;
            }
        };

        match agent_run_visible_to_user(&db, &user_id, &db_run).await {
            Ok(true) => {}
            Ok(false) => {
                let _ = tx
                    .send(StreamEvent::Status {
                        status: "failed".to_string(),
                        message: Some("Agent run not found in database.".to_string()),
                    })
                    .await;
                return;
            }
            Err(e) => {
                let _ = tx
                    .send(StreamEvent::Status {
                        status: "failed".to_string(),
                        message: Some(format!("Database error: {}", e)),
                    })
                    .await;
                return;
            }
        }

        let runtime_session_id = match &db_run.cc_session_id {
            Some(sid) => sid.clone(),
            None => {
                let _ = tx
                    .send(StreamEvent::Status {
                        status: "failed".to_string(),
                        message: Some(
                            "Agent session cannot be resumed (no Codex session ID stored). Please run the agent again."
                                .to_string(),
                        ),
                    })
                    .await;
                return;
            }
        };

        let agent_type: AgentType =
            match serde_json::from_value(serde_json::Value::String(db_run.agent_type.clone())) {
                Ok(at) => at,
                Err(_) => {
                    let _ = tx
                        .send(StreamEvent::Status {
                            status: "failed".to_string(),
                            message: Some(format!("Unknown agent type: {}", db_run.agent_type)),
                        })
                        .await;
                    return;
                }
            };

        let organization = match &db_run.organization {
            Some(org) => org.clone(),
            None if db_run.ticket_id.is_some() => {
                let ticket_id = db_run.ticket_id.as_deref().unwrap_or_default();
                match ticketing_system::tickets::get_ticket_by_id(&db, ticket_id).await {
                    Ok(Some(ticket)) => ticket.organization,
                    Ok(None) => {
                        let _ = tx
                            .send(StreamEvent::Status {
                                status: "failed".to_string(),
                                message: Some(
                                    "Failed to load ticket for agent resume.".to_string(),
                                ),
                            })
                            .await;
                        return;
                    }
                    Err(e) => {
                        let _ = tx
                            .send(StreamEvent::Status {
                                status: "failed".to_string(),
                                message: Some(format!("Database error: {}", e)),
                            })
                            .await;
                        return;
                    }
                }
            }
            None => "agentic-flowstate".to_string(),
        };

        let working_dir = match resolve_working_dir(&db, &agent_type, &organization).await {
            Ok(wd) => wd,
            Err(e) => {
                let _ = tx
                    .send(StreamEvent::Status {
                        status: "failed".to_string(),
                        message: Some(format!("Failed to resolve working dir: {}", e)),
                    })
                    .await;
                return;
            }
        };

        let _ = tx
            .send(StreamEvent::Status {
                status: "running".to_string(),
                message: Some("Processing follow-up message...".to_string()),
            })
            .await;

        let _ = tx
            .send(StreamEvent::UserMessage {
                content: req.message.clone(),
            })
            .await;

        match run_codex_agent_turn(
            &agent_type,
            &working_dir,
            "",
            &req.message,
            Some(&runtime_session_id),
            true,
            Some(tx.clone()),
            &session_id_clone,
        )
        .await
        {
            Ok(turn) => {
                if turn.usage.has_usage() {
                    if let Err(e) = ticketing_system::token_usage::insert_token_usage(
                        &db,
                        "agent_run",
                        &session_id_clone,
                        None,
                        None,
                        turn.usage,
                    )
                    .await
                    {
                        tracing::warn!("[AGENT-MSG] Failed to record token usage: {}", e);
                    }
                }

                let _ = tx
                    .send(StreamEvent::Status {
                        status: "completed".to_string(),
                        message: None,
                    })
                    .await;
            }
            Err(e) => {
                tracing::error!("[AGENT-MSG] Codex resume failed: {}", e);
                let _ = tx
                    .send(StreamEvent::Status {
                        status: "failed".to_string(),
                        message: Some(format!("Agent follow-up failed: {}", e)),
                    })
                    .await;
            }
        }
    });

    // Get current max event index so new events continue the sequence
    let initial_index = match ticketing_system::agent_runs::get_events(&db, &session_id).await {
        Ok(events) => events.len() as i32,
        Err(_) => 0,
    };

    // Events are stored by the persister even if the client disconnects
    let sse_rx = spawn_event_persister((*db).clone(), session_id, rx, initial_index);
    let stream = create_sse_stream(sse_rx);
    Sse::new(stream).keep_alive(KeepAlive::default())
}
