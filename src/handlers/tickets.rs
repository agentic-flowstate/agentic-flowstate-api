use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;
use sqlx::SqlitePool;
use std::{sync::Arc, time::Instant};
use ticketing_system::{models::TicketType, work_ticket::EnsureWorkTicketRequest};
use tracing::{error, info};

use crate::{
    mcp_wrapper::call_mcp_tool,
    models::{CreateTicketHttpBody, UpdateTicketRequest},
    observability::streaming::{record_ticket_preflight, record_ticket_preflight_error},
};

use super::get_organization;

#[derive(Debug, Deserialize)]
pub struct TicketQuery {
    pub slice_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AllTicketsQuery {
    pub assignee: Option<String>,
}

pub async fn ensure_work_ticket(
    State(pool): State<Arc<SqlitePool>>,
    headers: HeaderMap,
    Json(mut request): Json<EnsureWorkTicketRequest>,
) -> Response {
    let header_org = get_organization(&headers);
    if request.organization.is_none() && !header_org.is_empty() {
        request.organization = Some(header_org);
    }

    let started = Instant::now();
    match ticketing_system::work_ticket::ensure_work_ticket(&pool, request).await {
        Ok(response) => {
            record_ticket_preflight(&response.status, &response.action, response.elapsed_ms);
            (StatusCode::OK, Json(response)).into_response()
        }
        Err(e) => {
            let duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
            record_ticket_preflight_error("failed", duration_ms);
            error!("Failed to ensure work ticket: {:?}", e);
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("Failed to ensure work ticket: {}", e) })),
            )
                .into_response()
        }
    }
}

// List all tickets for an organization, optionally filtered by assignee
pub async fn list_all_tickets(
    State(pool): State<Arc<SqlitePool>>,
    headers: HeaderMap,
    Query(params): Query<AllTicketsQuery>,
) -> Response {
    let organization = get_organization(&headers);

    if let Some(assignee) = params.assignee {
        // Use MCP tool for assignee-filtered query
        let args = json!({ "organization": organization, "assignee": assignee });
        match call_mcp_tool("list_tickets", Some(args)).await {
            Ok(result) => (StatusCode::OK, Json(result)).into_response(),
            Err(e) => {
                error!("Failed to list tickets by assignee: {:?}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "Failed to list tickets" })),
                )
                    .into_response()
            }
        }
    } else {
        // No assignee filter — list all tickets for org directly
        match ticketing_system::tickets::list_tickets_by_organization(&pool, &organization).await {
            Ok(tickets) => (StatusCode::OK, Json(tickets)).into_response(),
            Err(e) => {
                error!("Failed to list all tickets: {:?}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "Failed to list tickets" })),
                )
                    .into_response()
            }
        }
    }
}

// List tickets for an epic or a specific slice
pub async fn list_tickets(
    State(_pool): State<Arc<SqlitePool>>,
    headers: HeaderMap,
    Path(epic_id): Path<String>,
    Query(params): Query<TicketQuery>,
) -> Response {
    let organization = get_organization(&headers);
    let args = if let Some(slice_id) = params.slice_id {
        json!({
            "organization": organization,
            "epic_id": epic_id,
            "slice_id": slice_id
        })
    } else {
        json!({ "organization": organization, "epic_id": epic_id })
    };

    match call_mcp_tool("list_tickets", Some(args)).await {
        Ok(result) => (StatusCode::OK, Json(result)).into_response(),
        Err(e) => {
            error!("Failed to list tickets: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to list tickets: {}", e) })),
            )
                .into_response()
        }
    }
}

// Convenience function for listing tickets specifically in a slice (used by route)
pub async fn list_slice_tickets(
    State(pool): State<Arc<SqlitePool>>,
    headers: HeaderMap,
    Path((epic_id, slice_id)): Path<(String, String)>,
) -> Response {
    list_tickets(
        State(pool),
        headers,
        Path(epic_id),
        Query(TicketQuery {
            slice_id: Some(slice_id),
        }),
    )
    .await
}

