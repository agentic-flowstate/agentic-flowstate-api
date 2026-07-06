//! Backend request tracing and request-economics observability.
//!
//! This module owns the API request metrics that must stay low-cardinality:
//! route templates, stable method/status/error enums, and bounded economics
//! measurements for high-risk conversation and SSE endpoints.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use metrics::{counter, histogram};
use serde::Serialize;
use ticketing_system::SqlitePool;
use uuid::Uuid;

pub const METRIC_HTTP_REQUESTS_TOTAL: &str = "af_http_requests_total";
pub const METRIC_HTTP_REQUEST_DURATION_SECONDS: &str = "af_http_request_duration_seconds";
pub const METRIC_REQUEST_DB_QUERIES_TOTAL: &str = "af_request_db_queries_total";
pub const METRIC_REQUEST_DB_QUERY_DURATION_SECONDS: &str = "af_request_db_query_duration_seconds";
pub const METRIC_REQUEST_PAYLOAD_BYTES: &str = "af_request_payload_bytes";
pub const METRIC_REQUEST_PAYLOAD_OBJECTS: &str = "af_request_payload_objects";
pub const METRIC_REQUEST_SERIALIZATION_DURATION_SECONDS: &str =
    "af_request_serialization_duration_seconds";
pub const METRIC_REQUEST_SERIALIZATION_BYTES: &str = "af_request_serialization_bytes";

pub const ROUTE_CONVERSATIONS: &str = "/api/conversations";
pub const ROUTE_CONVERSATION_MESSAGES: &str = "/api/conversations/:id/messages";
pub const ROUTE_CONVERSATION_EVENTS_PAGE: &str = "/api/v1/conversations/:id/events/page";
pub const ROUTE_CONVERSATION_EVENTS_STREAM: &str = "/api/v1/conversations/:id/events";
pub const ROUTE_UNIFIED_EVENTS: &str = "/api/events/subscribe";

pub const QUERY_COUNT_WARN_THRESHOLD: u64 = 100;
pub const PAYLOAD_BYTES_WARN_THRESHOLD: u64 = 1024 * 1024;
pub const PAYLOAD_OBJECTS_WARN_THRESHOLD: u64 = 5_000;

#[derive(Clone, Debug)]
pub struct RequestTelemetryContext {
    pub request_id: String,
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub trace_flags: String,
    pub tracestate: Option<String>,
}

impl RequestTelemetryContext {
    pub fn from_headers(headers: &HeaderMap) -> Self {
        let request_id = headers
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .filter(|value| valid_request_id(value))
            .map(ToOwned::to_owned)
            .unwrap_or_else(new_request_id);

        let parsed_traceparent = headers
            .get("traceparent")
            .and_then(|value| value.to_str().ok())
            .and_then(parse_traceparent);
        let tracestate = headers
            .get("tracestate")
            .and_then(|value| value.to_str().ok())
            .filter(|value| valid_tracestate(value))
            .map(ToOwned::to_owned);

        match parsed_traceparent {
            Some(parsed) => Self {
                request_id,
                trace_id: parsed.trace_id,
                span_id: new_span_id(),
                parent_span_id: Some(parsed.parent_span_id),
                trace_flags: parsed.trace_flags,
                tracestate,
            },
            None => Self {
                request_id,
                trace_id: new_trace_id(),
                span_id: new_span_id(),
                parent_span_id: None,
                trace_flags: "01".to_string(),
                tracestate,
            },
        }
    }

    pub fn traceparent(&self) -> String {
        format!("00-{}-{}-{}", self.trace_id, self.span_id, self.trace_flags)
    }

    pub fn apply_response_headers(&self, headers: &mut HeaderMap) {
        if let Ok(value) = HeaderValue::from_str(&self.request_id) {
            headers.insert("x-request-id", value);
        }
        if let Ok(value) = HeaderValue::from_str(&self.traceparent()) {
            headers.insert("traceparent", value);
        }
        if let Some(tracestate) = self.tracestate.as_deref() {
            if let Ok(value) = HeaderValue::from_str(tracestate) {
                headers.insert("tracestate", value);
            }
        }
    }
}

