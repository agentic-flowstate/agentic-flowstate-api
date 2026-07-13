//! Automatic HTTP request tracing and durable request-summary middleware.
//! Logs sanitized route-template request summaries to `system_logs` and emits
//! route-template RED metrics. Raw URI paths, cookies, prompts, and message
//! bodies do not belong in this layer.

use std::sync::Arc;
use std::time::Instant;

use axum::{extract::MatchedPath, extract::Request, middleware::Next, response::Response};
use sqlx::SqlitePool;
use tracing::Instrument;

use crate::observability::request::{
    method_label, record_http_request, route_template_fallback, status_class, ErrorClass,
    RequestTelemetryContext,
};

use crate::observability::contracts;

/// Optional response extension that handlers can set to persist diagnostic
/// detail on the automatic request log row.
#[derive(Clone, Debug)]
pub struct RequestLogDetail(pub String);

/// Paths that are too noisy to log (SSE streams, health checks, polling endpoints).
const SKIP_PATHS: &[&str] = &[
    "/health",
    "/health/ready",
    "/metrics",
    "/api/data/subscribe",
    "/api/emails/subscribe",
    "/api/conversations/subscribe",
    "/api/dms/subscribe",
    "/api/meetings/subscribe",
    "/api/daily-plan/subscribe",
    "/api/meetings/signaling",
    "/api/events/subscribe",
];

fn should_skip_durable_log(route: &str, status: u16) -> bool {
    status < 500 && SKIP_PATHS.iter().any(|skip| route == *skip)
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
fn detect_component(route: &str) -> &'static str {
    if route.starts_with("/api/auth") {
        "auth"
    } else if route.starts_with("/api/agent-runs") || route.contains("/agent-runs") {
        "agent"
    } else if route.starts_with("/api/email")
        || route.starts_with("/api/drafts")
        || route.starts_with("/api/outreach")
        || route.starts_with("/u/")
    {
        "email"
    } else if route.starts_with("/api/conversations")
        || route.starts_with("/api/dms")
        || route.starts_with("/api/full-access/chat")
        || route.starts_with("/api/workspace-manager/chat")
        || route.starts_with("/api/scoped-workspace/chat")
        || route.starts_with("/api/events")
    {
        "chat"
    } else if route.starts_with("/api/meetings") || route.starts_with("/api/tts") {
        "meetings"
    } else if route.starts_with("/api/daily-plan")
        || route.starts_with("/api/focus")
        || route.starts_with("/api/home-planner")
    {
        "planner"
    } else if route.starts_with("/api/library")
        || route.starts_with("/api/tickets") && route.contains("/docs")
    {
        "library"
    } else if route.starts_with("/api/admin") {
        "admin"
    } else if route.starts_with("/api/epics")
        || route.starts_with("/api/tickets")
        || route.starts_with("/api/workspace")
    {
        "workspace"
    } else if route.starts_with("/api/memberships") {
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
    let mut request = request;
    let method = method_label(request.method()).to_string();
    let raw_path = request.uri().path().to_string();
    let matched_route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|matched| matched.as_str().to_string());
    let route_template = matched_route
        .clone()
        .unwrap_or_else(|| route_template_fallback(&raw_path).to_string());
    let durable_route = matched_route.unwrap_or_else(|| normalize_path(&raw_path));
    let trace_context = RequestTelemetryContext::from_headers(request.headers());
    request.extensions_mut().insert(trace_context.clone());

    // Try to get db pool from request extensions (set by State)
    // We'll grab it from extensions after the response, but we need the pool reference.
    // Since we can't easily get State in a plain middleware fn, we store the pool
    // in request extensions from main.rs.
    let pool = request.extensions().get::<Arc<SqlitePool>>().cloned();

    let span = tracing::info_span!(
        "http.request",
        http.method = %method,
        http.route = %route_template,
        request_id = %trace_context.request_id,
        trace_id = %trace_context.trace_id,
        span_id = %trace_context.span_id,
        parent_span_id = trace_context.parent_span_id.as_deref().unwrap_or("none")
    );

    let start = Instant::now();
    let mut response = next.run(request).instrument(span.clone()).await;
    let duration = start.elapsed();
    let status = response.status().as_u16();
    trace_context.apply_response_headers(response.headers_mut());

    let status_code = response.status();
    let error_class = ErrorClass::from_status(status_code);
    record_http_request(&method, &route_template, status_code, error_class, duration);

    let detail = response
        .extensions()
        .get::<RequestLogDetail>()
        .map(|detail| detail.0.clone());
    if let Some(detail) = detail.as_deref() {
        contracts::assert_system_log_detail(detail);
    }

    tracing::info!(
        parent: &span,
        http.status_code = status,
        http.status_class = status_class(status_code),
        outcome = if status_code.is_client_error() || status_code.is_server_error() { "error" } else { "success" },
        error_type = error_class.as_str(),
        duration_ms = duration.as_millis() as u64,
        "http request completed"
    );

    // Log in background task — never block the response.
    if let Some(pool) = pool.filter(|_| {
        !should_skip_durable_log(&route_template, status)
            && !should_skip_durable_log(&durable_route, status)
    }) {
        let component = detect_component(&route_template).to_string();
        let level = level_from_status(status).to_string();
        let duration_ms = duration.as_millis();
        let status_label = status_class(status_code);
        let message = format!(
            "HTTP {} {} completed with {} in {}ms",
            method, durable_route, status, duration_ms
        );
        let detail = serde_json::json!({
            "route": route_template,
            "durable_route": durable_route,
            "method": method,
            "status": status,
            "status_class": status_label,
            "duration_ms": duration_ms,
            "request_id": trace_context.request_id,
            "trace_id": trace_context.trace_id,
            "span_id": trace_context.span_id,
            "error_type": error_class.as_str(),
            "handler_detail": detail,
        })
        .to_string();

        tokio::spawn(async move {
            let _ = ticketing_system::system_logs::insert_log(
                &pool,
                &level,
                &component,
                &message,
                Some(&detail),
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
        assert!(should_skip_durable_log("/health", 200));
        assert!(should_skip_durable_log("/health/ready", 200));
        assert!(should_skip_durable_log("/api/events/subscribe", 200));
        assert!(!should_skip_durable_log("/api/events/subscribe", 500));
        assert!(!should_skip_durable_log("/api/health", 200));
        assert!(!should_skip_durable_log("/api/admin/pending-restart", 200));
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