// Get ticket with full path (epic_id, slice_id, ticket_id)
pub async fn get_ticket_nested(
    State(_pool): State<Arc<SqlitePool>>,
    headers: HeaderMap,
    Path((epic_id, slice_id, ticket_id)): Path<(String, String, String)>,
) -> Response {
    let organization = get_organization(&headers);
    let args = json!({
        "organization": organization,
        "epic_id": epic_id,
        "slice_id": slice_id,
        "ticket_id": ticket_id
    });

    match call_mcp_tool("get_ticket", Some(args)).await {
        Ok(result) => (StatusCode::OK, Json(result)).into_response(),
        Err(e) => {
            error!("Failed to get ticket: {:?}", e);
            if e.to_string().contains("not found") {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "Ticket not found" })),
                )
                    .into_response()
            } else {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("Failed to get ticket: {}", e) })),
                )
                    .into_response()
            }
        }
    }
}

pub async fn create_ticket(
    State(_pool): State<Arc<SqlitePool>>,
    headers: HeaderMap,
    Path((epic_id, slice_id)): Path<(String, String)>,
    Json(request): Json<CreateTicketHttpBody>,
) -> Response {
    let organization = get_organization(&headers);
    let ref_handle = format!(
        "api-{}",
        uuid::Uuid::new_v4()
            .to_string()
            .split('-')
            .next()
            .unwrap_or("0")
    );
    let args = match create_ticket_args(&organization, &epic_id, &slice_id, &ref_handle, request) {
        Ok(args) => args,
        Err(message) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response();
        }
    };

    match call_mcp_tool("create_slice_tickets", Some(args)).await {
        Ok(result) => {
            // Extract first ticket from batch result for single-item response
            let ticket = result
                .get("tickets")
                .and_then(|t| t.get(0))
                .and_then(|t| t.get("ticket"))
                .cloned()
                .unwrap_or(result);
            info!("Created ticket: {:?}", ticket);
            (StatusCode::CREATED, Json(ticket)).into_response()
        }
        Err(e) => {
            error!("Failed to create ticket: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to create ticket: {}", e) })),
            )
                .into_response()
        }
    }
}

fn create_ticket_args(
    organization: &str,
    epic_id: &str,
    slice_id: &str,
    ref_handle: &str,
    request: CreateTicketHttpBody,
) -> Result<serde_json::Value, String> {
    let title = request.title.trim();
    if title.is_empty() {
        return Err("Ticket title is required.".to_string());
    }

    let due_date = request.due_date.as_deref().map(str::trim).unwrap_or("");
    if due_date.is_empty() {
        return Err("Ticket due_date is required.".to_string());
    }

    let ticket_type = request
        .ticket_type
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("task");
    if !matches!(ticket_type, "task" | "bug" | "milestone") {
        return Err("ticket_type must be one of: task, bug, milestone.".to_string());
    }

    let milestone_id = request
        .milestone_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if ticket_type != "milestone" && milestone_id.is_none() {
        return Err("milestone_id is required when creating task or bug tickets.".to_string());
    }

    let item = json!({
        "ref": ref_handle,
        "title": title,
        "description": request.description,
        "ticket_type": ticket_type,
        "milestone_id": milestone_id,
        "blocked_by": request.blocked_by,
        "assignee": request.assignee,
        "agent": request.agent,
        "repository": request.repository,
        "due_date": due_date,
        "classification": request.classification,
    });

    Ok(json!({
        "organization": organization,
        "epic_id": epic_id,
        "slice_id": slice_id,
        "tickets": [item]
    }))
}

