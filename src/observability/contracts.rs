//! Versioned observability contract registry and guardrails.
//!
//! The registry is checked into `observability/contracts/v0.1/registry.toml`
//! and parsed at process startup. Invalid registry content, unknown metric
//! names, unsafe metric labels, and content-bearing client telemetry are
//! deliberate failures.

use std::collections::HashSet;
use std::fmt;

use once_cell::sync::Lazy;
use serde::Deserialize;
use serde_json::Value;
use ticketing_system::models::ClientEventInput;

const REGISTRY_TOML: &str = include_str!("../../observability/contracts/v0.1/registry.toml");
const REQUIRED_CONTRACT_IDS: &[&str] = &["C-OBS-001", "C-OBS-002", "C-OBS-003", "C-OBS-004"];
const MAX_DETAIL_STRING_BYTES: usize = 4096;

static REGISTRY: Lazy<Registry> = Lazy::new(|| {
    let registry: Registry =
        toml::from_str(REGISTRY_TOML).expect("observability registry TOML must parse");
    registry
        .validate()
        .expect("observability registry must be internally consistent");
    registry
});

#[derive(Debug, Deserialize)]
pub struct Registry {
    pub version: String,
    pub effective_date: String,
    #[allow(dead_code)]
    pub source_artifacts: Vec<String>,
    pub ticket_id: String,
    pub contracts: Vec<ContractDefinition>,
    pub privacy: PrivacyRules,
    pub events: Vec<EventDefinition>,
    pub metrics: Vec<MetricDefinition>,
    pub spans: Vec<SpanDefinition>,
}

#[derive(Debug, Deserialize)]
pub struct ContractDefinition {
    pub id: String,
    pub name: String,
    pub status: String,
    pub owner: String,
}

