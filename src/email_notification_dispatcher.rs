use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::{FromRow, SqlitePool};
use tokio_util::sync::CancellationToken;

use crate::apns::ApnsService;
use crate::email_classifier;

const DISPATCH_INTERVAL_SECONDS: u64 = 60;
const DISPATCH_BATCH_LIMIT: i64 = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmailNotificationDispatchOutcome {
    Disabled { pending: usize },
    NoPending,
    Completed { sent: usize, failed: usize },
}

#[derive(Debug, Clone, FromRow)]
struct PendingIntentRow {
    id: i64,
    email_id: i64,
    mailbox: String,
    thread_id: Option<String>,
    payload_json: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DispatchIntentStatus {
    Sent,
    NoRecipients,
}

pub fn spawn_email_notification_dispatcher(pool: Arc<SqlitePool>, token: CancellationToken) {
    tokio::spawn(async move {
        tracing::info!(
            target: "agentic_api::email_notifications",
            event = "email_notifications.dispatcher_starting",
            "email notification dispatcher starting"
        );

        if let Err(e) = email_classifier::ensure_email_classifier_schema(&pool).await {
            tracing::error!(
                target: "agentic_api::email_notifications",
                event = "email_notifications.schema_failed",
                error = ?e,
                "email notification dispatcher schema bootstrap failed"
            );
            return;
        }

        let mut interval = tokio::time::interval(Duration::from_secs(DISPATCH_INTERVAL_SECONDS));
        loop {
            tokio::select! {
                _ = token.cancelled() => {
                    tracing::info!(
                        target: "agentic_api::email_notifications",
                        event = "email_notifications.dispatcher_stopping",
                        "email notification dispatcher stopping"
                    );
                    break;
                }
                _ = interval.tick() => {
                    match dispatch_pending_email_notifications_once(&pool).await {
                        Ok(EmailNotificationDispatchOutcome::NoPending) => {}
                        Ok(outcome) => tracing::info!(
                            target: "agentic_api::email_notifications",
                            event = "email_notifications.dispatcher_tick",
                            outcome = ?outcome,
                            "email notification dispatcher tick completed"
                        ),
                        Err(e) => tracing::warn!(
                            target: "agentic_api::email_notifications",
                            event = "email_notifications.dispatcher_tick_failed",
                            error = ?e,
                            "email notification dispatcher tick failed"
                        ),
                    }
                }
            }
        }
    });
}

pub async fn dispatch_pending_email_notifications_once(
    pool: &SqlitePool,
) -> Result<EmailNotificationDispatchOutcome> {
    email_classifier::ensure_email_classifier_schema(pool).await?;
    let pending = pending_intents(pool, DISPATCH_BATCH_LIMIT).await?;
    if pending.is_empty() {
        return Ok(EmailNotificationDispatchOutcome::NoPending);
    }

    let Some(apns) = ApnsService::global().cloned() else {
        return Ok(EmailNotificationDispatchOutcome::Disabled {
            pending: pending.len(),
        });
    };

    let mut sent = 0;
    let mut failed = 0;
    for intent in pending {
        if !claim_intent(pool, intent.id).await? {
            continue;
        }

        match dispatch_intent(pool, &apns, &intent).await {
            Ok(DispatchIntentStatus::Sent) => {
                mark_intent_status(pool, intent.id, "sent").await?;
                sent += 1;
            }
            Ok(DispatchIntentStatus::NoRecipients) => {
                mark_intent_status(pool, intent.id, "no_recipients").await?;
            }
            Err(e) => {
                tracing::warn!(
                    target: "agentic_api::email_notifications",
                    event = "email_notifications.dispatch_failed",
                    intent_id = intent.id,
                    email_id = intent.email_id,
                    mailbox = %intent.mailbox,
                    error = ?e,
                    "email notification dispatch failed"
                );
                mark_intent_status(pool, intent.id, "failed").await?;
                failed += 1;
            }
        }
    }

    Ok(EmailNotificationDispatchOutcome::Completed { sent, failed })
}

async fn pending_intents(pool: &SqlitePool, limit: i64) -> Result<Vec<PendingIntentRow>> {
    sqlx::query_as::<_, PendingIntentRow>(
        r#"
        SELECT id, email_id, mailbox, thread_id, payload_json
        FROM email_notification_intents
        WHERE intent = 'eligible'
          AND status = 'pending'
        ORDER BY updated_at ASC, id ASC
        LIMIT ?
        "#,
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("Failed to list pending email notification intents")
}

async fn claim_intent(pool: &SqlitePool, id: i64) -> Result<bool> {
    let updated = sqlx::query(
        r#"
        UPDATE email_notification_intents
        SET status = 'dispatching', updated_at = ?
        WHERE id = ? AND status = 'pending'
        "#,
    )
    .bind(chrono::Utc::now().timestamp())
    .bind(id)
    .execute(pool)
    .await
    .context("Failed to claim email notification intent")?;
    Ok(updated.rows_affected() > 0)
}

async fn dispatch_intent(
    pool: &SqlitePool,
    apns: &ApnsService,
    intent: &PendingIntentRow,
) -> Result<DispatchIntentStatus> {
    let recipients = mailbox_notification_recipients(pool, &intent.mailbox).await?;
    if recipients.is_empty() {
        return Ok(DispatchIntentStatus::NoRecipients);
    }

    let payload: Value =
        serde_json::from_str(&intent.payload_json).context("Failed to parse intent payload")?;
    let title = notification_title(&payload);
    let body = "Classified as relevant. Tap to review in Emails.";
    let deeplink = email_deeplink(intent.email_id);

    for user_id in recipients {
        apns.send_email_notification_to_user(
            pool,
            &user_id,
            &title,
            body,
            intent.email_id,
            intent.thread_id.as_deref(),
            Some(&intent.mailbox),
            &deeplink,
        )
        .await
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("Failed to send APNs email alert to user {user_id}"))?;
    }

    Ok(DispatchIntentStatus::Sent)
}

async fn mailbox_notification_recipients(pool: &SqlitePool, mailbox: &str) -> Result<Vec<String>> {
    let rows: Vec<(String,)> = sqlx::query_as(
        r#"
        SELECT user_id
        FROM email_accounts
        WHERE email = ? AND is_active = 1
        UNION
        SELECT access.user_id
        FROM email_account_access access
        JOIN email_accounts account ON account.id = access.email_account_id
        WHERE account.email = ? AND account.is_active = 1
        "#,
    )
    .bind(mailbox)
    .bind(mailbox)
    .fetch_all(pool)
    .await
    .context("Failed to list mailbox notification recipients")?;

    let mut seen = HashSet::new();
    let mut recipients = Vec::new();
    for (user_id,) in rows {
        if seen.insert(user_id.clone()) {
            recipients.push(user_id);
        }
    }
    Ok(recipients)
}

async fn mark_intent_status(pool: &SqlitePool, id: i64, status: &str) -> Result<()> {
    sqlx::query(
        r#"
        UPDATE email_notification_intents
        SET status = ?, updated_at = ?
        WHERE id = ?
        "#,
    )
    .bind(status)
    .bind(chrono::Utc::now().timestamp())
    .bind(id)
    .execute(pool)
    .await
    .context("Failed to update email notification intent status")?;
    Ok(())
}

fn notification_title(payload: &Value) -> String {
    let labels = payload
        .get("labels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();

    if labels.contains(&"project/laminarforge") {
        "LaminarForge email".to_string()
    } else {
        "Relevant email".to_string()
    }
}

fn email_deeplink(email_id: i64) -> String {
    format!("agenticflowstate://emails/{email_id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn laminarforge_label_sets_attention_only_title() {
        let payload = json!({"labels":["project/laminarforge","notify/eligible"]});
        assert_eq!(notification_title(&payload), "LaminarForge email");
    }

    #[test]
    fn email_deeplink_targets_email_tab_without_body_content() {
        assert_eq!(email_deeplink(42), "agenticflowstate://emails/42");
    }
}
