use axum::{
    extract::{Extension, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use std::sync::Arc;
use ticketing_system::SqlitePool;

use crate::auth_middleware::AuthenticatedUser;

#[derive(Debug, Deserialize)]
pub struct RegisterTokenRequest {
    pub device_token: String,
    pub device_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RemoveTokenRequest {
    pub device_token: String,
}

/// POST /api/device-tokens — register a device token for push notifications
pub async fn register_device_token(
    State(db): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(req): Json<RegisterTokenRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
    ticketing_system::device_tokens::upsert_token(
        &db,
        &user.user_id,
        &req.device_token,
        req.device_name.as_deref(),
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    tracing::info!("[DEVICE-TOKEN] Registered token for user {}", user.user_id);
    Ok(StatusCode::OK)
}

/// DELETE /api/device-tokens — unregister a device token
pub async fn remove_device_token(
    State(db): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(req): Json<RemoveTokenRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
    ticketing_system::device_tokens::remove_token(&db, &req.device_token)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    tracing::info!("[DEVICE-TOKEN] Removed token for user {}", user.user_id);
    Ok(StatusCode::OK)
}
