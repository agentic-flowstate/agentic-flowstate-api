//! Public and authenticated BallotRadar commercial-outreach boundaries.
//!
//! The public endpoints never log raw tokens, recipient addresses, bodies,
//! cookies, IP addresses, or user agents. GET is confirmation-only; only an
//! exact RFC 8058 POST or a signed human confirmation mutates suppression.

use std::sync::Arc;
use std::time::Instant;

use axum::body::{to_bytes, Body, Bytes};
use axum::extract::{Extension, Path, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use futures::stream;
use serde::Serialize;
use serde_json::json;
use sqlx::{FromRow, SqlitePool};

use crate::auth_middleware::AuthenticatedUser;
use crate::observability::outreach::{
    self as outreach_metrics, ComplianceOutcome, UnsubscribeMechanism,
};
use crate::observability::request::RequestTelemetryContext;
use crate::outreach_compliance::{
    GlobalComplianceRegistry, OutreachComplianceConfig, OutreachSecrets,
};

use ticketing_system::outreach::{
    self, ComplianceAuditEventType, NewComplianceAuditEvent, NewUnsubscribeToken, PauseScope,
    UnsubscribeApplyResult, UnsubscribeRequest,
};

const COMPONENT: &str = "outreach_unsubscribe";
const MAX_PUBLIC_BODY_BYTES: usize = 16 * 1024;

#[derive(Debug, FromRow)]
struct PublicTokenRecord {
    recipient_normalized: String,
    outreach_message_id: String,
}

#[derive(Debug, FromRow)]
struct ReservedMessage {
    outreach_message_id: String,
    sequence_id: String,
    contact_id: String,
    cohort_id: String,
    template_version: String,
    recipient_normalized: String,
    subject: String,
    body_text: String,
    status: String,
}

#[derive(Debug, Serialize)]
pub struct CommercialSendResponse {
    pub outreach_message_id: String,
    pub message_id: String,
    pub provider_message_id: String,
    pub success: bool,
}

/// RFC 8058 public endpoint. Only the exact List-Unsubscribe=One-Click
/// parameter is accepted, with no authentication, cookies, redirect, or
/// confirmation message.
pub async fn one_click_post(
    State(pool): State<Arc<SqlitePool>>,
    Path(raw_token): Path<String>,
    Extension(trace): Extension<RequestTelemetryContext>,
    request: Request,
) -> Response {
    let started = Instant::now();
    if parse_exact_one_click(request).await.is_err() {
        outreach_metrics::record_unsubscribe(
            UnsubscribeMechanism::OneClick,
            ComplianceOutcome::Invalid,
            started.elapsed().as_secs_f64() * 1_000.0,
        );
        return public_error(StatusCode::BAD_REQUEST);
    }
    apply_public_unsubscribe(
        &pool,
        &raw_token,
        &trace.request_id,
        ComplianceAuditEventType::OneClickPost,
        "rfc8058_one_click",
        UnsubscribeMechanism::OneClick,
        started,
    )
    .await
}

/// Scanner-safe human landing page. A valid GET never changes suppression.
pub async fn human_get(
    State(pool): State<Arc<SqlitePool>>,
    Path(raw_token): Path<String>,
    Extension(trace): Extension<RequestTelemetryContext>,
) -> Response {
    let started = Instant::now();
    let (config, secrets, registry) = match load_compliance().await {
        Ok(value) => value,
        Err(error) => {
            log_configuration_failure(&pool, &trace.request_id, &error).await;
            outreach_metrics::record_unsubscribe(
                UnsubscribeMechanism::HumanGet,
                ComplianceOutcome::ConfigurationError,
                started.elapsed().as_secs_f64() * 1_000.0,
            );
            return public_error(StatusCode::SERVICE_UNAVAILABLE);
        }
    };
    let token_id_hash = match secrets.verify_token(&config, &raw_token) {
        Ok(value) => value,
        Err(_) => {
            outreach_metrics::record_unsubscribe(
                UnsubscribeMechanism::HumanGet,
                ComplianceOutcome::Invalid,
                started.elapsed().as_secs_f64() * 1_000.0,
            );
            return public_error(StatusCode::NOT_FOUND);
        }
    };
    let active =
        match load_public_token(&pool, &token_id_hash, chrono::Utc::now().timestamp()).await {
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(error) => {
                log_storage_failure(&pool, &trace.request_id, "human_get", &error).await;
                outreach_metrics::record_unsubscribe(
                    UnsubscribeMechanism::HumanGet,
                    ComplianceOutcome::StorageError,
                    started.elapsed().as_secs_f64() * 1_000.0,
                );
                return public_error(StatusCode::SERVICE_UNAVAILABLE);
            }
        };
    let remotely_active = match registry
        .active_token(&token_id_hash, chrono::Utc::now().timestamp())
        .await
    {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(error) => {
            log_storage_failure(&pool, &trace.request_id, "human_get", &error).await;
            return public_error(StatusCode::SERVICE_UNAVAILABLE);
        }
    };
    if !active || !remotely_active {
        outreach_metrics::record_unsubscribe(
            UnsubscribeMechanism::HumanGet,
            ComplianceOutcome::Invalid,
            started.elapsed().as_secs_f64() * 1_000.0,
        );
        return public_error(StatusCode::NOT_FOUND);
    }
    let nonce = match secrets.issue_confirmation_nonce(
        &config,
        &token_id_hash,
        chrono::Utc::now().timestamp(),
    ) {
        Ok(value) => value,
        Err(error) => {
            log_configuration_failure(&pool, &trace.request_id, &error).await;
            return public_error(StatusCode::SERVICE_UNAVAILABLE);
        }
    };
    outreach_metrics::record_unsubscribe(
        UnsubscribeMechanism::HumanGet,
        ComplianceOutcome::Success,
        started.elapsed().as_secs_f64() * 1_000.0,
    );
    let action = match config.unsubscribe_url(&raw_token) {
        Ok(url) => format!("{url}/confirm"),
        Err(error) => {
            tracing::error!(
                component = "outreach_unsubscribe",
                mechanism = "human_get",
                error = %error,
                "failed to construct unsubscribe confirmation URL"
            );
            return public_error(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    secure_html(StatusCode::OK, confirmation_page(&action, &nonce))
}

/// Human confirmation target. The nonce is short-lived, stateless, and bound
/// to the opaque unsubscribe token; no cookie or email entry is used.
pub async fn human_confirm(
    State(pool): State<Arc<SqlitePool>>,
    Path(raw_token): Path<String>,
    Extension(trace): Extension<RequestTelemetryContext>,
    request: Request,
) -> Response {
    let started = Instant::now();
    let nonce = match parse_confirmation_nonce(request).await {
        Ok(value) => value,
        Err(_) => {
            outreach_metrics::record_unsubscribe(
                UnsubscribeMechanism::HumanConfirmation,
                ComplianceOutcome::Invalid,
                started.elapsed().as_secs_f64() * 1_000.0,
            );
            return public_error(StatusCode::BAD_REQUEST);
        }
    };
    let (config, secrets, registry) = match load_compliance().await {
        Ok(value) => value,
        Err(error) => {
            log_configuration_failure(&pool, &trace.request_id, &error).await;
            return public_error(StatusCode::SERVICE_UNAVAILABLE);
        }
    };
    let token_id_hash = match secrets.verify_token(&config, &raw_token) {
        Ok(value) => value,
        Err(_) => return public_error(StatusCode::NOT_FOUND),
    };
    if secrets
        .verify_confirmation_nonce(
            &config,
            &token_id_hash,
            &nonce,
            chrono::Utc::now().timestamp(),
        )
        .is_err()
    {
        outreach_metrics::record_unsubscribe(
            UnsubscribeMechanism::HumanConfirmation,
            ComplianceOutcome::Invalid,
            started.elapsed().as_secs_f64() * 1_000.0,
        );
        return public_error(StatusCode::NOT_FOUND);
    }
    let response = apply_public_unsubscribe_loaded(
        &pool,
        &secrets,
        &registry,
        &config,
        &raw_token,
        &trace.request_id,
        ComplianceAuditEventType::HumanConfirmation,
        "human_get_confirmation",
        UnsubscribeMechanism::HumanConfirmation,
        started,
    )
    .await;
    if response.status().is_success() {
        secure_html(StatusCode::OK, success_page())
    } else {
        response
    }
}

/// Authenticated, one-message-at-a-time commercial send boundary. Production
/// remains blocked by the persisted global pause and P0 control state.
pub async fn send_commercial_message(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Extension(trace): Extension<RequestTelemetryContext>,
    Path(outreach_message_id): Path<String>,
) -> Result<JsonResponse<CommercialSendResponse>, (StatusCode, String)> {
    let (config, secrets, registry) = load_compliance().await.map_err(|error| {
        outreach_metrics::record_commercial_send(ComplianceOutcome::ConfigurationError);
        tracing::error!(
            target: "agentic_api::outreach_send",
            event = "outreach_send.configuration_failed",
            request_id = %trace.request_id,
            error = %error,
            "commercial outreach configuration is unavailable"
        );
        (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("commercial outreach configuration is unavailable: {error}"),
        )
    })?;
    let message = sqlx::query_as::<_, ReservedMessage>(
        r#"
        SELECT outreach_message_id, sequence_id, contact_id,
               cohort_id, template_version, recipient_normalized,
               subject, body_text, status
        FROM outreach_messages WHERE outreach_message_id = ?
        "#,
    )
    .bind(&outreach_message_id)
    .fetch_optional(&*pool)
    .await
    .map_err(internal_error)?
    .ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            "outreach reservation not found".to_string(),
        )
    })?;
    if message.status != "reserved" {
        outreach_metrics::record_commercial_send(ComplianceOutcome::Blocked);
        return Err((
            StatusCode::CONFLICT,
            "outreach reservation is not sendable".to_string(),
        ));
    }

    let issued_at = chrono::Utc::now().timestamp();
    let issued = secrets.issue_token(&config).map_err(internal_error)?;
    let recipient_hash = secrets
        .recipient_hash(&message.recipient_normalized)
        .map_err(internal_error)?;
    registry
        .register_token(
            &issued,
            &message.recipient_normalized,
            &recipient_hash,
            &message.outreach_message_id,
            issued_at,
            issued_at + config.token_ttl_seconds,
        )
        .await
        .map_err(internal_error)?;
    outreach::register_unsubscribe_token(
        &pool,
        NewUnsubscribeToken {
            token_id_hash: issued.token_id_hash.clone(),
            token_version: "u1".to_string(),
            key_id: issued.key_id,
            outreach_message_id: message.outreach_message_id.clone(),
            recipient: message.recipient_normalized.clone(),
            recipient_hash: recipient_hash.clone(),
            issued_at,
            expires_at: issued_at + config.token_ttl_seconds,
            idempotency_key: format!(
                "token:{}:{}",
                message.outreach_message_id, issued.token_id_hash
            ),
            actor: user.user_id.clone(),
        },
    )
    .await
    .map_err(internal_error)?;
    let unsubscribe_url = config
        .unsubscribe_url(&issued.raw)
        .map_err(internal_error)?
        .to_string();
    let mailto_url = config.mailto_url();
    let (final_text, final_html) =
        commercial_bodies(&message.body_text, &unsubscribe_url, &config.postal_address);
    let list_unsubscribe = format!("<{unsubscribe_url}>, <{mailto_url}>");
    let evidence_headers = json!({
        "From": format!("{} <{}>", config.from_name, config.from_address),
        "Reply-To": config.reply_to,
        "List-Unsubscribe": list_unsubscribe,
        "List-Unsubscribe-Post": "List-Unsubscribe=One-Click",
        "message_class": "commercial_outreach",
        "footer_version": "ballotradar-commercial-v1"
    });
    let updated = sqlx::query(
        "UPDATE outreach_messages SET body_text = ?, headers_json = ?, updated_at = ? \
         WHERE outreach_message_id = ? AND status = 'reserved'",
    )
    .bind(&final_text)
    .bind(evidence_headers.to_string())
    .bind(issued_at)
    .bind(&message.outreach_message_id)
    .execute(&*pool)
    .await
    .map_err(internal_error)?
    .rows_affected();
    if updated != 1 {
        outreach_metrics::record_commercial_send(ComplianceOutcome::Blocked);
        return Err((
            StatusCode::CONFLICT,
            "outreach reservation changed before compliance evidence was finalized".to_string(),
        ));
    }

    let decision = outreach::revalidate_reservation(
        &pool,
        &message.outreach_message_id,
        chrono::Utc::now().timestamp(),
    )
    .await
    .map_err(internal_error)?;
    outreach::record_compliance_audit_event(
        &pool,
        NewComplianceAuditEvent {
            idempotency_key: format!(
                "eligibility:{}:{}",
                message.outreach_message_id, trace.request_id
            ),
            event_type: ComplianceAuditEventType::FinalEligibilityRecheck,
            request_id: Some(trace.request_id.clone()),
            contact_id: Some(message.contact_id.clone()),
            outreach_message_id: Some(message.outreach_message_id.clone()),
            recipient_hash: Some(recipient_hash.clone()),
            token_id_hash: Some(issued.token_id_hash),
            source: "api_outreach_send".to_string(),
            actor: user.user_id.clone(),
            before_state: "reserved".to_string(),
            after_state: if decision.eligible {
                "eligible".to_string()
            } else {
                "blocked".to_string()
            },
            result: if decision.eligible {
                "recorded".to_string()
            } else {
                "blocked".to_string()
            },
            detail: json!({
                "reasons": decision.reasons,
                "configuration_set": config.configuration_set,
                "message_class": "commercial_outreach"
            }),
            event_at: chrono::Utc::now().timestamp(),
        },
    )
    .await
    .map_err(internal_error)?;
    if !decision.eligible {
        outreach_metrics::record_commercial_send(ComplianceOutcome::Blocked);
        return Err((
            StatusCode::CONFLICT,
            "commercial outreach remains blocked by eligibility or operational controls"
                .to_string(),
        ));
    }

    let from = format!("{} <{}>", config.from_name, config.from_address);
    let delivery = crate::email_delivery::send_outbound_email(
        &pool,
        &user.user_id,
        &crate::email_delivery::OutboundEmail {
            from: from.clone(),
            to: vec![message.recipient_normalized.clone()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: message.subject.clone(),
            body_text: Some(final_text.clone()),
            body_html: Some(final_html),
            reply_to: Some(config.reply_to.clone()),
            in_reply_to: None,
            headers: vec![
                ("List-Unsubscribe".to_string(), list_unsubscribe),
                (
                    "List-Unsubscribe-Post".to_string(),
                    "List-Unsubscribe=One-Click".to_string(),
                ),
            ],
            ses_tags: vec![
                (
                    "MessageClass".to_string(),
                    "commercial-outreach".to_string(),
                ),
                (
                    "outreach_message_id".to_string(),
                    message.outreach_message_id.clone(),
                ),
                ("sequence_id".to_string(), message.sequence_id.clone()),
                ("cohort".to_string(), message.cohort_id.clone()),
                (
                    "template_version".to_string(),
                    message.template_version.clone(),
                ),
            ],
            required_configuration_set: Some(config.configuration_set.clone()),
            outreach_message_id: Some(message.outreach_message_id.clone()),
            outreach_recipient_hash: Some(recipient_hash.clone()),
        },
    )
    .await
    .map_err(|error| {
        outreach_metrics::record_commercial_send(ComplianceOutcome::ProviderError);
        (
            StatusCode::CONFLICT,
            format!("commercial outreach was not submitted: {error}"),
        )
    })?;
    let provider_message_id = delivery.provider_message_id.clone().ok_or_else(|| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "SES did not return provider correlation".to_string(),
        )
    })?;
    if let Err(error) = outreach::register_provider_acceptance(
        &pool,
        &message.outreach_message_id,
        &provider_message_id,
        chrono::Utc::now().timestamp(),
    )
    .await
    {
        correlation_failure_pause(&pool, &trace.request_id, &error).await;
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "SES accepted the message but durable provider correlation failed; outreach is paused"
                .to_string(),
        ));
    }

    let stored_email = ticketing_system::emails::create_email(
        &pool,
        &ticketing_system::CreateEmailRequest {
            message_id: delivery.message_id.clone(),
            mailbox: delivery.source_mailbox.clone(),
            folder: "Sent".to_string(),
            from_address: from,
            from_name: Some(config.from_name),
            to_addresses: vec![message.recipient_normalized],
            cc_addresses: None,
            subject: Some(message.subject),
            body_text: Some(final_text),
            body_html: None,
            received_at: chrono::Utc::now().timestamp(),
            thread_id: Some(delivery.message_id.clone()),
            in_reply_to: None,
        },
    )
    .await
    .map_err(internal_error)?;
    if let Err(error) = ticketing_system::email_tracking::register_provider_message(
        &pool,
        stored_email.id,
        "ses",
        &provider_message_id,
        &delivery.message_id,
        Some(&config.configuration_set),
    )
    .await
    {
        correlation_failure_pause(&pool, &trace.request_id, &error).await;
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "SES accepted the message but email timeline correlation failed; outreach is paused"
                .to_string(),
        ));
    }

    outreach_metrics::record_commercial_send(ComplianceOutcome::ProviderAccepted);
    tracing::info!(
        target: "agentic_api::outreach_send",
        event = "outreach_send.provider_accepted",
        request_id = %trace.request_id,
        outreach_message_id = %message.outreach_message_id,
        provider_message_id = %provider_message_id,
        configuration_set = %config.configuration_set,
        "SES accepted one compliant commercial-outreach message"
    );
    crate::system_log_helper::log_info(
        &pool,
        "outreach_send",
        "SES accepted one compliant commercial-outreach message",
        Some(
            &json!({
                "request_id": trace.request_id,
                "outreach_message_id": message.outreach_message_id,
                "provider_message_id": provider_message_id,
                "configuration_set": config.configuration_set
            })
            .to_string(),
        ),
    )
    .await;
    Ok(JsonResponse(CommercialSendResponse {
        outreach_message_id: outreach_message_id.clone(),
        message_id: delivery.message_id,
        provider_message_id,
        success: true,
    }))
}