#[derive(Debug, Deserialize)]
pub struct PrivacyRules {
    pub max_message_bytes: usize,
    pub max_detail_bytes: usize,
    pub allowed_fields: Vec<String>,
    pub forbidden_field_names: Vec<String>,
    pub forbidden_metric_labels: Vec<String>,
    pub secret_patterns: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct EventDefinition {
    pub name: String,
    pub contract_id: String,
    #[allow(dead_code)]
    pub signal: String,
}

#[derive(Debug, Deserialize)]
pub struct MetricDefinition {
    pub name: String,
    pub kind: String,
    pub contract_id: String,
    pub labels: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct SpanDefinition {
    pub name: String,
    pub contract_id: String,
    pub attributes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelemetryValidationError {
    pub event_index: Option<usize>,
    pub reason: String,
}

impl TelemetryValidationError {
    fn event(index: usize, reason: impl Into<String>) -> Self {
        Self {
            event_index: Some(index),
            reason: reason.into(),
        }
    }

    fn unscoped(reason: impl Into<String>) -> Self {
        Self {
            event_index: None,
            reason: reason.into(),
        }
    }
}

impl fmt::Display for TelemetryValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(index) = self.event_index {
            write!(f, "event {}: {}", index, self.reason)
        } else {
            f.write_str(&self.reason)
        }
    }
}

impl std::error::Error for TelemetryValidationError {}

impl Registry {
    fn validate(&self) -> Result<(), String> {
        if self.version.trim().is_empty() {
            return Err("registry version is required".to_string());
        }
        if self.effective_date.trim().is_empty() {
            return Err("registry effective_date is required".to_string());
        }
        if self.ticket_id.trim().is_empty() {
            return Err("registry ticket_id is required".to_string());
        }

        let contract_ids = unique_names(
            self.contracts.iter().map(|contract| contract.id.as_str()),
            "contract",
        )?;
        for required in REQUIRED_CONTRACT_IDS {
            if !contract_ids.contains(*required) {
                return Err(format!("required contract `{}` is missing", required));
            }
        }
        for contract in &self.contracts {
            require_identifier("contract id", &contract.id)?;
            require_nonempty("contract name", &contract.name)?;
            require_identifier("contract status", &contract.status)?;
            require_identifier("contract owner", &contract.owner)?;
        }

        unique_names(self.events.iter().map(|event| event.name.as_str()), "event")?;
        unique_names(
            self.metrics.iter().map(|metric| metric.name.as_str()),
            "metric",
        )?;
        unique_names(self.spans.iter().map(|span| span.name.as_str()), "span")?;

        let forbidden_metric_labels: HashSet<&str> = self
            .privacy
            .forbidden_metric_labels
            .iter()
            .map(String::as_str)
            .collect();
        for metric in &self.metrics {
            require_identifier("metric name", &metric.name)?;
            require_identifier("metric kind", &metric.kind)?;
            if !contract_ids.contains(metric.contract_id.as_str()) {
                return Err(format!(
                    "metric `{}` references unknown contract `{}`",
                    metric.name, metric.contract_id
                ));
            }
            let mut seen = HashSet::new();
            for label in &metric.labels {
                require_identifier("metric label", label)?;
                if forbidden_metric_labels.contains(label.as_str()) {
                    return Err(format!(
                        "metric `{}` uses forbidden label `{}`",
                        metric.name, label
                    ));
                }
                if !seen.insert(label.as_str()) {
                    return Err(format!(
                        "metric `{}` repeats label `{}`",
                        metric.name, label
                    ));
                }
            }
        }

        for event in &self.events {
            require_identifier("event name", &event.name)?;
            if !contract_ids.contains(event.contract_id.as_str()) {
                return Err(format!(
                    "event `{}` references unknown contract `{}`",
                    event.name, event.contract_id
                ));
            }
        }

        for span in &self.spans {
            require_identifier("span name", &span.name)?;
            if !contract_ids.contains(span.contract_id.as_str()) {
                return Err(format!(
                    "span `{}` references unknown contract `{}`",
                    span.name, span.contract_id
                ));
            }
            let mut seen = HashSet::new();
            for attr in &span.attributes {
                require_identifier("span attribute", attr)?;
                if !seen.insert(attr.as_str()) {
                    return Err(format!("span `{}` repeats attribute `{}`", span.name, attr));
                }
            }
        }

        if self.privacy.max_message_bytes == 0 || self.privacy.max_detail_bytes == 0 {
            return Err("privacy byte limits must be positive".to_string());
        }
        unique_names(
            self.privacy.allowed_fields.iter().map(String::as_str),
            "allowed privacy field",
        )?;
        unique_names(
            self.privacy
                .forbidden_field_names
                .iter()
                .map(String::as_str),
            "forbidden privacy field",
        )?;
        unique_names(
            self.privacy
                .forbidden_metric_labels
                .iter()
                .map(String::as_str),
            "forbidden metric label",
        )?;
        Ok(())
    }
}

pub fn registry() -> &'static Registry {
    &REGISTRY
}

pub fn registry_version() -> &'static str {
    registry().version.as_str()
}

pub fn assert_registry_loaded() {
    let _ = registry();
}

pub fn assert_metric_labels(metric_name: &str, labels: &[(&str, &str)]) {
    let metric = registry()
        .metrics
        .iter()
        .find(|metric| metric.name == metric_name)
        .unwrap_or_else(|| {
            panic!(
                "metric `{}` is not registered in observability registry v{}",
                metric_name,
                registry_version()
            )
        });

    if metric.labels.len() != labels.len() {
        panic!(
            "metric `{}` expected labels {:?}, got {:?}",
            metric_name,
            metric.labels,
            labels.iter().map(|(key, _)| *key).collect::<Vec<_>>()
        );
    }

    for ((actual_key, actual_value), expected_key) in labels.iter().zip(metric.labels.iter()) {
        if registry()
            .privacy
            .forbidden_metric_labels
            .iter()
            .any(|forbidden| forbidden == actual_key)
        {
            panic!(
                "metric `{}` uses forbidden label key `{}`",
                metric_name, actual_key
            );
        }
        if actual_key != expected_key {
            panic!(
                "metric `{}` expected label `{}`, got `{}`",
                metric_name, expected_key, actual_key
            );
        }
        if let Err(reason) = validate_metric_label_value(actual_value) {
            panic!(
                "metric `{}` label `{}` has unsafe value: {}",
                metric_name, actual_key, reason
            );
        }
    }
}

