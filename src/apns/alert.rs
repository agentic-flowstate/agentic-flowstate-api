use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use std::error::Error as StdError;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;

const APNS_PRODUCTION: &str = "https://api.push.apple.com";
const APNS_SANDBOX: &str = "https://api.sandbox.push.apple.com";
const BUNDLE_ID: &str = "com.agenticflowstate.app";
const TOKEN_REFRESH_SECS: u64 = 50 * 60; // Refresh every 50 minutes (Apple max is 60)

static APNS_INSTANCE: OnceCell<Arc<ApnsService>> = OnceCell::new();

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

pub struct ApnsService {
    encoding_key: EncodingKey,
    key_id: String,
    team_id: String,
    base_url: String,
    client: reqwest::Client,
    cached_token: RwLock<Option<CachedToken>>,
}

impl ApnsService {
    /// Get the global APNs service instance (set during init).
    pub fn global() -> Option<&'static Arc<ApnsService>> {
        APNS_INSTANCE.get()
    }

    /// Initialize from environment. Returns None if .p8 key not found (APNs disabled).
    pub fn init() -> Option<Arc<Self>> {
        let key_path = dirs::home_dir()?
            .join(".agentic-flowstate")
            .join("apns-key.p8");

        if !key_path.exists() {
            tracing::warn!(
                "[APNS] No .p8 key found at {:?} — push notifications disabled",
                key_path
            );
            return None;
        }

        let key_id = match std::env::var("APNS_KEY_ID") {
            Ok(id) => id,
            Err(_) => {
                tracing::warn!("[APNS] APNS_KEY_ID not set — push notifications disabled");
                return None;
            }
        };

        let team_id = std::env::var("APNS_TEAM_ID").unwrap_or_else(|_| "M3C97KFGK9".to_string());

        let key_data = match std::fs::read(&key_path) {
            Ok(data) => data,
            Err(e) => {
                tracing::error!("[APNS] Failed to read .p8 key: {}", e);
                return None;
            }
        };

        let encoding_key = match EncodingKey::from_ec_pem(&key_data) {
            Ok(key) => key,
            Err(e) => {
                tracing::error!("[APNS] Failed to parse .p8 key: {}", e);
                return None;
            }
        };

        let is_sandbox = std::env::var("APNS_SANDBOX").unwrap_or_default() == "true";
        let base_url = if is_sandbox {
            APNS_SANDBOX
        } else {
            APNS_PRODUCTION
        }
        .to_string();

        let client = reqwest::Client::builder()
            .use_rustls_tls()
            .http2_prior_knowledge()
            .build()
            .expect("Failed to build HTTP/2 APNs client");

        tracing::info!("[APNS] Initialized (endpoint: {})", base_url);

        let service = Arc::new(Self {
            encoding_key,
            key_id,
            team_id,
            base_url,
            client,
            cached_token: RwLock::new(None),
        });

        let _ = APNS_INSTANCE.set(service.clone());
        Some(service)
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
        let token = match self.get_token().await {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("[APNS] JWT token generation failed: {}", e);
                return Err(e);
            }
        };

        let payload = ApnsPayload {
            aps: Aps {
                alert: ApnsAlert {
                    title: title.to_string(),
                    body: body.to_string(),
                },
                sound: "default".to_string(),
                thread_id: "agent-completion".to_string(),
            },
            conversation_id: conversation_id.map(|s| s.to_string()),
            agent_name: agent_name.map(|s| s.to_string()),
        };

        let url = format!("{}/3/device/{}", self.base_url, device_token);
        tracing::info!(
            "[APNS] Sending to {} via {}",
            &device_token[..std::cmp::min(8, device_token.len())],
            self.base_url
        );

        let response = match self
            .client
            .post(&url)
            .header("authorization", format!("bearer {}", token))
            .header("apns-topic", BUNDLE_ID)
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
                conversation_id.unwrap_or("agent-completion"),
            )
            .json(&payload)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("[APNS] HTTP request failed: {} | source: {:?} | is_connect: {} | is_timeout: {}",
                        e, e.source(), e.is_connect(), e.is_timeout());
                return Err(format!("APNs request failed: {}", e));
            }
        };

        let status = response.status();

        if status.is_success() {
            tracing::info!(
                "[APNS] Push sent OK (HTTP {}) to {}",
                status.as_u16(),
                &device_token[..std::cmp::min(8, device_token.len())]
            );
            Ok(())
        } else {
            let error: ApnsErrorResponse = response
                .json()
                .await
                .unwrap_or(ApnsErrorResponse { reason: None });
            let reason = error.reason.unwrap_or_else(|| format!("HTTP {}", status));
            tracing::warn!(
                "[APNS] Push failed for {}: {}",
                &device_token[..std::cmp::min(8, device_token.len())],
                reason
            );
            Err(reason)
        }
    }

    /// Send a push notification to all of a user's devices.
    /// Removes invalid tokens automatically.
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
                .send(token, title, body, conversation_id, agent_name)
                .await
            {
                Ok(()) => {}
                Err(reason) => {
                    tracing::warn!("[APNS] Send failed for user {}: {}", user_id, reason);
                    // Soft-delete invalid tokens so they stop being pushed to.
                    if reason == "BadDeviceToken"
                        || reason == "Unregistered"
                        || reason == "DeviceTokenNotForTopic"
                    {
                        tracing::info!("[APNS] Soft-deleting invalid token for user {}", user_id);
                        let _ = ticketing_system::device_tokens::soft_delete_device_token(
                            db, user_id, token,
                        )
                        .await;
                    }
                }
            }
        }

        Ok(())
    }
}
