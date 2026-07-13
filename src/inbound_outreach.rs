//! Fail-closed recognition of commercial-outreach replies and mailto opt-outs.
//!
//! No message body or recipient address is logged or copied into the audit
//! detail. The public DynamoDB suppression authority is committed before the
//! local SQLite mirror is updated.

use anyhow::{Context, Result};
use serde_json::json;
use sqlx::SqlitePool;
use std::sync::Arc;
use std::time::Instant;
use ticketing_system::models::Email;
use ticketing_system::outreach::{
    self, ComplianceAuditEventType, NewComplianceAuditEvent, PauseScope, ReplyDisposition,
    SuppressionReason,
};

use crate::observability::outreach::{
    self as outreach_metrics, ComplianceOutcome, UnsubscribeMechanism,
};
use crate::outreach_compliance::OutreachComplianceConfig;

const COMPONENT: &str = "outreach_inbound_opt_out";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboundOutreachOutcome {
    NotApplicable,
    ReplyPaused,
    GloballySuppressed,
    AlreadyProcessed,
}

#[derive(Debug, sqlx::FromRow)]
struct CorrelatedOutreach {
    outreach_message_id: String,
    contact_id: String,
}

pub async fn process_inbound_outreach_email(
    pool: &SqlitePool,
    email: &Email,
    actor: &str,
) -> Result<InboundOutreachOutcome> {
    let started = Instant::now();
    let config = OutreachComplianceConfig::from_env()?;
    let sender = email.from_address.trim().to_ascii_lowercase();
    let is_mailto = email.to_addresses.iter().any(|address| {
        address
            .trim()
            .eq_ignore_ascii_case(&config.unsubscribe_mailbox)
    });
    let correlation = correlate_reply(pool, email, &sender).await?;
    if !is_mailto && correlation.is_none() {
        return Ok(InboundOutreachOutcome::NotApplicable);
    }

    let explicit_opt_out = is_mailto || contains_explicit_opt_out(email.body_text.as_deref());
    let event_at = email.received_at.max(1);
    let idempotency_key = format!("inbound-email:{}", email.id);
    let request_id = format!("email-{}", email.id);
    let source = if is_mailto {
        "mailto_unsubscribe"
    } else {
        "inbound_outreach_reply"
    };

    let audit_exists: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM outreach_compliance_audit_events WHERE idempotency_key = ?",
    )
    .bind(&idempotency_key)
    .fetch_optional(pool)
    .await
    .context("failed to inspect inbound opt-out idempotency")?;
    if audit_exists.is_some() {
        if is_mailto || explicit_opt_out {
            outreach_metrics::record_unsubscribe(
                if is_mailto {
                    UnsubscribeMechanism::Mailto
                } else {
                    UnsubscribeMechanism::InboundReply
                },
                ComplianceOutcome::AlreadySuppressed,
                started.elapsed().as_secs_f64() * 1_000.0,
            );
        }
        return Ok(InboundOutreachOutcome::AlreadyProcessed);
    }

    let mut recipient_hash = None;
    if explicit_opt_out {
        let secrets = config.load_secrets().await?;
        let registry = config.load_registry().await?;
        let hash = secrets.recipient_hash(&sender)?;
        registry
            .suppress_recipient(
                &sender,
                &hash,
                &idempotency_key,
                &request_id,
                source,
                event_at,
            )
            .await
            .context("failed to commit authoritative inbound global suppression")?;
        recipient_hash = Some(hash);
    }

    if let Some(correlation) = &correlation {
        outreach::record_reply(
            pool,
            &correlation.outreach_message_id,
            if explicit_opt_out {
                ReplyDisposition::NoInterest
            } else {
                ReplyDisposition::Reply
            },
            &format!(
                "inbound email {} classified by deterministic compliance rules",
                email.id
            ),
            actor,
            event_at,
        )
        .await
        .context("failed to apply correlated outreach reply")?;
    } else if explicit_opt_out {
        outreach::suppress_recipient(
            pool,
            &sender,
            SuppressionReason::Unsubscribe,
            source,
            &format!(
                "inbound email {} addressed to the unsubscribe mailbox",
                email.id
            ),
            actor,
            event_at,
        )
        .await
        .context("failed to mirror mailto suppression locally")?;
    }

    let inserted = outreach::record_compliance_audit_event(
        pool,
        NewComplianceAuditEvent {
            idempotency_key,
            event_type: if is_mailto {
                ComplianceAuditEventType::MailtoReply
            } else {
                ComplianceAuditEventType::InboundReply
            },
            request_id: Some(request_id),
            contact_id: correlation.as_ref().map(|value| value.contact_id.clone()),
            outreach_message_id: correlation
                .as_ref()
                .map(|value| value.outreach_message_id.clone()),
            recipient_hash,
            token_id_hash: None,
            source: source.to_string(),
            actor: actor.to_string(),
            before_state: "active_or_unknown".to_string(),
            after_state: if explicit_opt_out {
                "globally_suppressed".to_string()
            } else {
                "sequence_paused".to_string()
            },
            result: if explicit_opt_out {
                "suppressed".to_string()
            } else {
                "recorded".to_string()
            },
            detail: json!({
                "mechanism": source,
                "disposition": if explicit_opt_out { "opt_out" } else { "reply" }
            }),
            event_at,
        },
    )
    .await
    .context("failed to append inbound outreach compliance audit")?;
    if !inserted {
        return Ok(InboundOutreachOutcome::AlreadyProcessed);
    }

    tracing::info!(
        target: "agentic_api::outreach_inbound",
        event = "outreach_inbound.processed",
        email_id = email.id,
        mechanism = source,
        outcome = if explicit_opt_out { "globally_suppressed" } else { "reply_paused" },
        "processed inbound outreach response"
    );
    if explicit_opt_out {
        outreach_metrics::record_unsubscribe(
            if is_mailto {
                UnsubscribeMechanism::Mailto
            } else {
                UnsubscribeMechanism::InboundReply
            },
            ComplianceOutcome::Success,
            started.elapsed().as_secs_f64() * 1_000.0,
        );
    }
    Ok(if explicit_opt_out {
        InboundOutreachOutcome::GloballySuppressed
    } else {
        InboundOutreachOutcome::ReplyPaused
    })
}

