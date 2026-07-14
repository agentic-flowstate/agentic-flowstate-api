use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use std::error::Error as StdError;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;

const APNS_PRODUCTION: &str = "https://api.push.apple.com";
const APNS_SANDBOX: &str = "https://api.sandbox.push.apple.com";
const TOKEN_REFRESH_SECS: u64 = 50 * 60; // Refresh every 50 minutes (Apple max is 60)

static APNS_INSTANCE: OnceCell<Arc<ApnsService>> = OnceCell::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApnsEndpoint {
    Production,
    Sandbox,
}

impl ApnsEndpoint {
    fn from_use_sandbox(use_sandbox: bool) -> Self {
        if use_sandbox {
            Self::Sandbox
        } else {
            Self::Production
        }
    }

    fn base_url(self) -> &'static str {
        match self {
            Self::Production => APNS_PRODUCTION,
            Self::Sandbox => APNS_SANDBOX,
        }
    }

    fn alternate(self) -> Self {
        match self {
            Self::Production => Self::Sandbox,
            Self::Sandbox => Self::Production,
        }
    }
}

impl std::fmt::Display for ApnsEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Production => "production",
            Self::Sandbox => "sandbox",
        })
    }
}

#[derive(Debug, Clone)]
struct ApnsDelivery {
    endpoint: ApnsEndpoint,
}

#[derive(Debug, Clone)]
struct ApnsSendFailure {
    endpoint: ApnsEndpoint,
    status: Option<u16>,
    reason: String,
}

impl ApnsSendFailure {
    fn should_retry_alternate_endpoint(&self) -> bool {
        self.reason == "BadDeviceToken"
    }

    fn should_soft_delete_token(&self) -> bool {
        self.status == Some(410) || self.reason == "Unregistered"
    }
}

impl std::fmt::Display for ApnsSendFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(status) => write!(
                f,
                "{} from {} APNs (HTTP {})",
                self.reason, self.endpoint, status
            ),
            None => write!(f, "{} from {} APNs", self.reason, self.endpoint),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ApnsAlertError {
    #[error("APNs alert push misconfigured: {0}")]
    MissingConfig(String),

    #[error("Failed to read APNs .p8 key at {path}: {source}")]
    KeyRead {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("Failed to parse APNs .p8 key: {0}")]
    KeyParse(#[source] jsonwebtoken::errors::Error),

    #[error("Failed to build HTTP/2 APNs client: {0}")]
    ClientBuild(#[source] reqwest::Error),
}

#[derive(Debug, Clone)]
pub struct ApnsAlertConfig {
    pub key_id: String,
    pub team_id: String,
    pub bundle_id: String,
    pub key_path: String,
    pub use_sandbox: bool,
}

impl ApnsAlertConfig {
    pub fn from_env() -> Result<Self, ApnsAlertError> {
        fn require(key: &str) -> Result<String, ApnsAlertError> {
            match std::env::var(key) {
                Ok(v) if !v.trim().is_empty() => Ok(v),
                _ => Err(ApnsAlertError::MissingConfig(format!(
                    "env var {} is required and must be non-empty",
                    key
                ))),
            }
        }

        let key_id = require("APNS_KEY_ID")?;
        let team_id = require("APNS_TEAM_ID")?;
        let bundle_id = require("APNS_BUNDLE_ID")?;
        let key_path = require("APNS_KEY_PATH")?;
        let sandbox_raw = require("APNS_USE_SANDBOX")?;
        let use_sandbox = match sandbox_raw.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => true,
            "false" | "0" | "no" => false,
            other => {
                return Err(ApnsAlertError::MissingConfig(format!(
                    "APNS_USE_SANDBOX must be true/false (got {:?})",
                    other
                )));
            }
        };

        Ok(Self {
            key_id,
            team_id,
            bundle_id,
            key_path,
            use_sandbox,
        })
    }
}

#[derive(Debug, Serialize)]
struct Claims {
    iss: String, // Team ID
    iat: u64,    // Issued at
}

#[derive(Debug, Serialize)]
struct ApnsPayload {
    aps: Aps,
    #[serde(skip_serializing_if = "Option::is_none")]
    conversation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    notification_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    email_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "thread_id")]
    email_thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mailbox: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    deeplink: Option<String>,
}

