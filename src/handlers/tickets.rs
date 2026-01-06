use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use tracing::{error, info};

use crate::{
    db::DynamoDbPool,
    models::{CreateTicketRequest, UpdateTicketRequest},
    mcp_wrapper::call_mcp_tool,
};

#[derive(Debug, Deserialize)]
pub struct TicketQuery {
    pub slice_id: Option<String>,
}

// List tickets for an epic or a specific slice
pub async fn list_tickets(
    State(_pool): State<Arc<DynamoDbPool>>,
    Path(epic_id): Path<String>,
    Query(params): Query<TicketQuery>,
) -> Response {
    let args = if let Some(slice_id) = params.slice_id {
        json!({
            "epic_id": epic_id,
            "slice_id": slice_id
        })
    } else {
        json!({ "epic_id": epic_id })
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
    State(pool): State<Arc<DynamoDbPool>>,
    Path((epic_id, slice_id)): Path<(String, String)>,
) -> Response {
    list_tickets(
        State(pool),
        Path(epic_id),
        Query(TicketQuery { slice_id: Some(slice_id) })
    ).await
}

pub async fn get_ticket(
    State(_pool): State<Arc<DynamoDbPool>>,
    Path(ticket_id): Path<String>,
) -> Response {
    let args = json!({ "ticket_id": ticket_id });

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
    State(_pool): State<Arc<DynamoDbPool>>,
    Path((epic_id, slice_id)): Path<(String, String)>,
    Json(request): Json<CreateTicketRequest>,
) -> Response {
    let args = json!({
        "epic_id": epic_id,
        "slice_id": slice_id,
        "title": request.title,
        "notes": request.notes,
        "priority": request.priority,
        "assignees": request.assignees,
        "tags": request.tags,
    });

    match call_mcp_tool("create_ticket", Some(args)).await {
        Ok(result) => {
            info!("Created ticket: {:?}", result);
            (StatusCode::CREATED, Json(result)).into_response()
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

pub async fn update_ticket(
    State(_pool): State<Arc<DynamoDbPool>>,
    Path(ticket_id): Path<String>,
    Json(request): Json<UpdateTicketRequest>,
) -> Response {
    // Determine which update operation to use based on what's being updated
    if let Some(status) = request.status {
        let args = json!({
            "ticket_id": ticket_id,
            "status": status
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

pub async fn delete_ticket(
    State(_pool): State<Arc<DynamoDbPool>>,
    Path(ticket_id): Path<String>,
) -> Response {
    let args = json!({ "ticket_id": ticket_id });

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

// Add support for ticket relationships
pub async fn add_ticket_relationship(
    State(_pool): State<Arc<DynamoDbPool>>,
    Path(ticket_id): Path<String>,
    Json(request): Json<serde_json::Value>,
) -> Response {
    let args = json!({
        "ticket_id": ticket_id,
        "related_ticket_id": request["related_ticket_id"],
        "relationship_type": request["relationship_type"]
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

pub async fn remove_ticket_relationship(
    State(_pool): State<Arc<DynamoDbPool>>,
    Path((ticket_id, related_ticket_id)): Path<(String, String)>,
) -> Response {
    let args = json!({
        "ticket_id": ticket_id,
        "related_ticket_id": related_ticket_id
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