use axum::{
    extract::{Path, State},
    http::{StatusCode, HeaderMap},
    Json,
    response::{IntoResponse, Response},
};
use serde_json::json;
use sqlx::SqlitePool;
use std::sync::Arc;
use tracing::{error, info};

use crate::{
    models::CreateSliceRequest,
    mcp_wrapper::call_mcp_tool,
};

use super::get_organization;

pub async fn list_slices(
    State(_pool): State<Arc<SqlitePool>>,
    headers: HeaderMap,
    Path(epic_id): Path<String>,
) -> Response {
    let organization = get_organization(&headers);
    let args = json!({ "organization": organization, "epic_id": epic_id });

    match call_mcp_tool("list_slices", Some(args)).await {
        Ok(result) => {
            (StatusCode::OK, Json(result)).into_response()
        }
        Err(e) => {
            error!("Failed to list slices: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to list slices: {}", e) }))
            ).into_response()
        }
    }
}

pub async fn get_slice(
    State(_pool): State<Arc<SqlitePool>>,
    headers: HeaderMap,
    Path((epic_id, slice_id)): Path<(String, String)>,
) -> Response {
    let organization = get_organization(&headers);
    let args = json!({
        "organization": organization,
        "epic_id": epic_id,
        "slice_id": slice_id
    });

    match call_mcp_tool("get_slice", Some(args)).await {
        Ok(result) => {
            (StatusCode::OK, Json(result)).into_response()
        }
        Err(e) => {
            error!("Failed to get slice: {:?}", e);
            if e.to_string().contains("not found") {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "Slice not found" }))
                ).into_response()
            } else {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("Failed to get slice: {}", e) }))
                ).into_response()
            }
        }
    }
}

pub async fn create_slice(
    State(_pool): State<Arc<SqlitePool>>,
    headers: HeaderMap,
    Path(epic_id): Path<String>,
    Json(request): Json<CreateSliceRequest>,
) -> Response {
    let organization = get_organization(&headers);
    let args = json!({
        "organization": organization,
        "slices": [{
            "epic_id": epic_id,
            "slice_id": request.slice_id,
            "title": request.title,
            "notes": request.notes,
        }]
    });

    match call_mcp_tool("create_slices", Some(args)).await {
        Ok(result) => {
            // Extract first slice from batch result for single-item response
            let slice = result.get("slices")
                .and_then(|s| s.get(0))
                .cloned()
                .unwrap_or(result);
            info!("Created slice: {:?}", slice);
            (StatusCode::CREATED, Json(slice)).into_response()
        }
        Err(e) => {
            error!("Failed to create slice: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to create slice: {}", e) }))
            ).into_response()
        }
    }
}

pub async fn delete_slice(
    State(_pool): State<Arc<SqlitePool>>,
    headers: HeaderMap,
    Path((epic_id, slice_id)): Path<(String, String)>,
) -> Response {
    let organization = get_organization(&headers);
    let args = json!({
        "organization": organization,
        "epic_id": epic_id,
        "slice_id": slice_id
    });

    match call_mcp_tool("delete_slice", Some(args)).await {
        Ok(result) => {
            info!("Deleted slice: {:?}", result);
            (StatusCode::OK, Json(result)).into_response()
        }
        Err(e) => {
            error!("Failed to delete slice: {:?}", e);
            if e.to_string().contains("not found") {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "Slice not found" }))
                ).into_response()
            } else {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("Failed to delete slice: {}", e) }))
                ).into_response()
            }
        }
    }
}