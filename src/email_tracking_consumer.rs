//! Continuously consumes SES event notifications from the configured SQS queue.
//!
//! Messages are deleted only after both the generic provider timeline and the
//! outreach authority have durably accepted every normalized recipient event.
//! Permanent parse failures are returned to the queue with zero visibility so
//! the queue's configured max-receive policy, rather than this process, owns
//! poison-message redrive. No event body or recipient address is logged.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use aws_sdk_sqs::types::{Message, MessageSystemAttributeName, QueueAttributeName};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use ticketing_system::email_tracking::NewEmailDeliveryEvent;
use ticketing_system::outreach::{
    AutomationClass, BounceType, NewSesEvent, PauseScope, SesEventType, SubscriptionStatus,
};
use ticketing_system::SqlitePool;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::observability::outreach::{
    self as outreach_metrics, EventOutcome, MessageOutcome, QueueKind,
};
use crate::system_log_helper;

const COMPONENT: &str = "ses_outreach_consumer";
const ACTOR: &str = "ses_outreach_consumer";
const WAIT_TIME_SECONDS: i32 = 20;
const VISIBILITY_TIMEOUT_SECONDS: i32 = 120;
const QUEUE_AGE_LIMIT_SECONDS: u64 = 900;
const HEALTH_VALID_FOR_SECONDS: i64 = 60;
const CONFIG_RETRY_SECONDS: u64 = 30;
const TRANSIENT_RETRY_SECONDS: u64 = 5;
const RETENTION_INTERVAL_SECONDS: u64 = 24 * 60 * 60;

