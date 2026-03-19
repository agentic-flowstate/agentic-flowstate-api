//! Organization membership API handlers

use std::sync::Arc;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Extension, Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use ticketing_system::SqlitePool;

use crate::auth_middleware::AuthenticatedUser;

#[derive(Deserialize)]
pub struct AddMemberRequest {
    pub user_id: String,
    pub organization: String,
    pub role: Option<String>,
}

/// GET /api/memberships - list my organizations
pub async fn list_my_organizations(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let orgs = ticketing_system::memberships::list_user_organizations(&pool, &user.user_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to list user organizations: {:?}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "Failed to list organizations"})))
        })?;

    Ok(Json(json!(orgs)))
}

/// GET /api/memberships/:org - list members of an organization (owner only)
pub async fn list_members(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let is_owner = ticketing_system::memberships::is_owner(&pool, &user.user_id, &org)
        .await
        .map_err(|e| {
            tracing::error!("Failed to check ownership: {:?}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "Failed to check ownership"})))
        })?;

    if !is_owner {
        return Err((StatusCode::FORBIDDEN, Json(json!({"error": "Only organization owners can list members"}))));
    }

    let members = ticketing_system::memberships::list_members(&pool, &org)
        .await
        .map_err(|e| {
            tracing::error!("Failed to list members: {:?}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "Failed to list members"})))
        })?;

    Ok(Json(json!(members)))
}

/// POST /api/memberships - add a member (owner only)
pub async fn add_member(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(req): Json<AddMemberRequest>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    let role = req.role.as_deref().unwrap_or("member");

    if role != "owner" && role != "member" {
        return Err((StatusCode::BAD_REQUEST, Json(json!({"error": "role must be 'owner' or 'member'"}))));
    }

    let is_owner = ticketing_system::memberships::is_owner(&pool, &user.user_id, &req.organization)
        .await
        .map_err(|e| {
            tracing::error!("Failed to check ownership: {:?}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "Failed to check ownership"})))
        })?;

    if !is_owner {
        return Err((StatusCode::FORBIDDEN, Json(json!({"error": "Only organization owners can add members"}))));
    }

    let membership = ticketing_system::memberships::add_member(&pool, &req.user_id, &req.organization, role)
        .await
        .map_err(|e| {
            tracing::error!("Failed to add member: {:?}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "Failed to add member"})))
        })?;

    Ok((StatusCode::CREATED, Json(json!(membership))))
}

/// DELETE /api/memberships/:org/:user_id - remove a member (owner only)
pub async fn remove_member(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path((org, target_user_id)): Path<(String, String)>,
) -> Result<StatusCode, (StatusCode, Json<Value>)> {
    let is_owner = ticketing_system::memberships::is_owner(&pool, &user.user_id, &org)
        .await
        .map_err(|e| {
            tracing::error!("Failed to check ownership: {:?}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "Failed to check ownership"})))
        })?;

    if !is_owner {
        return Err((StatusCode::FORBIDDEN, Json(json!({"error": "Only organization owners can remove members"}))));
    }

    let removed = ticketing_system::memberships::remove_member(&pool, &target_user_id, &org)
        .await
        .map_err(|e| {
            tracing::error!("Failed to remove member: {:?}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "Failed to remove member"})))
        })?;

    if removed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err((StatusCode::NOT_FOUND, Json(json!({"error": "Membership not found"}))))
    }
}