// Update ticket with full path (epic_id, slice_id, ticket_id)
pub async fn update_ticket_nested(
    State(pool): State<Arc<SqlitePool>>,
    headers: HeaderMap,
    Path((epic_id, slice_id, ticket_id)): Path<(String, String, String)>,
    Json(request): Json<UpdateTicketRequest>,
) -> Response {
    let organization = get_organization(&headers);

    // Determine which update operation to use based on what's being updated
    if let Some(status) = request.status.as_deref() {
        let args = json!({
            "organization": organization,
            "epic_id": epic_id,
            "slice_id": slice_id,
            "ticket_id": ticket_id,
            "new_status": status
        });

        match call_mcp_tool("update_ticket_status", Some(args)).await {
            Ok(result) => {
                info!("Updated ticket status: {:?}", result);
                (StatusCode::OK, Json(result)).into_response()
            }
            Err(e) => {
                error!("Failed to update ticket: {:?}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("Failed to update ticket: {}", e) })),
                )
                    .into_response()
            }
        }
    } else if request.notes.is_some() {
        let args = json!({
            "organization": organization,
            "epic_id": epic_id,
            "slice_id": slice_id,
            "ticket_id": ticket_id,
            "notes": request.notes.as_deref()
        });

        match call_mcp_tool("update_ticket_notes", Some(args)).await {
            Ok(result) => {
                info!("Updated ticket notes: {:?}", result);
                (StatusCode::OK, Json(result)).into_response()
            }
            Err(e) => {
                error!("Failed to update ticket: {:?}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("Failed to update ticket: {}", e) })),
                )
                    .into_response()
            }
        }
    } else if request.has_field_updates() {
        match update_ticket_fields(
            &pool,
            &organization,
            &epic_id,
            &slice_id,
            &ticket_id,
            request,
        )
        .await
        {
            Ok(ticket) => (StatusCode::OK, Json(ticket)).into_response(),
            Err(e) => {
                error!("Failed to update ticket fields: {:?}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("Failed to update ticket fields: {}", e) })),
                )
                    .into_response()
            }
        }
    } else {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "No fields to update" })),
        )
            .into_response()
    }
}

impl UpdateTicketRequest {
    fn has_field_updates(&self) -> bool {
        self.due_date.is_some()
            || self.title.is_some()
            || self.description.is_some()
            || self.assignee.is_some()
            || self.agent.is_some()
            || self.repository.is_some()
            || self.ticket_type.is_some()
            || self.guidance.is_some()
    }
}