pub fn validate_client_events(events: &[ClientEventInput]) -> Result<(), TelemetryValidationError> {
    for (index, event) in events.iter().enumerate() {
        validate_client_event(index, event)?;
    }
    Ok(())
}

pub fn assert_system_log_detail(detail: &str) {
    validate_content_free_detail(detail).unwrap_or_else(|error| {
        panic!("unsafe request log detail rejected by observability guardrail: {error}")
    });
}

fn validate_client_event(
    index: usize,
    event: &ClientEventInput,
) -> Result<(), TelemetryValidationError> {
    require_nonempty("event_id", &event.event_id).map_err(|reason| {
        TelemetryValidationError::event(index, format!("invalid event_id: {}", reason))
    })?;
    require_nonempty("device_id", &event.device_id).map_err(|reason| {
        TelemetryValidationError::event(index, format!("invalid device_id: {}", reason))
    })?;
    require_nonempty("session_id", &event.session_id).map_err(|reason| {
        TelemetryValidationError::event(index, format!("invalid session_id: {}", reason))
    })?;
    require_identifier("event_type", &event.event_type).map_err(|reason| {
        TelemetryValidationError::event(index, format!("invalid event_type: {}", reason))
    })?;
    require_identifier("component", &event.component).map_err(|reason| {
        TelemetryValidationError::event(index, format!("invalid component: {}", reason))
    })?;
    if !registry()
        .events
        .iter()
        .any(|definition| definition.name == event.event_type)
    {
        return Err(TelemetryValidationError::event(
            index,
            format!("event_type `{}` is not registered", event.event_type),
        ));
    }

    validate_level(&event.level).map_err(|reason| {
        TelemetryValidationError::event(index, format!("invalid level: {}", reason))
    })?;
    validate_content_free_text(
        "message",
        &event.message,
        registry().privacy.max_message_bytes,
    )
    .map_err(|error| TelemetryValidationError::event(index, error.reason))?;
    if let Some(detail) = &event.detail {
        validate_content_free_detail(detail)
            .map_err(|error| TelemetryValidationError::event(index, error.reason))?;
    }
    if let Some(network_state) = &event.network_state {
        validate_optional_state("network_state", network_state)
            .map_err(|reason| TelemetryValidationError::event(index, reason))?;
    }
    if let Some(app_state) = &event.app_state {
        validate_optional_state("app_state", app_state)
            .map_err(|reason| TelemetryValidationError::event(index, reason))?;
    }

    Ok(())
}

fn validate_content_free_detail(detail: &str) -> Result<(), TelemetryValidationError> {
    validate_content_free_text("detail", detail, registry().privacy.max_detail_bytes)?;
    if let Ok(value) = serde_json::from_str::<Value>(detail) {
        validate_json_value("$", &value)?;
    } else {
        validate_key_value_detail(detail)?;
    }
    Ok(())
}

fn validate_json_value(path: &str, value: &Value) -> Result<(), TelemetryValidationError> {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                validate_detail_key(key).map_err(|reason| {
                    TelemetryValidationError::unscoped(format!(
                        "forbidden detail field `{}` at {}: {}",
                        key, path, reason
                    ))
                })?;
                let child_path = format!("{}.{}", path, key);
                validate_json_value(&child_path, value)?;
            }
        }
        Value::Array(items) => {
            for (index, value) in items.iter().enumerate() {
                let child_path = format!("{}[{}]", path, index);
                validate_json_value(&child_path, value)?;
            }
        }
        Value::String(value) => {
            validate_content_free_text(path, value, MAX_DETAIL_STRING_BYTES)?;
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    Ok(())
}