#[derive(Debug, Serialize)]
struct Aps {
    alert: ApnsAlert,
    sound: String,
    #[serde(rename = "thread-id")]
    thread_id: String,
}

#[derive(Debug, Serialize)]
struct ApnsAlert {
    title: String,
    body: String,
}

#[derive(Debug, Deserialize)]
struct ApnsErrorResponse {
    reason: Option<String>,
}

struct CachedToken {
    token: String,
    created_at: u64,
}

#[derive(Debug, Clone, Default)]
struct AlertMetadata<'a> {
    conversation_id: Option<&'a str>,
    agent_name: Option<&'a str>,
    notification_type: Option<&'a str>,
    email_id: Option<i64>,
    email_thread_id: Option<&'a str>,
    mailbox: Option<&'a str>,
    deeplink: Option<&'a str>,
    collapse_id: Option<String>,
    apns_thread_id: &'a str,
}

pub struct ApnsService {
    encoding_key: EncodingKey,
    key_id: String,
    team_id: String,
    bundle_id: String,
    endpoint: ApnsEndpoint,
    client: reqwest::Client,
    cached_token: RwLock<Option<CachedToken>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApnsAlertFanoutReport {
    pub registered_devices: usize,
    pub delivered_devices: usize,
    pub failed_devices: usize,
}

impl ApnsService {
    /// Get the global APNs service instance (set during init).
    pub fn global() -> Option<&'static Arc<ApnsService>> {
        APNS_INSTANCE.get()
    }

    pub fn init_from_env() -> Result<Arc<Self>, ApnsAlertError> {
        let cfg = ApnsAlertConfig::from_env()?;
        Self::init(cfg)
    }

    pub fn init(cfg: ApnsAlertConfig) -> Result<Arc<Self>, ApnsAlertError> {
        if let Some(existing) = APNS_INSTANCE.get() {
            return Ok(existing.clone());
        }

        let key_data = std::fs::read(&cfg.key_path).map_err(|e| ApnsAlertError::KeyRead {
            path: cfg.key_path.clone(),
            source: e,
        })?;
        let encoding_key = EncodingKey::from_ec_pem(&key_data).map_err(ApnsAlertError::KeyParse)?;

        let endpoint = ApnsEndpoint::from_use_sandbox(cfg.use_sandbox);

        let client = reqwest::Client::builder()
            .use_rustls_tls()
            .http2_prior_knowledge()
            .build()
            .map_err(ApnsAlertError::ClientBuild)?;

        tracing::info!(
            "[APNS] Alert push initialized (bundle_id={}, endpoint={}, team={}, key={})",
            cfg.bundle_id,
            endpoint.base_url(),
            cfg.team_id,
            cfg.key_id
        );

        let service = Arc::new(Self {
            encoding_key,
            key_id: cfg.key_id,
            team_id: cfg.team_id,
            bundle_id: cfg.bundle_id,
            endpoint,
            client,
            cached_token: RwLock::new(None),
        });

        let _ = APNS_INSTANCE.set(service.clone());
        Ok(service)
    }

    /// Get or refresh the JWT bearer token.
    async fn get_token(&self) -> Result<String, String> {
        // Check cache first
        {
            let cache = self.cached_token.read().await;
            if let Some(ref cached) = *cache {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                if now - cached.created_at < TOKEN_REFRESH_SECS {
                    return Ok(cached.token.clone());
                }
            }
        }

        // Create new token
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let claims = Claims {
            iss: self.team_id.clone(),
            iat: now,
        };

        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some(self.key_id.clone());

        let token = encode(&header, &claims, &self.encoding_key)
            .map_err(|e| format!("JWT encode failed: {}", e))?;

        // Cache it
        let mut cache = self.cached_token.write().await;
        *cache = Some(CachedToken {
            token: token.clone(),
            created_at: now,
        });

        Ok(token)
    }

