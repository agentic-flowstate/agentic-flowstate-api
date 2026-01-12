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
    models::CreateEpicRequest,
    mcp_wrapper::call_mcp_tool,
};

fn get_organization(headers: &HeaderMap) -> String {
    headers.get("X-Organization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("telemetryops")
        .to_string()
}

#[derive(Debug, Deserialize)]
pub struct ListEpicsQuery {
    pub organization: Option<String>,
}

pub async fn list_epics(
    State(_pool): State<Arc<SqlitePool>>,
    headers: HeaderMap,
    Query(query): Query<ListEpicsQuery>,
) -> Response {
    // Use query param if provided, otherwise check header, otherwise list ALL
    let org = query.organization.or_else(|| {
        headers.get("X-Organization")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    });
    let args = org.map(|o| json!({ "organization": o }));

    match call_mcp_tool("list_epics", args).await {
        Ok(result) => {
            (StatusCode::OK, Json(result)).into_response()
        }
        Err(e) => {
            error!("Failed to list epics: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to list epics: {}", e) }))
            ).into_response()
        }
    }
}

pub async fn get_epic(
    State(_pool): State<Arc<SqlitePool>>,
    headers: HeaderMap,
    Path(epic_id): Path<String>,
) -> Response {
    let organization = get_organization(&headers);
    let args = json!({ "organization": organization, "epic_id": epic_id });

    match call_mcp_tool("get_epic", Some(args)).await {
        Ok(result) => {
            (StatusCode::OK, Json(result)).into_response()
        }
        Err(e) => {
            error!("Failed to get epic: {:?}", e);
            if e.to_string().contains("not found") {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "Epic not found" }))
                ).into_response()
            } else {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("Failed to get epic: {}", e) }))
                ).into_response()
            }
        }
    }
}

pub async fn create_epic(
    State(_pool): State<Arc<SqlitePool>>,
    Json(request): Json<CreateEpicRequest>,
) -> Response {
    let args = json!({
        "epic_id": request.epic_id,
        "title": request.title,
        "organization": request.organization,
        "notes": request.notes,
        "assignees": request.assignees,
    });

    match call_mcp_tool("create_epic", Some(args)).await {
        Ok(result) => {
            info!("Created epic: {:?}", result);
            (StatusCode::CREATED, Json(result)).into_response()
        }
        Err(e) => {
            error!("Failed to create epic: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to create epic: {}", e) }))
            ).into_response()
        }
    }
}

pub async fn delete_epic(
    State(_pool): State<Arc<SqlitePool>>,
    headers: HeaderMap,
    Path(epic_id): Path<String>,
) -> Response {
    let organization = get_organization(&headers);
    let args = json!({ "organization": organization, "epic_id": epic_id });

    match call_mcp_tool("delete_epic", Some(args)).await {
        Ok(result) => {
            info!("Deleted epic: {:?}", result);
            (StatusCode::OK, Json(result)).into_response()
        }
        Err(e) => {
            error!("Failed to delete epic: {:?}", e);
            if e.to_string().contains("not found") {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "Epic not found" }))
                ).into_response()
            } else {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("Failed to delete epic: {}", e) }))
                ).into_response()
            }
        }
    }
}