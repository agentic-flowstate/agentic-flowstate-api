//! Authentication middleware - validates session cookie on protected routes

use std::sync::Arc;
use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use tower_cookies::Cookies;

use ticketing_system::SqlitePool;

const SESSION_COOKIE: &str = "session";

/// Middleware that requires a valid session cookie.
/// Returns 401 if no cookie or session is invalid/expired.
pub async fn require_auth(
    State(pool): State<Arc<SqlitePool>>,
    cookies: Cookies,
    request: Request,
    next: Next,
) -> Response {
    let session_id = match cookies.get(SESSION_COOKIE) {
        Some(cookie) => cookie.value().to_string(),
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "Authentication required"})),
            )
                .into_response();
        }
    };

    match ticketing_system::auth::validate_session(&pool, &session_id).await {
        Ok(Some(_user)) => next.run(request).await,
        Ok(None) => (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "Session expired or invalid"})),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("Auth middleware error: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Authentication check failed"})),
            )
                .into_response()
        }
    }
}
