use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sqlx::SqlitePool;
use std::{collections::BTreeMap, sync::Arc};
use ticketing_system::models::ClientEvent;

#[derive(Debug, Deserialize)]
pub struct IosDegradationQuery {
    /// Unix seconds. Defaults to the last 24 hours.
    pub since: Option<i64>,
    /// Relative window in hours when `since` is not supplied. Defaults to 24.
    pub hours: Option<i64>,
    /// Optional device id, useful when Alex reports a specific TestFlight phone.
    pub device_id: Option<String>,
    /// Maximum client_events rows to scan. Defaults to 5000, capped at 10000.
    pub limit: Option<i64>,
    /// Slow-request threshold in milliseconds. Defaults to 2000.
    pub slow_ms: Option<i64>,
    /// Large-response threshold in bytes. Defaults to 256 KiB.
    pub large_bytes: Option<i64>,
}

#[derive(Debug, Default)]
struct DeviceSummary {
    device_id: String,
    latest_session_id: Option<String>,
    latest_received_at: i64,
    latest_received_at_iso: String,
    event_count: usize,
    degradation_count: usize,
    app_version: Option<String>,
    build: Option<String>,
    release_channel: Option<String>,
    platform: Option<String>,
    bundle_id: Option<String>,
    os_version: Option<String>,
    latest_message: Option<String>,
}

#[derive(Debug, Default)]
struct DegradationStats {
    hang_count: usize,
    slow_request_count: usize,
    large_request_count: usize,
    conversation_refresh_count: usize,
    app_performance_count: usize,
    max_hang_ms: Option<i64>,
    max_request_ms: Option<i64>,
    max_response_bytes: Option<i64>,
    max_conversation_refresh_ms: Option<i64>,
    max_rendered_count: Option<i64>,
    max_main_thread_cpu: Option<f64>,
    max_tv_layout_delta: Option<i64>,
}

/// GET /api/admin/app-diagnostics/ios-degradation
pub async fn ios_degradation_diagnostics(
    State(pool): State<Arc<SqlitePool>>,
    Query(query): Query<IosDegradationQuery>,
) -> Response {
    let now = chrono::Utc::now().timestamp();
    let since = query
        .since
        .unwrap_or_else(|| now - query.hours.unwrap_or(24).clamp(1, 168) * 3600);
    let limit = query.limit.unwrap_or(5000).clamp(100, 10_000);
    let slow_ms = query.slow_ms.unwrap_or(2000).max(1);
    let large_bytes = query.large_bytes.unwrap_or(256 * 1024).max(1);

    let result = ticketing_system::client_events::list_client_events(
        &pool,
        None,
        None,
        None,
        None,
        Some(since),
        None,
        limit,
    )
    .await;

    let mut events = match result {
        Ok(events) => filter_device(events, query.device_id.as_deref()),
        Err(error) => {
            tracing::error!(
                target: "agentic_api::app_diagnostics",
                event = "ios_degradation_diagnostics.list_failed",
                error = %error,
                "failed to list client telemetry for iOS degradation diagnostics"
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Failed to list iOS degradation diagnostics"})),
            )
                .into_response();
        }
    };

    let requested_window_empty = events.is_empty();
    let mut source = "requested_window";
    if requested_window_empty {
        match ticketing_system::client_events::list_client_events(
            &pool, None, None, None, None, None, None, limit,
        )
        .await
        {
            Ok(fallback) => {
                events = filter_device(fallback, query.device_id.as_deref());
                source = "latest_available_fallback";
            }
            Err(error) => {
                tracing::error!(
                    target: "agentic_api::app_diagnostics",
                    event = "ios_degradation_diagnostics.fallback_failed",
                    error = %error,
                    "failed to list fallback client telemetry for iOS degradation diagnostics"
                );
            }
        }
    }

    let response = build_diagnostics_response(
        &events,
        now,
        since,
        source,
        requested_window_empty,
        query.device_id.as_deref(),
        limit,
        slow_ms,
        large_bytes,
    );

    (StatusCode::OK, Json(response)).into_response()
}

fn filter_device(events: Vec<ClientEvent>, device_id: Option<&str>) -> Vec<ClientEvent> {
    let Some(device_id) = device_id else {
        return events;
    };
    events
        .into_iter()
        .filter(|event| event.device_id == device_id)
        .collect()
}

