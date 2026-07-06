//! Automatic HTTP request logging middleware.
//! Logs every request to the system_logs table with method, normalized route-ish
//! path, status, duration, and auto-detected component.

use std::sync::Arc;
use std::time::Instant;

use axum::{extract::Request, middleware::Next, response::Response};
use sqlx::SqlitePool;

use crate::observability::contracts;

/// Optional response extension that handlers can set to persist diagnostic
/// detail on the automatic request log row.
#[derive(Clone, Debug)]
pub struct RequestLogDetail(pub String);

/// Paths that are too noisy to log (SSE streams, health checks, polling endpoints).
const SKIP_PATHS: &[&str] = &[
    "/health",
    "/health/ready",
    "/api/data/subscribe",
    "/api/emails/subscribe",
    "/api/conversations/subscribe",
    "/api/dms/subscribe",
    "/api/meetings/subscribe",
    "/api/daily-plan/subscribe",
    "/api/meetings/signaling",
];

fn should_skip_path(path: &str) -> bool {
    SKIP_PATHS.iter().any(|skip| path == *skip)
}

fn normalize_path(path: &str) -> String {
    if path == "/" {
        return "/".to_string();
    }

    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            if is_high_cardinality_segment(segment) {
                ":id"
            } else {
                segment
            }
        })
        .collect::<Vec<_>>();

    format!("/{}", segments.join("/"))
}

fn is_high_cardinality_segment(segment: &str) -> bool {
    if segment.chars().all(|ch| ch.is_ascii_digit()) {
        return true;
    }
    if looks_like_uuid(segment) {
        return true;
    }
    if matches!(
        segment.get(0..2),
        Some("T-") | Some("A-") | Some("D-") | Some("R-")
    ) || segment.starts_with("CP-")
    {
        return true;
    }
    segment.len() >= 24
        && segment
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

fn looks_like_uuid(value: &str) -> bool {
    let mut parts = value.split('-');
    let lengths = [8, 4, 4, 4, 12];
    for expected_len in lengths {
        let Some(part) = parts.next() else {
            return false;
        };
        if part.len() != expected_len || !part.chars().all(|ch| ch.is_ascii_hexdigit()) {
            return false;
        }
    }
    parts.next().is_none()
}

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
    let normalized_path = normalize_path(&path);

    // Skip noisy endpoints
    if should_skip_path(&path) {
        return next.run(request).await;
    }

    // Try to get db pool from request extensions (set by State)
    // We'll grab it from extensions after the response, but we need the pool reference.
    // Since we can't easily get State in a plain middleware fn, we store the pool
    // in request extensions from main.rs.
    let pool = request.extensions().get::<Arc<SqlitePool>>().cloned();

    let start = Instant::now();
    let response = next.run(request).await;
    let duration = start.elapsed();
    let status = response.status().as_u16();
    let detail = response
        .extensions()
        .get::<RequestLogDetail>()
        .map(|detail| detail.0.clone());
    if let Some(detail) = detail.as_deref() {
        contracts::assert_system_log_detail(detail);
    }

    // Log in background task — never block the response
    if let Some(pool) = pool {
        let component = detect_component(&path).to_string();
        let level = level_from_status(status).to_string();
        let duration_ms = duration.as_millis();

        let message = format!(
            "{} {} -> {} ({}ms)",
            method, normalized_path, status, duration_ms
        );

        tokio::spawn(async move {
            let _ = ticketing_system::system_logs::insert_log(
                &pool,
                &level,
                &component,
                &message,
                detail.as_deref(),
                None,
                None,
            )
            .await;
        });
    }

    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_probe_is_skipped_as_health_check_noise() {
        assert!(should_skip_path("/health"));
        assert!(should_skip_path("/health/ready"));
        assert!(!should_skip_path("/api/health"));
        assert!(!should_skip_path("/api/admin/pending-restart"));
    }

    #[test]
    fn non_health_5xx_responses_still_log_as_errors() {
        assert_eq!(level_from_status(500), "error");
        assert_eq!(level_from_status(503), "error");
        assert_eq!(level_from_status(404), "warn");
        assert_eq!(level_from_status(200), "info");
    }

    #[test]
    fn request_paths_are_normalized_before_durable_logging() {
        assert_eq!(
            normalize_path("/api/tickets/T-12345678"),
            "/api/tickets/:id"
        );
        assert_eq!(
            normalize_path("/api/conversations/123/messages"),
            "/api/conversations/:id/messages"
        );
        assert_eq!(
            normalize_path("/api/runs/550e8400-e29b-41d4-a716-446655440000"),
            "/api/runs/:id"
        );
        assert_eq!(normalize_path("/api/admin/logs"), "/api/admin/logs");
    }
}
