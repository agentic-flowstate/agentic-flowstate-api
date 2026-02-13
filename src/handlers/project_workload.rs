//! Project workload REST API handlers

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use cc_sdk::{query, ClaudeCodeOptions, Message, ContentBlock, ToolsConfig};
use futures::StreamExt;

use ticketing_system::WorkloadItem;

use crate::agents::types::AgentType;
use crate::agents::prompts::load_prompt;

/// GET /api/project-workload
/// Returns all unchecked workload items
pub async fn list_project_workload(
    State(db): State<Arc<SqlitePool>>,
) -> Result<Json<Vec<WorkloadItem>>, (StatusCode, String)> {
    let items = ticketing_system::project_workload::list_workload(&db, false)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(items))
}

#[derive(Deserialize)]
pub struct PullRequest {
    pub organization: String,
}

/// POST /api/project-workload/pull
/// Run pull-ticket agent, parse output, add to workload
pub async fn pull_project_ticket(
    State(db): State<Arc<SqlitePool>>,
    Json(req): Json<PullRequest>,
) -> Result<Json<WorkloadItem>, (StatusCode, String)> {
    let org = &req.organization;

    // Get current workload for this org
    let current = ticketing_system::project_workload::list_workload_ticket_ids(&db, org)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let current_workload_str = if current.is_empty() {
        "(none)".to_string()
    } else {
        current
            .iter()
            .map(|(tid, title)| format!("- {} — {}", tid, title))
            .collect::<Vec<_>>()
            .join("\n")
    };

    // Load prompt with variables
    let mut vars = HashMap::new();
    vars.insert("organization".to_string(), org.clone());
    vars.insert("current_workload".to_string(), current_workload_str);

    let system_prompt = load_prompt("pull-ticket", vars)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to load prompt: {}", e)))?;

    let agent_type = AgentType::PullTicket;
    let tools_list: Vec<String> = agent_type
        .allowed_tools()
        .iter()
        .map(|s| s.to_string())
        .collect();

    let working_dir = PathBuf::from("/Users/jarvisgpt/projects");

    let mut builder = ClaudeCodeOptions::builder()
        .system_prompt(&system_prompt)
        .model(agent_type.model())
        .tools(ToolsConfig::list(tools_list.clone()))
        .allowed_tools(tools_list)
        .cwd(&working_dir);

    if let Some(turns) = agent_type.max_turns() {
        builder = builder.max_turns(turns);
    }

    let options = builder.build();

    let prompt = format!("Select the best next ticket to work on for the {} organization.", org);

    tracing::info!("[PULL-TICKET] Starting agent for org={}", org);

    // Run agent and collect output
    let mut output_parts = Vec::new();

    match query(prompt.as_str(), Some(options)).await {
        Ok(stream) => {
            let mut stream = Box::pin(stream);

            while let Some(message_result) = stream.next().await {
                match message_result {
                    Ok(message) => {
                        if let Message::Assistant { message: assistant_msg } = &message {
                            for block in &assistant_msg.content {
                                if let ContentBlock::Text(text_content) = block {
                                    output_parts.push(text_content.text.clone());
                                }
                            }
                        }
                        if let Message::Result { .. } = &message {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::error!("[PULL-TICKET] Stream error: {}", e);
                        return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Agent error: {}", e)));
                    }
                }
            }
        }
        Err(e) => {
            tracing::error!("[PULL-TICKET] Failed to start agent: {}", e);
            return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to start agent: {}", e)));
        }
    }

    let full_output = output_parts.join("");
    tracing::info!("[PULL-TICKET] Agent output: {} chars", full_output.len());

    // Parse <selected_ticket>T-XXXXXXXX</selected_ticket>
    let ticket_id = parse_selected_ticket(&full_output)
        .ok_or_else(|| {
            tracing::error!("[PULL-TICKET] No <selected_ticket> tag found in output");
            (StatusCode::UNPROCESSABLE_ENTITY, "Agent did not select a ticket".to_string())
        })?;

    if ticket_id == "NONE" {
        return Err((StatusCode::NOT_FOUND, "No suitable tickets found".to_string()));
    }

    tracing::info!("[PULL-TICKET] Selected ticket: {}", ticket_id);

    // Fetch ticket from DB to get title/epic_id/slice_id
    let ticket = ticketing_system::tickets::get_ticket_by_id(&db, &ticket_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("Ticket {} not found", ticket_id)))?;

    // Add to workload
    let item = ticketing_system::project_workload::add_to_workload(
        &db,
        org,
        &ticket.ticket_id,
        &ticket.title,
        &ticket.epic_id,
        &ticket.slice_id,
    )
    .await
    .map_err(|e| {
        let msg = e.to_string();
        if msg.contains("UNIQUE") {
            (StatusCode::CONFLICT, format!("Ticket {} is already in workload", ticket_id))
        } else {
            (StatusCode::INTERNAL_SERVER_ERROR, msg)
        }
    })?;

    tracing::info!("[PULL-TICKET] Added to workload: {} — {}", item.ticket_id, item.ticket_title);
    Ok(Json(item))
}

#[derive(Deserialize)]
pub struct ToggleWorkloadRequest {
    pub id: String,
}

/// POST /api/project-workload/toggle
pub async fn toggle_project_workload(
    State(db): State<Arc<SqlitePool>>,
    Json(req): Json<ToggleWorkloadRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let checked = ticketing_system::project_workload::toggle_workload_item(&db, &req.id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Workload item not found".to_string()))?;

    Ok(Json(serde_json::json!({
        "id": req.id,
        "checked": checked,
    })))
}

/// DELETE /api/project-workload/:id
pub async fn remove_project_workload(
    Path(id): Path<String>,
    State(db): State<Arc<SqlitePool>>,
) -> Result<StatusCode, (StatusCode, String)> {
    let deleted = ticketing_system::project_workload::remove_workload_item(&db, &id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err((StatusCode::NOT_FOUND, "Workload item not found".to_string()))
    }
}

/// Parse <selected_ticket>TICKET_ID</selected_ticket> from agent output
fn parse_selected_ticket(text: &str) -> Option<String> {
    let start_tag = "<selected_ticket>";
    let end_tag = "</selected_ticket>";
    let start = text.find(start_tag)?;
    let end = text.find(end_tag)?;
    let ticket_id = text[start + start_tag.len()..end].trim().to_string();
    if ticket_id.is_empty() {
        None
    } else {
        Some(ticket_id)
    }
}