async fn update_ticket_fields(
    pool: &SqlitePool,
    organization: &str,
    epic_id: &str,
    slice_id: &str,
    ticket_id: &str,
    request: UpdateTicketRequest,
) -> anyhow::Result<ticketing_system::models::Ticket> {
    let mut ticket =
        ticketing_system::tickets::get_ticket(pool, organization, epic_id, slice_id, ticket_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Ticket not found"))?;

    if let Some(title) = request.title {
        let title = title.trim();
        if title.is_empty() {
            anyhow::bail!("Ticket title cannot be empty");
        }
        ticket.title = title.to_string();
    }
    if let Some(description) = request.description {
        ticket.description = non_empty_string(description);
    }
    if let Some(assignee) = request.assignee {
        ticket.assignee = non_empty_string(assignee);
    }
    if let Some(agent) = request.agent {
        ticket.agent = non_empty_string(agent);
    }
    if let Some(repository) = request.repository {
        ticket.repository = non_empty_string(repository);
    }
    if let Some(guidance) = request.guidance {
        ticket.guidance = non_empty_string(guidance);
    }
    if let Some(due_date) = request.due_date {
        let due_date = due_date.trim();
        if due_date.is_empty() {
            anyhow::bail!("Cannot clear due_date — due dates are required on all tickets. Provide a new date instead.");
        }
        ticket.due_date = Some(due_date.to_string());
    }
    if let Some(ticket_type) = request.ticket_type {
        ticket.ticket_type = parse_ticket_type(&ticket_type)?;
    }

    ticketing_system::tickets::update_ticket(pool, &ticket).await
}

fn non_empty_string(value: String) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn parse_ticket_type(value: &str) -> anyhow::Result<TicketType> {
    match value.trim() {
        "task" => Ok(TicketType::Task),
        "milestone" => Ok(TicketType::Milestone),
        "bug" => Ok(TicketType::Bug),
        other => anyhow::bail!("Invalid ticket_type '{}'", other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_ticket_args_preserves_full_create_fields() {
        let request: CreateTicketHttpBody = serde_json::from_value(json!({
            "title": "Create from macOS",
            "description": "Detailed work",
            "ticket_type": "bug",
            "milestone_id": "T-MILESTONE",
            "blocked_by": ["T-BLOCKER"],
            "assignee": "Alex",
            "agent": "codex",
            "repository": "agentic-flowstate-app",
            "due_date": "2026-06-09",
            "classification": "automated"
        }))
        .expect("decode request");

        let args = create_ticket_args(
            "agentic-flowstate",
            "frontend",
            "ios-app",
            "api-test",
            request,
        )
        .expect("create args");

        assert_eq!(args["organization"], "agentic-flowstate");
        assert_eq!(args["epic_id"], "frontend");
        assert_eq!(args["slice_id"], "ios-app");
        let ticket = &args["tickets"][0];
        assert_eq!(ticket["ref"], "api-test");
        assert_eq!(ticket["title"], "Create from macOS");
        assert_eq!(ticket["description"], "Detailed work");
        assert_eq!(ticket["ticket_type"], "bug");
        assert_eq!(ticket["milestone_id"], "T-MILESTONE");
        assert_eq!(ticket["blocked_by"][0], "T-BLOCKER");
        assert_eq!(ticket["assignee"], "Alex");
        assert_eq!(ticket["agent"], "codex");
        assert_eq!(ticket["repository"], "agentic-flowstate-app");
        assert_eq!(ticket["due_date"], "2026-06-09");
        assert_eq!(ticket["classification"], "automated");
    }

    #[test]
    fn create_ticket_args_requires_due_date() {
        let request: CreateTicketHttpBody = serde_json::from_value(json!({
            "title": "Missing due date",
            "ticket_type": "milestone"
        }))
        .expect("decode request");

        let error = create_ticket_args("org", "epic", "slice", "ref", request)
            .expect_err("due date should be required");
        assert_eq!(error, "Ticket due_date is required.");
    }

    #[test]
    fn create_ticket_args_requires_milestone_for_task_or_bug() {
        let request: CreateTicketHttpBody = serde_json::from_value(json!({
            "title": "Missing milestone",
            "ticket_type": "task",
            "due_date": "2026-06-09"
        }))
        .expect("decode request");

        let error = create_ticket_args("org", "epic", "slice", "ref", request)
            .expect_err("milestone_id should be required");
        assert_eq!(
            error,
            "milestone_id is required when creating task or bug tickets."
        );
    }

    #[test]
    fn update_ticket_request_detects_full_field_updates() {
        let request: UpdateTicketRequest = serde_json::from_value(json!({
            "title": "Updated title",
            "description": "",
            "ticket_type": "bug",
            "guidance": "Ship the narrow fix",
            "due_date": "2026-06-08"
        }))
        .expect("decode request");

        assert!(request.has_field_updates());
        assert_eq!(request.title.as_deref(), Some("Updated title"));
        assert_eq!(request.description.as_deref(), Some(""));
        assert_eq!(request.ticket_type.as_deref(), Some("bug"));
        assert_eq!(request.guidance.as_deref(), Some("Ship the narrow fix"));
        assert_eq!(request.due_date.as_deref(), Some("2026-06-08"));
    }

    #[test]
    fn parse_ticket_type_rejects_unknown_values() {
        assert!(matches!(parse_ticket_type("task"), Ok(TicketType::Task)));
        assert!(parse_ticket_type("feature").is_err());
    }
}

// Delete ticket with full path (epic_id, slice_id, ticket_id)
pub async fn delete_ticket_nested(
    State(_pool): State<Arc<SqlitePool>>,
    headers: HeaderMap,
    Path((epic_id, slice_id, ticket_id)): Path<(String, String, String)>,
) -> Response {
    let organization = get_organization(&headers);
    let args = json!({
        "organization": organization,
        "epic_id": epic_id,
        "slice_id": slice_id,
        "ticket_id": ticket_id
    });

    match call_mcp_tool("delete_ticket", Some(args)).await {
        Ok(result) => {
            info!("Deleted ticket: {:?}", result);
            (StatusCode::OK, Json(result)).into_response()
        }
        Err(e) => {
            error!("Failed to delete ticket: {:?}", e);
            if e.to_string().contains("not found") {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "Ticket not found" })),
                )
                    .into_response()
            } else {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("Failed to delete ticket: {}", e) })),
                )
                    .into_response()
            }
        }
    }
}