pub struct JsonResponse<T>(pub T);

impl<T: Serialize> IntoResponse for JsonResponse<T> {
    fn into_response(self) -> Response {
        axum::Json(self.0).into_response()
    }
}

async fn apply_public_unsubscribe(
    pool: &Arc<SqlitePool>,
    raw_token: &str,
    request_id: &str,
    event_type: ComplianceAuditEventType,
    source: &str,
    mechanism: UnsubscribeMechanism,
    started: Instant,
) -> Response {
    let (config, secrets, registry) = match load_compliance().await {
        Ok(value) => value,
        Err(error) => {
            log_configuration_failure(pool, request_id, &error).await;
            outreach_metrics::record_unsubscribe(
                mechanism,
                ComplianceOutcome::ConfigurationError,
                started.elapsed().as_secs_f64() * 1_000.0,
            );
            return public_error(StatusCode::SERVICE_UNAVAILABLE);
        }
    };
    apply_public_unsubscribe_loaded(
        pool, &secrets, &registry, &config, raw_token, request_id, event_type, source, mechanism,
        started,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn apply_public_unsubscribe_loaded(
    pool: &Arc<SqlitePool>,
    secrets: &OutreachSecrets,
    registry: &GlobalComplianceRegistry,
    config: &OutreachComplianceConfig,
    raw_token: &str,
    request_id: &str,
    event_type: ComplianceAuditEventType,
    source: &str,
    mechanism: UnsubscribeMechanism,
    started: Instant,
) -> Response {
    let token_id_hash = match secrets.verify_token(config, raw_token) {
        Ok(value) => value,
        Err(_) => {
            outreach_metrics::record_unsubscribe(
                mechanism,
                ComplianceOutcome::Invalid,
                started.elapsed().as_secs_f64() * 1_000.0,
            );
            return public_error(StatusCode::NOT_FOUND);
        }
    };
    let token = match load_public_token(pool, &token_id_hash, chrono::Utc::now().timestamp()).await
    {
        Ok(Some(value)) => value,
        Ok(None) => return public_error(StatusCode::NOT_FOUND),
        Err(error) => {
            log_storage_failure(pool, request_id, source, &error).await;
            outreach_metrics::record_unsubscribe(
                mechanism,
                ComplianceOutcome::StorageError,
                started.elapsed().as_secs_f64() * 1_000.0,
            );
            return public_error(StatusCode::SERVICE_UNAVAILABLE);
        }
    };
    let recipient_hash = match secrets.recipient_hash(&token.recipient_normalized) {
        Ok(value) => value,
        Err(error) => {
            log_configuration_failure(pool, request_id, &error).await;
            return public_error(StatusCode::SERVICE_UNAVAILABLE);
        }
    };
    let idempotency_key = format!("{source}:{request_id}");
    let remote = match registry
        .suppress_with_token(
            &token_id_hash,
            &idempotency_key,
            request_id,
            source,
            chrono::Utc::now().timestamp(),
        )
        .await
    {
        Ok(Some(result)) if result.token.recipient_hash == recipient_hash => result,
        Ok(Some(_)) => {
            tracing::error!(
                target: "agentic_api::outreach_unsubscribe",
                event = "outreach_unsubscribe.authority_mismatch",
                request_id,
                "public and local token authorities disagreed; request failed closed"
            );
            return public_error(StatusCode::SERVICE_UNAVAILABLE);
        }
        Ok(None) => return public_error(StatusCode::NOT_FOUND),
        Err(error) => {
            log_storage_failure(pool, request_id, source, &error).await;
            return public_error(StatusCode::SERVICE_UNAVAILABLE);
        }
    };
    if let Err(error) = registry
        .sync_ses_suppression(&token.recipient_normalized)
        .await
    {
        tracing::error!(
            target: "agentic_api::outreach_unsubscribe",
            event = "outreach_unsubscribe.ses_secondary_failed",
            request_id,
            error = %error,
            "primary global suppression committed; SES secondary synchronization failed"
        );
        let _ = outreach::pause_outreach(
            pool,
            PauseScope::Global,
            "SES secondary suppression synchronization failed",
            "public_unsubscribe_handler",
            chrono::Utc::now().timestamp(),
        )
        .await;
    }
    let outcome = outreach::suppress_with_unsubscribe_token(
        pool,
        &token_id_hash,
        UnsubscribeRequest {
            idempotency_key,
            request_id: request_id.to_string(),
            recipient_hash,
            event_type,
            source: source.to_string(),
            actor: "public_unsubscribe_handler".to_string(),
            event_at: chrono::Utc::now().timestamp(),
        },
    )
    .await;
    match outcome {
        Ok(UnsubscribeApplyResult {
            valid: true,
            already_suppressed,
            outreach_message_id,
        }) => {
            let already_suppressed = already_suppressed || remote.already_suppressed;
            let metric_outcome = if already_suppressed {
                ComplianceOutcome::AlreadySuppressed
            } else {
                ComplianceOutcome::Success
            };
            outreach_metrics::record_unsubscribe(
                mechanism,
                metric_outcome,
                started.elapsed().as_secs_f64() * 1_000.0,
            );
            let detail = json!({
                "request_id": request_id,
                "mechanism": mechanism.to_string(),
                "outcome": metric_outcome.to_string(),
                "outreach_message_id": outreach_message_id
                    .unwrap_or(token.outreach_message_id)
            })
            .to_string();
            tracing::info!(
                target: "agentic_api::outreach_unsubscribe",
                event = "outreach_unsubscribe.suppressed",
                request_id,
                mechanism = %mechanism,
                already_suppressed,
                "global commercial suppression committed before response"
            );
            crate::system_log_helper::log_info(
                pool,
                COMPONENT,
                "Global commercial suppression committed from unsubscribe request",
                Some(&detail),
            )
            .await;
            secure_empty(StatusCode::NO_CONTENT)
        }
        Ok(_) => {
            outreach_metrics::record_unsubscribe(
                mechanism,
                ComplianceOutcome::Invalid,
                started.elapsed().as_secs_f64() * 1_000.0,
            );
            public_error(StatusCode::NOT_FOUND)
        }
        Err(error) => {
            log_storage_failure(pool, request_id, source, &error).await;
            outreach_metrics::record_unsubscribe(
                mechanism,
                ComplianceOutcome::StorageError,
                started.elapsed().as_secs_f64() * 1_000.0,
            );
            public_error(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

async fn load_compliance() -> anyhow::Result<(
    OutreachComplianceConfig,
    OutreachSecrets,
    GlobalComplianceRegistry,
)> {
    let config = OutreachComplianceConfig::from_env()?;
    let secrets = config.load_secrets().await?;
    let registry = config.load_registry().await?;
    Ok((config, secrets, registry))
}

async fn load_public_token(
    pool: &SqlitePool,
    token_id_hash: &str,
    now: i64,
) -> anyhow::Result<Option<PublicTokenRecord>> {
    sqlx::query_as(
        "SELECT recipient_normalized, outreach_message_id \
         FROM outreach_unsubscribe_tokens \
         WHERE token_id_hash = ? AND issued_at <= ? AND expires_at > ?",
    )
    .bind(token_id_hash)
    .bind(now)
    .bind(now)
    .fetch_optional(pool)
    .await
    .map_err(Into::into)
}

async fn parse_exact_one_click(request: Request) -> anyhow::Result<()> {
    let content_type = content_type(&request)?;
    let body = to_bytes(request.into_body(), MAX_PUBLIC_BODY_BYTES).await?;
    let fields = parse_form_fields(&content_type, body).await?;
    if fields.len() != 1 || fields[0].0 != "List-Unsubscribe" || fields[0].1 != "One-Click" {
        anyhow::bail!("invalid RFC 8058 form");
    }
    Ok(())
}

async fn parse_confirmation_nonce(request: Request) -> anyhow::Result<String> {
    let content_type = content_type(&request)?;
    if !content_type
        .to_ascii_lowercase()
        .starts_with("application/x-www-form-urlencoded")
    {
        anyhow::bail!("human confirmation must be URL encoded");
    }
    let body = to_bytes(request.into_body(), MAX_PUBLIC_BODY_BYTES).await?;
    let fields: Vec<(String, String)> = serde_urlencoded::from_bytes(&body)?;
    if fields.len() != 1 || fields[0].0 != "confirmation_nonce" {
        anyhow::bail!("invalid confirmation form");
    }
    Ok(fields[0].1.clone())
}

async fn parse_form_fields(
    content_type: &str,
    body: Bytes,
) -> anyhow::Result<Vec<(String, String)>> {
    if content_type
        .to_ascii_lowercase()
        .starts_with("application/x-www-form-urlencoded")
    {
        return serde_urlencoded::from_bytes(&body).map_err(Into::into);
    }
    if content_type
        .to_ascii_lowercase()
        .starts_with("multipart/form-data")
    {
        let boundary = multer::parse_boundary(content_type)?;
        let source = stream::once(async move { Ok::<Bytes, std::io::Error>(body) });
        let mut multipart = multer::Multipart::new(source, boundary);
        let mut fields = Vec::new();
        while let Some(field) = multipart.next_field().await? {
            if field.file_name().is_some() {
                anyhow::bail!("file fields are forbidden");
            }
            let name = field
                .name()
                .ok_or_else(|| anyhow::anyhow!("multipart field name is required"))?
                .to_string();
            let value = field.text().await?;
            fields.push((name, value));
            if fields.len() > 1 {
                anyhow::bail!("only one form field is allowed");
            }
        }
        return Ok(fields);
    }
    anyhow::bail!("unsupported content type")
}

fn content_type(request: &Request) -> anyhow::Result<String> {
    request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow::anyhow!("content type is required"))
}

fn confirmation_page(action: &str, nonce: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Unsubscribe from BallotRadar</title><style>body{{font-family:system-ui,sans-serif;max-width:36rem;margin:4rem auto;padding:0 1rem;color:#1c1b1f}}button{{font:inherit;padding:.8rem 1rem;border:0;border-radius:999px;background:#3855a3;color:white;cursor:pointer}}</style></head><body><main><h1>Unsubscribe from BallotRadar</h1><p>This will stop all BallotRadar commercial email to the address associated with this message.</p><form method=\"post\" action=\"{}\"><input type=\"hidden\" name=\"confirmation_nonce\" value=\"{}\"><button type=\"submit\">Unsubscribe from all BallotRadar commercial email</button></form></main></body></html>",
        html_escape(action),
        html_escape(nonce)
    )
}

fn success_page() -> String {
    "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Unsubscribed</title></head><body><main><h1>You are unsubscribed from BallotRadar commercial email.</h1></main></body></html>".to_string()
}

fn commercial_bodies(body: &str, unsubscribe_url: &str, postal_address: &str) -> (String, String) {
    let text = format!(
        "{body}\n\nAdvertisement — This commercial email was sent by BallotRadar.\n\nUnsubscribe from all BallotRadar commercial email: {unsubscribe_url}\nYou may also reply with “unsubscribe.”\n\nBallotRadar\n{postal_address}"
    );
    let html = format!(
        "<!doctype html><html lang=\"en\"><body><div style=\"white-space:pre-wrap;font-family:system-ui,-apple-system,sans-serif\">{}</div><hr><p><strong>Advertisement — This commercial email was sent by BallotRadar.</strong></p><p><a href=\"{}\">Unsubscribe from all BallotRadar commercial email</a><br>You may also reply with “unsubscribe.”</p><p>BallotRadar<br>{}</p></body></html>",
        html_escape(body),
        html_escape(unsubscribe_url),
        html_escape(postal_address)
    );
    (text, html)
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn secure_html(status: StatusCode, body: String) -> Response {
    let mut response = (status, Html(body)).into_response();
    apply_security_headers(response.headers_mut());
    response
}

fn secure_empty(status: StatusCode) -> Response {
    let mut response = (status, Body::empty()).into_response();
    apply_security_headers(response.headers_mut());
    response
}

fn public_error(status: StatusCode) -> Response {
    let mut response = (status, "Unable to process this unsubscribe request.").into_response();
    apply_security_headers(response.headers_mut());
    response
}

fn apply_security_headers(headers: &mut HeaderMap) {
    for (name, value) in [
        (header::CACHE_CONTROL, "no-store, max-age=0"),
        (header::PRAGMA, "no-cache"),
        (header::REFERRER_POLICY, "no-referrer"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (
            header::STRICT_TRANSPORT_SECURITY,
            "max-age=63072000; includeSubDomains",
        ),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'",
        ),
    );
}

async fn correlation_failure_pause(
    pool: &Arc<SqlitePool>,
    request_id: &str,
    error: &impl std::fmt::Display,
) {
    let now = chrono::Utc::now().timestamp();
    let _ = outreach::pause_outreach(
        pool,
        PauseScope::Global,
        "SES provider acceptance could not be durably correlated",
        "api_outreach_send",
        now,
    )
    .await;
    outreach_metrics::record_commercial_send(ComplianceOutcome::ProviderError);
    let detail = json!({"request_id": request_id, "status": "paused"}).to_string();
    tracing::error!(
        target: "agentic_api::outreach_send",
        event = "outreach_send.correlation_failed",
        request_id,
        error = %error,
        "SES provider acceptance correlation failed; outreach paused"
    );
    crate::system_log_helper::log_error(
        pool,
        "outreach_send",
        "SES provider acceptance correlation failed; commercial outreach is paused",
        Some(&detail),
    )
    .await;
}

async fn log_configuration_failure(
    pool: &Arc<SqlitePool>,
    request_id: &str,
    error: &impl std::fmt::Display,
) {
    tracing::error!(
        target: "agentic_api::outreach_unsubscribe",
        event = "outreach_unsubscribe.configuration_failed",
        request_id,
        error = %error,
        "required commercial-outreach compliance configuration is unavailable"
    );
    let detail = json!({"request_id": request_id, "status": "configuration_error"}).to_string();
    crate::system_log_helper::log_error(
        pool,
        COMPONENT,
        "Required commercial-outreach compliance configuration is unavailable",
        Some(&detail),
    )
    .await;
}

async fn log_storage_failure(
    pool: &Arc<SqlitePool>,
    request_id: &str,
    mechanism: &str,
    error: &impl std::fmt::Display,
) {
    tracing::error!(
        target: "agentic_api::outreach_unsubscribe",
        event = "outreach_unsubscribe.storage_failed",
        request_id,
        mechanism,
        error = %error,
        "global suppression was not acknowledged because durable storage failed"
    );
    let detail =
        json!({"request_id": request_id, "mechanism": mechanism, "status": "storage_error"})
            .to_string();
    crate::system_log_helper::log_error(
        pool,
        COMPONENT,
        "Global suppression storage failed; unsubscribe was not acknowledged",
        Some(&detail),
    )
    .await;
}

fn internal_error(error: impl std::fmt::Display) -> (StatusCode, String) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("commercial outreach operation failed: {error}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request as HttpRequest;

    #[tokio::test]
    async fn exact_urlencoded_one_click_is_accepted() {
        let request = HttpRequest::builder()
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("List-Unsubscribe=One-Click"))
            .unwrap();
        assert!(parse_exact_one_click(request).await.is_ok());
    }

    #[tokio::test]
    async fn multipart_one_click_is_accepted_but_extra_fields_are_rejected() {
        let boundary = "ballotradar-boundary";
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"List-Unsubscribe\"\r\n\r\nOne-Click\r\n--{boundary}--\r\n"
        );
        let request = HttpRequest::builder()
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body))
            .unwrap();
        assert!(parse_exact_one_click(request).await.is_ok());

        let request = HttpRequest::builder()
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "List-Unsubscribe=One-Click&recipient=person%40example.com",
            ))
            .unwrap();
        assert!(parse_exact_one_click(request).await.is_err());
    }

    #[test]
    fn footer_is_honest_visible_and_contains_no_tracking_markup() {
        let (text, html) = commercial_bodies(
            "A useful founder note.",
            "https://email.ballotradar.com/u/opaque",
            "123 Verified Avenue, Conroe, TX 77301",
        );
        for required in [
            "Advertisement",
            "Unsubscribe from all BallotRadar commercial email",
            "reply with “unsubscribe.”",
            "123 Verified Avenue",
        ] {
            assert!(text.contains(required));
            assert!(html.contains(required));
        }
        assert!(!html.contains("<img"));
        assert!(!html.contains("track."));
    }

    #[test]
    fn public_responses_never_redirect_or_set_cookies_and_disable_caching() {
        for response in [
            secure_empty(StatusCode::NO_CONTENT),
            secure_html(StatusCode::OK, success_page()),
            public_error(StatusCode::NOT_FOUND),
        ] {
            assert!(!response.status().is_redirection());
            assert!(response.headers().get(header::SET_COOKIE).is_none());
            assert_eq!(
                response.headers().get(header::CACHE_CONTROL).unwrap(),
                "no-store, max-age=0"
            );
            assert_eq!(
                response.headers().get(header::REFERRER_POLICY).unwrap(),
                "no-referrer"
            );
        }
    }
}