#[derive(Clone, Debug)]
struct ParsedTraceparent {
    trace_id: String,
    parent_span_id: String,
    trace_flags: String,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorClass {
    None,
    Validation,
    Authentication,
    Authorization,
    NotFound,
    Conflict,
    CursorExpired,
    PayloadTooLarge,
    RateLimited,
    Timeout,
    Client,
    Database,
    Serialization,
    Dependency,
    Internal,
}

impl ErrorClass {
    pub fn from_status(status: StatusCode) -> Self {
        match status.as_u16() {
            100..=399 => ErrorClass::None,
            400 => ErrorClass::Validation,
            401 => ErrorClass::Authentication,
            403 => ErrorClass::Authorization,
            404 => ErrorClass::NotFound,
            408 => ErrorClass::Timeout,
            409 => ErrorClass::Conflict,
            410 => ErrorClass::CursorExpired,
            413 => ErrorClass::PayloadTooLarge,
            429 => ErrorClass::RateLimited,
            400..=499 => ErrorClass::Client,
            500..=599 => ErrorClass::Internal,
            _ => ErrorClass::Internal,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ErrorClass::None => "none",
            ErrorClass::Validation => "validation",
            ErrorClass::Authentication => "authentication",
            ErrorClass::Authorization => "authorization",
            ErrorClass::NotFound => "not_found",
            ErrorClass::Conflict => "conflict",
            ErrorClass::CursorExpired => "cursor_expired",
            ErrorClass::PayloadTooLarge => "payload_too_large",
            ErrorClass::RateLimited => "rate_limited",
            ErrorClass::Timeout => "timeout",
            ErrorClass::Client => "client_error",
            ErrorClass::Database => "database",
            ErrorClass::Serialization => "serialization",
            ErrorClass::Dependency => "dependency",
            ErrorClass::Internal => "internal",
        }
    }
}

impl fmt::Display for ErrorClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Success,
    Error,
}

impl Outcome {
    pub fn from_status(status: StatusCode) -> Self {
        if status.is_server_error() || status.is_client_error() {
            Outcome::Error
        } else {
            Outcome::Success
        }
    }

    pub fn from_result<T, E>(result: &Result<T, E>) -> Self {
        if result.is_ok() {
            Outcome::Success
        } else {
            Outcome::Error
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Success => "success",
            Outcome::Error => "error",
        }
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

pub fn method_label(method: &Method) -> &'static str {
    match *method {
        Method::GET => "GET",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::PATCH => "PATCH",
        Method::DELETE => "DELETE",
        Method::HEAD => "HEAD",
        Method::OPTIONS => "OPTIONS",
        _ => "OTHER",
    }
}

pub fn status_class(status: StatusCode) -> &'static str {
    match status.as_u16() {
        100..=199 => "1xx",
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        500..=599 => "5xx",
        _ => "unknown",
    }
}

pub fn route_template_fallback(path: &str) -> &'static str {
    match path {
        "/health" => "/health",
        "/health/ready" => "/health/ready",
        "/metrics" => "/metrics",
        _ if path.starts_with("/api/") => "/api/{unmatched}",
        _ => "/{unmatched}",
    }
}

pub fn record_http_request(
    method: &str,
    route: &str,
    status: StatusCode,
    error_class: ErrorClass,
    duration: Duration,
) {
    let status_label = status_class(status);
    let outcome = Outcome::from_status(status);
    let error_label = error_class.as_str();

    counter!(
        METRIC_HTTP_REQUESTS_TOTAL,
        "method" => method.to_string(),
        "route" => route.to_string(),
        "status_class" => status_label,
        "outcome" => outcome.as_str(),
        "error_type" => error_label
    )
    .increment(1);
    histogram!(
        METRIC_HTTP_REQUEST_DURATION_SECONDS,
        "method" => method.to_string(),
        "route" => route.to_string(),
        "status_class" => status_label,
        "outcome" => outcome.as_str(),
        "error_type" => error_label
    )
    .record(duration.as_secs_f64());
}

