//! Low-cardinality metrics for the continuously running SES event consumer.

use std::fmt;

use metrics::{counter, gauge, histogram};

use super::contracts::assert_metric_labels;

const QUEUE_MESSAGES: &str = "outreach_ses_queue_messages_total";
const EVENT_RECORDS: &str = "outreach_ses_event_records_total";
const QUEUE_DEPTH: &str = "outreach_ses_queue_depth";
const OLDEST_AGE: &str = "outreach_ses_oldest_message_age_seconds";
const CONSUMER_HEALTHY: &str = "outreach_ses_consumer_healthy";
const PROCESSING_DURATION: &str = "outreach_ses_message_processing_duration_ms";
const RETENTION_DELETED: &str = "outreach_ses_retention_deleted_total";
const UNSUBSCRIBE_REQUESTS: &str = "outreach_unsubscribe_requests_total";
const UNSUBSCRIBE_DURATION: &str = "outreach_unsubscribe_duration_ms";
const COMMERCIAL_SENDS: &str = "outreach_commercial_send_total";

#[derive(Debug, Clone, Copy)]
pub enum MessageOutcome {
    Received,
    Stored,
    ControlAcknowledged,
    Duplicate,
    Poison,
    StorageError,
    DeleteError,
}

impl fmt::Display for MessageOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Received => "received",
            Self::Stored => "stored",
            Self::ControlAcknowledged => "control_acknowledged",
            Self::Duplicate => "duplicate",
            Self::Poison => "poison",
            Self::StorageError => "storage_error",
            Self::DeleteError => "delete_error",
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub enum EventOutcome {
    Inserted,
    Duplicate,
}

impl fmt::Display for EventOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Inserted => "inserted",
            Self::Duplicate => "duplicate",
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub enum QueueKind {
    Main,
    Dlq,
}

#[derive(Debug, Clone, Copy)]
pub enum UnsubscribeMechanism {
    OneClick,
    HumanConfirmation,
    HumanGet,
    Mailto,
    InboundReply,
}

impl fmt::Display for UnsubscribeMechanism {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::OneClick => "one_click",
            Self::HumanConfirmation => "human_confirmation",
            Self::HumanGet => "human_get",
            Self::Mailto => "mailto",
            Self::InboundReply => "inbound_reply",
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub enum ComplianceOutcome {
    Success,
    AlreadySuppressed,
    Invalid,
    ConfigurationError,
    StorageError,
    Blocked,
    ProviderAccepted,
    ProviderError,
}

impl fmt::Display for ComplianceOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Success => "success",
            Self::AlreadySuppressed => "already_suppressed",
            Self::Invalid => "invalid",
            Self::ConfigurationError => "configuration_error",
            Self::StorageError => "storage_error",
            Self::Blocked => "blocked",
            Self::ProviderAccepted => "provider_accepted",
            Self::ProviderError => "provider_error",
        })
    }
}

impl fmt::Display for QueueKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Main => "main",
            Self::Dlq => "dlq",
        })
    }
}

pub fn record_message(outcome: MessageOutcome) {
    let outcome = outcome.to_string();
    assert_metric_labels(QUEUE_MESSAGES, &[("outcome", &outcome)]);
    counter!(QUEUE_MESSAGES, "outcome" => outcome).increment(1);
}

pub fn record_event(event_type: &'static str, outcome: EventOutcome) {
    let outcome = outcome.to_string();
    assert_metric_labels(
        EVENT_RECORDS,
        &[("event_type", event_type), ("outcome", &outcome)],
    );
    counter!(
        EVENT_RECORDS,
        "event_type" => event_type,
        "outcome" => outcome
    )
    .increment(1);
}

pub fn set_queue_depth(queue: QueueKind, depth: u64) {
    let queue = queue.to_string();
    assert_metric_labels(QUEUE_DEPTH, &[("queue", &queue)]);
    gauge!(QUEUE_DEPTH, "queue" => queue).set(depth as f64);
}

pub fn set_oldest_age(age_seconds: u64) {
    assert_metric_labels(OLDEST_AGE, &[]);
    gauge!(OLDEST_AGE).set(age_seconds as f64);
}

pub fn set_consumer_healthy(healthy: bool) {
    assert_metric_labels(CONSUMER_HEALTHY, &[]);
    gauge!(CONSUMER_HEALTHY).set(if healthy { 1.0 } else { 0.0 });
}

pub fn record_processing_duration(outcome: MessageOutcome, elapsed_ms: f64) {
    let outcome = outcome.to_string();
    assert_metric_labels(PROCESSING_DURATION, &[("outcome", &outcome)]);
    histogram!(PROCESSING_DURATION, "outcome" => outcome).record(elapsed_ms);
}

pub fn record_retention(retention_class: &'static str, deleted: u64) {
    assert_metric_labels(RETENTION_DELETED, &[("retention_class", retention_class)]);
    counter!(RETENTION_DELETED, "retention_class" => retention_class).increment(deleted);
}

pub fn record_unsubscribe(
    mechanism: UnsubscribeMechanism,
    outcome: ComplianceOutcome,
    elapsed_ms: f64,
) {
    let mechanism = mechanism.to_string();
    let outcome = outcome.to_string();
    assert_metric_labels(
        UNSUBSCRIBE_REQUESTS,
        &[("mechanism", &mechanism), ("outcome", &outcome)],
    );
    counter!(
        UNSUBSCRIBE_REQUESTS,
        "mechanism" => mechanism.clone(),
        "outcome" => outcome.clone()
    )
    .increment(1);
    assert_metric_labels(
        UNSUBSCRIBE_DURATION,
        &[("mechanism", &mechanism), ("outcome", &outcome)],
    );
    histogram!(
        UNSUBSCRIBE_DURATION,
        "mechanism" => mechanism,
        "outcome" => outcome
    )
    .record(elapsed_ms);
}

pub fn record_commercial_send(outcome: ComplianceOutcome) {
    let outcome = outcome.to_string();
    assert_metric_labels(COMMERCIAL_SENDS, &[("outcome", &outcome)]);
    counter!(COMMERCIAL_SENDS, "outcome" => outcome).increment(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outreach_metrics_use_registered_low_cardinality_labels() {
        crate::observability::install_for_test();
        record_message(MessageOutcome::Stored);
        record_message(MessageOutcome::ControlAcknowledged);
        record_event("delivery", EventOutcome::Inserted);
        set_queue_depth(QueueKind::Main, 2);
        set_oldest_age(901);
        set_consumer_healthy(false);
        record_processing_duration(MessageOutcome::Stored, 12.0);
        record_retention("raw", 1);
        record_unsubscribe(
            UnsubscribeMechanism::OneClick,
            ComplianceOutcome::Success,
            3.0,
        );
        record_commercial_send(ComplianceOutcome::Blocked);
    }
}
