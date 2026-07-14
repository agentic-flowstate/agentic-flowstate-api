//! Focus REST API handlers

use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use ticketing_system::FocusItem;

use crate::agents::prompts::load_prompt;
use crate::agents::run_oneshot;
use crate::agents::AgentType;
use crate::auth_middleware::AuthenticatedUser;

/// GET /api/focus
/// Returns all unchecked focus items for the authenticated user
pub async fn list_focus(
    Extension(user): Extension<AuthenticatedUser>,
    State(db): State<Arc<SqlitePool>>,
) -> Result<Json<Vec<FocusItem>>, (StatusCode, String)> {
    let items = ticketing_system::focus::list_focus(&db, &user.user_id, false)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(items))
}

#[derive(Deserialize)]
pub struct PullRequest {
    pub organization: String,
}

/// POST /api/focus/pull
/// Run pull-ticket agent, parse output, add to focus
pub async fn pull_focus_ticket(
    Extension(user): Extension<AuthenticatedUser>,
    State(db): State<Arc<SqlitePool>>,
    Json(req): Json<PullRequest>,
) -> Result<Json<FocusItem>, (StatusCode, String)> {
    let org = &req.organization;

    // Get current focus for this org
    let current = ticketing_system::focus::list_focus_ticket_ids(&db, &user.user_id, org)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let current_focus_str = if current.is_empty() {
        "(none)".to_string()
    } else {
        current
            .iter()
            .map(|(tid, title)| format!("- {} — {}", tid, title))
            .collect::<Vec<_>>()
            .join("\n")
    };

    // Pre-fetch all organization data for injection into user message
    let org_data = build_org_data_snapshot(&db, org)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    // Load system prompt (role + rules only, no data)
    let mut vars = HashMap::new();
    vars.insert("organization".to_string(), org.clone());

    let system_prompt = load_prompt("pull-ticket", vars).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to load prompt: {}", e),
        )
    })?;

    let working_dir = PathBuf::from("/Users/jarvisgpt/projects");

    // User message: data at top, query at bottom (per Conversation best practices)
    let prompt = format!(
        "<current_focus>\n{}\n</current_focus>\n\n<organization_tickets>\n{}\n</organization_tickets>\n\nSelect the best next ticket to work on for the {} organization.",
        current_focus_str, org_data, org
    );

    // Create agent run record BEFORE execution
    let session_id = uuid::Uuid::new_v4().to_string();
    let create_req = ticketing_system::CreateAgentRunRequest {
        session_id: session_id.clone(),
        organization: Some(org.clone()),
        epic_id: None,
        slice_id: None,
        ticket_id: None,
        agent_type: "pull-ticket".to_string(),
        input_message: prompt.clone(),
    };
    if let Err(e) = ticketing_system::agent_runs::create_agent_run(&db, create_req).await {
        tracing::error!("[PULL-TICKET] Failed to create agent run record: {}", e);
    }

    tracing::info!(
        "[PULL-TICKET] Starting agent for org={} session={}",
        org,
        session_id
    );

    let result = run_oneshot(
        Some(AgentType::PullTicket),
        &system_prompt,
        &working_dir,
        prompt,
    )
    .await;

    let (result_text, tool_call_count) = match result {
        Ok(r) => (r.text, r.tool_call_count),
        Err(e) => {
            tracing::error!("[PULL-TICKET] Agent failed: {}", e);
            // Update run as failed
            let failed_run = ticketing_system::AgentRun {
                session_id: session_id.clone(),
                organization: Some(org.clone()),
                epic_id: None,
                slice_id: None,
                ticket_id: None,
                agent_type: "pull-ticket".to_string(),
                status: "failed".to_string(),
                started_at: chrono::Utc::now().to_rfc3339(),
                completed_at: Some(chrono::Utc::now().to_rfc3339()),
                input_message: String::new(),
                output_summary: Some(format!("Agent failed: {}", e)),
                tool_call_count: 0,
                runtime_session_id: None,
            };
            let _ = ticketing_system::agent_runs::update_agent_run(&db, &failed_run).await;
            return Err((StatusCode::INTERNAL_SERVER_ERROR, e));
        }
    };

    tracing::info!(
        "[PULL-TICKET] Agent output: {} chars, tool_calls: {}",
        result_text.len(),
        tool_call_count
    );

    // Parse <selected_ticket>T-XXXXXXXX</selected_ticket>
    let ticket_id = parse_selected_ticket(&result_text).ok_or_else(|| {
        tracing::error!("[PULL-TICKET] No <selected_ticket> tag found in output");
        // Store the run as failed with output for debugging
        let failed_run = ticketing_system::AgentRun {
            session_id: session_id.clone(),
            organization: Some(org.clone()),
            epic_id: None,
            slice_id: None,
            ticket_id: None,
            agent_type: "pull-ticket".to_string(),
            status: "failed".to_string(),
            started_at: String::new(),
            completed_at: Some(chrono::Utc::now().to_rfc3339()),
            input_message: String::new(),
            output_summary: Some(truncate_output(&result_text)),
            tool_call_count,
            runtime_session_id: None,
        };
        let db_clone = db.clone();
        tokio::spawn(async move {
            let _ = ticketing_system::agent_runs::update_agent_run(&db_clone, &failed_run).await;
        });
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "Agent did not select a ticket".to_string(),
        )
    })?;

    if ticket_id == "NONE" {
        // Store as completed with NONE result
        let completed_run = ticketing_system::AgentRun {
            session_id: session_id.clone(),
            organization: Some(org.clone()),
            epic_id: None,
            slice_id: None,
            ticket_id: None,
            agent_type: "pull-ticket".to_string(),
            status: "completed".to_string(),
            started_at: String::new(),
            completed_at: Some(chrono::Utc::now().to_rfc3339()),
            input_message: String::new(),
            output_summary: Some(truncate_output(&result_text)),
            tool_call_count,
            runtime_session_id: None,
        };
        let _ = ticketing_system::agent_runs::update_agent_run(&db, &completed_run).await;
        return Err((
            StatusCode::NOT_FOUND,
            "No suitable tickets found".to_string(),
        ));
    }

    tracing::info!("[PULL-TICKET] Selected ticket: {}", ticket_id);

    // Fetch ticket from DB to get title/epic_id/slice_id
    let ticket = ticketing_system::tickets::get_ticket_by_id(&db, &ticket_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| {
            tracing::error!(
                "[PULL-TICKET] Ticket {} not found — agent hallucinated",
                ticket_id
            );
            // Store as failed with output for debugging
            let failed_run = ticketing_system::AgentRun {
                session_id: session_id.clone(),
                organization: Some(org.clone()),
                epic_id: None,
                slice_id: None,
                ticket_id: Some(ticket_id.clone()),
                agent_type: "pull-ticket".to_string(),
                status: "failed".to_string(),
                started_at: String::new(),
                completed_at: Some(chrono::Utc::now().to_rfc3339()),
                input_message: String::new(),
                output_summary: Some(format!(
                    "Hallucinated ticket ID: {}\n\n{}",
                    ticket_id,
                    truncate_output(&result_text)
                )),
                tool_call_count,
                runtime_session_id: None,
            };
            let db_clone = db.clone();
            tokio::spawn(async move {
                let _ =
                    ticketing_system::agent_runs::update_agent_run(&db_clone, &failed_run).await;
            });
            (
                StatusCode::NOT_FOUND,
                format!("Ticket {} not found", ticket_id),
            )
        })?;

    // Add to focus
    let item = ticketing_system::focus::add_to_focus(
        &db,
        &user.user_id,
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
            (
                StatusCode::CONFLICT,
                format!("Ticket {} is already in focus", ticket_id),
            )
        } else {
            (StatusCode::INTERNAL_SERVER_ERROR, msg)
        }
    })?;

    // Store completed run with selected ticket
    let completed_run = ticketing_system::AgentRun {
        session_id,
        organization: Some(org.clone()),
        epic_id: Some(ticket.epic_id.clone()),
        slice_id: Some(ticket.slice_id.clone()),
        ticket_id: Some(ticket.ticket_id.clone()),
        agent_type: "pull-ticket".to_string(),
        status: "completed".to_string(),
        started_at: String::new(),
        completed_at: Some(chrono::Utc::now().to_rfc3339()),
        input_message: String::new(),
        output_summary: Some(truncate_output(&result_text)),
        tool_call_count,
        runtime_session_id: None,
    };
    let _ = ticketing_system::agent_runs::update_agent_run(&db, &completed_run).await;

    tracing::info!(
        "[PULL-TICKET] Added to focus: {} — {}",
        item.ticket_id,
        item.ticket_title
    );
    Ok(Json(item))
}

