//! Privacy-safe quarantine evidence for messages that are not SES recipient events.
//!
//! This module never stores or logs a queue body, recipient, sender identifier,
//! or message-attribute value. Evidence is limited to an opaque body digest,
//! SQS transport identifiers/timestamps, bounded classifications, and attribute
//! names so operators can prove provenance without exposing message content.

use anyhow::{Context, Result};
use aws_sdk_sqs::types::{Message, MessageSystemAttributeName};
use chrono::Utc;
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

pub const COMPONENT: &str = "ses_outreach_consumer";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyClassification {
    SesEventDestinationValidation,
    MalformedSesEvent,
    MissingBody,
}

impl BodyClassification {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SesEventDestinationValidation => "ses_event_destination_validation",
            Self::MalformedSesEvent => "malformed_ses_event",
            Self::MissingBody => "missing_body",
        }
    }

    pub fn is_acknowledgeable_control(self) -> bool {
        self == Self::SesEventDestinationValidation
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantineEvidence {
    pub quarantine_id: String,
    pub fingerprint: String,
    pub sqs_message_id: Option<String>,
    pub body_size_bytes: Option<u64>,
    pub classification: BodyClassification,
    pub parse_error_type: String,
    pub sender_kind: &'static str,
    pub sent_at_ms: Option<i64>,
    pub first_received_at_ms: Option<i64>,
    pub receive_count: u64,
    pub message_attribute_names: Vec<String>,
}

impl QuarantineEvidence {
    pub fn from_message(
        message: &Message,
        classification: BodyClassification,
        parse_error_type: impl Into<String>,
    ) -> Self {
        let body = message.body();
        let message_id = message.message_id().map(ToString::to_string);
        let fingerprint = body
            .map(body_fingerprint)
            .unwrap_or_else(|| missing_body_fingerprint(message_id.as_deref()));
        let quarantine_id = message_id
            .as_deref()
            .map(|value| format!("sqs:{value}"))
            .unwrap_or_else(|| format!("body:{fingerprint}"));
        let attributes = message.attributes();
        let sender_kind = attributes
            .and_then(|items| items.get(&MessageSystemAttributeName::SenderId))
            .map(|value| classify_sender_id(value))
            .unwrap_or("unavailable");
        let sent_at_ms = system_attribute_i64(message, MessageSystemAttributeName::SentTimestamp);
        let first_received_at_ms = system_attribute_i64(
            message,
            MessageSystemAttributeName::ApproximateFirstReceiveTimestamp,
        );
        let receive_count =
            system_attribute_i64(message, MessageSystemAttributeName::ApproximateReceiveCount)
                .and_then(|value| u64::try_from(value).ok())
                .unwrap_or(0);
        let mut message_attribute_names = message
            .message_attributes()
            .map(|items| items.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        message_attribute_names.sort();

        Self {
            quarantine_id,
            fingerprint,
            sqs_message_id: message_id,
            body_size_bytes: body.map(|value| value.len() as u64),
            classification,
            parse_error_type: parse_error_type.into(),
            sender_kind,
            sent_at_ms,
            first_received_at_ms,
            receive_count,
            message_attribute_names,
        }
    }

    pub fn diagnostic_detail(&self, status: &str) -> String {
        json!({
            "status": status,
            "classification": self.classification.as_str(),
            "parse_error_type": self.parse_error_type,
            "body_sha256": self.fingerprint,
            "body_size_bytes": self.body_size_bytes,
            "sqs_message_id": self.sqs_message_id,
            "sender_kind": self.sender_kind,
            "sent_at_ms": self.sent_at_ms,
            "first_received_at_ms": self.first_received_at_ms,
            "receive_count": self.receive_count,
            "message_attribute_names": self.message_attribute_names,
        })
        .to_string()
    }
}

pub fn classify_non_json_body(body: &str) -> BodyClassification {
    let normalized = body.trim().to_ascii_lowercase();
    let sns_topic_reference = normalized.contains("sns") && normalized.contains("topic");
    let validation_reference = normalized.contains("validat");
    let success_reference = normalized.contains("success");
    let bounded_plain_text = !normalized.is_empty()
        && normalized.len() <= 4_096
        && !normalized.starts_with('{')
        && !normalized.starts_with('[');

    if bounded_plain_text && sns_topic_reference && validation_reference && success_reference {
        BodyClassification::SesEventDestinationValidation
    } else {
        BodyClassification::MalformedSesEvent
    }
}

pub fn body_fingerprint(body: &str) -> String {
    format!("{:x}", Sha256::digest(body.as_bytes()))
}

fn missing_body_fingerprint(message_id: Option<&str>) -> String {
    body_fingerprint(&format!("missing-body:{}", message_id.unwrap_or("unknown")))
}

fn classify_sender_id(value: &str) -> &'static str {
    if value.len() == 12 && value.bytes().all(|item| item.is_ascii_digit()) {
        "aws_account"
    } else if value.starts_with("AID") {
        "iam_user"
    } else if value.starts_with("ARO") {
        "iam_role"
    } else {
        "other"
    }
}

fn system_attribute_i64(message: &Message, name: MessageSystemAttributeName) -> Option<i64> {
    message.attributes()?.get(&name)?.parse().ok()
}

pub async fn ensure_schema(pool: &SqlitePool) -> Result<()> {
    let legacy_schema: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)
        FROM pragma_table_info('ses_event_quarantine')
        WHERE name = 'fingerprint'
          AND NOT EXISTS (
              SELECT 1 FROM pragma_table_info('ses_event_quarantine')
              WHERE name = 'quarantine_id'
          )
        "#,
    )
    .fetch_one(pool)
    .await
    .context("failed to inspect SES event quarantine schema")?;
    if legacy_schema > 0 {
        sqlx::query("ALTER TABLE ses_event_quarantine RENAME TO ses_event_quarantine_legacy")
            .execute(pool)
            .await
            .context("failed to preserve legacy SES event quarantine records")?;
    }
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS ses_event_quarantine (
            quarantine_id TEXT PRIMARY KEY,
            fingerprint TEXT NOT NULL,
            sqs_message_id TEXT,
            body_size_bytes INTEGER,
            classification TEXT NOT NULL,
            parse_error_type TEXT NOT NULL,
            sender_kind TEXT NOT NULL,
            sent_at_ms INTEGER,
            first_received_at_ms INTEGER,
            receive_count INTEGER NOT NULL,
            message_attribute_names TEXT NOT NULL,
            status TEXT NOT NULL,
            ticket_id TEXT,
            first_seen_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        )
        "#,
    )
    .execute(pool)
    .await
    .context("failed to create SES event quarantine table")?;
    if legacy_schema > 0 {
        sqlx::query(
            r#"
            INSERT INTO ses_event_quarantine (
                quarantine_id, fingerprint, sqs_message_id, body_size_bytes,
                classification, parse_error_type, sender_kind, sent_at_ms,
                first_received_at_ms, receive_count, message_attribute_names,
                status, ticket_id, first_seen_at, updated_at
            )
            SELECT 'legacy:' || fingerprint, fingerprint, sqs_message_id,
                   body_size_bytes, classification, parse_error_type,
                   sender_kind, sent_at_ms, first_received_at_ms,
                   receive_count, message_attribute_names, status, ticket_id,
                   first_seen_at, updated_at
            FROM ses_event_quarantine_legacy
            "#,
        )
        .execute(pool)
        .await
        .context("failed to migrate legacy SES event quarantine records")?;
        sqlx::query("DROP TABLE ses_event_quarantine_legacy")
            .execute(pool)
            .await
            .context("failed to remove migrated SES event quarantine table")?;
    }
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_ses_event_quarantine_status ON ses_event_quarantine(status, updated_at)",
    )
    .execute(pool)
    .await
    .context("failed to create SES event quarantine status index")?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_ses_event_quarantine_fingerprint ON ses_event_quarantine(fingerprint)",
    )
    .execute(pool)
    .await
    .context("failed to create SES event quarantine fingerprint index")?;
    Ok(())
}