#[derive(Debug, Clone, PartialEq, Eq)]
struct QueueConfig {
    profile: String,
    region: String,
    queue_url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HealthReason {
    Healthy,
    ConfigurationUnavailable,
    QueueInspectionFailed,
    QueueAgeExceeded,
    DlqNotEmpty,
    BacklogUnavailable,
    ReceiveFailed,
    PoisonMessage,
    StorageFailed,
    DeleteFailed,
    Shutdown,
}

impl HealthReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::ConfigurationUnavailable => "configuration_unavailable",
            Self::QueueInspectionFailed => "queue_inspection_failed",
            Self::QueueAgeExceeded => "queue_age_exceeded",
            Self::DlqNotEmpty => "dlq_not_empty",
            Self::BacklogUnavailable => "backlog_unavailable",
            Self::ReceiveFailed => "receive_failed",
            Self::PoisonMessage => "poison_message",
            Self::StorageFailed => "storage_failed",
            Self::DeleteFailed => "delete_failed",
            Self::Shutdown => "shutdown",
        }
    }

    fn pause_reason(self) -> &'static str {
        match self {
            Self::Healthy => "SES outreach event consumer is healthy",
            Self::ConfigurationUnavailable => {
                "SES outreach event consumer configuration is unavailable"
            }
            Self::QueueInspectionFailed => "SES outreach queue or DLQ inspection failed",
            Self::QueueAgeExceeded => "SES outreach queue age exceeded 900 seconds",
            Self::DlqNotEmpty => "SES outreach event DLQ is not empty",
            Self::BacklogUnavailable => "SES outreach queue backlog could not be received",
            Self::ReceiveFailed => "SES outreach queue receive failed",
            Self::PoisonMessage => "SES outreach queue contains a poison message",
            Self::StorageFailed => "SES outreach event durable storage failed",
            Self::DeleteFailed => "SES outreach queue acknowledgement failed",
            Self::Shutdown => "SES outreach event consumer stopped",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HealthState {
    consumer_healthy: bool,
    queue_healthy: bool,
    reason: HealthReason,
    main_depth: u64,
    dlq_depth: u64,
    oldest_age_seconds: u64,
}

impl HealthState {
    fn healthy(main_depth: u64) -> Self {
        Self {
            consumer_healthy: true,
            queue_healthy: true,
            reason: HealthReason::Healthy,
            main_depth,
            dlq_depth: 0,
            oldest_age_seconds: 0,
        }
    }

    fn unhealthy(reason: HealthReason) -> Self {
        Self {
            consumer_healthy: false,
            queue_healthy: false,
            reason,
            main_depth: 0,
            dlq_depth: 0,
            oldest_age_seconds: 0,
        }
    }
}

#[derive(Debug)]
struct HealthReporter {
    last_state: Option<HealthState>,
}

impl HealthReporter {
    fn new() -> Self {
        Self { last_state: None }
    }

    async fn report(&mut self, pool: &Arc<SqlitePool>, state: HealthState) {
        let now = Utc::now().timestamp();
        outreach_metrics::set_consumer_healthy(state.consumer_healthy && state.queue_healthy);
        outreach_metrics::set_queue_depth(QueueKind::Main, state.main_depth);
        outreach_metrics::set_queue_depth(QueueKind::Dlq, state.dlq_depth);
        outreach_metrics::set_oldest_age(state.oldest_age_seconds);

        let changed = self.last_state != Some(state);
        if !state.consumer_healthy || !state.queue_healthy {
            if changed {
                if let Err(error) = ticketing_system::outreach::pause_outreach(
                    pool,
                    PauseScope::Global,
                    state.reason.pause_reason(),
                    ACTOR,
                    now,
                )
                .await
                {
                    tracing::error!(
                        target: "agentic_api::ses_outreach_consumer",
                        event = "ses_outreach_consumer.pause_failed",
                        reason = state.reason.as_str(),
                        error = %error,
                        "failed to persist automatic global outreach pause"
                    );
                }
            }
        }

        let evidence = json!({
            "status": state.reason.as_str(),
            "main_depth": state.main_depth,
            "dlq_depth": state.dlq_depth,
            "oldest_age_seconds": state.oldest_age_seconds,
        })
        .to_string();
        let update = sqlx::query(
            r#"
            UPDATE outreach_operational_controls
            SET event_consumer_healthy = ?, queue_healthy = ?,
                health_valid_until = ?, updated_by = ?, updated_at = ?
            WHERE control_id = 'global'
            "#,
        )
        .bind(i64::from(state.consumer_healthy))
        .bind(i64::from(state.queue_healthy))
        .bind(now + HEALTH_VALID_FOR_SECONDS)
        .bind(ACTOR)
        .bind(now)
        .execute(pool.as_ref())
        .await;
        match update {
            Ok(result) if result.rows_affected() == 1 => {}
            Ok(_) => {
                tracing::error!(
                    target: "agentic_api::ses_outreach_consumer",
                    event = "ses_outreach_consumer.controls_missing",
                    "global outreach operational controls row is missing"
                );
            }
            Err(error) => {
                tracing::error!(
                    target: "agentic_api::ses_outreach_consumer",
                    event = "ses_outreach_consumer.controls_update_failed",
                    error = %error,
                    "failed to persist SES consumer operational controls"
                );
            }
        }

        if changed {
            let level = if state.consumer_healthy && state.queue_healthy {
                "info"
            } else {
                "error"
            };
            system_log_helper::log_event(
                pool,
                level,
                COMPONENT,
                "SES outreach consumer health changed",
                Some(&evidence),
                None,
                None,
            )
            .await;
            tracing::event!(
                target: "agentic_api::ses_outreach_consumer",
                tracing::Level::INFO,
                event = "ses_outreach_consumer.health_changed",
                status = state.reason.as_str(),
                consumer_healthy = state.consumer_healthy,
                queue_healthy = state.queue_healthy,
                main_depth = state.main_depth,
                dlq_depth = state.dlq_depth,
                oldest_age_seconds = state.oldest_age_seconds,
                "SES outreach consumer health changed"
            );
        }
        self.last_state = Some(state);
    }
}

#[derive(Debug, Clone)]
struct ParsedEvent {
    provider_message_id: String,
    event_type: SesEventType,
    event_at: i64,
    recipients: Vec<String>,
    bounce_type: Option<BounceType>,
    feedback_id: Option<String>,
    url: Option<String>,
    subscription_status: Option<SubscriptionStatus>,
    automation_class: AutomationClass,
    diagnostic_type: Option<String>,
    diagnostic_text: Option<String>,
    raw_payload: String,
}

#[derive(Debug, Default)]
struct MessageStoreResult {
    inserted: usize,
    duplicates: usize,
}

#[derive(Debug, Clone, Copy)]
struct QueueSnapshot {
    visible: u64,
    not_visible: u64,
    dlq_visible: Option<u64>,
}

impl QueueSnapshot {
    fn main_depth(self) -> u64 {
        self.visible.saturating_add(self.not_visible)
    }
}

pub fn spawn(pool: Arc<SqlitePool>, token: CancellationToken) {
    let consumer_pool = pool.clone();
    let consumer_token = token.child_token();
    tokio::spawn(async move {
        run_consumer(consumer_pool, consumer_token).await;
    });

    tokio::spawn(async move {
        run_retention(pool, token).await;
    });
}

async fn run_consumer(pool: Arc<SqlitePool>, token: CancellationToken) {
    let mut reporter = HealthReporter::new();
    system_log_helper::log_info(
        &pool,
        COMPONENT,
        "SES outreach consumer started",
        Some(r#"{"status":"starting"}"#),
    )
    .await;

    loop {
        if token.is_cancelled() {
            break;
        }
        let config = match load_queue_config(&pool).await {
            Ok(config) => config,
            Err(error) => {
                tracing::error!(
                    target: "agentic_api::ses_outreach_consumer",
                    event = "ses_outreach_consumer.configuration_failed",
                    error = %error,
                    "SES outreach consumer configuration is unavailable"
                );
                reporter
                    .report(
                        &pool,
                        HealthState::unhealthy(HealthReason::ConfigurationUnavailable),
                    )
                    .await;
                if wait_or_cancel(&token, CONFIG_RETRY_SECONDS).await {
                    break;
                }
                continue;
            }
        };

        let aws = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .profile_name(&config.profile)
            .region(aws_config::Region::new(config.region.clone()))
            .load()
            .await;
        let client = aws_sdk_sqs::Client::new(&aws);
        if let Err(error) =
            consume_configured_queue(&pool, &client, &config, &token, &mut reporter).await
        {
            tracing::error!(
                target: "agentic_api::ses_outreach_consumer",
                event = "ses_outreach_consumer.loop_failed",
                error = %error,
                "SES outreach consumer loop failed"
            );
            reporter
                .report(
                    &pool,
                    HealthState::unhealthy(HealthReason::QueueInspectionFailed),
                )
                .await;
            if wait_or_cancel(&token, CONFIG_RETRY_SECONDS).await {
                break;
            }
        }
    }

    reporter
        .report(&pool, HealthState::unhealthy(HealthReason::Shutdown))
        .await;
    system_log_helper::log_warn(
        &pool,
        COMPONENT,
        "SES outreach consumer stopped",
        Some(r#"{"status":"shutdown"}"#),
    )
    .await;
}

async fn consume_configured_queue(
    pool: &Arc<SqlitePool>,
    client: &aws_sdk_sqs::Client,
    config: &QueueConfig,
    token: &CancellationToken,
    reporter: &mut HealthReporter,
) -> Result<()> {
    let dlq_url = resolve_dlq_url(client, &config.queue_url).await?;
    loop {
        if token.is_cancelled() {
            return Ok(());
        }
        let before = inspect_queues(client, &config.queue_url, &dlq_url).await?;
        outreach_metrics::set_queue_depth(QueueKind::Main, before.main_depth());
        if let Some(dlq_visible) = before.dlq_visible {
            outreach_metrics::set_queue_depth(QueueKind::Dlq, dlq_visible);
        }
        if before.dlq_visible.is_none() {
            reporter
                .report(
                    pool,
                    HealthState {
                        consumer_healthy: true,
                        queue_healthy: false,
                        reason: HealthReason::QueueInspectionFailed,
                        main_depth: before.main_depth(),
                        dlq_depth: 0,
                        oldest_age_seconds: 0,
                    },
                )
                .await;
        } else if before.dlq_visible.is_some_and(|depth| depth > 0) {
            reporter
                .report(
                    pool,
                    HealthState {
                        consumer_healthy: true,
                        queue_healthy: false,
                        reason: HealthReason::DlqNotEmpty,
                        main_depth: before.main_depth(),
                        dlq_depth: before.dlq_visible.unwrap_or(0),
                        oldest_age_seconds: 0,
                    },
                )
                .await;
        }

        let receive = tokio::select! {
            _ = token.cancelled() => return Ok(()),
            result = client
                .receive_message()
                .queue_url(&config.queue_url)
                .max_number_of_messages(10)
                .wait_time_seconds(WAIT_TIME_SECONDS)
                .visibility_timeout(VISIBILITY_TIMEOUT_SECONDS)
                .message_system_attribute_names(MessageSystemAttributeName::SentTimestamp)
                .message_system_attribute_names(MessageSystemAttributeName::ApproximateReceiveCount)
                .send() => result,
        };
        let response = match receive {
            Ok(response) => response,
            Err(error) => {
                reporter
                    .report(pool, HealthState::unhealthy(HealthReason::ReceiveFailed))
                    .await;
                tracing::warn!(
                    target: "agentic_api::ses_outreach_consumer",
                    event = "ses_outreach_consumer.receive_failed",
                    error = %error,
                    "failed to receive SES events from SQS"
                );
                if wait_or_cancel(token, TRANSIENT_RETRY_SECONDS).await {
                    return Ok(());
                }
                continue;
            }
        };

        let messages = response.messages();
        if messages.is_empty() {
            let after = inspect_queues(client, &config.queue_url, &dlq_url).await?;
            let state = if after.dlq_visible.is_none() {
                HealthState {
                    consumer_healthy: true,
                    queue_healthy: false,
                    reason: HealthReason::QueueInspectionFailed,
                    main_depth: after.main_depth(),
                    dlq_depth: 0,
                    oldest_age_seconds: 0,
                }
            } else if after.dlq_visible.is_some_and(|depth| depth > 0) {
                HealthState {
                    consumer_healthy: true,
                    queue_healthy: false,
                    reason: HealthReason::DlqNotEmpty,
                    main_depth: after.main_depth(),
                    dlq_depth: after.dlq_visible.unwrap_or(0),
                    oldest_age_seconds: 0,
                }
            } else if after.main_depth() > 0 {
                HealthState {
                    consumer_healthy: false,
                    queue_healthy: false,
                    reason: HealthReason::BacklogUnavailable,
                    main_depth: after.main_depth(),
                    dlq_depth: 0,
                    oldest_age_seconds: 0,
                }
            } else {
                HealthState::healthy(0)
            };
            reporter.report(pool, state).await;
            continue;
        }

        let message_ages: Vec<Option<u64>> = messages.iter().map(message_age_seconds).collect();
        let age_known = message_ages.iter().all(Option::is_some);
        let oldest_age = message_ages.into_iter().flatten().max().unwrap_or(0);
        if !age_known {
            reporter
                .report(
                    pool,
                    HealthState {
                        consumer_healthy: true,
                        queue_healthy: false,
                        reason: HealthReason::QueueInspectionFailed,
                        main_depth: before.main_depth().max(messages.len() as u64),
                        dlq_depth: before.dlq_visible.unwrap_or(0),
                        oldest_age_seconds: oldest_age,
                    },
                )
                .await;
        } else if oldest_age > QUEUE_AGE_LIMIT_SECONDS {
            reporter
                .report(
                    pool,
                    HealthState {
                        consumer_healthy: true,
                        queue_healthy: false,
                        reason: HealthReason::QueueAgeExceeded,
                        main_depth: before.main_depth().max(messages.len() as u64),
                        dlq_depth: before.dlq_visible.unwrap_or(0),
                        oldest_age_seconds: oldest_age,
                    },
                )
                .await;
        }

        let mut batch_inserted = 0_usize;
        let mut batch_duplicates = 0_usize;
        let mut batch_failed = 0_usize;
        for message in messages {
            outreach_metrics::record_message(MessageOutcome::Received);
            let started = Instant::now();
            match process_message(pool, client, &config.queue_url, message).await {
                Ok(result) => {
                    batch_inserted += result.inserted;
                    batch_duplicates += result.duplicates;
                    let outcome = if result.inserted == 0 {
                        MessageOutcome::Duplicate
                    } else {
                        MessageOutcome::Stored
                    };
                    outreach_metrics::record_message(outcome);
                    outreach_metrics::record_processing_duration(
                        outcome,
                        started.elapsed().as_secs_f64() * 1_000.0,
                    );
                }
                Err(ProcessError::Poison {
                    error,
                    receive_count,
                }) => {
                    batch_failed += 1;
                    outreach_metrics::record_message(MessageOutcome::Poison);
                    outreach_metrics::record_processing_duration(
                        MessageOutcome::Poison,
                        started.elapsed().as_secs_f64() * 1_000.0,
                    );
                    reporter
                        .report(pool, HealthState::unhealthy(HealthReason::PoisonMessage))
                        .await;
                    let detail = json!({
                        "status": "poison",
                        "receive_count": receive_count,
                        "error_type": error,
                    })
                    .to_string();
                    system_log_helper::log_error(
                        pool,
                        COMPONENT,
                        "SES event message failed permanent parsing and remains for redrive",
                        Some(&detail),
                    )
                    .await;
                }
                Err(ProcessError::Storage(error)) => {
                    batch_failed += 1;
                    outreach_metrics::record_message(MessageOutcome::StorageError);
                    outreach_metrics::record_processing_duration(
                        MessageOutcome::StorageError,
                        started.elapsed().as_secs_f64() * 1_000.0,
                    );
                    reporter
                        .report(pool, HealthState::unhealthy(HealthReason::StorageFailed))
                        .await;
                    tracing::error!(
                        target: "agentic_api::ses_outreach_consumer",
                        event = "ses_outreach_consumer.storage_failed",
                        error = %error,
                        "SES event storage failed; message was not deleted"
                    );
                }
                Err(ProcessError::Delete(error)) => {
                    batch_failed += 1;
                    outreach_metrics::record_message(MessageOutcome::DeleteError);
                    outreach_metrics::record_processing_duration(
                        MessageOutcome::DeleteError,
                        started.elapsed().as_secs_f64() * 1_000.0,
                    );
                    reporter
                        .report(pool, HealthState::unhealthy(HealthReason::DeleteFailed))
                        .await;
                    tracing::error!(
                        target: "agentic_api::ses_outreach_consumer",
                        event = "ses_outreach_consumer.delete_failed",
                        error = %error,
                        "SES event acknowledgement failed; idempotent retry is required"
                    );
                }
            }
        }
        tracing::info!(
            target: "agentic_api::ses_outreach_consumer",
            event = "ses_outreach_consumer.batch_complete",
            messages = messages.len(),
            events_inserted = batch_inserted,
            event_duplicates = batch_duplicates,
            failures = batch_failed,
            oldest_age_seconds = oldest_age,
            "processed SES event queue batch"
        );

        if batch_failed == 0
            && age_known
            && oldest_age <= QUEUE_AGE_LIMIT_SECONDS
            && before.dlq_visible == Some(0)
        {
            match inspect_queues(client, &config.queue_url, &dlq_url).await {
                Ok(after) if after.dlq_visible == Some(0) => {
                    reporter
                        .report(pool, HealthState::healthy(after.main_depth()))
                        .await;
                }
                Ok(after) if after.dlq_visible.is_some() => {
                    reporter
                        .report(
                            pool,
                            HealthState {
                                consumer_healthy: true,
                                queue_healthy: false,
                                reason: HealthReason::DlqNotEmpty,
                                main_depth: after.main_depth(),
                                dlq_depth: after.dlq_visible.unwrap_or(0),
                                oldest_age_seconds: 0,
                            },
                        )
                        .await;
                }
                Ok(after) => {
                    reporter
                        .report(
                            pool,
                            HealthState {
                                consumer_healthy: true,
                                queue_healthy: false,
                                reason: HealthReason::QueueInspectionFailed,
                                main_depth: after.main_depth(),
                                dlq_depth: 0,
                                oldest_age_seconds: 0,
                            },
                        )
                        .await;
                }
                Err(error) => {
                    tracing::warn!(
                        target: "agentic_api::ses_outreach_consumer",
                        event = "ses_outreach_consumer.post_batch_inspection_failed",
                        error = %error,
                        "failed to inspect queues after SES event batch"
                    );
                    reporter
                        .report(
                            pool,
                            HealthState::unhealthy(HealthReason::QueueInspectionFailed),
                        )
                        .await;
                }
            }
        }
    }
}

#[derive(Debug)]
enum ProcessError {
    Poison {
        error: &'static str,
        receive_count: u64,
    },
    Storage(anyhow::Error),
    Delete(anyhow::Error),
}

async fn process_message(
    pool: &Arc<SqlitePool>,
    client: &aws_sdk_sqs::Client,
    queue_url: &str,
    message: &Message,
) -> std::result::Result<MessageStoreResult, ProcessError> {
    let receive_count = message_receive_count(message);
    let receipt_handle = match message.receipt_handle() {
        Some(value) => value,
        None => {
            return Err(ProcessError::Poison {
                error: "missing_receipt_handle",
                receive_count,
            })
        }
    };
    let body = match message.body() {
        Some(value) => value,
        None => {
            accelerate_redrive(client, queue_url, receipt_handle).await;
            return Err(ProcessError::Poison {
                error: "missing_body",
                receive_count,
            });
        }
    };
    let event = match parse_ses_event(body) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(
                target: "agentic_api::ses_outreach_consumer",
                event = "ses_outreach_consumer.poison_detected",
                receive_count,
                error = %error,
                "SES event could not be parsed; returning it for SQS redrive"
            );
            accelerate_redrive(client, queue_url, receipt_handle).await;
            return Err(ProcessError::Poison {
                error: classify_parse_error(&error),
                receive_count,
            });
        }
    };

    let result = store_event(pool, &event)
        .await
        .map_err(ProcessError::Storage)?;
    client
        .delete_message()
        .queue_url(queue_url)
        .receipt_handle(receipt_handle)
        .send()
        .await
        .map_err(|error| ProcessError::Delete(anyhow!(error)))?;
    Ok(result)
}

async fn accelerate_redrive(client: &aws_sdk_sqs::Client, queue_url: &str, receipt_handle: &str) {
    if let Err(error) = client
        .change_message_visibility()
        .queue_url(queue_url)
        .receipt_handle(receipt_handle)
        .visibility_timeout(0)
        .send()
        .await
    {
        tracing::warn!(
            target: "agentic_api::ses_outreach_consumer",
            event = "ses_outreach_consumer.poison_visibility_reset_failed",
            error = %error,
            "poison message remains protected by its existing visibility timeout and redrive policy"
        );
    }
}

async fn store_event(pool: &SqlitePool, event: &ParsedEvent) -> Result<MessageStoreResult> {
    let mut result = MessageStoreResult::default();
    let event_type = event_type_label(event.event_type);
    for recipient in &event.recipients {
        let dedupe_material = json!({
            "provider": "ses",
            "provider_message_id": event.provider_message_id,
            "recipient": recipient,
            "event_type": event_type,
            "event_at": event.event_at,
            "bounce_type": bounce_type_label(event.bounce_type),
            "feedback_id": event.feedback_id,
            "url": event.url,
            "subscription_status": subscription_status_label(event.subscription_status),
        });
        let dedupe_key = format!(
            "ses:{}",
            Uuid::new_v5(&Uuid::NAMESPACE_OID, dedupe_material.to_string().as_bytes()).simple()
        );
        let generic = NewEmailDeliveryEvent {
            provider: "ses".to_string(),
            provider_message_id: event.provider_message_id.clone(),
            event_type: event_type.to_ascii_uppercase(),
            event_at: event.event_at,
            recipient: Some(recipient.clone()),
            url: event.url.clone(),
            diagnostic_type: event.diagnostic_type.clone(),
            diagnostic_text: event.diagnostic_text.clone(),
            raw_payload: r#"{"stored_in":"outreach_ses_raw_events"}"#.to_string(),
            dedupe_key,
        };
        let generic_inserted =
            ticketing_system::email_tracking::record_delivery_event(pool, &generic).await?;
        let outreach_inserted = ticketing_system::outreach::record_ses_event(
            pool,
            NewSesEvent {
                provider_message_id: event.provider_message_id.clone(),
                recipient: recipient.clone(),
                event_type: event.event_type,
                bounce_type: event.bounce_type,
                feedback_id: event.feedback_id.clone(),
                url: event.url.clone(),
                subscription_status: event.subscription_status,
                automation_class: event.automation_class,
                event_at: event.event_at,
                received_at: Utc::now().timestamp(),
                raw_payload: event.raw_payload.clone(),
            },
        )
        .await?;
        if generic_inserted || outreach_inserted {
            result.inserted += 1;
            outreach_metrics::record_event(event_type, EventOutcome::Inserted);
        } else {
            result.duplicates += 1;
            outreach_metrics::record_event(event_type, EventOutcome::Duplicate);
        }
    }
    Ok(result)
}

async fn load_queue_config(pool: &SqlitePool) -> Result<QueueConfig> {
    let accounts = ticketing_system::email_accounts::list_email_accounts(pool, true).await?;
    let mut queues = BTreeMap::<String, QueueConfig>::new();
    for account in accounts
        .into_iter()
        .filter(|account| account.outbound_transport == "ses")
    {
        let profile = account
            .aws_profile
            .filter(|value| !value.trim().is_empty())
            .with_context(|| format!("SES account {} has no AWS profile", account.id))?;
        let queue_url = account
            .ses_event_queue_url
            .filter(|value| !value.trim().is_empty())
            .with_context(|| format!("SES account {} has no event queue URL", account.id))?;
        let candidate = QueueConfig {
            profile,
            region: account.aws_region,
            queue_url: queue_url.clone(),
        };
        if let Some(existing) = queues.get(&queue_url) {
            if existing != &candidate {
                bail!("SES accounts disagree on credentials or region for one event queue");
            }
        } else {
            queues.insert(queue_url, candidate);
        }
    }
    if queues.len() != 1 {
        bail!(
            "SES outreach consumer requires exactly one configured event queue; found {}",
            queues.len()
        );
    }
    Ok(queues.into_values().next().expect("length checked"))
}

async fn resolve_dlq_url(client: &aws_sdk_sqs::Client, queue_url: &str) -> Result<String> {
    let response = client
        .get_queue_attributes()
        .queue_url(queue_url)
        .attribute_names(QueueAttributeName::RedrivePolicy)
        .send()
        .await
        .context("failed to inspect SES queue redrive policy")?;
    let policy = response
        .attributes()
        .and_then(|values| values.get(&QueueAttributeName::RedrivePolicy))
        .context("SES event queue has no redrive policy")?;
    dlq_url_from_redrive_policy(queue_url, policy)
}

fn dlq_url_from_redrive_policy(queue_url: &str, redrive_policy: &str) -> Result<String> {
    let value: Value = serde_json::from_str(redrive_policy).context("redrive policy is invalid")?;
    let arn = value
        .get("deadLetterTargetArn")
        .and_then(Value::as_str)
        .context("redrive policy has no deadLetterTargetArn")?;
    let arn_parts: Vec<&str> = arn.split(':').collect();
    if arn_parts.len() != 6 || arn_parts[2] != "sqs" {
        bail!("redrive target is not an SQS ARN");
    }
    let mut url_parts: Vec<&str> = queue_url.trim_end_matches('/').split('/').collect();
    if url_parts.len() < 2 {
        bail!("SES event queue URL is invalid");
    }
    let account = url_parts[url_parts.len() - 2];
    if account != arn_parts[4] {
        bail!("SES event queue and DLQ accounts do not match");
    }
    let last = url_parts.len() - 1;
    url_parts[last] = arn_parts[5];
    Ok(url_parts.join("/"))
}

async fn inspect_queues(
    client: &aws_sdk_sqs::Client,
    queue_url: &str,
    dlq_url: &str,
) -> Result<QueueSnapshot> {
    let main = client
        .get_queue_attributes()
        .queue_url(queue_url)
        .attribute_names(QueueAttributeName::ApproximateNumberOfMessages)
        .attribute_names(QueueAttributeName::ApproximateNumberOfMessagesNotVisible)
        .send()
        .await
        .context("failed to inspect SES event queue depth")?;
    let dlq = client
        .get_queue_attributes()
        .queue_url(dlq_url)
        .attribute_names(QueueAttributeName::ApproximateNumberOfMessages)
        .attribute_names(QueueAttributeName::ApproximateNumberOfMessagesNotVisible)
        .send()
        .await;
    let dlq_visible = match dlq {
        Ok(dlq) => Some(
            queue_attribute(&dlq, QueueAttributeName::ApproximateNumberOfMessages)?.saturating_add(
                queue_attribute(
                    &dlq,
                    QueueAttributeName::ApproximateNumberOfMessagesNotVisible,
                )?,
            ),
        ),
        Err(error) => {
            tracing::warn!(
                target: "agentic_api::ses_outreach_consumer",
                event = "ses_outreach_consumer.dlq_inspection_failed",
                error = %error,
                "failed to inspect SES event DLQ; outreach remains paused while main-queue draining continues"
            );
            None
        }
    };
    Ok(QueueSnapshot {
        visible: queue_attribute(&main, QueueAttributeName::ApproximateNumberOfMessages)?,
        not_visible: queue_attribute(
            &main,
            QueueAttributeName::ApproximateNumberOfMessagesNotVisible,
        )?,
        dlq_visible,
    })
}

fn queue_attribute(
    output: &aws_sdk_sqs::operation::get_queue_attributes::GetQueueAttributesOutput,
    name: QueueAttributeName,
) -> Result<u64> {
    output
        .attributes()
        .and_then(|values| values.get(&name))
        .with_context(|| format!("SQS response omitted {}", name.as_str()))?
        .parse::<u64>()
        .with_context(|| format!("SQS attribute {} is not numeric", name.as_str()))
}

fn message_age_seconds(message: &Message) -> Option<u64> {
    let sent_ms = message
        .attributes()?
        .get(&MessageSystemAttributeName::SentTimestamp)?
        .parse::<i64>()
        .ok()?;
    let now_ms = Utc::now().timestamp_millis();
    Some(now_ms.saturating_sub(sent_ms).max(0) as u64 / 1_000)
}

fn message_receive_count(message: &Message) -> u64 {
    message
        .attributes()
        .and_then(|values| values.get(&MessageSystemAttributeName::ApproximateReceiveCount))
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0)
}