fn validate_key_value_detail(detail: &str) -> Result<(), TelemetryValidationError> {
    for token in detail
        .split(|ch: char| ch.is_whitespace() || ch == ',' || ch == ';')
        .filter(|token| !token.is_empty())
    {
        if let Some((key, _)) = token.split_once('=') {
            validate_detail_key(key).map_err(|reason| {
                TelemetryValidationError::unscoped(format!(
                    "forbidden detail field `{}`: {}",
                    key, reason
                ))
            })?;
        }
    }
    Ok(())
}

fn validate_detail_key(key: &str) -> Result<(), String> {
    let normalized = normalize_key(key);
    if normalized.is_empty() {
        return Err("empty key".to_string());
    }
    if registry()
        .privacy
        .forbidden_field_names
        .iter()
        .any(|field| normalize_key(field) == normalized)
    {
        return Err("field is explicitly forbidden".to_string());
    }
    if normalized.contains("prompt")
        || normalized.contains("clipboard")
        || normalized.contains("screenshot")
        || normalized.contains("password")
        || normalized.contains("secret")
        || normalized == "token"
        || normalized.ends_with("_token")
        || normalized == "cookie"
        || normalized.ends_with("_cookie")
        || normalized == "body"
        || normalized.contains("_body_")
        || normalized.ends_with("_body")
        || normalized == "raw_sql"
        || normalized == "sql"
        || normalized.ends_with("_sql")
        || normalized == "stdout"
        || normalized == "stderr"
    {
        return Err("field name implies content, credentials, or raw payload data".to_string());
    }
    Ok(())
}

fn validate_content_free_text(
    field_name: &str,
    value: &str,
    max_bytes: usize,
) -> Result<(), TelemetryValidationError> {
    if value.len() > max_bytes {
        return Err(TelemetryValidationError::unscoped(format!(
            "{} exceeds {} byte limit",
            field_name, max_bytes
        )));
    }
    let lowered = value.to_ascii_lowercase();
    for pattern in &registry().privacy.secret_patterns {
        if lowered.contains(pattern) {
            return Err(TelemetryValidationError::unscoped(format!(
                "{} contains forbidden secret marker `{}`",
                field_name, pattern
            )));
        }
    }
    if lowered.contains("http://") || lowered.contains("https://") {
        return Err(TelemetryValidationError::unscoped(format!(
            "{} contains a raw URL",
            field_name
        )));
    }
    if value
        .split_whitespace()
        .any(|token| token.starts_with('/') && token.len() > 1)
    {
        return Err(TelemetryValidationError::unscoped(format!(
            "{} contains a raw path",
            field_name
        )));
    }
    Ok(())
}

fn validate_metric_label_value(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("empty label value".to_string());
    }
    if value.len() > 64 {
        return Err("label value exceeds 64 bytes".to_string());
    }
    if value.contains('/')
        || value.contains('?')
        || value.contains('&')
        || value.contains('=')
        || value.contains(' ')
        || value.contains('{')
        || value.contains('}')
        || value.contains('"')
        || value.contains('\'')
    {
        return Err("label value is not a bounded enum token".to_string());
    }
    if value.contains("://") {
        return Err("label value contains a URL".to_string());
    }
    if looks_like_uuid(value) {
        return Err("label value looks like a UUID".to_string());
    }
    if value.starts_with("T-")
        || value.starts_with("A-")
        || value.starts_with("D-")
        || value.starts_with("CP-")
        || value.starts_with("R-")
    {
        return Err("label value looks like a high-cardinality object ID".to_string());
    }
    if !value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | ':'))
    {
        return Err("label value contains unsupported characters".to_string());
    }
    Ok(())
}

fn validate_level(level: &str) -> Result<(), String> {
    match level {
        "debug" | "info" | "warn" | "warning" | "error" => Ok(()),
        _ => Err(format!("`{}` is not an approved telemetry level", level)),
    }
}