// Add relationship with full path
pub async fn add_relationship_nested(
    State(_pool): State<Arc<SqlitePool>>,
    headers: HeaderMap,
    Path((epic_id, slice_id, ticket_id)): Path<(String, String, String)>,
    Json(request): Json<serde_json::Value>,
) -> Response {
    let organization = get_organization(&headers);
    let args = json!({
        "organization": organization,
        "epic_id": epic_id,
        "slice_id": slice_id,
        "ticket_id": ticket_id,
        "relationship_type": request["relationship_type"],
        "target_ticket_id": request["target_ticket_id"]
    });

    match call_mcp_tool("add_ticket_relationship", Some(args)).await {
        Ok(result) => {
            info!("Added ticket relationship: {:?}", result);
            (StatusCode::CREATED, Json(result)).into_response()
        }
        Err(e) => {
            error!("Failed to add relationship: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to add relationship: {}", e) })),
            )
                .into_response()
        }
    }
}

// Remove relationship with full path
pub async fn remove_relationship_nested(
    State(_pool): State<Arc<SqlitePool>>,
    headers: HeaderMap,
    Path((epic_id, slice_id, ticket_id)): Path<(String, String, String)>,
    Json(request): Json<serde_json::Value>,
) -> Response {
    let organization = get_organization(&headers);
    let args = json!({
        "organization": organization,
        "epic_id": epic_id,
        "slice_id": slice_id,
        "ticket_id": ticket_id,
        "relationship_type": request["relationship_type"],
        "target_ticket_id": request["target_ticket_id"]
    });

    match call_mcp_tool("remove_ticket_relationship", Some(args)).await {
        Ok(result) => {
            info!("Removed ticket relationship: {:?}", result);
            (StatusCode::OK, Json(result)).into_response()
        }
        Err(e) => {
            error!("Failed to remove relationship: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to remove relationship: {}", e) })),
            )
                .into_response()
        }
    }
}

// Get ticket by ID only (uses index lookup - ticket_id is globally unique)
pub async fn get_ticket_by_id(
    State(pool): State<Arc<SqlitePool>>,
    headers: HeaderMap,
    Path(ticket_id): Path<String>,
) -> Response {
    let organization = get_organization(&headers);

    match ticketing_system::tickets::get_ticket_by_id(&pool, &ticket_id).await {
        Ok(Some(ticket)) if ticket.organization == organization => {
            (StatusCode::OK, Json(ticket)).into_response()
        }
        Ok(Some(_)) | Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Ticket not found" })),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to get ticket by id: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to get ticket: {}", e) })),
            )
                .into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct UpdateGuidanceRequest {
    pub guidance: Option<String>,
}

// Update ticket guidance by ID
pub async fn update_ticket_guidance(
    State(pool): State<Arc<SqlitePool>>,
    Path(ticket_id): Path<String>,
    Json(request): Json<UpdateGuidanceRequest>,
) -> Response {
    match ticketing_system::tickets::update_ticket_guidance(
        &pool,
        &ticket_id,
        request.guidance.as_deref(),
    )
    .await
    {
        Ok(()) => {
            // Fetch and return the updated ticket
            match ticketing_system::tickets::get_ticket_by_id(&pool, &ticket_id).await {
                Ok(Some(ticket)) => {
                    info!("Updated ticket guidance for: {}", ticket_id);
                    (StatusCode::OK, Json(ticket)).into_response()
                }
                Ok(None) => (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "Ticket not found" })),
                )
                    .into_response(),
                Err(e) => {
                    error!("Failed to fetch updated ticket: {:?}", e);
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": format!("Failed to fetch ticket: {}", e) })),
                    )
                        .into_response()
                }
            }
        }
        Err(e) => {
            error!("Failed to update ticket guidance: {:?}", e);
            if e.to_string().contains("not found") {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "Ticket not found" })),
                )
                    .into_response()
            } else {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("Failed to update guidance: {}", e) })),
                )
                    .into_response()
            }
        }
    }
}