pub async fn fail_closed_on_processing_error(
    pool: &SqlitePool,
    email_id: i64,
    error: &impl std::fmt::Display,
) {
    let now = chrono::Utc::now().timestamp();
    let _ = outreach::pause_outreach(
        pool,
        PauseScope::Global,
        "inbound opt-out processing failed",
        COMPONENT,
        now,
    )
    .await;
    let detail = json!({"email_id": email_id, "status": "global_pause_enforced"}).to_string();
    tracing::error!(
        target: "agentic_api::outreach_inbound",
        event = "outreach_inbound.failed_closed",
        email_id,
        error = %error,
        "inbound outreach processing failed; commercial outreach remains globally paused"
    );
    crate::system_log_helper::log_error(
        &Arc::new(pool.clone()),
        COMPONENT,
        "Inbound outreach processing failed; commercial outreach is globally paused",
        Some(&detail),
    )
    .await;
}

async fn correlate_reply(
    pool: &SqlitePool,
    email: &Email,
    sender: &str,
) -> Result<Option<CorrelatedOutreach>> {
    let Some(in_reply_to) = email.in_reply_to.as_deref() else {
        return Ok(None);
    };
    sqlx::query_as::<_, CorrelatedOutreach>(
        r#"
        SELECT om.outreach_message_id, om.contact_id
        FROM emails sent
        JOIN email_provider_messages epm ON epm.email_id = sent.id
        JOIN outreach_messages om
          ON om.provider = epm.provider
         AND om.provider_message_id = epm.provider_message_id
        WHERE sent.message_id = ?
          AND lower(om.recipient_normalized) = ?
        LIMIT 1
        "#,
    )
    .bind(in_reply_to.trim())
    .bind(sender)
    .fetch_optional(pool)
    .await
    .context("failed to correlate inbound outreach reply")
}

fn contains_explicit_opt_out(body: Option<&str>) -> bool {
    let Some(body) = body else {
        return false;
    };
    let normalized = body
        .chars()
        .take(2_000)
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '\'' {
                character.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>();
    let normalized = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
    [
        "unsubscribe",
        "opt out",
        "remove me",
        "do not contact",
        "don't contact",
        "not interested",
        "no interest",
        "stop emailing",
        "stop email",
    ]
    .iter()
    .any(|phrase| contains_phrase(&normalized, phrase))
}

fn contains_phrase(text: &str, phrase: &str) -> bool {
    text.match_indices(phrase).any(|(index, _)| {
        let before = text[..index].chars().next_back();
        let after = text[index + phrase.len()..].chars().next();
        before.is_none_or(char::is_whitespace) && after.is_none_or(char::is_whitespace)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_opt_out_recognition_is_deterministic_and_word_bounded() {
        for text in [
            "Unsubscribe",
            "Please opt out.",
            "REMOVE ME from this list",
            "I'm not interested, thanks.",
            "Please stop emailing me.",
        ] {
            assert!(contains_explicit_opt_out(Some(text)), "{text}");
        }
        for text in [
            "I am interested",
            "The stopper is installed",
            "Subscription details attached",
            "Thanks for the note",
        ] {
            assert!(!contains_explicit_opt_out(Some(text)), "{text}");
        }
    }
}