fn build_diagnostics_response(
    events: &[ClientEvent],
    now: i64,
    since: i64,
    source: &str,
    requested_window_empty: bool,
    device_id: Option<&str>,
    limit: i64,
    slow_ms: i64,
    large_bytes: i64,
) -> Value {
    let mut devices: BTreeMap<String, DeviceSummary> = BTreeMap::new();
    let mut stats = DegradationStats::default();
    let mut hangs = Vec::new();
    let mut slow_requests = Vec::new();
    let mut large_requests = Vec::new();
    let mut conversation_refreshes = Vec::new();
    let mut app_performance = Vec::new();

    for event in events {
        let is_hang = is_hang_event(event);
        let duration_ms = metric_i64(event.detail.as_deref(), "duration_ms");
        let response_bytes = metric_i64(event.detail.as_deref(), "response_bytes");
        let elapsed_ms = metric_i64(event.detail.as_deref(), "elapsed_ms");
        let rendered_count = metric_i64(event.detail.as_deref(), "rendered_count");
        let main_thread_cpu = metric_f64(event.detail.as_deref(), "main_thread_cpu");
        let tv_layout_delta = metric_i64(event.detail.as_deref(), "tv_layout_delta");

        let slow = is_network_event(event) && duration_ms.is_some_and(|value| value >= slow_ms);
        let large =
            is_network_event(event) && response_bytes.is_some_and(|value| value >= large_bytes);
        let conversation_refresh = is_conversation_refresh_event(event);
        let app_perf = is_app_performance_event(event, main_thread_cpu, tv_layout_delta);
        let degraded = is_hang || slow || large || conversation_refresh || app_perf;

        update_device_summary(
            devices.entry(event.device_id.clone()).or_default(),
            event,
            degraded,
        );

        if is_hang {
            stats.hang_count += 1;
            stats.max_hang_ms = max_i64(stats.max_hang_ms, duration_ms.or(elapsed_ms));
            push_limited(&mut hangs, diagnostic_event(event));
        }
        if slow {
            stats.slow_request_count += 1;
            stats.max_request_ms = max_i64(stats.max_request_ms, duration_ms);
            push_limited(&mut slow_requests, diagnostic_event(event));
        }
        if large {
            stats.large_request_count += 1;
            stats.max_response_bytes = max_i64(stats.max_response_bytes, response_bytes);
            push_limited(&mut large_requests, diagnostic_event(event));
        }
        if conversation_refresh {
            stats.conversation_refresh_count += 1;
            stats.max_conversation_refresh_ms =
                max_i64(stats.max_conversation_refresh_ms, elapsed_ms);
            stats.max_rendered_count = max_i64(stats.max_rendered_count, rendered_count);
            push_limited(&mut conversation_refreshes, diagnostic_event(event));
        }
        if app_perf {
            stats.app_performance_count += 1;
            stats.max_main_thread_cpu = max_f64(stats.max_main_thread_cpu, main_thread_cpu);
            stats.max_tv_layout_delta = max_i64(stats.max_tv_layout_delta, tv_layout_delta);
            push_limited(&mut app_performance, diagnostic_event(event));
        }
    }

    let oldest = events.last().map(|event| event.received_at_iso.clone());
    let newest = events.first().map(|event| event.received_at_iso.clone());
    let latest_devices: Vec<Value> = devices.values().map(device_summary_json).collect();

    json!({
        "generated_at": chrono::DateTime::from_timestamp(now, 0)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_default(),
        "source": source,
        "window": {
            "since": since,
            "since_iso": chrono::DateTime::from_timestamp(since, 0)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_default(),
            "oldest": oldest,
            "newest": newest,
            "event_count": events.len(),
            "requested_window_empty": requested_window_empty,
            "limit": limit,
            "device_id": device_id,
        },
        "thresholds": {
            "slow_ms": slow_ms,
            "large_bytes": large_bytes,
            "app_main_thread_cpu_percent": 25.0,
            "app_tv_layout_delta": 10,
        },
        "latest_devices": latest_devices,
        "summary": {
            "main_thread_hangs": {
                "count": stats.hang_count,
                "max_duration_ms": stats.max_hang_ms,
            },
            "slow_requests": {
                "count": stats.slow_request_count,
                "max_duration_ms": stats.max_request_ms,
            },
            "large_requests": {
                "count": stats.large_request_count,
                "max_response_bytes": stats.max_response_bytes,
            },
            "conversation_list_refresh": {
                "count": stats.conversation_refresh_count,
                "max_elapsed_ms": stats.max_conversation_refresh_ms,
                "max_rendered_count": stats.max_rendered_count,
            },
            "app_performance": {
                "count": stats.app_performance_count,
                "max_main_thread_cpu": stats.max_main_thread_cpu,
                "max_tv_layout_delta": stats.max_tv_layout_delta,
            },
        },
        "events": {
            "main_thread_hangs": hangs,
            "slow_requests": slow_requests,
            "large_requests": large_requests,
            "conversation_list_refresh": conversation_refreshes,
            "app_performance": app_performance,
        }
    })
}