pub async fn record(
    pool: &SqlitePool,
    evidence: &QuarantineEvidence,
    status: &str,
    ticket_id: Option<&str>,
) -> Result<()> {
    let now = Utc::now().timestamp();
    let attribute_names = serde_json::to_string(&evidence.message_attribute_names)
        .context("failed to serialize SQS message attribute names")?;
    sqlx::query(
        r#"
        INSERT INTO ses_event_quarantine (
            quarantine_id, fingerprint, sqs_message_id, body_size_bytes, classification,
            parse_error_type, sender_kind, sent_at_ms, first_received_at_ms,
            receive_count, message_attribute_names, status, ticket_id,
            first_seen_at, updated_at
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(quarantine_id) DO UPDATE SET
            fingerprint = excluded.fingerprint,
            sqs_message_id = excluded.sqs_message_id,
            body_size_bytes = excluded.body_size_bytes,
            classification = excluded.classification,
            parse_error_type = excluded.parse_error_type,
            sender_kind = excluded.sender_kind,
            sent_at_ms = excluded.sent_at_ms,
            first_received_at_ms = excluded.first_received_at_ms,
            receive_count = MAX(ses_event_quarantine.receive_count, excluded.receive_count),
            message_attribute_names = excluded.message_attribute_names,
            status = excluded.status,
            ticket_id = COALESCE(excluded.ticket_id, ses_event_quarantine.ticket_id),
            updated_at = excluded.updated_at
        "#,
    )
    .bind(&evidence.quarantine_id)
    .bind(&evidence.fingerprint)
    .bind(&evidence.sqs_message_id)
    .bind(
        evidence
            .body_size_bytes
            .and_then(|value| i64::try_from(value).ok()),
    )
    .bind(evidence.classification.as_str())
    .bind(&evidence.parse_error_type)
    .bind(evidence.sender_kind)
    .bind(evidence.sent_at_ms)
    .bind(evidence.first_received_at_ms)
    .bind(i64::try_from(evidence.receive_count).unwrap_or(i64::MAX))
    .bind(attribute_names)
    .bind(status)
    .bind(ticket_id)
    .bind(now)
    .bind(now)
    .execute(pool)
    .await
    .context("failed to persist SES event quarantine evidence")?;
    Ok(())
}

pub async fn log_diagnostic(
    pool: &SqlitePool,
    evidence: &QuarantineEvidence,
    level: &str,
    message: &str,
    status: &str,
) -> Result<()> {
    let detail = evidence.diagnostic_detail(status);
    ticketing_system::system_logs::insert_log(
        pool,
        level,
        COMPONENT,
        message,
        Some(&detail),
        None,
        None,
    )
    .await
    .context("failed to persist SES quarantine diagnostic")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    #[test]
    fn classifies_only_bounded_successful_sns_validation_control_text() {
        assert_eq!(
            classify_non_json_body(
                "Amazon SES successfully validated the configured Amazon SNS topic."
            ),
            BodyClassification::SesEventDestinationValidation
        );
        assert_eq!(
            classify_non_json_body("not-json"),
            BodyClassification::MalformedSesEvent
        );
        assert_eq!(
            classify_non_json_body("SNS topic validation failed"),
            BodyClassification::MalformedSesEvent
        );
    }

    #[test]
    fn fingerprint_is_stable_and_does_not_contain_body_text() {
        let digest = body_fingerprint("sensitive-control-body");
        assert_eq!(digest.len(), 64);
        assert!(!digest.contains("sensitive"));
        assert_eq!(digest, body_fingerprint("sensitive-control-body"));
    }

    #[tokio::test]
    async fn identical_control_bodies_keep_distinct_transport_records() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        ensure_schema(&pool).await.unwrap();
        let body = "Amazon SES successfully validated the configured Amazon SNS topic.";
        for message_id in ["transport-1", "transport-2"] {
            let message = Message::builder().message_id(message_id).body(body).build();
            let evidence = QuarantineEvidence::from_message(
                &message,
                classify_non_json_body(body),
                "invalid_json",
            );
            record(&pool, &evidence, "dlq_quarantined", Some("T-TEST"))
                .await
                .unwrap();
        }
        let records: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ses_event_quarantine")
            .fetch_one(&pool)
            .await
            .unwrap();
        let fingerprints: i64 =
            sqlx::query_scalar("SELECT COUNT(DISTINCT fingerprint) FROM ses_event_quarantine")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(records, 2);
        assert_eq!(fingerprints, 1);
    }
}
