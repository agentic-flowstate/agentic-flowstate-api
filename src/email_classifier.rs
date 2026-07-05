use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{FromRow, SqlitePool};
use ticketing_system::email_intake::{EmailIntakeResult, EmailSecurityScan};
use ticketing_system::text_normalization::normalize_codex_token_delta_output;
use tokio_util::sync::CancellationToken;

use crate::agents::codex_app_server::{
    read_codex_account_rate_limits, spawn_codex_app_server, CodexAccountRateLimits,
    CodexAppServerEvent, CodexAppServerOptions, CodexRateLimitSnapshot, CodexSandboxMode,
    CodexToolProfile,
};
use crate::agents::prompts::load_prompt;
use crate::agents::AgentType;

const CLASSIFIER_CREATED_BY: &str = "email_classifier";
const CLASSIFIER_JOB_POLL_SECONDS: u64 = 60;
const QUOTA_RECHECK_SECONDS: i64 = 5 * 60;
const FIVE_HOUR_WINDOW_MINS: i64 = 300;
const WEEKLY_WINDOW_MINS: i64 = 10080;
const FIVE_HOUR_DEFER_THRESHOLD_PERCENT: i32 = 90;
const WEEKLY_DEFER_THRESHOLD_PERCENT: i32 = 95;
const LABEL_CONFIDENCE_THRESHOLD: f64 = 0.50;
const NOTIFICATION_CONFIDENCE_THRESHOLD: f64 = 0.80;