fn update_device_summary(summary: &mut DeviceSummary, event: &ClientEvent, degraded: bool) {
    if summary.device_id.is_empty() {
        summary.device_id = event.device_id.clone();
    }
    summary.event_count += 1;
    if degraded {
        summary.degradation_count += 1;
    }
    if event.received_at >= summary.latest_received_at {
        summary.latest_received_at = event.received_at;
        summary.latest_received_at_iso = event.received_at_iso.clone();
        summary.latest_session_id = Some(event.session_id.clone());
        summary.latest_message = Some(event.message.clone());
    }

    let detail = event.detail.as_deref();
    fill_if_missing(
        &mut summary.app_version,
        detail,
        &["app_version", "appVersion"],
    );
    fill_if_missing(&mut summary.build, detail, &["build", "appBuildVersion"]);
    fill_if_missing(&mut summary.release_channel, detail, &["release_channel"]);
    fill_if_missing(&mut summary.platform, detail, &["platform"]);
    fill_if_missing(&mut summary.bundle_id, detail, &["bundle_id"]);
    fill_if_missing(&mut summary.os_version, detail, &["os_version", "os"]);
}

fn fill_if_missing(target: &mut Option<String>, detail: Option<&str>, keys: &[&str]) {
    if target.is_some() {
        return;
    }
    for key in keys {
        if let Some(value) = metric_string(detail, key) {
            *target = Some(value);
            return;
        }
    }
}

fn device_summary_json(summary: &DeviceSummary) -> Value {
    json!({
        "device_id": summary.device_id,
        "latest_session_id": summary.latest_session_id,
        "latest_received_at": summary.latest_received_at,
        "latest_received_at_iso": summary.latest_received_at_iso,
        "event_count": summary.event_count,
        "degradation_count": summary.degradation_count,
        "app_version": summary.app_version,
        "build": summary.build,
        "release_channel": summary.release_channel,
        "platform": summary.platform,
        "bundle_id": summary.bundle_id,
        "os_version": summary.os_version,
        "latest_message": summary.latest_message,
    })
}

fn diagnostic_event(event: &ClientEvent) -> Value {
    let mut metrics = Map::new();
    for key in [
        "duration_ms",
        "response_bytes",
        "elapsed_ms",
        "server_count",
        "rendered_count",
        "main_thread_cpu",
        "tv_layouts",
        "tv_layout_delta",
        "runloop_wakeups",
        "rl_delta",
        "cache_hits",
        "cache_h_delta",
        "memory_mb",
        "cpu_percent",
        "threshold_ms",
    ] {
        if let Some(value) = metric_value(event.detail.as_deref(), key) {
            metrics.insert(key.to_string(), value);
        }
    }

    json!({
        "id": event.id,
        "received_at": event.received_at_iso,
        "client_created_at": event.client_created_at_iso,
        "device_id": event.device_id,
        "session_id": event.session_id,
        "event_type": event.event_type,
        "component": event.component,
        "level": event.level,
        "message": event.message,
        "path": network_path(&event.message),
        "network_state": event.network_state,
        "app_state": event.app_state,
        "detail": event.detail,
        "metrics": Value::Object(metrics),
    })
}

fn is_hang_event(event: &ClientEvent) -> bool {
    event.event_type.contains("hang")
        || event.component.contains("hang")
        || event
            .message
            .to_ascii_lowercase()
            .contains("main thread unresponsive")
        || event
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("hangDiagnostics"))
}

fn is_network_event(event: &ClientEvent) -> bool {
    event.component == "api_client"
        && matches!(
            event.event_type.as_str(),
            "network_perf" | "network_request" | "error"
        )
}

fn is_conversation_refresh_event(event: &ClientEvent) -> bool {
    event.event_type == "chat_performance"
        && (event.message.contains("Conversation list refresh")
            || event
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("rendered_count=")))
}