pub fn record_db_operation(route: &str, operation: &str, duration: Duration, outcome: Outcome) {
    counter!(
        METRIC_REQUEST_DB_QUERIES_TOTAL,
        "route" => route.to_string(),
        "operation" => operation.to_string(),
        "outcome" => outcome.as_str()
    )
    .increment(1);
    histogram!(
        METRIC_REQUEST_DB_QUERY_DURATION_SECONDS,
        "route" => route.to_string(),
        "operation" => operation.to_string(),
        "outcome" => outcome.as_str()
    )
    .record(duration.as_secs_f64());
}

pub fn record_db_query_count(route: &str, operation: &str, query_count: u64, outcome: Outcome) {
    counter!(
        METRIC_REQUEST_DB_QUERIES_TOTAL,
        "route" => route.to_string(),
        "operation" => operation.to_string(),
        "outcome" => outcome.as_str()
    )
    .increment(query_count);
}

pub fn record_payload(route: &str, payload_kind: &str, bytes: u64, objects: u64, outcome: Outcome) {
    histogram!(
        METRIC_REQUEST_PAYLOAD_BYTES,
        "route" => route.to_string(),
        "payload_kind" => payload_kind.to_string(),
        "outcome" => outcome.as_str()
    )
    .record(bytes as f64);
    histogram!(
        METRIC_REQUEST_PAYLOAD_OBJECTS,
        "route" => route.to_string(),
        "payload_kind" => payload_kind.to_string(),
        "outcome" => outcome.as_str()
    )
    .record(objects as f64);
}

pub fn record_serialization(
    route: &str,
    payload_kind: &str,
    duration: Duration,
    bytes: u64,
    outcome: Outcome,
) {
    histogram!(
        METRIC_REQUEST_SERIALIZATION_DURATION_SECONDS,
        "route" => route.to_string(),
        "payload_kind" => payload_kind.to_string(),
        "outcome" => outcome.as_str()
    )
    .record(duration.as_secs_f64());
    histogram!(
        METRIC_REQUEST_SERIALIZATION_BYTES,
        "route" => route.to_string(),
        "payload_kind" => payload_kind.to_string(),
        "outcome" => outcome.as_str()
    )
    .record(bytes as f64);
}

pub fn observe_serialized_payload<T: Serialize>(
    route: &str,
    payload_kind: &str,
    payload: &T,
    objects: u64,
) -> Result<u64, serde_json::Error> {
    let started = std::time::Instant::now();
    let serialized = serde_json::to_vec(payload);
    let duration = started.elapsed();
    let outcome = if serialized.is_ok() {
        Outcome::Success
    } else {
        Outcome::Error
    };
    let bytes = serialized
        .as_ref()
        .map(|body| body.len() as u64)
        .unwrap_or(0);
    record_serialization(route, payload_kind, duration, bytes, outcome);
    record_payload(route, payload_kind, bytes, objects, outcome);
    serialized.map(|_| bytes)
}

pub fn maybe_log_economics_guardrail(
    pool: Arc<SqlitePool>,
    component: &'static str,
    route: &'static str,
    operation: &'static str,
    query_count: u64,
    payload_bytes: u64,
    object_count: u64,
) {
    let query_count_exceeded = query_count > QUERY_COUNT_WARN_THRESHOLD;
    let payload_bytes_exceeded = payload_bytes > PAYLOAD_BYTES_WARN_THRESHOLD;
    let payload_objects_exceeded = object_count > PAYLOAD_OBJECTS_WARN_THRESHOLD;

    if !query_count_exceeded && !payload_bytes_exceeded && !payload_objects_exceeded {
        return;
    }

    let detail = serde_json::json!({
        "route": route,
        "operation": operation,
        "query_count": query_count,
        "payload_bytes": payload_bytes,
        "object_count": object_count,
        "guardrails": {
            "query_count_warn_threshold": QUERY_COUNT_WARN_THRESHOLD,
            "payload_bytes_warn_threshold": PAYLOAD_BYTES_WARN_THRESHOLD,
            "payload_objects_warn_threshold": PAYLOAD_OBJECTS_WARN_THRESHOLD,
            "query_count_exceeded": query_count_exceeded,
            "payload_bytes_exceeded": payload_bytes_exceeded,
            "payload_objects_exceeded": payload_objects_exceeded
        }
    })
    .to_string();
    let message = format!("Request economics guardrail breached for {route}");

    tokio::spawn(async move {
        let _ = ticketing_system::system_logs::insert_log(
            &pool,
            "warn",
            component,
            &message,
            Some(&detail),
            None,
            None,
        )
        .await;
    });
}