#[derive(Deserialize)]
pub struct ToggleFocusRequest {
    pub id: String,
}

/// POST /api/focus/toggle
pub async fn toggle_focus(
    Extension(user): Extension<AuthenticatedUser>,
    State(db): State<Arc<SqlitePool>>,
    Json(req): Json<ToggleFocusRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let checked = ticketing_system::focus::toggle_focus_item(&db, &user.user_id, &req.id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Focus item not found".to_string()))?;

    Ok(Json(serde_json::json!({
        "id": req.id,
        "checked": checked,
    })))
}

/// DELETE /api/focus/:id
pub async fn remove_focus(
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
    State(db): State<Arc<SqlitePool>>,
) -> Result<StatusCode, (StatusCode, String)> {
    let deleted = ticketing_system::focus::remove_focus_item(&db, &user.user_id, &id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err((StatusCode::NOT_FOUND, "Focus item not found".to_string()))
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

/// Build a text snapshot of all org data for injection into the prompt
async fn build_org_data_snapshot(db: &SqlitePool, org: &str) -> Result<String, String> {
    let epics = ticketing_system::epics::list_epics(db, Some(org))
        .await
        .map_err(|e| format!("Failed to list epics: {}", e))?;

    // Get all tickets in one query
    let all_tickets = ticketing_system::tickets::list_tickets_by_organization(db, org)
        .await
        .map_err(|e| format!("Failed to list tickets: {}", e))?;

    let mut output = String::new();

    for epic in &epics {
        output.push_str(&format!("\n### Epic: {} — {}\n", epic.epic_id, epic.title));
        if let Some(ref notes) = epic.notes {
            output.push_str(&format!("Notes: {}\n", notes));
        }

        let slices = ticketing_system::slices::list_slices(db, org, &epic.epic_id)
            .await
            .map_err(|e| format!("Failed to list slices: {}", e))?;

        for slice in &slices {
            output.push_str(&format!(
                "\n#### Slice: {} — {}\n",
                slice.slice_id, slice.title
            ));

            // Filter tickets for this slice
            let slice_tickets: Vec<_> = all_tickets
                .iter()
                .filter(|t| t.epic_id == epic.epic_id && t.slice_id == slice.slice_id)
                .collect();

            for ticket in &slice_tickets {
                output.push_str(&format!(
                    "- {} | {} | status={} | type={:?}",
                    ticket.ticket_id, ticket.title, ticket.status, ticket.ticket_type,
                ));
                if let Some(ref blocked_by) = ticket.blocked_by {
                    if !blocked_by.is_empty() {
                        output.push_str(&format!(" | blocked_by={}", blocked_by.join(",")));
                    }
                }
                if let Some(ref blocks) = ticket.blocks {
                    if !blocks.is_empty() {
                        output.push_str(&format!(" | blocks={}", blocks.join(",")));
                    }
                }
                if let Some(ref mid) = ticket.milestone_id {
                    output.push_str(&format!(" | milestone={}", mid));
                }
                output.push('\n');
            }
        }
    }

    Ok(output)
}

/// Truncate output to a reasonable size for storage
fn truncate_output(text: &str) -> String {
    if text.len() > 50000 {
        let end = (0..=50000)
            .rev()
            .find(|&i| text.is_char_boundary(i))
            .unwrap_or(0);
        format!("{}...\n\n[Output truncated at {} bytes]", &text[..end], end)
    } else {
        text.to_string()
    }
}
