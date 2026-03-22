use axum::{
    extract::{Path, State},
    response::sse::{Event, KeepAlive, Sse},
    http::StatusCode,
    Json,
};
use futures::stream::Stream;
use futures::StreamExt;
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::mpsc;
use sqlx::SqlitePool;

use crate::agents::{
    AgentExecutor, AgentType, AgentRun, AgentRunsResponse, StreamEvent,
    RunAgentRequest, RunAgentResponse, SendMessageRequest,
    resolve_working_dir,
};
use crate::agents::executor::build_agent_options;
use crate::agents::prompts::load_prompt;
use crate::handlers::chat_client_manager::{ChatClientManager, ChatClient};
use super::{
    artifacts::write_artifact,
    context::{build_ticket_context, gather_agent_context},
    conversions::{db_run_to_api_run, store_agent_run},
    sse_helpers::{create_sse_stream, spawn_event_persister, create_reconnect_stream, create_error_stream},
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

    let (agent_run, _sdk_client) = executor
        .execute(req.agent_type, context, combined_previous, selected_context, sender_info, None)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Agent execution failed: {}", e)))?;

    store_agent_run(&db, &agent_run)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to store agent run: {}", e)))?;

    // Write artifact to DB if agent completed successfully
    if agent_run.status == crate::agents::AgentRunStatus::Completed {
        if let Some(ref output) = agent_run.output_summary {
            if let Some(artifact_id) = write_artifact(
                &db,
                &ticket_id,
                agent_run.agent_type.as_str(),
                output,
                Some(&agent_run.session_id),
            ).await {
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
    State(client_manager): State<Arc<ChatClientManager>>,
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
                    Ok((mut agent_run, sdk_client)) => {
                        agent_run.session_id = session_id_clone.clone();

                        // Store the still-connected client in the manager so follow-up
                        // messages can reuse it (same pattern as home-planner/workspace-manager)
                        let client = ChatClient {
                            sdk_client,
                            session_id: Some(session_id_clone.clone()),
                            last_used: std::time::Instant::now(),
                        };
                        client_manager.insert(session_id_clone.clone(), client).await;
                        tracing::info!("Stored agent client in manager for session {}", session_id_clone);

                        tracing::info!(
                            "Storing agent run: session={}, status={:?}",
                            agent_run.session_id, agent_run.status
                        );
                        if let Err(e) = store_agent_run(&db_clone, &agent_run).await {
                            tracing::error!("Failed to store completed agent run: {}", e);
                            crate::system_log_helper::log_event(
                                &db_clone, "error", "agent",
                                &format!("Failed to store agent run: {}", e),
                                Some(&format!("session={}, status={:?}", agent_run.session_id, agent_run.status)),
                                None, Some(&agent_run.session_id),
                            ).await;
                        }

                        if let Err(e) = ticketing_system::ticket_history::log_agent_run_completed(
                            &db_clone, &ticket_id, &agent_run.session_id,
                            agent_run.agent_type.as_str(), agent_run.status.as_str(),
                        ).await {
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
                                ).await {
                                    tracing::info!("Artifact created: {}", artifact_id);
                                }
                            }
                        }

                        let _ = tx.send(StreamEvent::Status {
                            status: agent_run.status.as_str().to_string(),
                            message: Some("Agent completed".to_string()),
                        }).await;
                    }
                    Err(e) => {
                        crate::system_log_helper::log_event(
                            &db_clone, "error", "agent",
                            &format!("Agent execution failed: {}", e),
                            Some(&format!("ticket={}, agent_type={}", ticket_id, agent_type_for_error.as_str())),
                            None, Some(&session_id_clone),
                        ).await;

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

    // Events are stored by the persister even if the frontend disconnects
    let sse_rx = spawn_event_persister((*db).clone(), session_id, rx, 0);
    let stream = create_sse_stream(sse_rx);
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
///
/// Sends a follow-up message to an agent by reusing the still-connected
/// ClaudeSDKClient from the ChatClientManager (same pattern as home-planner).
pub async fn send_message_to_agent(
    Path(session_id): Path<String>,
    State(db): State<Arc<SqlitePool>>,
    State(client_manager): State<Arc<ChatClientManager>>,
    Json(req): Json<SendMessageRequest>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    tracing::info!("=== SEND_MESSAGE_TO_AGENT START === session={}", session_id);

    let (tx, rx) = mpsc::channel::<StreamEvent>(100);
    let session_id_clone = session_id.clone();
    let db_clone = db.clone();

    tokio::spawn(async move {
        let db = db_clone;
        // Get the cached client, or try to resume from DB if missing/disconnected
        let from_cache = if let Some(existing) = client_manager.get(&session_id_clone).await {
            let guard = existing.lock().await;
            if guard.sdk_client.is_connected().await {
                drop(guard);
                tracing::info!("[AGENT-MSG] Reusing cached client for session {}", session_id_clone);
                Some(existing)
            } else {
                drop(guard);
                tracing::warn!("[AGENT-MSG] Cached client disconnected for session {}, will try resume", session_id_clone);
                client_manager.remove(&session_id_clone).await;
                None
            }
        } else {
            None
        };

        let client_arc = match from_cache {
            Some(arc) => arc,
            None => {
                tracing::info!("[AGENT-MSG] No cached client for session {}, attempting resume from DB", session_id_clone);

                // Load the agent run from DB to get cc_session_id
                let db_run = match ticketing_system::agent_runs::get_agent_run(&db, &session_id_clone).await {
                    Ok(Some(run)) => run,
                    Ok(None) => {
                        let _ = tx.send(StreamEvent::Status {
                            status: "failed".to_string(),
                            message: Some("Agent run not found in database.".to_string()),
                        }).await;
                        return;
                    }
                    Err(e) => {
                        let _ = tx.send(StreamEvent::Status {
                            status: "failed".to_string(),
                            message: Some(format!("Database error: {}", e)),
                        }).await;
                        return;
                    }
                };

                let cc_sid = match &db_run.cc_session_id {
                    Some(sid) => sid.clone(),
                    None => {
                        let _ = tx.send(StreamEvent::Status {
                            status: "failed".to_string(),
                            message: Some("Agent session cannot be resumed (no SDK session ID stored). Please run the agent again.".to_string()),
                        }).await;
                        return;
                    }
                };

                // Parse agent type from stored string
                let agent_type: AgentType = match serde_json::from_value(serde_json::Value::String(db_run.agent_type.clone())) {
                    Ok(at) => at,
                    Err(_) => {
                        let _ = tx.send(StreamEvent::Status {
                            status: "failed".to_string(),
                            message: Some(format!("Unknown agent type: {}", db_run.agent_type)),
                        }).await;
                        return;
                    }
                };

                // Load ticket for system prompt context
                let ticket_id = db_run.ticket_id.as_deref().unwrap_or("");
                let epic_id = db_run.epic_id.as_deref().unwrap_or("");
                let slice_id = db_run.slice_id.as_deref().unwrap_or("");

                let ticket = match ticketing_system::tickets::get_ticket_by_id(&db, ticket_id).await {
                    Ok(Some(t)) => t,
                    _ => {
                        let _ = tx.send(StreamEvent::Status {
                            status: "failed".to_string(),
                            message: Some("Failed to load ticket for session resume.".to_string()),
                        }).await;
                        return;
                    }
                };

                // Build system prompt with ticket context
                let mut vars = std::collections::HashMap::new();
                vars.insert("epic_id".to_string(), epic_id.to_string());
                vars.insert("slice_id".to_string(), slice_id.to_string());
                vars.insert("ticket_id".to_string(), ticket_id.to_string());
                vars.insert("ticket_title".to_string(), ticket.title.clone());
                vars.insert("ticket_intent".to_string(), ticket.description.unwrap_or_default());

                let system_prompt = match load_prompt(agent_type.as_str(), vars) {
                    Ok(p) => p,
                    Err(e) => {
                        let _ = tx.send(StreamEvent::Status {
                            status: "failed".to_string(),
                            message: Some(format!("Failed to load prompt: {}", e)),
                        }).await;
                        return;
                    }
                };

                // Resolve working directory
                let working_dir = match resolve_working_dir(&db, &agent_type, &ticket.organization).await {
                    Ok(wd) => wd,
                    Err(e) => {
                        let _ = tx.send(StreamEvent::Status {
                            status: "failed".to_string(),
                            message: Some(format!("Failed to resolve working dir: {}", e)),
                        }).await;
                        return;
                    }
                };

                // Build and connect client with resume
                let options = build_agent_options(&agent_type, &system_prompt, &working_dir, Some(&cc_sid));
                let mut sdk_client = cc_sdk::ClaudeSDKClient::new(options);

                if let Err(e) = sdk_client.connect(None).await {
                    tracing::error!("[AGENT-MSG] Failed to resume client: {}", e);
                    let _ = tx.send(StreamEvent::Status {
                        status: "failed".to_string(),
                        message: Some(format!("Failed to resume agent session: {}", e)),
                    }).await;
                    return;
                }

                tracing::info!("[AGENT-MSG] Successfully resumed client for session {} via cc_session_id {}", session_id_clone, cc_sid);

                let client = ChatClient {
                    sdk_client,
                    session_id: Some(cc_sid),
                    last_used: std::time::Instant::now(),
                };
                client_manager.insert(session_id_clone.clone(), client).await
            }
        };

        let _ = tx.send(StreamEvent::Status {
            status: "running".to_string(),
            message: Some("Processing follow-up message...".to_string()),
        }).await;

        // Persist the user message so it replays on reconnect
        let _ = tx.send(StreamEvent::UserMessage {
            content: req.message.clone(),
        }).await;

        // Send message and stream response — exactly like chat_stream::chat()
        let mut client = client_arc.lock().await;
        client.last_used = std::time::Instant::now();

        if let Err(e) = client.sdk_client.send_user_message(req.message).await {
            tracing::error!("[AGENT-MSG] Failed to send message: {}", e);
            let _ = tx.send(StreamEvent::Status {
                status: "failed".to_string(),
                message: Some(format!("Failed to send: {}", e)),
            }).await;
            return;
        }

        let mut response_stream = client.sdk_client.receive_messages().await;
        let mut message_count = 0u32;

        while let Some(msg_result) = response_stream.next().await {
            message_count += 1;
            match msg_result {
                Ok(msg) => {
                    if let cc_sdk::Message::Assistant { message: assistant_msg } = &msg {
                        for block in &assistant_msg.content {
                            match block {
                                cc_sdk::ContentBlock::Text(text_content) => {
                                    let _ = tx.send(StreamEvent::Text {
                                        content: text_content.text.clone()
                                    }).await;
                                }
                                cc_sdk::ContentBlock::ToolUse(tool_use) => {
                                    let _ = tx.send(StreamEvent::ToolUse {
                                        id: tool_use.id.clone(),
                                        name: tool_use.name.clone(),
                                        input: tool_use.input.clone(),
                                    }).await;
                                }
                                cc_sdk::ContentBlock::ToolResult(tool_result) => {
                                    let content = tool_result.content.as_ref()
                                        .map(|c| serde_json::to_string(c).unwrap_or_default())
                                        .unwrap_or_default();
                                    let _ = tx.send(StreamEvent::ToolResult {
                                        tool_use_id: tool_result.tool_use_id.clone(),
                                        content,
                                        is_error: tool_result.is_error.unwrap_or(false),
                                    }).await;
                                }
                                cc_sdk::ContentBlock::Thinking(thinking) => {
                                    let _ = tx.send(StreamEvent::Thinking {
                                        content: thinking.thinking.clone()
                                    }).await;
                                }
                            }
                        }
                    }

                    if let cc_sdk::Message::Result { session_id: sess_id, is_error, subtype, usage, .. } = &msg {
                        // Track token usage for agent runs
                        if let Some(usage_json) = usage {
                            let input_tok = usage_json.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                            let output_tok = usage_json.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                            if input_tok > 0 || output_tok > 0 {
                                let db_ref = db.clone();
                                let sid = session_id_clone.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = ticketing_system::token_usage::insert_token_usage(
                                        &db_ref, "agent_run", &sid,
                                        None, None, input_tok, output_tok,
                                    ).await {
                                        tracing::warn!("[AGENT-MSG] Failed to record token usage: {}", e);
                                    }
                                });
                            }
                        }

                        let _ = tx.send(StreamEvent::Result {
                            session_id: sess_id.clone(),
                            status: subtype.clone(),
                            is_error: *is_error,
                        }).await;
                        break;
                    }
                }
                Err(e) => {
                    tracing::error!("[AGENT-MSG] Error receiving message #{}: {}", message_count, e);
                    let _ = tx.send(StreamEvent::Status {
                        status: "failed".to_string(),
                        message: Some(format!("Error: {}", e)),
                    }).await;
                    break;
                }
            }
        }

        tracing::info!("[AGENT-MSG] Stream ended after {} messages", message_count);
        let _ = tx.send(StreamEvent::Status {
            status: "completed".to_string(),
            message: None,
        }).await;
    });

    // Get current max event index so new events continue the sequence
    let initial_index = match ticketing_system::agent_runs::get_events(&db, &session_id).await {
        Ok(events) => events.len() as i32,
        Err(_) => 0,
    };

    // Events are stored by the persister even if the frontend disconnects
    let sse_rx = spawn_event_persister((*db).clone(), session_id, rx, initial_index);
    let stream = create_sse_stream(sse_rx);
    Sse::new(stream).keep_alive(KeepAlive::default())
}
