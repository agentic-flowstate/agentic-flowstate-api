//! Authentication middleware - validates session cookie on protected routes

use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use std::sync::Arc;
use tower_cookies::Cookies;

use ticketing_system::SqlitePool;

const SESSION_COOKIE: &str = "session";

/// Authenticated user info stored in request extensions by require_auth middleware.
#[derive(Clone, Debug)]
pub struct AuthenticatedUser {
    pub user_id: String,
}

/// Middleware that requires a valid session cookie.
/// Inserts `AuthenticatedUser` into request extensions for downstream handlers/middleware.
/// Returns 401 if no cookie or session is invalid/expired.
pub async fn require_auth(
    State(pool): State<Arc<SqlitePool>>,
    cookies: Cookies,
    mut request: Request,
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
        Ok(Some(user)) => {
            request.extensions_mut().insert(AuthenticatedUser {
                user_id: user.user_id,
            });
            next.run(request).await
        }
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

/// Middleware that checks if the authenticated user is an admin.
/// Must be applied AFTER require_auth (needs AuthenticatedUser in extensions).
/// Returns 403 if user is not an admin.
pub async fn require_admin(
    State(pool): State<Arc<SqlitePool>>,
    request: Request,
    next: Next,
) -> Response {
    let user = match request.extensions().get::<AuthenticatedUser>() {
        Some(u) => u.clone(),
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "Authentication required"})),
            )
                .into_response();
        }
    };

    match ticketing_system::system_logs::is_admin(&pool, &user.user_id).await {
        Ok(true) => next.run(request).await,
        Ok(false) => (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "Admin access required"})),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("Admin check error: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Admin access check failed"})),
            )
                .into_response()
        }
    }
}

/// Middleware that checks X-Organization header against user's memberships.
/// Must be applied AFTER require_auth (needs AuthenticatedUser in extensions).
/// Returns 403 if user is not a member of the requested organization.
pub async fn require_org_access(
    State(pool): State<Arc<SqlitePool>>,
    request: Request,
    next: Next,
) -> Response {
    let user = match request.extensions().get::<AuthenticatedUser>() {
        Some(u) => u.clone(),
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "Authentication required"})),
            )
                .into_response();
        }
    };

    let org = match request
        .headers()
        .get("X-Organization")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
    {
        Some(org) => org.to_string(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "X-Organization header is required"})),
            )
                .into_response();
        }
    };

    match ticketing_system::memberships::check_membership(&pool, &user.user_id, &org).await {
        Ok(true) => next.run(request).await,
        Ok(false) => (
            StatusCode::FORBIDDEN,
            Json(json!({"error": format!("No access to organization: {}", org)})),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("Org access check error: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Organization access check failed"})),
            )
                .into_response()
        }
    }
}