fn parse_ses_event(raw: &str) -> Result<ParsedEvent> {
    let envelope: Value = serde_json::from_str(raw).context("event_body_not_json")?;
    let event = if envelope.get("Type").and_then(Value::as_str) == Some("Notification") {
        let message = envelope
            .get("Message")
            .and_then(Value::as_str)
            .context("sns_notification_missing_message")?;
        serde_json::from_str(message).context("sns_message_not_json")?
    } else {
        envelope
    };
    let mail = event
        .get("mail")
        .and_then(Value::as_object)
        .context("ses_event_missing_mail")?;
    let provider_message_id = mail
        .get("messageId")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .context("ses_event_missing_message_id")?
        .to_string();
    let raw_type = event
        .get("eventType")
        .and_then(Value::as_str)
        .context("ses_event_missing_type")?;
    let (event_type, detail_key) = parse_event_type(raw_type)?;
    let detail = event.get(detail_key).and_then(Value::as_object);
    let timestamp = detail
        .and_then(|item| item.get("timestamp"))
        .and_then(Value::as_str)
        .or_else(|| mail.get("timestamp").and_then(Value::as_str))
        .context("ses_event_missing_timestamp")?;
    let event_at = DateTime::parse_from_rfc3339(timestamp)
        .context("ses_event_invalid_timestamp")?
        .timestamp();
    let recipients = recipients(&event, detail_key);
    if recipients.is_empty() {
        bail!("ses_event_missing_recipient");
    }
    let bounce_type = if event_type == SesEventType::Bounce {
        Some(parse_bounce_type(
            detail
                .and_then(|item| item.get("bounceType"))
                .and_then(Value::as_str)
                .context("bounce_missing_type")?,
        )?)
    } else {
        None
    };
    let feedback_id = detail
        .and_then(|item| item.get("feedbackId"))
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let url = detail
        .and_then(|item| item.get("link"))
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let subscription_status = if event_type == SesEventType::Subscription {
        Some(parse_subscription_status(
            detail.context("subscription_missing_detail")?,
        )?)
    } else {
        None
    };
    let automation_class = classify_automation(event_type, detail);
    let (diagnostic_type, diagnostic_text) = diagnostics(detail);
    Ok(ParsedEvent {
        provider_message_id,
        event_type,
        event_at,
        recipients,
        bounce_type,
        feedback_id,
        url,
        subscription_status,
        automation_class,
        diagnostic_type,
        diagnostic_text,
        raw_payload: raw.to_string(),
    })
}

