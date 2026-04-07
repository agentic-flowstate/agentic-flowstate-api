//! Quick commands REST API handler

use axum::{
    extract::{Extension, State},
    http::StatusCode,
    Json,
};
use serde::Serialize;
use sqlx::SqlitePool;
use std::sync::Arc;

use crate::auth_middleware::AuthenticatedUser;

/// API response — only the fields the iOS app needs
#[derive(Serialize)]
pub struct QuickCommandResponse {
    pub id: i64,
    pub label: String,
    pub icon: String,
    pub message: String,
    pub sort_order: i64,
}

/// GET /api/quick-commands
pub async fn list_quick_commands(
    State(db): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<Json<Vec<QuickCommandResponse>>, (StatusCode, String)> {
    let commands = ticketing_system::quick_commands::list_commands(&db, &user.user_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let response: Vec<QuickCommandResponse> = commands
        .into_iter()
        .map(|c| QuickCommandResponse {
            id: c.id,
            label: c.label,
            icon: c.icon,
            message: c.message,
            sort_order: c.sort_order,
        })
        .collect();

    Ok(Json(response))
}