    /// Send a push notification to a single device token.
    pub async fn send(
        &self,
        device_token: &str,
        title: &str,
        body: &str,
        conversation_id: Option<&str>,
        agent_name: Option<&str>,
    ) -> Result<(), String> {
        let metadata = AlertMetadata {
            conversation_id,
            agent_name,
            collapse_id: conversation_id.map(str::to_string),
            apns_thread_id: "agent-completion",
            ..Default::default()
        };
        self.send_with_endpoint_retry(device_token, title, body, &metadata)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    pub async fn send_email_notification_to_user(
        &self,
        db: &sqlx::SqlitePool,
        user_id: &str,
        title: &str,
        body: &str,
        email_id: i64,
        email_thread_id: Option<&str>,
        mailbox: Option<&str>,
        deeplink: &str,
    ) -> Result<(), String> {
        let metadata = AlertMetadata {
            notification_type: Some("email"),
            email_id: Some(email_id),
            email_thread_id,
            mailbox,
            deeplink: Some(deeplink),
            collapse_id: Some(format!("email-{email_id}")),
            apns_thread_id: "email-classifier",
            ..Default::default()
        };
        self.send_to_user_with_metadata(db, user_id, title, body, &metadata)
            .await
            .map(|_| ())
    }

    /// Send one visible disk-pressure alert to every active device for a user.
    ///
    /// The caller supplies a stable action ID for `apns-collapse-id`, so a
    /// retried delivery attempt for the same durable pressure transition is
    /// coalesced by APNs instead of appearing as a new alert.
    pub async fn send_disk_pressure_notification_to_user(
        &self,
        db: &sqlx::SqlitePool,
        user_id: &str,
        title: &str,
        body: &str,
        conversation_id: Option<&str>,
        action_id: &str,
    ) -> Result<ApnsAlertFanoutReport, String> {
        let metadata = AlertMetadata {
            conversation_id,
            agent_name: Some("full-access"),
            notification_type: Some("disk_pressure"),
            collapse_id: Some(format!("disk-pressure-{action_id}")),
            apns_thread_id: "disk-pressure",
            ..Default::default()
        };
        let report = self
            .send_to_user_with_metadata(db, user_id, title, body, &metadata)
            .await?;
        if report.registered_devices == 0 {
            return Err(format!(
                "No active APNs device tokens are registered for user {user_id}"
            ));
        }
        if report.failed_devices > 0 {
            return Err(format!(
                "APNs disk-pressure fan-out delivered to {} of {} registered devices",
                report.delivered_devices, report.registered_devices
            ));
        }
        Ok(report)
    }

    async fn send_with_endpoint_retry(
        &self,
        device_token: &str,
        title: &str,
        body: &str,
        metadata: &AlertMetadata<'_>,
    ) -> Result<ApnsDelivery, ApnsSendFailure> {
        match self
            .send_once(self.endpoint, device_token, title, body, metadata)
            .await
        {
            Ok(delivery) => Ok(delivery),
            Err(first) if first.should_retry_alternate_endpoint() => {
                let alternate = self.endpoint.alternate();
                tracing::warn!(
                    "[APNS] {} for {}; retrying {} endpoint",
                    first,
                    &device_token[..std::cmp::min(8, device_token.len())],
                    alternate
                );

                match self
                    .send_once(alternate, device_token, title, body, metadata)
                    .await
                {
                    Ok(delivery) => {
                        tracing::info!(
                            "[APNS] Push delivered via alternate {} endpoint for {}",
                            delivery.endpoint,
                            &device_token[..std::cmp::min(8, device_token.len())]
                        );
                        Ok(delivery)
                    }
                    Err(second) => Err(second),
                }
            }
            Err(first) => Err(first),
        }
    }

    async fn send_once(
        &self,
        endpoint: ApnsEndpoint,
        device_token: &str,
        title: &str,
        body: &str,
        metadata: &AlertMetadata<'_>,
    ) -> Result<ApnsDelivery, ApnsSendFailure> {
        let token = match self.get_token().await {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("[APNS] JWT token generation failed: {}", e);
                return Err(ApnsSendFailure {
                    endpoint,
                    status: None,
                    reason: e,
                });
            }
        };

        let payload = ApnsPayload {
            aps: Aps {
                alert: ApnsAlert {
                    title: title.to_string(),
                    body: body.to_string(),
                },
                sound: "default".to_string(),
                thread_id: metadata.apns_thread_id.to_string(),
            },
            conversation_id: metadata.conversation_id.map(|s| s.to_string()),
            agent_name: metadata.agent_name.map(|s| s.to_string()),
            notification_type: metadata.notification_type.map(|s| s.to_string()),
            email_id: metadata.email_id,
            email_thread_id: metadata.email_thread_id.map(|s| s.to_string()),
            mailbox: metadata.mailbox.map(|s| s.to_string()),
            deeplink: metadata.deeplink.map(|s| s.to_string()),
        };

        let url = format!("{}/3/device/{}", endpoint.base_url(), device_token);
        tracing::info!(
            "[APNS] Sending to {} via {}",
            &device_token[..std::cmp::min(8, device_token.len())],
            endpoint.base_url()
        );

        let response = match self
            .client
            .post(&url)
            .header("authorization", format!("bearer {}", token))
            .header("apns-topic", &self.bundle_id)
            .header("apns-push-type", "alert")
            .header("apns-priority", "10")
            .header(
                "apns-expiration",
                (SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
                    + 3600)
                    .to_string(),
            )
            .header(
                "apns-collapse-id",
                metadata
                    .collapse_id
                    .as_deref()
                    .or(metadata.conversation_id)
                    .unwrap_or("agent-completion"),
            )
            .json(&payload)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("[APNS] HTTP request failed: {} | source: {:?} | is_connect: {} | is_timeout: {}",
                        e, e.source(), e.is_connect(), e.is_timeout());
                return Err(ApnsSendFailure {
                    endpoint,
                    status: None,
                    reason: format!("APNs request failed: {}", e),
                });
            }
        };

        let status = response.status();
        let status_code = status.as_u16();

        if status.is_success() {
            tracing::info!(
                "[APNS] Push sent OK (HTTP {}) to {}",
                status_code,
                &device_token[..std::cmp::min(8, device_token.len())]
            );
            Ok(ApnsDelivery { endpoint })
        } else {
            let error: ApnsErrorResponse = response
                .json()
                .await
                .unwrap_or(ApnsErrorResponse { reason: None });
            let reason = error.reason.unwrap_or_else(|| format!("HTTP {}", status));
            tracing::warn!(
                "[APNS] Push failed for {} via {} APNs: {}",
                &device_token[..std::cmp::min(8, device_token.len())],
                endpoint,
                reason
            );
            Err(ApnsSendFailure {
                endpoint,
                status: Some(status_code),
                reason,
            })
        }
    }

    /// Send a push notification to all of a user's devices.
    /// Removes APNs-unregistered tokens automatically.
    pub async fn send_to_user(
        &self,
        db: &sqlx::SqlitePool,
        user_id: &str,
        title: &str,
        body: &str,
        conversation_id: Option<&str>,
        agent_name: Option<&str>,
    ) -> Result<(), String> {
        let tokens =
            match ticketing_system::device_tokens::get_active_tokens_for_user(db, user_id).await {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!(
                        "[APNS] DB error fetching tokens for user {}: {}",
                        user_id,
                        e
                    );
                    return Err(format!("DB error: {}", e));
                }
            };

        tracing::info!(
            "[APNS] Found {} device token(s) for user {}",
            tokens.len(),
            user_id
        );

        if tokens.is_empty() {
            return Ok(());
        }

        for token in &tokens {
            match self
                .send_with_endpoint_retry(
                    token,
                    title,
                    body,
                    &AlertMetadata {
                        conversation_id,
                        agent_name,
                        collapse_id: conversation_id.map(str::to_string),
                        apns_thread_id: "agent-completion",
                        ..Default::default()
                    },
                )
                .await
            {
                Ok(delivery) => {
                    if delivery.endpoint != self.endpoint {
                        tracing::warn!(
                            "[APNS] Token for user {} delivered via {} APNs while configured endpoint is {}",
                            user_id,
                            delivery.endpoint,
                            self.endpoint
                        );
                    }
                }
                Err(failure) => {
                    tracing::warn!("[APNS] Send failed for user {}: {}", user_id, failure);
                    if failure.should_soft_delete_token() {
                        tracing::info!("[APNS] Soft-deleting invalid token for user {}", user_id);
                        let _ = ticketing_system::device_tokens::soft_delete_device_token(
                            db, user_id, token,
                        )
                        .await;
                    } else {
                        tracing::warn!(
                            "[APNS] Keeping token for user {} after non-terminal APNs rejection: {}",
                            user_id,
                            failure
                        );
                    }
                }
            }
        }

        Ok(())
    }

    async fn send_to_user_with_metadata(
        &self,
        db: &sqlx::SqlitePool,
        user_id: &str,
        title: &str,
        body: &str,
        metadata: &AlertMetadata<'_>,
    ) -> Result<ApnsAlertFanoutReport, String> {
        let tokens =
            match ticketing_system::device_tokens::get_active_tokens_for_user(db, user_id).await {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!(
                        "[APNS] DB error fetching tokens for user {}: {}",
                        user_id,
                        e
                    );
                    return Err(format!("DB error: {}", e));
                }
            };

        tracing::info!(
            "[APNS] Found {} device token(s) for user {}",
            tokens.len(),
            user_id
        );

        let mut delivered_devices = 0usize;
        let mut failed_devices = 0usize;

        for token in &tokens {
            match self
                .send_with_endpoint_retry(token, title, body, metadata)
                .await
            {
                Ok(delivery) => {
                    delivered_devices += 1;
                    if delivery.endpoint != self.endpoint {
                        tracing::warn!(
                            "[APNS] Token for user {} delivered via {} APNs while configured endpoint is {}",
                            user_id,
                            delivery.endpoint,
                            self.endpoint
                        );
                    }
                }
                Err(failure) => {
                    failed_devices += 1;
                    tracing::warn!("[APNS] Send failed for user {}: {}", user_id, failure);
                    if failure.should_soft_delete_token() {
                        tracing::info!("[APNS] Soft-deleting invalid token for user {}", user_id);
                        let _ = ticketing_system::device_tokens::soft_delete_device_token(
                            db, user_id, token,
                        )
                        .await;
                    }
                }
            }
        }

        Ok(ApnsAlertFanoutReport {
            registered_devices: tokens.len(),
            delivered_devices,
            failed_devices,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_device_token_retries_alternate_endpoint_but_does_not_soft_delete() {
        let failure = ApnsSendFailure {
            endpoint: ApnsEndpoint::Production,
            status: Some(400),
            reason: "BadDeviceToken".to_string(),
        };

        assert!(failure.should_retry_alternate_endpoint());
        assert!(!failure.should_soft_delete_token());
    }

    #[test]
    fn unregistered_soft_deletes_without_endpoint_retry() {
        let failure = ApnsSendFailure {
            endpoint: ApnsEndpoint::Production,
            status: Some(410),
            reason: "Unregistered".to_string(),
        };

        assert!(!failure.should_retry_alternate_endpoint());
        assert!(failure.should_soft_delete_token());
    }

    #[test]
    fn endpoints_have_expected_alternates() {
        assert_eq!(ApnsEndpoint::Production.alternate(), ApnsEndpoint::Sandbox);
        assert_eq!(ApnsEndpoint::Sandbox.alternate(), ApnsEndpoint::Production);
    }
}