fn is_app_performance_event(
    event: &ClientEvent,
    main_thread_cpu: Option<f64>,
    tv_layout_delta: Option<i64>,
) -> bool {
    event.event_type == "performance"
        && event.component == "app"
        && (main_thread_cpu.is_some_and(|value| value >= 25.0)
            || tv_layout_delta.is_some_and(|value| value >= 10))
}

fn metric_i64(detail: Option<&str>, key: &str) -> Option<i64> {
    metric_string(detail, key).and_then(|value| {
        value
            .trim_end_matches('%')
            .trim_end_matches(',')
            .parse::<i64>()
            .ok()
    })
}

fn metric_f64(detail: Option<&str>, key: &str) -> Option<f64> {
    metric_string(detail, key).and_then(|value| {
        value
            .trim_end_matches('%')
            .trim_end_matches(',')
            .parse::<f64>()
            .ok()
    })
}

fn metric_value(detail: Option<&str>, key: &str) -> Option<Value> {
    let value = metric_string(detail, key)?;
    if let Ok(number) = value.parse::<i64>() {
        return Some(json!(number));
    }
    if let Ok(number) = value.parse::<f64>() {
        return Some(json!(number));
    }
    Some(json!(value))
}

fn metric_string(detail: Option<&str>, key: &str) -> Option<String> {
    let detail = detail?;
    if let Ok(json_detail) = serde_json::from_str::<Value>(detail) {
        if let Some(value) = find_json_key(&json_detail, key) {
            return json_value_string(value);
        }
    }

    let needle = format!("{key}=");
    detail
        .split_whitespace()
        .find_map(|part| part.strip_prefix(&needle))
        .map(|value| value.trim_matches('"').trim_matches(',').to_string())
        .filter(|value| !value.is_empty() && value != "nil")
}

fn find_json_key<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) => {
            if let Some(value) = map.get(key) {
                return Some(value);
            }
            map.values().find_map(|value| find_json_key(value, key))
        }
        Value::Array(values) => values.iter().find_map(|value| find_json_key(value, key)),
        _ => None,
    }
}

fn json_value_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

fn network_path(message: &str) -> Option<String> {
    if let Some(rest) = message.strip_prefix("Slow/large: ") {
        return rest.split_whitespace().next().map(ToString::to_string);
    }
    let mut parts = message.split_whitespace();
    let method = parts.next()?;
    if matches!(method, "GET" | "POST" | "PUT" | "PATCH" | "DELETE") {
        return parts.next().map(ToString::to_string);
    }
    None
}

fn push_limited(events: &mut Vec<Value>, event: Value) {
    if events.len() < 20 {
        events.push(event);
    }
}

fn max_i64(current: Option<i64>, next: Option<i64>) -> Option<i64> {
    match (current, next) {
        (Some(current), Some(next)) => Some(current.max(next)),
        (None, Some(next)) => Some(next),
        (current, None) => current,
    }
}

fn max_f64(current: Option<f64>, next: Option<f64>) -> Option<f64> {
    match (current, next) {
        (Some(current), Some(next)) => Some(current.max(next)),
        (None, Some(next)) => Some(next),
        (current, None) => current,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_key_value_metrics() {
        let detail = "duration_ms=3330 response_bytes=71768 decode=off_main main_thread_cpu=73.9%";
        assert_eq!(metric_i64(Some(detail), "duration_ms"), Some(3330));
        assert_eq!(metric_i64(Some(detail), "response_bytes"), Some(71768));
        assert_eq!(metric_f64(Some(detail), "main_thread_cpu"), Some(73.9));
    }

    #[test]
    fn extracts_nested_json_metrics() {
        let detail =
            r#"{"metaData":{"appBuildVersion":"631"},"app_version":"2.1.321","duration_ms":4200}"#;
        assert_eq!(
            metric_string(Some(detail), "appBuildVersion"),
            Some("631".to_string())
        );
        assert_eq!(
            metric_string(Some(detail), "app_version"),
            Some("2.1.321".to_string())
        );
        assert_eq!(metric_i64(Some(detail), "duration_ms"), Some(4200));
    }

    #[test]
    fn extracts_network_path_from_messages() {
        assert_eq!(
            network_path("Slow/large: /api/conversations?hierarchy_scope=all 3330ms 70KB"),
            Some("/api/conversations?hierarchy_scope=all".to_string())
        );
        assert_eq!(
            network_path("GET /api/tickets 200 42ms 1.0KB"),
            Some("/api/tickets".to_string())
        );
    }
}