fn validate_optional_state(field_name: &str, value: &str) -> Result<(), String> {
    if value.len() > 64 {
        return Err(format!("{} exceeds 64 byte limit", field_name));
    }
    if !value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
    {
        return Err(format!("{} is not a bounded state token", field_name));
    }
    Ok(())
}

fn require_nonempty(label: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        Err(format!("{} must not be empty", label))
    } else {
        Ok(())
    }
}

fn require_identifier(label: &str, value: &str) -> Result<(), String> {
    require_nonempty(label, value)?;
    if value.len() > 128 {
        return Err(format!("{} exceeds 128 byte limit", label));
    }
    if !value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
    {
        return Err(format!("{} `{}` is not a stable identifier", label, value));
    }
    Ok(())
}

fn unique_names<'a, I>(values: I, label: &str) -> Result<HashSet<&'a str>, String>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut seen = HashSet::new();
    for value in values {
        if !seen.insert(value) {
            return Err(format!("duplicate {} `{}`", label, value));
        }
    }
    Ok(seen)
}

fn normalize_key(key: &str) -> String {
    key.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .split('_')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("_")
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_event(event_type: &str, message: &str, detail: Option<&str>) -> ClientEventInput {
        ClientEventInput {
            event_id: "evt-1".to_string(),
            device_id: "device-1".to_string(),
            user_id: None,
            session_id: "session-1".to_string(),
            event_type: event_type.to_string(),
            level: "info".to_string(),
            component: "chat".to_string(),
            message: message.to_string(),
            detail: detail.map(str::to_string),
            network_state: Some("wifi".to_string()),
            app_state: Some("foreground".to_string()),
            client_created_at: 1,
        }
    }

    #[test]
    fn registry_loads_required_contracts() {
        assert_registry_loaded();
        let ids: HashSet<&str> = registry()
            .contracts
            .iter()
            .map(|contract| contract.id.as_str())
            .collect();
        for required in REQUIRED_CONTRACT_IDS {
            assert!(ids.contains(required), "missing contract {}", required);
        }
    }

    #[test]
    fn registered_content_free_client_event_is_valid() {
        let event = sample_event(
            "chat_latency",
            "Chat latency: first_token",
            Some(r#"{"phase":"first_token","trace_id":"trace-safe","duration_ms":42}"#),
        );

        validate_client_events(&[event]).expect("event should pass guardrails");
    }

    #[test]
    fn unknown_client_event_type_is_rejected() {
        let event = sample_event("freeform_payload", "Freeform payload", None);
        let error = validate_client_events(&[event]).expect_err("event type should fail");
        assert_eq!(error.event_index, Some(0));
        assert!(error.reason.contains("not registered"));
    }

    #[test]
    fn raw_response_body_detail_is_rejected() {
        let event = sample_event(
            "network_perf",
            "Request failed",
            Some(r#"{"status":500,"server_error_body_prefix":"user content"}"#),
        );

        let error = validate_client_events(&[event]).expect_err("body detail should fail");
        assert_eq!(error.event_index, Some(0));
        assert!(error.reason.contains("server_error_body_prefix"));
    }

    #[test]
    fn raw_path_message_is_rejected() {
        let event = sample_event("network_request", "GET /api/tickets/T-12345678 200", None);
        let error = validate_client_events(&[event]).expect_err("raw path should fail");
        assert!(error.reason.contains("raw path"));
    }

    #[test]
    #[should_panic(expected = "uses forbidden label key")]
    fn metric_guard_rejects_forbidden_label_key() {
        assert_metric_labels("stream_opened_total", &[("user_id", "alex")]);
    }

    #[test]
    #[should_panic(expected = "not a bounded enum token")]
    fn metric_guard_rejects_path_label_value() {
        assert_metric_labels(
            "stream_opened_total",
            &[("resume", "/api/tickets/T-12345678")],
        );
    }

    #[test]
    fn metric_guard_accepts_registered_label_set() {
        assert_metric_labels("stream_opened_total", &[("resume", "true")]);
    }
}
