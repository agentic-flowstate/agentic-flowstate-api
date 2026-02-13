use axum::{
    extract::{Path, Query, State},
    http::{StatusCode, HeaderMap},
    Json,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::json;
use sqlx::SqlitePool;
use std::sync::Arc;
use tracing::{error, info};

use crate::{
    models::{CreateTicketRequest, UpdateTicketRequest},
    mcp_wrapper::call_mcp_tool,
};

use super::get_organization;

#[derive(Debug, Deserialize)]
pub struct TicketQuery {
    pub slice_id: Option<String>,
}

// List all tickets for an organization
pub async fn list_all_tickets(
    State(pool): State<Arc<SqlitePool>>,
    headers: HeaderMap,
) -> Response {
    let organization = get_organization(&headers);

    match ticketing_system::tickets::list_tickets_by_organization(&pool, &organization).await {
        Ok(tickets) => {
            (StatusCode::OK, Json(tickets)).into_response()
        }
        Err(e) => {
            error!("Failed to list all tickets: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to list tickets: {}", e) }))
            ).into_response()
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
        Ok(result) => {
            (StatusCode::OK, Json(result)).into_response()
        }
        Err(e) => {
            error!("Failed to list tickets: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to list tickets: {}", e) }))
            ).into_response()
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
        Query(TicketQuery { slice_id: Some(slice_id) })
    ).await
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
        Ok(result) => {
            (StatusCode::OK, Json(result)).into_response()
        }
        Err(e) => {
            error!("Failed to get ticket: {:?}", e);
            if e.to_string().contains("not found") {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "Ticket not found" }))
                ).into_response()
            } else {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("Failed to get ticket: {}", e) }))
                ).into_response()
            }
        }
    }
}

pub async fn create_ticket(
    State(_pool): State<Arc<SqlitePool>>,
    headers: HeaderMap,
    Path((epic_id, slice_id)): Path<(String, String)>,
    Json(request): Json<CreateTicketRequest>,
) -> Response {
    let organization = get_organization(&headers);
    let ref_handle = format!("api-{}", uuid::Uuid::new_v4().to_string().split('-').next().unwrap_or("0"));
    let args = json!({
        "organization": organization,
        "epic_id": epic_id,
        "slice_id": slice_id,
        "tickets": [{
            "ref": ref_handle,
            "title": request.title,
            "ticket_type": "milestone",
            "pipeline_template_id": "human-task",
        }]
    });

    match call_mcp_tool("create_slice_tickets", Some(args)).await {
        Ok(result) => {
            // Extract first ticket from batch result for single-item response
            let ticket = result.get("tickets")
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
                Json(json!({ "error": format!("Failed to create ticket: {}", e) }))
            ).into_response()
        }
    }
}

// Update ticket with full path (epic_id, slice_id, ticket_id)
pub async fn update_ticket_nested(
    State(_pool): State<Arc<SqlitePool>>,
    headers: HeaderMap,
    Path((epic_id, slice_id, ticket_id)): Path<(String, String, String)>,
    Json(request): Json<UpdateTicketRequest>,
) -> Response {
    let organization = get_organization(&headers);

    // Determine which update operation to use based on what's being updated
    if let Some(status) = request.status {
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
                    Json(json!({ "error": format!("Failed to update ticket: {}", e) }))
                ).into_response()
            }
        }
    } else if request.notes.is_some() {
        let args = json!({
            "organization": organization,
            "epic_id": epic_id,
            "slice_id": slice_id,
            "ticket_id": ticket_id,
            "notes": request.notes
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
                    Json(json!({ "error": format!("Failed to update ticket: {}", e) }))
                ).into_response()
            }
        }
    } else {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "No fields to update" }))
        ).into_response()
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
                    Json(json!({ "error": "Ticket not found" }))
                ).into_response()
            } else {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("Failed to delete ticket: {}", e) }))
                ).into_response()
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
                Json(json!({ "error": format!("Failed to add relationship: {}", e) }))
            ).into_response()
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
                Json(json!({ "error": format!("Failed to remove relationship: {}", e) }))
            ).into_response()
        }
    }
}

// Get ticket by ID only (uses index lookup - ticket_id is globally unique)
pub async fn get_ticket_by_id(
    State(_pool): State<Arc<SqlitePool>>,
    Path(ticket_id): Path<String>,
) -> Response {
    // ticket_id is globally unique, no organization needed
    let args = json!({
        "ticket_id": ticket_id
    });

    match call_mcp_tool("get_ticket", Some(args)).await {
        Ok(result) => {
            (StatusCode::OK, Json(result)).into_response()
        }
        Err(e) => {
            error!("Failed to get ticket by id: {:?}", e);
            if e.to_string().contains("not found") {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "Ticket not found" }))
                ).into_response()
            } else {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("Failed to get ticket: {}", e) }))
                ).into_response()
            }
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
    ).await {
        Ok(()) => {
            // Fetch and return the updated ticket
            match ticketing_system::tickets::get_ticket_by_id(&pool, &ticket_id).await {
                Ok(Some(ticket)) => {
                    info!("Updated ticket guidance for: {}", ticket_id);
                    (StatusCode::OK, Json(ticket)).into_response()
                }
                Ok(None) => {
                    (
                        StatusCode::NOT_FOUND,
                        Json(json!({ "error": "Ticket not found" }))
                    ).into_response()
                }
                Err(e) => {
                    error!("Failed to fetch updated ticket: {:?}", e);
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": format!("Failed to fetch ticket: {}", e) }))
                    ).into_response()
                }
            }
        }
        Err(e) => {
            error!("Failed to update ticket guidance: {:?}", e);
            if e.to_string().contains("not found") {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "Ticket not found" }))
                ).into_response()
            } else {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("Failed to update guidance: {}", e) }))
                ).into_response()
            }
        }
    }
}
