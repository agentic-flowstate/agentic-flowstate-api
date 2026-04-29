//! Automatic HTTP request logging middleware.
//! Logs every request to the system_logs table with method, path, status, duration,
//! user_id, and auto-detected component.

use std::sync::Arc;
use std::time::Instant;

use axum::{extract::Request, middleware::Next, response::Response};
use sqlx::SqlitePool;

/// Paths that are too noisy to log (SSE streams, health checks, polling endpoints).
const SKIP_PATHS: &[&str] = &[
    "/health",
    "/api/data/subscribe",
    "/api/emails/subscribe",
    "/api/conversations/subscribe",
    "/api/dms/subscribe",
    "/api/meetings/subscribe",
    "/api/daily-plan/subscribe",
    "/api/meetings/signaling",
];

/// Detect component from the URL path prefix.
fn detect_component(path: &str) -> &'static str {
    if path.starts_with("/api/auth") {
        "auth"
    } else if path.starts_with("/api/agent-runs") || path.contains("/agent-runs") {
        "agent"
    } else if path.starts_with("/api/email") || path.starts_with("/api/drafts") {
        "email"
    } else if path.starts_with("/api/conversations")
        || path.starts_with("/api/dms")
        || path.starts_with("/api/full-access/chat")
        || path.starts_with("/api/workspace-manager/chat")
        || path.starts_with("/api/scoped-workspace/chat")
    {
        "chat"
    } else if path.starts_with("/api/meetings") || path.starts_with("/api/tts") {
        "meetings"
    } else if path.starts_with("/api/daily-plan")
        || path.starts_with("/api/focus")
        || path.starts_with("/api/home-planner")
    {
        "planner"
    } else if path.starts_with("/api/library")
        || path.starts_with("/api/tickets") && path.contains("/docs")
    {
        "library"
    } else if path.starts_with("/api/admin") {
        "admin"
    } else if path.starts_with("/api/epics")
        || path.starts_with("/api/tickets")
        || path.starts_with("/api/workspace")
    {
        "workspace"
    } else if path.starts_with("/api/memberships") {
        "auth"
    } else {
        "api"
    }
}

/// Determine log level from HTTP status code.
fn level_from_status(status: u16) -> &'static str {
    match status {
        200..=299 => "info",
        400..=499 => "warn",
        _ => "error",
    }
}

/// Request logging middleware. Applied as an outermost layer so it captures all requests
/// including auth failures. Writes to system_logs in a background task (non-blocking).
pub async fn request_logger(request: Request, next: Next) -> Response {
    let method = request.method().to_string();
    let path = request.uri().path().to_string();

    // Skip noisy endpoints
    if SKIP_PATHS.iter().any(|skip| path == *skip) {
        return next.run(request).await;
    }

    // Extract session cookie for session_id (before passing request to next)
    let session_id = request
        .headers()
        .get_all("cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|cookie| {
            let cookie = cookie.trim();
            if cookie.starts_with("session=") {
                Some(cookie[8..].to_string())
            } else {
                None
            }
        });

    // Try to get db pool from request extensions (set by State)
    // We'll grab it from extensions after the response, but we need the pool reference.
    // Since we can't easily get State in a plain middleware fn, we store the pool
    // in request extensions from main.rs.
    let pool = request.extensions().get::<Arc<SqlitePool>>().cloned();

    let start = Instant::now();
    let response = next.run(request).await;
    let duration = start.elapsed();
    let status = response.status().as_u16();

    // Log in background task — never block the response
    if let Some(pool) = pool {
        let component = detect_component(&path).to_string();
        let level = level_from_status(status).to_string();
        let duration_ms = duration.as_millis();

        let message = format!("{} {} → {} ({}ms)", method, path, status, duration_ms);

        // Extract user_id from response extensions (set by require_auth middleware)
        // We can't easily get it here since AuthenticatedUser is set on the request.
        // Instead, we use session_id which is always available from the cookie.
        let sid = session_id.clone();

        tokio::spawn(async move {
            let _ = ticketing_system::system_logs::insert_log(
                &pool,
                &level,
                &component,
                &message,
                None,
                None,
                sid.as_deref(),
            )
            .await;
        });
    }

    response
}