const ALLOWED_CLASSIFIER_LABELS: &[&str] = &[
    "safety/clean",
    "safety/low",
    "safety/medium",
    "safety/high",
    "safety/critical",
    "mail/spam_suspected",
    "mail/marketing",
    "mail/newsletter",
    "mail/transactional",
    "mail/auth_code",
    "mail/personal",
    "workflow/reply_needed",
    "workflow/waiting_on_sender",
    "workflow/follow_up",
    "priority/urgent",
    "project/laminarforge",
    "notify/eligible",
    "notify/suppressed",
    "notify/security_only",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailClassifierEnqueueOutcome {
    pub email_id: i64,
    pub enqueued: bool,
    pub status: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailClassifierSafetyDecision {
    pub safe: bool,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexQuotaGateDecision {
    pub allowed: bool,
    pub reason: String,
    pub defer_until: Option<i64>,
    pub five_hour_used_percent: Option<i32>,
    pub five_hour_resets_at: Option<i64>,
    pub weekly_used_percent: Option<i32>,
    pub weekly_resets_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmailClassifierSchedulerOutcome {
    NoReadyJobs,
    Deferred { reason: String },
    Completed { job_id: String, email_id: i64 },
    Skipped { job_id: String, email_id: i64 },
    Failed { job_id: String, email_id: i64 },
}

#[derive(Debug, Clone, FromRow)]
struct EmailClassifierJobRow {
    job_id: String,
    email_id: i64,
}

#[derive(Debug, Clone, FromRow)]
struct EmailClassifierMetadataRow {
    id: i64,
    message_id: String,
    mailbox: String,
    folder: String,
    from_address: String,
    from_name: Option<String>,
    to_addresses: String,
    cc_addresses: Option<String>,
    subject: Option<String>,
    received_at: i64,
    thread_id: Option<String>,
    in_reply_to: Option<String>,
    labels: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct EmailClassifierMetadata {
    id: i64,
    message_id: String,
    mailbox: String,
    folder: String,
    from_address: String,
    from_name: Option<String>,
    to_addresses: Vec<String>,
    cc_addresses: Vec<String>,
    subject: Option<String>,
    received_at: i64,
    thread_id: Option<String>,
    in_reply_to: Option<String>,
    labels: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ClassifierVerdict {
    schema_version: Option<String>,
    #[serde(default)]
    labels: Vec<ClassifierLabel>,
    attention: Option<ClassifierAttention>,
    notification: Option<ClassifierNotification>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ClassifierLabel {
    label: String,
    confidence: f64,
    rationale: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ClassifierAttention {
    #[serde(default)]
    create: bool,
    priority: Option<String>,
    reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ClassifierNotification {
    intent: Option<String>,
    reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct AppliedClassifierActions {
    labels: Vec<String>,
    attention_created: bool,
    notification_intent: StoredNotificationIntent,
}

#[derive(Debug, Clone, Serialize)]
struct StoredNotificationIntent {
    intent: String,
    reason: String,
    payload: Value,
}

#[derive(Debug, Clone)]
struct ClassifierTurnOutput {
    text: String,
    tool_call_count: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmailClassifierJobDisposition {
    Completed,
    Skipped,
}

pub fn spawn_email_classifier_scheduler(pool: Arc<SqlitePool>, token: CancellationToken) {
    tokio::spawn(async move {
        tracing::info!(
            target: "agentic_api::email_classifier",
            event = "email_classifier.scheduler_starting",
            "email classifier scheduler starting"
        );

        if let Err(e) = ensure_email_classifier_schema(&pool).await {
            tracing::error!(
                target: "agentic_api::email_classifier",
                event = "email_classifier.schema_failed",
                error = ?e,
                "email classifier schema bootstrap failed"
            );
            return;
        }

        let mut interval = tokio::time::interval(Duration::from_secs(CLASSIFIER_JOB_POLL_SECONDS));
        loop {
            tokio::select! {
                _ = token.cancelled() => {
                    tracing::info!(
                        target: "agentic_api::email_classifier",
                        event = "email_classifier.scheduler_stopping",
                        "email classifier scheduler stopping"
                    );
                    break;
                }
                _ = interval.tick() => {
                    match run_email_classifier_scheduler_once(&pool).await {
                        Ok(EmailClassifierSchedulerOutcome::NoReadyJobs) => {}
                        Ok(outcome) => tracing::info!(
                            target: "agentic_api::email_classifier",
                            event = "email_classifier.scheduler_tick",
                            outcome = ?outcome,
                            "email classifier scheduler tick completed"
                        ),
                        Err(e) => tracing::warn!(
                            target: "agentic_api::email_classifier",
                            event = "email_classifier.scheduler_tick_failed",
                            error = ?e,
                            "email classifier scheduler tick failed"
                        ),
                    }
                }
            }
        }
    });
}

pub async fn enqueue_classifier_after_intake(
    pool: &SqlitePool,
    result: &EmailIntakeResult,
    created_by: &str,
) -> Result<EmailClassifierEnqueueOutcome> {
    ensure_email_classifier_schema(pool).await?;
    let metadata = get_email_classifier_metadata(pool, result.email_id).await?;
    let safety = classifier_safety_decision(&metadata.folder, &result.security_scan);
    if !safety.safe {
        return Ok(EmailClassifierEnqueueOutcome {
            email_id: result.email_id,
            enqueued: false,
            status: "skipped".to_string(),
            reason: safety.reason,
        });
    }

    let now = Utc::now().timestamp();
    let job_id = format!("email-classifier-{}", uuid::Uuid::new_v4());
    sqlx::query(
        r#"
        INSERT INTO email_classifier_jobs
            (job_id, email_id, scan_id, status, attempts, created_by, created_at, updated_at)
        VALUES (?, ?, ?, 'pending', 0, ?, ?, ?)
        ON CONFLICT(email_id) DO UPDATE SET
            scan_id = excluded.scan_id,
            status = CASE
                WHEN email_classifier_jobs.status IN ('failed', 'deferred') THEN 'pending'
                ELSE email_classifier_jobs.status
            END,
            defer_reason = CASE
                WHEN email_classifier_jobs.status IN ('failed', 'deferred') THEN NULL
                ELSE email_classifier_jobs.defer_reason
            END,
            deferred_until = CASE
                WHEN email_classifier_jobs.status IN ('failed', 'deferred') THEN NULL
                ELSE email_classifier_jobs.deferred_until
            END,
            updated_at = excluded.updated_at
        "#,
    )
    .bind(&job_id)
    .bind(result.email_id)
    .bind(result.security_scan.id)
    .bind(created_by)
    .bind(now)
    .bind(now)
    .execute(pool)
    .await
    .context("Failed to enqueue email classifier job")?;

    let status: String =
        sqlx::query_scalar("SELECT status FROM email_classifier_jobs WHERE email_id = ?")
            .bind(result.email_id)
            .fetch_one(pool)
            .await
            .context("Failed to read email classifier job status")?;

    Ok(EmailClassifierEnqueueOutcome {
        email_id: result.email_id,
        enqueued: status == "pending",
        status,
        reason: "safe_enough_for_classifier".to_string(),
    })
}

pub fn classifier_safety_decision(
    folder: &str,
    scan: &EmailSecurityScan,
) -> EmailClassifierSafetyDecision {
    if !folder.eq_ignore_ascii_case("INBOX") {
        return skipped("non_inbox_folder");
    }
    if !matches!(scan.risk_level.as_str(), "clean" | "low") {
        return skipped("scan_risk_not_safe_enough");
    }
    if scan.status == "flagged" {
        return skipped("scan_flagged");
    }
    if scan.has_prompt_injection {
        return skipped("prompt_injection_detected");
    }
    if scan.has_secret_request {
        return skipped("secret_request_detected");
    }
    if scan.has_suspicious_attachments {
        return skipped("suspicious_attachment_detected");
    }
    if scan.has_hidden_content {
        return skipped("hidden_content_detected");
    }

    EmailClassifierSafetyDecision {
        safe: true,
        reason: "safe_enough_for_classifier".to_string(),
    }
}

fn skipped(reason: &str) -> EmailClassifierSafetyDecision {
    EmailClassifierSafetyDecision {
        safe: false,
        reason: reason.to_string(),
    }
}

pub async fn run_email_classifier_scheduler_once(
    pool: &SqlitePool,
) -> Result<EmailClassifierSchedulerOutcome> {
    ensure_email_classifier_schema(pool).await?;
    if !has_ready_classifier_job(pool).await? {
        return Ok(EmailClassifierSchedulerOutcome::NoReadyJobs);
    }

    let quota = read_and_persist_quota_gate(pool).await?;
    if !quota.allowed {
        defer_ready_classifier_jobs(pool, &quota).await?;
        return Ok(EmailClassifierSchedulerOutcome::Deferred {
            reason: quota.reason,
        });
    }

    let Some(job) = claim_next_ready_classifier_job(pool).await? else {
        return Ok(EmailClassifierSchedulerOutcome::NoReadyJobs);
    };

    let result = run_email_classifier_job(pool, &job).await;
    match result {
        Ok(EmailClassifierJobDisposition::Completed) => {
            mark_classifier_job_completed(pool, &job.job_id).await?;
            Ok(EmailClassifierSchedulerOutcome::Completed {
                job_id: job.job_id,
                email_id: job.email_id,
            })
        }
        Ok(EmailClassifierJobDisposition::Skipped) => {
            Ok(EmailClassifierSchedulerOutcome::Skipped {
                job_id: job.job_id,
                email_id: job.email_id,
            })
        }
        Err(e) => {
            let error = e.to_string();
            mark_classifier_job_failed(pool, &job.job_id, &error).await?;
            tracing::warn!(
                target: "agentic_api::email_classifier",
                event = "email_classifier.job_failed",
                job_id = %job.job_id,
                email_id = job.email_id,
                error = %error,
                "email classifier job failed"
            );
            Ok(EmailClassifierSchedulerOutcome::Failed {
                job_id: job.job_id,
                email_id: job.email_id,
            })
        }
    }
}

async fn run_email_classifier_job(
    pool: &SqlitePool,
    job: &EmailClassifierJobRow,
) -> Result<EmailClassifierJobDisposition> {
    let metadata = get_email_classifier_metadata(pool, job.email_id).await?;
    let scan = ticketing_system::email_intake::get_security_scan_by_email_id(pool, job.email_id)
        .await
        .context("Failed to load classifier email security scan")?;
    let safety = classifier_safety_decision(&metadata.folder, &scan);
    if !safety.safe {
        mark_classifier_job_skipped(pool, &job.job_id, &safety.reason).await?;
        return Ok(EmailClassifierJobDisposition::Skipped);
    }

    let agent_type = AgentType::EmailClassifier;
    let system_prompt = load_prompt(agent_type.as_str(), HashMap::new())
        .context("Failed to load classifier prompt")?;
    let prompt = build_classifier_job_prompt(&metadata, &scan)?;
    let output = run_scoped_classifier_turn(&agent_type, &system_prompt, &prompt, job.email_id)
        .await
        .context("Email classifier Codex turn failed")?;
    let verdict = parse_classifier_verdict(&output.text)?;
    let applied = apply_classifier_verdict(pool, &metadata, &scan, &verdict).await?;
    persist_classifier_job_output(pool, job, &output, &verdict, &applied).await?;
    Ok(EmailClassifierJobDisposition::Completed)
}

async fn run_scoped_classifier_turn(
    agent_type: &AgentType,
    system_prompt: &str,
    prompt: &str,
    email_id: i64,
) -> Result<ClassifierTurnOutput> {
    let mut turn = spawn_codex_app_server(CodexAppServerOptions {
        model: agent_type.model(),
        reasoning_effort: agent_type.effort(),
        system_prompt,
        working_dir: Path::new(env!("CARGO_MANIFEST_DIR")),
        prompt,
        sandbox: CodexSandboxMode::ReadOnly,
        bypass_approvals_and_sandbox: false,
        resume_session_id: None,
        ephemeral: true,
        tool_profile: CodexToolProfile::ConfiguredMcpOnly,
        scoped_user_id: None,
        current_conversation_id: None,
        scoped_email_id: Some(email_id),
        approved_mcp_tools: agent_type.approved_mcp_tool_names(),
    })
    .await
    .map_err(anyhow::Error::msg)?;

    let mut message_order: Vec<String> = Vec::new();
    let mut agent_messages: HashMap<String, String> = HashMap::new();
    let mut tool_call_count = 0;

    while let Some(event) = turn.events.recv().await {
        match event {
            CodexAppServerEvent::AgentMessageDelta { id, text } => {
                if text.is_empty() {
                    continue;
                }
                if !agent_messages.contains_key(&id) {
                    message_order.push(id.clone());
                    agent_messages.insert(id.clone(), String::new());
                }
                if let Some(message) = agent_messages.get_mut(&id) {
                    message.push_str(&text);
                }
            }
            CodexAppServerEvent::AgentMessageCompleted { id, text } => {
                if text.is_empty() {
                    continue;
                }
                if !agent_messages.contains_key(&id) {
                    message_order.push(id.clone());
                }
                agent_messages.insert(id, text);
            }
            CodexAppServerEvent::ToolCallStarted { .. } => {
                tool_call_count += 1;
            }
            CodexAppServerEvent::ThreadStarted { .. }
            | CodexAppServerEvent::ReasoningDelta { .. }
            | CodexAppServerEvent::ToolCallCompleted { .. }
            | CodexAppServerEvent::TurnCompleted { .. } => {}
        }
    }

    let outcome = turn.wait().await.map_err(anyhow::Error::msg)?;
    if !outcome.success() {
        return Err(anyhow!(
            outcome.failure_summary("email classifier codex app-server")
        ));
    }

    let text = message_order
        .iter()
        .rev()
        .find_map(|id| agent_messages.get(id))
        .map(|text| normalize_codex_token_delta_output(text))
        .ok_or_else(|| anyhow!("No output from email classifier"))?;

    Ok(ClassifierTurnOutput {
        text,
        tool_call_count,
    })
}

fn build_classifier_job_prompt(
    metadata: &EmailClassifierMetadata,
    scan: &EmailSecurityScan,
) -> Result<String> {
    let mut vars = HashMap::new();
    vars.insert("email_id".to_string(), metadata.id.to_string());
    vars.insert(
        "allowed_labels_json".to_string(),
        serde_json::to_string_pretty(&ALLOWED_CLASSIFIER_LABELS)
            .context("Failed to encode allowed classifier labels")?,
    );
    vars.insert(
        "metadata_json".to_string(),
        serde_json::to_string_pretty(metadata).context("Failed to encode classifier metadata")?,
    );
    vars.insert(
        "scan_summary_json".to_string(),
        serde_json::to_string_pretty(&scan_summary_json(scan))
            .context("Failed to encode classifier scan summary")?,
    );
    load_prompt("email-classifier-job", vars).context("Failed to load classifier job prompt")
}

fn scan_summary_json(scan: &EmailSecurityScan) -> Value {
    json!({
        "scan_id": scan.id,
        "scanner_version": scan.scanner_version,
        "risk_score": scan.risk_score,
        "risk_level": &scan.risk_level,
        "flags": &scan.flags,
        "reasons": &scan.reasons,
        "extracted_url_count": scan.extracted_urls.len(),
        "suspicious_url_count": scan.suspicious_urls.len(),
        "attachment_count": scan.attachment_count,
        "suspicious_attachment_count": scan.suspicious_attachment_count,
        "has_prompt_injection": scan.has_prompt_injection,
        "has_secret_request": scan.has_secret_request,
        "has_suspicious_links": scan.has_suspicious_links,
        "has_suspicious_attachments": scan.has_suspicious_attachments,
        "has_hidden_content": scan.has_hidden_content,
        "status": &scan.status,
        "updated_at": scan.updated_at,
    })
}

fn parse_classifier_verdict(output: &str) -> Result<ClassifierVerdict> {
    let json_text = extract_json_object(output).context("Email classifier did not return JSON")?;
    let verdict: ClassifierVerdict =
        serde_json::from_str(json_text).context("Failed to parse email classifier JSON")?;
    if verdict
        .schema_version
        .as_deref()
        .is_some_and(|version| version != "email_classifier_v1")
    {
        return Err(anyhow!("Unsupported email classifier schema_version"));
    }
    Ok(verdict)
}

fn extract_json_object(output: &str) -> Option<&str> {
    let trimmed = output.trim();
    let without_fence = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|rest| rest.strip_suffix("```"))
        .map(str::trim)
        .unwrap_or(trimmed);

    let start = without_fence.find('{')?;
    let end = without_fence.rfind('}')?;
    (start <= end).then(|| &without_fence[start..=end])
}

async fn apply_classifier_verdict(
    pool: &SqlitePool,
    metadata: &EmailClassifierMetadata,
    scan: &EmailSecurityScan,
    verdict: &ClassifierVerdict,
) -> Result<AppliedClassifierActions> {
    let labels = sanitize_classifier_labels(&verdict.labels);
    update_email_labels(pool, metadata.id, &labels).await?;
    let attention_created = create_classifier_attention_if_needed(pool, metadata, &labels).await?;
    let notification_intent = derive_notification_intent(metadata, scan, verdict, &labels);
    persist_notification_intent(pool, metadata, &notification_intent).await?;

    Ok(AppliedClassifierActions {
        labels,
        attention_created,
        notification_intent,
    })
}

fn sanitize_classifier_labels(labels: &[ClassifierLabel]) -> Vec<String> {
    let allowed: HashSet<&str> = ALLOWED_CLASSIFIER_LABELS.iter().copied().collect();
    let mut sanitized = labels
        .iter()
        .filter(|label| label.confidence >= LABEL_CONFIDENCE_THRESHOLD)
        .filter_map(|label| {
            let label = label.label.trim();
            allowed.contains(label).then(|| label.to_string())
        })
        .collect::<Vec<_>>();
    sanitized.sort();
    sanitized.dedup();
    sanitized
}

async fn update_email_labels(pool: &SqlitePool, email_id: i64, labels: &[String]) -> Result<()> {
    if labels.is_empty() {
        return Ok(());
    }

    let current_raw: Option<String> = sqlx::query_scalar("SELECT labels FROM emails WHERE id = ?")
        .bind(email_id)
        .fetch_one(pool)
        .await
        .context("Failed to load email labels")?;
    let mut current = parse_json_string_vec(current_raw.as_deref());
    let mut seen: HashSet<String> = current.iter().cloned().collect();
    for label in labels {
        if seen.insert(label.clone()) {
            current.push(label.clone());
        }
    }
    let labels_json = serde_json::to_string(&current).context("Failed to encode email labels")?;
    sqlx::query("UPDATE emails SET labels = ?, updated_at = ? WHERE id = ?")
        .bind(labels_json)
        .bind(Utc::now().timestamp())
        .bind(email_id)
        .execute(pool)
        .await
        .context("Failed to update email labels")?;
    Ok(())
}

async fn create_classifier_attention_if_needed(
    pool: &SqlitePool,
    metadata: &EmailClassifierMetadata,
    labels: &[String],
) -> Result<bool> {
    let needs_attention = labels.iter().any(|label| {
        matches!(
            label.as_str(),
            "workflow/reply_needed" | "workflow/follow_up" | "priority/urgent"
        )
    });
    if !needs_attention {
        return Ok(false);
    }

    let exists: Option<i64> = sqlx::query_scalar(
        r#"
        SELECT id
        FROM email_attention_items
        WHERE email_id = ?
          AND item_type = 'classifier_review'
          AND status = 'open'
        LIMIT 1
        "#,
    )
    .bind(metadata.id)
    .fetch_optional(pool)
    .await
    .context("Failed to check classifier attention item")?;
    if exists.is_some() {
        return Ok(false);
    }

    let context_id: Option<String> = sqlx::query_scalar(
        r#"
        SELECT context_id
        FROM email_context_emails
        WHERE email_id = ?
        ORDER BY id DESC
        LIMIT 1
        "#,
    )
    .bind(metadata.id)
    .fetch_optional(pool)
    .await
    .context("Failed to load classifier attention context")?;
    let now = Utc::now().timestamp();
    let priority = if labels.iter().any(|label| label == "priority/urgent") {
        "high"
    } else {
        "normal"
    };
    let title = format!(
        "Classifier review: {}",
        compact_one_line(
            metadata
                .subject
                .as_deref()
                .unwrap_or(&metadata.from_address),
            140,
        )
    );
    let detail = format!("Classifier labels: {}", labels.join(", "));

    sqlx::query(
        r#"
        INSERT INTO email_attention_items
            (email_id, context_id, expected_response_id, mailbox, item_type, priority,
             status, title, detail, risk_level, created_by, created_at, updated_at)
        VALUES (?, ?, NULL, ?, 'classifier_review', ?, 'open', ?, ?, NULL, ?, ?, ?)
        "#,
    )
    .bind(metadata.id)
    .bind(context_id.as_deref())
    .bind(&metadata.mailbox)
    .bind(priority)
    .bind(title)
    .bind(detail)
    .bind(CLASSIFIER_CREATED_BY)
    .bind(now)
    .bind(now)
    .execute(pool)
    .await
    .context("Failed to create classifier attention item")?;

    Ok(true)
}

fn derive_notification_intent(
    metadata: &EmailClassifierMetadata,
    scan: &EmailSecurityScan,
    verdict: &ClassifierVerdict,
    labels: &[String],
) -> StoredNotificationIntent {
    let requested_intent = verdict
        .notification
        .as_ref()
        .and_then(|notification| notification.intent.as_deref())
        .unwrap_or("suppressed");
    let eligible_confidence = label_confidence(&verdict.labels, "notify/eligible");
    let clean_or_low = matches!(scan.risk_level.as_str(), "clean" | "low");

    let (intent, reason) = if labels.iter().any(|label| label == "notify/security_only") {
        (
            "security_only".to_string(),
            "classifier_requested_security_only_notification".to_string(),
        )
    } else if requested_intent == "eligible"
        && clean_or_low
        && eligible_confidence >= NOTIFICATION_CONFIDENCE_THRESHOLD
        && labels.iter().any(|label| label == "notify/eligible")
    {
        (
            "eligible".to_string(),
            "high_confidence_safe_enough_email_notification".to_string(),
        )
    } else {
        (
            "suppressed".to_string(),
            "notification_not_high_confidence_or_not_requested".to_string(),
        )
    };

    let payload = json!({
        "type": "email",
        "email_id": metadata.id,
        "mailbox": &metadata.mailbox,
        "thread_id": &metadata.thread_id,
        "risk_level": &scan.risk_level,
        "labels": labels,
    });

    StoredNotificationIntent {
        intent,
        reason,
        payload,
    }
}

fn label_confidence(labels: &[ClassifierLabel], target: &str) -> f64 {
    labels
        .iter()
        .filter(|label| label.label.trim() == target)
        .map(|label| label.confidence)
        .fold(0.0, f64::max)
}

async fn persist_notification_intent(
    pool: &SqlitePool,
    metadata: &EmailClassifierMetadata,
    intent: &StoredNotificationIntent,
) -> Result<()> {
    let now = Utc::now().timestamp();
    let payload_json =
        serde_json::to_string(&intent.payload).context("Failed to encode notification payload")?;
    sqlx::query(
        r#"
        INSERT INTO email_notification_intents
            (email_id, mailbox, thread_id, intent, reason, payload_json, status,
             created_by, created_at, updated_at)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(email_id) DO UPDATE SET
            mailbox = excluded.mailbox,
            thread_id = excluded.thread_id,
            intent = excluded.intent,
            reason = excluded.reason,
            payload_json = excluded.payload_json,
            status = excluded.status,
            updated_at = excluded.updated_at
        "#,
    )
    .bind(metadata.id)
    .bind(&metadata.mailbox)
    .bind(metadata.thread_id.as_deref())
    .bind(&intent.intent)
    .bind(&intent.reason)
    .bind(payload_json)
    .bind(if intent.intent == "eligible" {
        "pending"
    } else {
        intent.intent.as_str()
    })
    .bind(CLASSIFIER_CREATED_BY)
    .bind(now)
    .bind(now)
    .execute(pool)
    .await
    .context("Failed to persist email notification intent")?;
    Ok(())
}

async fn get_email_classifier_metadata(
    pool: &SqlitePool,
    email_id: i64,
) -> Result<EmailClassifierMetadata> {
    let row = sqlx::query_as::<_, EmailClassifierMetadataRow>(
        r#"
        SELECT id, message_id, mailbox, folder, from_address, from_name,
               to_addresses, cc_addresses, subject, received_at, thread_id,
               in_reply_to, labels
        FROM emails
        WHERE id = ?
        "#,
    )
    .bind(email_id)
    .fetch_one(pool)
    .await
    .context("Failed to load classifier email metadata")?;

    Ok(EmailClassifierMetadata {
        id: row.id,
        message_id: row.message_id,
        mailbox: row.mailbox,
        folder: row.folder,
        from_address: row.from_address,
        from_name: row.from_name,
        to_addresses: parse_json_string_vec(Some(&row.to_addresses)),
        cc_addresses: parse_json_string_vec(row.cc_addresses.as_deref()),
        subject: row.subject,
        received_at: row.received_at,
        thread_id: row.thread_id,
        in_reply_to: row.in_reply_to,
        labels: parse_json_string_vec(row.labels.as_deref()),
    })
}

fn parse_json_string_vec(raw: Option<&str>) -> Vec<String> {
    raw.and_then(|value| serde_json::from_str::<Vec<String>>(value).ok())
        .unwrap_or_default()
}

fn compact_one_line(value: &str, max_chars: usize) -> String {
    let mut compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() > max_chars {
        compact = compact.chars().take(max_chars).collect::<String>();
    }
    compact
}

async fn has_ready_classifier_job(pool: &SqlitePool) -> Result<bool> {
    let now = Utc::now().timestamp();
    let count: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)
        FROM email_classifier_jobs
        WHERE status = 'pending'
           OR (status = 'deferred' AND COALESCE(deferred_until, 0) <= ?)
        "#,
    )
    .bind(now)
    .fetch_one(pool)
    .await
    .context("Failed to count ready email classifier jobs")?;
    Ok(count > 0)
}

async fn claim_next_ready_classifier_job(
    pool: &SqlitePool,
) -> Result<Option<EmailClassifierJobRow>> {
    let now = Utc::now().timestamp();
    let Some(job) = sqlx::query_as::<_, EmailClassifierJobRow>(
        r#"
        SELECT job_id, email_id
        FROM email_classifier_jobs
        WHERE status = 'pending'
           OR (status = 'deferred' AND COALESCE(deferred_until, 0) <= ?)
        ORDER BY created_at ASC, job_id ASC
        LIMIT 1
        "#,
    )
    .bind(now)
    .fetch_optional(pool)
    .await
    .context("Failed to select ready email classifier job")?
    else {
        return Ok(None);
    };

    let updated = sqlx::query(
        r#"
        UPDATE email_classifier_jobs
        SET status = 'running',
            attempts = attempts + 1,
            started_at = ?,
            updated_at = ?,
            defer_reason = NULL,
            deferred_until = NULL
        WHERE job_id = ?
          AND (status = 'pending' OR (status = 'deferred' AND COALESCE(deferred_until, 0) <= ?))
        "#,
    )
    .bind(now)
    .bind(now)
    .bind(&job.job_id)
    .bind(now)
    .execute(pool)
    .await
    .context("Failed to claim email classifier job")?;

    if updated.rows_affected() == 0 {
        return Ok(None);
    }
    Ok(Some(job))
}

async fn defer_ready_classifier_jobs(
    pool: &SqlitePool,
    quota: &CodexQuotaGateDecision,
) -> Result<()> {
    let now = Utc::now().timestamp();
    let defer_until = quota.defer_until.unwrap_or(now + QUOTA_RECHECK_SECONDS);
    sqlx::query(
        r#"
        UPDATE email_classifier_jobs
        SET status = 'deferred',
            defer_reason = ?,
            deferred_until = ?,
            updated_at = ?
        WHERE status = 'pending'
           OR (status = 'deferred' AND COALESCE(deferred_until, 0) <= ?)
        "#,
    )
    .bind(&quota.reason)
    .bind(defer_until)
    .bind(now)
    .bind(now)
    .execute(pool)
    .await
    .context("Failed to defer email classifier jobs")?;
    Ok(())
}

async fn mark_classifier_job_completed(pool: &SqlitePool, job_id: &str) -> Result<()> {
    let now = Utc::now().timestamp();
    sqlx::query(
        r#"
        UPDATE email_classifier_jobs
        SET status = 'completed',
            completed_at = ?,
            updated_at = ?
        WHERE job_id = ?
        "#,
    )
    .bind(now)
    .bind(now)
    .bind(job_id)
    .execute(pool)
    .await
    .context("Failed to mark email classifier job completed")?;
    Ok(())
}

async fn mark_classifier_job_failed(pool: &SqlitePool, job_id: &str, error: &str) -> Result<()> {
    let now = Utc::now().timestamp();
    sqlx::query(
        r#"
        UPDATE email_classifier_jobs
        SET status = 'failed',
            last_error = ?,
            completed_at = ?,
            updated_at = ?
        WHERE job_id = ?
        "#,
    )
    .bind(compact_one_line(error, 1000))
    .bind(now)
    .bind(now)
    .bind(job_id)
    .execute(pool)
    .await
    .context("Failed to mark email classifier job failed")?;
    Ok(())
}

async fn mark_classifier_job_skipped(pool: &SqlitePool, job_id: &str, reason: &str) -> Result<()> {
    let now = Utc::now().timestamp();
    sqlx::query(
        r#"
        UPDATE email_classifier_jobs
        SET status = 'skipped',
            last_error = ?,
            completed_at = ?,
            updated_at = ?
        WHERE job_id = ?
        "#,
    )
    .bind(reason)
    .bind(now)
    .bind(now)
    .bind(job_id)
    .execute(pool)
    .await
    .context("Failed to mark email classifier job skipped")?;
    Ok(())
}

async fn persist_classifier_job_output(
    pool: &SqlitePool,
    job: &EmailClassifierJobRow,
    output: &ClassifierTurnOutput,
    verdict: &ClassifierVerdict,
    applied: &AppliedClassifierActions,
) -> Result<()> {
    let verdict_json =
        serde_json::to_string(verdict).context("Failed to encode classifier verdict")?;
    let applied_labels_json =
        serde_json::to_string(&applied.labels).context("Failed to encode classifier labels")?;
    let notification_json = serde_json::to_string(&applied.notification_intent)
        .context("Failed to encode classifier notification intent")?;
    sqlx::query(
        r#"
        UPDATE email_classifier_jobs
        SET classifier_output_json = ?,
            applied_labels_json = ?,
            notification_intent_json = ?,
            tool_call_count = ?,
            updated_at = ?
        WHERE job_id = ?
        "#,
    )
    .bind(verdict_json)
    .bind(applied_labels_json)
    .bind(notification_json)
    .bind(output.tool_call_count)
    .bind(Utc::now().timestamp())
    .bind(&job.job_id)
    .execute(pool)
    .await
    .context("Failed to persist classifier job output")?;
    Ok(())
}

async fn read_and_persist_quota_gate(pool: &SqlitePool) -> Result<CodexQuotaGateDecision> {
    match read_codex_account_rate_limits().await {
        Ok(snapshot) => {
            let decision = evaluate_codex_quota_gate(&snapshot, Utc::now().timestamp());
            persist_quota_gate_snapshot(pool, Some(&snapshot), &decision).await?;
            Ok(decision)
        }
        Err(e) => {
            let now = Utc::now().timestamp();
            let decision = CodexQuotaGateDecision {
                allowed: false,
                reason: format!("quota_unavailable: {}", compact_one_line(&e, 300)),
                defer_until: Some(now + QUOTA_RECHECK_SECONDS),
                five_hour_used_percent: None,
                five_hour_resets_at: None,
                weekly_used_percent: None,
                weekly_resets_at: None,
            };
            persist_quota_gate_snapshot(pool, None, &decision).await?;
            Ok(decision)
        }
    }
}

pub fn evaluate_codex_quota_gate(
    snapshot: &CodexAccountRateLimits,
    now: i64,
) -> CodexQuotaGateDecision {
    let five_hour = find_window_observation(snapshot, FIVE_HOUR_WINDOW_MINS);
    let weekly = find_window_observation(snapshot, WEEKLY_WINDOW_MINS);
    let reached_type = rate_limit_reached_type(snapshot);
    let mut reasons = Vec::new();
    let mut defer_untils = Vec::new();

    if let Some(window) = &five_hour {
        if window.used_percent > FIVE_HOUR_DEFER_THRESHOLD_PERCENT {
            reasons.push(format!(
                "five_hour_usage_above_90_percent:{}",
                window.used_percent
            ));
            if let Some(reset) = window.resets_at.filter(|reset| *reset > now) {
                defer_untils.push(reset);
            }
        }
    }

    if let Some(window) = &weekly {
        if window.used_percent >= WEEKLY_DEFER_THRESHOLD_PERCENT {
            reasons.push(format!("weekly_usage_near_full:{}", window.used_percent));
            if let Some(reset) = window.resets_at.filter(|reset| *reset > now) {
                defer_untils.push(reset);
            }
        }
    }

    if let Some(reached_type) = reached_type {
        reasons.push(format!("rate_limit_reached:{reached_type}"));
        if let Some(reset) = weekly
            .as_ref()
            .and_then(|window| window.resets_at)
            .filter(|reset| *reset > now)
        {
            defer_untils.push(reset);
        }
    }

    if reasons.is_empty() {
        return CodexQuotaGateDecision {
            allowed: true,
            reason: "quota_available".to_string(),
            defer_until: None,
            five_hour_used_percent: five_hour.as_ref().map(|window| window.used_percent),
            five_hour_resets_at: five_hour.as_ref().and_then(|window| window.resets_at),
            weekly_used_percent: weekly.as_ref().map(|window| window.used_percent),
            weekly_resets_at: weekly.as_ref().and_then(|window| window.resets_at),
        };
    }

    CodexQuotaGateDecision {
        allowed: false,
        reason: reasons.join(";"),
        defer_until: defer_untils
            .into_iter()
            .min()
            .or(Some(now + QUOTA_RECHECK_SECONDS)),
        five_hour_used_percent: five_hour.as_ref().map(|window| window.used_percent),
        five_hour_resets_at: five_hour.as_ref().and_then(|window| window.resets_at),
        weekly_used_percent: weekly.as_ref().map(|window| window.used_percent),
        weekly_resets_at: weekly.as_ref().and_then(|window| window.resets_at),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QuotaWindowObservation {
    used_percent: i32,
    resets_at: Option<i64>,
}

fn find_window_observation(
    snapshot: &CodexAccountRateLimits,
    window_duration_mins: i64,
) -> Option<QuotaWindowObservation> {
    rate_limit_snapshots(snapshot)
        .into_iter()
        .flat_map(|bucket| [bucket.primary.as_ref(), bucket.secondary.as_ref()])
        .flatten()
        .filter(|window| window.window_duration_mins == Some(window_duration_mins))
        .fold(None, |best, window| {
            let candidate = QuotaWindowObservation {
                used_percent: window.used_percent,
                resets_at: window.resets_at,
            };
            match best {
                Some(best) if best.used_percent >= candidate.used_percent => Some(best),
                _ => Some(candidate),
            }
        })
}

fn rate_limit_snapshots(snapshot: &CodexAccountRateLimits) -> Vec<&CodexRateLimitSnapshot> {
    let mut snapshots = vec![&snapshot.rate_limits];
    if let Some(by_limit_id) = &snapshot.rate_limits_by_limit_id {
        snapshots.extend(by_limit_id.values());
    }
    snapshots
}

fn rate_limit_reached_type(snapshot: &CodexAccountRateLimits) -> Option<&str> {
    rate_limit_snapshots(snapshot)
        .into_iter()
        .filter_map(|bucket| bucket.rate_limit_reached_type.as_deref())
        .map(str::trim)
        .find(|value| !value.is_empty())
}

async fn persist_quota_gate_snapshot(
    pool: &SqlitePool,
    snapshot: Option<&CodexAccountRateLimits>,
    decision: &CodexQuotaGateDecision,
) -> Result<()> {
    let snapshot_json = snapshot
        .map(serde_json::to_string)
        .transpose()
        .context("Failed to encode Codex quota snapshot")?;
    sqlx::query(
        r#"
        INSERT INTO email_classifier_quota_snapshots
            (source, snapshot_json, five_hour_used_percent, five_hour_resets_at,
             weekly_used_percent, weekly_resets_at, allowed, reason, created_at)
        VALUES ('account_rate_limits', ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(snapshot_json)
    .bind(decision.five_hour_used_percent)
    .bind(decision.five_hour_resets_at)
    .bind(decision.weekly_used_percent)
    .bind(decision.weekly_resets_at)
    .bind(if decision.allowed { 1_i64 } else { 0_i64 })
    .bind(&decision.reason)
    .bind(Utc::now().timestamp())
    .execute(pool)
    .await
    .context("Failed to persist email classifier quota snapshot")?;
    Ok(())
}

pub async fn ensure_email_classifier_schema(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS email_classifier_jobs (
            job_id TEXT PRIMARY KEY,
            email_id INTEGER NOT NULL UNIQUE REFERENCES emails(id) ON DELETE CASCADE,
            scan_id INTEGER NOT NULL REFERENCES email_security_scans(id) ON DELETE CASCADE,
            status TEXT NOT NULL,
            attempts INTEGER NOT NULL DEFAULT 0,
            defer_reason TEXT,
            deferred_until INTEGER,
            last_error TEXT,
            classifier_output_json TEXT,
            applied_labels_json TEXT,
            notification_intent_json TEXT,
            tool_call_count INTEGER NOT NULL DEFAULT 0,
            created_by TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            started_at INTEGER,
            completed_at INTEGER,
            updated_at INTEGER NOT NULL
        )
        "#,
    )
    .execute(pool)
    .await
    .context("Failed to create email_classifier_jobs")?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS email_notification_intents (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            email_id INTEGER NOT NULL UNIQUE REFERENCES emails(id) ON DELETE CASCADE,
            mailbox TEXT NOT NULL,
            thread_id TEXT,
            intent TEXT NOT NULL,
            reason TEXT NOT NULL,
            payload_json TEXT NOT NULL,
            status TEXT NOT NULL,
            created_by TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        )
        "#,
    )
    .execute(pool)
    .await
    .context("Failed to create email_notification_intents")?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS email_classifier_quota_snapshots (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            source TEXT NOT NULL,
            snapshot_json TEXT,
            five_hour_used_percent INTEGER,
            five_hour_resets_at INTEGER,
            weekly_used_percent INTEGER,
            weekly_resets_at INTEGER,
            allowed INTEGER NOT NULL,
            reason TEXT NOT NULL,
            created_at INTEGER NOT NULL
        )
        "#,
    )
    .execute(pool)
    .await
    .context("Failed to create email_classifier_quota_snapshots")?;

    let indexes = [
        "CREATE INDEX IF NOT EXISTS idx_email_classifier_jobs_status_due ON email_classifier_jobs(status, deferred_until, created_at)",
        "CREATE INDEX IF NOT EXISTS idx_email_classifier_jobs_email ON email_classifier_jobs(email_id)",
        "CREATE INDEX IF NOT EXISTS idx_email_notification_intents_status ON email_notification_intents(status, updated_at DESC)",
        "CREATE INDEX IF NOT EXISTS idx_email_classifier_quota_snapshots_created ON email_classifier_quota_snapshots(created_at DESC)",
    ];
    for index in indexes {
        sqlx::query(index)
            .execute(pool)
            .await
            .with_context(|| format!("Failed to create email classifier index: {index}"))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::codex_app_server::CodexRateLimitWindow;

    fn scan_with(level: &str) -> EmailSecurityScan {
        EmailSecurityScan {
            id: 7,
            email_id: 42,
            mailbox: "inbox@example.com".to_string(),
            scanner_version: 2,
            risk_score: 0,
            risk_level: level.to_string(),
            flags: Vec::new(),
            reasons: Vec::new(),
            extracted_urls: Vec::new(),
            suspicious_urls: Vec::new(),
            attachment_count: 0,
            suspicious_attachment_count: 0,
            has_prompt_injection: false,
            has_secret_request: false,
            has_suspicious_links: false,
            has_suspicious_attachments: false,
            has_hidden_content: false,
            status: "scanned".to_string(),
            created_at: 100,
            updated_at: 100,
            created_at_iso: "1970-01-01T00:01:40+00:00".to_string(),
            updated_at_iso: "1970-01-01T00:01:40+00:00".to_string(),
        }
    }

    fn quota_snapshot(five_hour: i32, weekly: i32, reset: i64) -> CodexAccountRateLimits {
        CodexAccountRateLimits {
            rate_limits: CodexRateLimitSnapshot {
                limit_id: Some("default".to_string()),
                limit_name: None,
                primary: Some(CodexRateLimitWindow {
                    used_percent: five_hour,
                    window_duration_mins: Some(FIVE_HOUR_WINDOW_MINS),
                    resets_at: Some(reset),
                }),
                secondary: Some(CodexRateLimitWindow {
                    used_percent: weekly,
                    window_duration_mins: Some(WEEKLY_WINDOW_MINS),
                    resets_at: Some(reset + 100),
                }),
                credits: None,
                plan_type: None,
                rate_limit_reached_type: None,
            },
            rate_limits_by_limit_id: None,
        }
    }

    #[test]
    fn safe_enough_classifier_admission_is_conservative() {
        let clean = scan_with("clean");
        assert!(classifier_safety_decision("INBOX", &clean).safe);

        let medium = scan_with("medium");
        let decision = classifier_safety_decision("INBOX", &medium);
        assert!(!decision.safe);
        assert_eq!(decision.reason, "scan_risk_not_safe_enough");

        let sent = classifier_safety_decision("Sent", &clean);
        assert!(!sent.safe);
        assert_eq!(sent.reason, "non_inbox_folder");

        let mut prompt_injection = scan_with("low");
        prompt_injection.has_prompt_injection = true;
        let decision = classifier_safety_decision("INBOX", &prompt_injection);
        assert!(!decision.safe);
        assert_eq!(decision.reason, "prompt_injection_detected");
    }

    #[test]
    fn quota_gate_defers_above_five_hour_threshold() {
        let decision = evaluate_codex_quota_gate(&quota_snapshot(91, 50, 10_000), 1_000);

        assert!(!decision.allowed);
        assert!(decision.reason.contains("five_hour_usage_above_90_percent"));
        assert_eq!(decision.defer_until, Some(10_000));
    }

    #[test]
    fn quota_gate_defers_near_full_weekly_usage() {
        let decision = evaluate_codex_quota_gate(&quota_snapshot(25, 95, 10_000), 1_000);

        assert!(!decision.allowed);
        assert!(decision.reason.contains("weekly_usage_near_full"));
        assert_eq!(decision.defer_until, Some(10_100));
    }

    #[test]
    fn quota_gate_allows_below_thresholds() {
        let decision = evaluate_codex_quota_gate(&quota_snapshot(90, 94, 10_000), 1_000);

        assert!(decision.allowed);
        assert_eq!(decision.reason, "quota_available");
    }

    #[test]
    fn classifier_verdict_parser_accepts_json_fence() {
        let verdict = parse_classifier_verdict(
            r#"```json
            {"schema_version":"email_classifier_v1","labels":[{"label":"mail/personal","confidence":0.9}],"attention":{"create":false},"notification":{"intent":"suppressed"}}
            ```"#,
        )
        .expect("parse verdict");

        assert_eq!(verdict.labels[0].label, "mail/personal");
    }

    #[test]
    fn classifier_label_sanitizer_keeps_only_allowed_confident_labels() {
        let labels = sanitize_classifier_labels(&[
            ClassifierLabel {
                label: "mail/personal".to_string(),
                confidence: 0.9,
                rationale: None,
            },
            ClassifierLabel {
                label: "delete/email".to_string(),
                confidence: 0.99,
                rationale: None,
            },
            ClassifierLabel {
                label: "notify/eligible".to_string(),
                confidence: 0.2,
                rationale: None,
            },
        ]);

        assert_eq!(labels, vec!["mail/personal".to_string()]);
    }

    #[test]
    fn classifier_job_prompt_contains_metadata_and_not_body() {
        let metadata = EmailClassifierMetadata {
            id: 42,
            message_id: "m-1".to_string(),
            mailbox: "inbox@example.com".to_string(),
            folder: "INBOX".to_string(),
            from_address: "sender@example.com".to_string(),
            from_name: Some("Sender".to_string()),
            to_addresses: vec!["inbox@example.com".to_string()],
            cc_addresses: Vec::new(),
            subject: Some("Project note".to_string()),
            received_at: 123,
            thread_id: Some("thread-1".to_string()),
            in_reply_to: None,
            labels: Vec::new(),
        };
        let mut scan = scan_with("clean");
        scan.reasons.push("deterministic scan summary".to_string());

        let prompt = build_classifier_job_prompt(&metadata, &scan).expect("build prompt");

        assert!(prompt.contains("Email ID:"));
        assert!(prompt.contains("Project note"));
        assert!(prompt.contains("deterministic scan summary"));
        assert!(!prompt.contains("body_text"));
        assert!(!prompt.contains("body_html"));
    }
}
