//! Controlled, privacy-safe reconciliation for the configured SES event DLQ.
//!
//! The command never prints or persists queue bodies or message-attribute
//! values. Destructive acknowledgement requires an exact expected batch count,
//! an explicit ticket ID, and unanimous classification as a known SES
//! event-destination validation control message.

use std::collections::BTreeMap;

use agentic_api::ses_event_quarantine::{self, BodyClassification, QuarantineEvidence};
use anyhow::{bail, Context, Result};
use aws_sdk_sqs::types::{MessageSystemAttributeName, QueueAttributeName};
use serde_json::json;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Inspect,
    AcknowledgeKnownControl,
}

#[derive(Debug)]
struct Args {
    action: Action,
    expected_count: Option<usize>,
    ticket_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QueueConfig {
    profile: String,
    region: String,
    queue_url: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = parse_args()?;
    let pool = ticketing_system::db::init_db().await?;
    ses_event_quarantine::ensure_schema(&pool).await?;
    let config = load_queue_config(&pool).await?;
    let aws = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .profile_name(&config.profile)
        .region(aws_config::Region::new(config.region.clone()))
        .load()
        .await;
    let client = aws_sdk_sqs::Client::new(&aws);
    let dlq_url = resolve_dlq_url(&client, &config.queue_url).await?;
    let messages = receive_quarantine_batch(&client, &dlq_url, args.expected_count).await?;

    let mut evidence = Vec::with_capacity(messages.len());
    for message in &messages {
        let classification = message
            .body()
            .map(ses_event_quarantine::classify_non_json_body)
            .unwrap_or(BodyClassification::MissingBody);
        let parse_error = if message.body().is_none() {
            "missing_body"
        } else {
            "invalid_json"
        };
        let item = QuarantineEvidence::from_message(message, classification, parse_error);
        ses_event_quarantine::record(&pool, &item, "dlq_quarantined", args.ticket_id.as_deref())
            .await?;
        evidence.push(item);
    }

    let known_control_count = evidence
        .iter()
        .filter(|item| item.classification.is_acknowledgeable_control())
        .count();
    let unknown_count = evidence.len().saturating_sub(known_control_count);

    match args.action {
        Action::Inspect => {
            for (message, item) in messages.iter().zip(&evidence) {
                ses_event_quarantine::log_diagnostic(
                    &pool,
                    item,
                    "info",
                    "SES DLQ message inspected and retained in quarantine",
                    "dlq_quarantined",
                )
                .await?;
                reset_visibility(&client, &dlq_url, message.receipt_handle()).await?;
            }
        }
        Action::AcknowledgeKnownControl => {
            let expected_count = args
                .expected_count
                .context("--expected-count is required for acknowledgement")?;
            let ticket_id = args
                .ticket_id
                .as_deref()
                .context("--ticket-id is required for acknowledgement")?;
            if messages.len() != expected_count || unknown_count != 0 {
                for message in &messages {
                    reset_visibility(&client, &dlq_url, message.receipt_handle()).await?;
                }
                bail!(
                    "refusing acknowledgement: expected {expected_count} known controls, received {} messages with {unknown_count} unknown",
                    evidence.len()
                );
            }
            for (message, item) in messages.iter().zip(&evidence) {
                let receipt_handle = message
                    .receipt_handle()
                    .context("DLQ message omitted receipt handle")?;
                client
                    .delete_message()
                    .queue_url(&dlq_url)
                    .receipt_handle(receipt_handle)
                    .send()
                    .await
                    .context("failed to acknowledge known SES validation control message")?;
                ses_event_quarantine::record(&pool, item, "acknowledged_control", Some(ticket_id))
                    .await?;
                ses_event_quarantine::log_diagnostic(
                    &pool,
                    item,
                    "info",
                    "Known SES event-destination validation control message acknowledged from DLQ",
                    "acknowledged_control",
                )
                .await?;
            }
        }
    }

    println!(
        "{}",
        json!({
            "action": match args.action {
                Action::Inspect => "inspect",
                Action::AcknowledgeKnownControl => "acknowledge_known_control",
            },
            "messages_observed": evidence.len(),
            "known_ses_validation_controls": known_control_count,
            "unknown_messages": unknown_count,
            "bodies_output": false,
            "message_attribute_values_output": false,
        })
    );
    Ok(())
}

fn parse_args() -> Result<Args> {
    let mut action = None;
    let mut expected_count = None;
    let mut ticket_id = None;
    let mut values = std::env::args().skip(1);
    while let Some(flag) = values.next() {
        match flag.as_str() {
            "--action" => {
                let value = values.next().context("--action requires a value")?;
                action = Some(match value.as_str() {
                    "inspect" => Action::Inspect,
                    "acknowledge-known-control" => Action::AcknowledgeKnownControl,
                    _ => bail!("unsupported --action value"),
                });
            }
            "--expected-count" => {
                expected_count = Some(
                    values
                        .next()
                        .context("--expected-count requires a value")?
                        .parse()
                        .context("--expected-count must be an integer")?,
                );
            }
            "--ticket-id" => {
                let value = values.next().context("--ticket-id requires a value")?;
                if !value.starts_with("T-") {
                    bail!("--ticket-id must be a ticket ID");
                }
                ticket_id = Some(value);
            }
            _ => bail!("unsupported argument"),
        }
    }
    Ok(Args {
        action: action.context("--action is required")?,
        expected_count,
        ticket_id,
    })
}

async fn load_queue_config(pool: &ticketing_system::SqlitePool) -> Result<QueueConfig> {
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
            "SES quarantine command requires exactly one configured event queue; found {}",
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
    let value: serde_json::Value =
        serde_json::from_str(policy).context("redrive policy is invalid")?;
    let arn = value
        .get("deadLetterTargetArn")
        .and_then(serde_json::Value::as_str)
        .context("redrive policy has no deadLetterTargetArn")?;
    let arn_parts: Vec<&str> = arn.split(':').collect();
    if arn_parts.len() != 6 || arn_parts[2] != "sqs" {
        bail!("redrive target is not an SQS ARN");
    }
    let mut url_parts: Vec<&str> = queue_url.trim_end_matches('/').split('/').collect();
    if url_parts.len() < 2 || url_parts[url_parts.len() - 2] != arn_parts[4] {
        bail!("SES queue and DLQ identifiers do not share an account");
    }
    let last = url_parts.len() - 1;
    url_parts[last] = arn_parts[5];
    Ok(url_parts.join("/"))
}

async fn receive_quarantine_batch(
    client: &aws_sdk_sqs::Client,
    queue_url: &str,
    expected_count: Option<usize>,
) -> Result<Vec<aws_sdk_sqs::types::Message>> {
    let target = expected_count.unwrap_or(10).min(10);
    let mut messages = Vec::with_capacity(target);
    loop {
        let remaining = target.saturating_sub(messages.len());
        if remaining == 0 {
            break;
        }
        let response = client
            .receive_message()
            .queue_url(queue_url)
            .max_number_of_messages(i32::try_from(remaining).unwrap_or(10))
            .wait_time_seconds(10)
            .visibility_timeout(120)
            .message_system_attribute_names(MessageSystemAttributeName::SentTimestamp)
            .message_system_attribute_names(
                MessageSystemAttributeName::ApproximateFirstReceiveTimestamp,
            )
            .message_system_attribute_names(MessageSystemAttributeName::ApproximateReceiveCount)
            .message_system_attribute_names(MessageSystemAttributeName::SenderId)
            .message_attribute_names("All")
            .send()
            .await
            .context("failed to receive privacy-safe SES DLQ diagnostic batch")?;
        if response.messages().is_empty() {
            break;
        }
        messages.extend(response.messages().iter().cloned());
        if expected_count.is_none() {
            break;
        }
    }
    Ok(messages)
}

async fn reset_visibility(
    client: &aws_sdk_sqs::Client,
    queue_url: &str,
    receipt_handle: Option<&str>,
) -> Result<()> {
    let receipt_handle = receipt_handle.context("DLQ message omitted receipt handle")?;
    client
        .change_message_visibility()
        .queue_url(queue_url)
        .receipt_handle(receipt_handle)
        .visibility_timeout(0)
        .send()
        .await
        .context("failed to release inspected DLQ message visibility")?;
    Ok(())
}