fn parse_event_type(value: &str) -> Result<(SesEventType, &'static str)> {
    match value
        .trim()
        .to_ascii_lowercase()
        .replace([' ', '_'], "")
        .as_str()
    {
        "send" => Ok((SesEventType::Send, "send")),
        "delivery" => Ok((SesEventType::Delivery, "delivery")),
        "deliverydelay" => Ok((SesEventType::DeliveryDelay, "deliveryDelay")),
        "bounce" => Ok((SesEventType::Bounce, "bounce")),
        "complaint" => Ok((SesEventType::Complaint, "complaint")),
        "reject" => Ok((SesEventType::Reject, "reject")),
        "renderingfailure" => Ok((SesEventType::RenderingFailure, "failure")),
        "open" => Ok((SesEventType::Open, "open")),
        "click" => Ok((SesEventType::Click, "click")),
        "subscription" => Ok((SesEventType::Subscription, "subscription")),
        _ => bail!("ses_event_unknown_type"),
    }
}

fn recipients(value: &Value, detail_key: &str) -> Vec<String> {
    let detail = value.get(detail_key);
    for key in ["recipients", "bouncedRecipients", "complainedRecipients"] {
        if let Some(items) = detail
            .and_then(|item| item.get(key))
            .and_then(Value::as_array)
        {
            let result: Vec<String> = items
                .iter()
                .filter_map(|item| {
                    item.as_str()
                        .or_else(|| item.get("emailAddress").and_then(Value::as_str))
                })
                .map(ToString::to_string)
                .collect();
            if !result.is_empty() {
                return result;
            }
        }
    }
    value
        .get("mail")
        .and_then(|mail| mail.get("destination"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(ToString::to_string)
        .collect()
}

fn parse_bounce_type(value: &str) -> Result<BounceType> {
    match value.trim().to_ascii_lowercase().as_str() {
        "permanent" | "hard" => Ok(BounceType::Hard),
        "transient" | "soft" => Ok(BounceType::Soft),
        "undetermined" => Ok(BounceType::Undetermined),
        _ => bail!("bounce_unknown_type"),
    }
}

fn parse_subscription_status(
    detail: &serde_json::Map<String, Value>,
) -> Result<SubscriptionStatus> {
    let direct = detail
        .get("status")
        .and_then(Value::as_str)
        .or_else(|| detail.get("subscriptionStatus").and_then(Value::as_str));
    let nested = detail
        .get("newTopicPreferences")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(|item| item.get("subscriptionStatus"))
        .and_then(Value::as_str);
    match direct
        .or(nested)
        .context("subscription_missing_status")?
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "subscribed" | "opt_in" | "optin" => Ok(SubscriptionStatus::Subscribed),
        "unsubscribed" | "opt_out" | "optout" => Ok(SubscriptionStatus::Unsubscribed),
        _ => bail!("subscription_unknown_status"),
    }
}

fn classify_automation(
    event_type: SesEventType,
    detail: Option<&serde_json::Map<String, Value>>,
) -> AutomationClass {
    if !matches!(event_type, SesEventType::Open | SesEventType::Click) {
        return AutomationClass::Human;
    }
    let user_agent = detail
        .and_then(|item| item.get("userAgent"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    if user_agent.contains("applewebkit") && user_agent.contains("mail")
        || user_agent.contains("apple mail privacy")
    {
        AutomationClass::PrivacyProxy
    } else if user_agent.contains("googleimageproxy")
        || user_agent.contains("scanner")
        || user_agent.contains("proofpoint")
        || user_agent.contains("mimecast")
    {
        AutomationClass::SuspectedAutomation
    } else if user_agent.is_empty() {
        AutomationClass::Unknown
    } else {
        AutomationClass::Human
    }
}

fn diagnostics(
    detail: Option<&serde_json::Map<String, Value>>,
) -> (Option<String>, Option<String>) {
    let Some(detail) = detail else {
        return (None, None);
    };
    let kind = ["bounceType", "bounceSubType", "delayType", "reason"]
        .iter()
        .find_map(|key| detail.get(*key).and_then(Value::as_str))
        .map(ToString::to_string);
    let text = ["diagnosticCode", "smtpResponse", "errorMessage", "reason"]
        .iter()
        .find_map(|key| detail.get(*key).and_then(Value::as_str))
        .map(ToString::to_string);
    (kind, text)
}

fn event_type_label(event_type: SesEventType) -> &'static str {
    match event_type {
        SesEventType::Send => "send",
        SesEventType::Delivery => "delivery",
        SesEventType::DeliveryDelay => "delivery_delay",
        SesEventType::Bounce => "bounce",
        SesEventType::Complaint => "complaint",
        SesEventType::Reject => "reject",
        SesEventType::RenderingFailure => "rendering_failure",
        SesEventType::Open => "open",
        SesEventType::Click => "click",
        SesEventType::Subscription => "subscription",
    }
}

fn bounce_type_label(value: Option<BounceType>) -> Option<&'static str> {
    value.map(|value| match value {
        BounceType::Hard => "hard",
        BounceType::Soft => "soft",
        BounceType::Undetermined => "undetermined",
    })
}

fn subscription_status_label(value: Option<SubscriptionStatus>) -> Option<&'static str> {
    value.map(|value| match value {
        SubscriptionStatus::Subscribed => "subscribed",
        SubscriptionStatus::Unsubscribed => "unsubscribed",
    })
}