fn parse_traceparent(value: &str) -> Option<ParsedTraceparent> {
    let mut parts = value.split('-');
    let version = parts.next()?;
    let trace_id = parts.next()?;
    let parent_span_id = parts.next()?;
    let trace_flags = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    if version.len() != 2
        || version.eq_ignore_ascii_case("ff")
        || trace_id.len() != 32
        || parent_span_id.len() != 16
        || trace_flags.len() != 2
        || !is_lower_hex(version)
        || !is_lower_hex(trace_id)
        || !is_lower_hex(parent_span_id)
        || !is_lower_hex(trace_flags)
        || all_zero(trace_id)
        || all_zero(parent_span_id)
    {
        return None;
    }

    Some(ParsedTraceparent {
        trace_id: trace_id.to_ascii_lowercase(),
        parent_span_id: parent_span_id.to_ascii_lowercase(),
        trace_flags: trace_flags.to_ascii_lowercase(),
    })
}

fn new_request_id() -> String {
    Uuid::new_v4().to_string()
}

fn new_trace_id() -> String {
    Uuid::new_v4().simple().to_string()
}

fn new_span_id() -> String {
    Uuid::new_v4().simple().to_string()[0..16].to_string()
}

fn valid_request_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn valid_tracestate(value: &str) -> bool {
    !value.is_empty() && value.len() <= 512 && !value.contains('\n') && !value.contains('\r')
}

fn is_lower_hex(value: &str) -> bool {
    value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn all_zero(value: &str) -> bool {
    value.bytes().all(|b| b == b'0')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_traceparent_and_generates_child_span() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "traceparent",
            HeaderValue::from_static("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
        );
        headers.insert("x-request-id", HeaderValue::from_static("req-123"));

        let context = RequestTelemetryContext::from_headers(&headers);

        assert_eq!(context.request_id, "req-123");
        assert_eq!(context.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(context.parent_span_id.as_deref(), Some("00f067aa0ba902b7"));
        assert_ne!(context.span_id, "00f067aa0ba902b7");
        assert_eq!(context.trace_flags, "01");
    }

    #[test]
    fn rejects_invalid_traceparent_and_request_id() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "traceparent",
            HeaderValue::from_static("00-00000000000000000000000000000000-00f067aa0ba902b7-01"),
        );
        headers.insert("x-request-id", HeaderValue::from_static("bad request id"));

        let context = RequestTelemetryContext::from_headers(&headers);

        assert_ne!(context.request_id, "bad request id");
        assert_ne!(context.trace_id, "00000000000000000000000000000000");
        assert!(context.parent_span_id.is_none());
    }

    #[test]
    fn route_fallback_never_exposes_raw_api_paths() {
        assert_eq!(
            route_template_fallback("/api/conversations/abc"),
            "/api/{unmatched}"
        );
        assert_eq!(route_template_fallback("/health/ready"), "/health/ready");
    }

    #[test]
    fn status_maps_to_stable_error_classes() {
        assert_eq!(ErrorClass::from_status(StatusCode::OK), ErrorClass::None);
        assert_eq!(
            ErrorClass::from_status(StatusCode::TOO_MANY_REQUESTS),
            ErrorClass::RateLimited
        );
        assert_eq!(
            ErrorClass::from_status(StatusCode::INTERNAL_SERVER_ERROR),
            ErrorClass::Internal
        );
    }
}