fn classify_parse_error(error: &anyhow::Error) -> &'static str {
    let value = error.to_string();
    if value.contains("not_json") {
        "invalid_json"
    } else if value.contains("unknown_type") {
        "unknown_event_type"
    } else if value.contains("missing_recipient") {
        "missing_recipient"
    } else if value.contains("timestamp") {
        "invalid_timestamp"
    } else {
        "invalid_schema"
    }
}

async fn run_retention(pool: Arc<SqlitePool>, token: CancellationToken) {
    let mut interval = tokio::time::interval(Duration::from_secs(RETENTION_INTERVAL_SECONDS));
    loop {
        tokio::select! {
            _ = token.cancelled() => break,
            _ = interval.tick() => {}
        }
        match ticketing_system::outreach::apply_event_retention(&pool, Utc::now().timestamp()).await
        {
            Ok(result) => {
                outreach_metrics::record_retention("normalized", result.normalized_events_deleted);
                outreach_metrics::record_retention("facets", result.normalized_facets_deleted);
                outreach_metrics::record_retention("raw", result.raw_events_deleted);
                tracing::info!(
                    target: "agentic_api::ses_outreach_consumer",
                    event = "ses_outreach_consumer.retention_complete",
                    normalized_events_deleted = result.normalized_events_deleted,
                    normalized_facets_deleted = result.normalized_facets_deleted,
                    raw_events_deleted = result.raw_events_deleted,
                    "applied SES outreach event retention"
                );
            }
            Err(error) => {
                tracing::error!(
                    target: "agentic_api::ses_outreach_consumer",
                    event = "ses_outreach_consumer.retention_failed",
                    error = %error,
                    "failed to apply SES outreach event retention"
                );
                system_log_helper::log_error(
                    &pool,
                    COMPONENT,
                    "SES outreach event retention failed",
                    Some(r#"{"status":"retention_failed"}"#),
                )
                .await;
            }
        }
    }
}

async fn wait_or_cancel(token: &CancellationToken, seconds: u64) -> bool {
    tokio::select! {
        _ = token.cancelled() => true,
        _ = tokio::time::sleep(Duration::from_secs(seconds)) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE emails (id INTEGER PRIMARY KEY)")
            .execute(&pool)
            .await
            .unwrap();
        ticketing_system::email_tracking::ensure_schema(&pool)
            .await
            .unwrap();
        ticketing_system::outreach::ensure_schema(&pool)
            .await
            .unwrap();
        pool
    }

    #[test]
    fn parses_multi_recipient_permanent_bounce_without_logging_content() {
        let parsed = parse_ses_event(
            r#"{"eventType":"Bounce","mail":{"timestamp":"2026-07-13T00:00:00Z","messageId":"provider-1","destination":["first@example.com","second@example.com"]},"bounce":{"timestamp":"2026-07-13T00:00:01Z","bounceType":"Permanent","feedbackId":"feedback-1","bouncedRecipients":[{"emailAddress":"first@example.com"},{"emailAddress":"second@example.com"}]}}"#,
        )
        .unwrap();
        assert_eq!(parsed.event_type, SesEventType::Bounce);
        assert_eq!(parsed.bounce_type, Some(BounceType::Hard));
        assert_eq!(parsed.recipients.len(), 2);
        assert_eq!(parsed.feedback_id.as_deref(), Some("feedback-1"));
    }

    #[test]
    fn parses_sns_wrapped_delivery_and_subscription_status() {
        let message = r#"{"eventType":"Subscription","mail":{"timestamp":"2026-07-13T00:00:00Z","messageId":"provider-2","destination":["person@example.com"]},"subscription":{"timestamp":"2026-07-13T00:00:01Z","newTopicPreferences":[{"topicName":"commercial-outreach","subscriptionStatus":"OPT_OUT"}]}}"#;
        let wrapped = json!({"Type":"Notification","Message":message}).to_string();
        let parsed = parse_ses_event(&wrapped).unwrap();
        assert_eq!(parsed.event_type, SesEventType::Subscription);
        assert_eq!(
            parsed.subscription_status,
            Some(SubscriptionStatus::Unsubscribed)
        );
    }

    #[test]
    fn rejects_unknown_events_as_poison() {
        let error = parse_ses_event(
            r#"{"eventType":"FutureType","mail":{"timestamp":"2026-07-13T00:00:00Z","messageId":"provider-3","destination":["person@example.com"]}}"#,
        )
        .unwrap_err();
        assert_eq!(classify_parse_error(&error), "unknown_event_type");
    }

    #[test]
    fn derives_dlq_url_from_policy_without_hardcoding_a_queue_name() {
        let policy = r#"{"deadLetterTargetArn":"arn:aws:sqs:us-east-1:445567085905:events-dead-letter","maxReceiveCount":5}"#;
        assert_eq!(
            dlq_url_from_redrive_policy(
                "https://sqs.us-east-1.amazonaws.com/445567085905/events",
                policy
            )
            .unwrap(),
            "https://sqs.us-east-1.amazonaws.com/445567085905/events-dead-letter"
        );
    }

    #[tokio::test]
    async fn stores_duplicate_and_out_of_order_events_idempotently() {
        crate::observability::install_for_test();
        let pool = pool().await;
        let delivery = parse_ses_event(
            r#"{"eventType":"Delivery","mail":{"timestamp":"2026-07-13T00:00:00Z","messageId":"provider-4","destination":["person@example.com"]},"delivery":{"timestamp":"2026-07-13T00:00:03Z","recipients":["person@example.com"]}}"#,
        )
        .unwrap();
        let bounce = parse_ses_event(
            r#"{"eventType":"Bounce","mail":{"timestamp":"2026-07-13T00:00:00Z","messageId":"provider-4","destination":["person@example.com"]},"bounce":{"timestamp":"2026-07-13T00:00:02Z","bounceType":"Permanent","feedbackId":"feedback-4","bouncedRecipients":[{"emailAddress":"person@example.com"}]}}"#,
        )
        .unwrap();

        let first = store_event(&pool, &delivery).await.unwrap();
        let duplicate = store_event(&pool, &delivery).await.unwrap();
        let older_bounce = store_event(&pool, &bounce).await.unwrap();
        assert_eq!(first.inserted, 1);
        assert_eq!(duplicate.duplicates, 1);
        assert_eq!(older_bounce.inserted, 1);

        let suppression: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM outreach_suppressions WHERE reason = 'hard_bounce' AND status = 'active'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(suppression, 1);
        let generic_events: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM email_delivery_events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(generic_events, 2);
    }
}
