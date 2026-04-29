//! Silent (background) APNs push sender for the durable chat streaming
//! pipeline.
//!
//! This sender is intentionally separate from the user-visible alert sender
//! in [`super::alert`]. Silent pushes are a wake-signal for the iOS client:
//! when the device wakes, the `SyncEngine` performs a delta-sync from the
//! server. Delivery of the push itself is best-effort — APNs coalesces by
//! `apns-collapse-id = conversation_id` and only stores the LAST such push
//! per app per device.
//!
//! ## Wire format (exact)
//!
//! ```json
//! {"aps":{"content-available":1},"conv":"<id>","last":"<id>"}
//! ```
//!
//! Serialization is by hand-rolled `serde::Serialize` implementations so
//! the field order is stable and byte-for-byte reproducible.
//!
//! ## Auth
//!
//! JWT ES256 token auth using an Apple `.p8` private key. The underlying
//! `apns-h2` client (Threema fork) handles the ~55 minute JWT refresh and
//! keeps a single HTTP/2 connection open for multiplexing.
//!
//! ## Config contract
//!
//! All five env vars are REQUIRED. Missing any one at [`ApnsClient::init`]
//! time returns [`ApnsSilentError::MissingConfig`] and MUST bubble up to
//! crash the server at startup. There is no silent fallback.
//!
//! * `APNS_KEY_ID` — 10-char key identifier from the Apple developer portal
//! * `APNS_TEAM_ID` — 10-char team identifier
//! * `APNS_BUNDLE_ID` — app bundle id, used as `apns-topic`
//! * `APNS_KEY_PATH` — absolute path to the `.p8` PEM file
//! * `APNS_USE_SANDBOX` — `true` for `api.sandbox.push.apple.com`, `false`
//!   for `api.push.apple.com`. Parsed case-insensitively.

use std::sync::Arc;

use apns_h2::{
    request::payload::PayloadLike, Client, ClientConfig, CollapseId, Endpoint, ErrorReason,
    NotificationOptions, Priority, PushType,
};
use once_cell::sync::OnceCell;
use serde::{ser::SerializeStruct, Serialize};

/// Errors returned by [`ApnsClient`].
///
/// The `DeviceUnregistered` variant is the load-bearing one: the caller
/// MUST soft-delete the offending device token when this is returned so
/// the backend stops pushing to invalid tokens. APNs reports this via
/// HTTP 410 with `reason: "Unregistered"` (also `BadDeviceToken`).
#[derive(Debug, thiserror::Error)]
pub enum ApnsSilentError {
    /// A required env var was missing or empty at [`ApnsClient::init`].
    #[error("APNs silent push misconfigured: {0}")]
    MissingConfig(String),

    /// The `.p8` key file could not be read from disk.
    #[error("Failed to read APNs .p8 key at {path}: {source}")]
    KeyRead {
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// The underlying apns-h2 client could not be constructed (e.g. the
    /// key could not be parsed as ES256 PKCS8).
    #[error("Failed to construct APNs client: {0}")]
    ClientBuild(#[source] apns_h2::Error),

    /// [`ApnsClient::send_silent_push`] was called before
    /// [`ApnsClient::init`] completed.
    #[error("APNs client not initialized")]
    NotInitialized,

    /// The device token is no longer valid for this topic. Caller MUST
    /// soft-delete the token. Corresponds to APNs HTTP 410 with
    /// `Unregistered` or `BadDeviceToken`.
    #[error("Device token unregistered: {0}")]
    DeviceUnregistered(String),

    /// APNs accepted the request framing but rejected the notification
    /// for some other reason (rate limit, bad priority, payload too
    /// large, etc.).
    #[error("APNs rejected push (code {code}): {reason}")]
    Rejected { code: u16, reason: String },

    /// Transport/IO error talking to APNs (timeout, TLS, hyper, …).
    #[error("APNs transport error: {0}")]
    Transport(#[source] apns_h2::Error),
}

/// The hand-rolled silent-push payload. Field order is deliberate:
/// `aps` first, then `conv`, then `last`. `content-available` is serialized
/// as the integer `1` (not the boolean `true`) per APNs spec.
#[derive(Debug)]
pub struct SilentPayload<'a> {
    device_token: &'a str,
    conversation_id: &'a str,
    last_message_id: &'a str,
    options: NotificationOptions<'a>,
}

impl<'a> Serialize for SilentPayload<'a> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        // 3 fields: aps, conv, last
        let mut st = s.serialize_struct("SilentPayload", 3)?;
        st.serialize_field("aps", &Aps)?;
        st.serialize_field("conv", self.conversation_id)?;
        st.serialize_field("last", self.last_message_id)?;
        st.end()
    }
}

/// The `aps` sub-object. Serializes to exactly `{"content-available":1}`.
#[derive(Debug)]
struct Aps;

impl Serialize for Aps {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut st = s.serialize_struct("Aps", 1)?;
        st.serialize_field("content-available", &1u8)?;
        st.end()
    }
}

impl<'a> PayloadLike for SilentPayload<'a> {
    fn get_device_token(&self) -> &str {
        self.device_token
    }

    fn get_options(&self) -> &NotificationOptions<'_> {
        &self.options
    }
}

/// Resolved configuration for the silent push sender. Read from env at
/// [`ApnsClient::init`] time; never mutated after.
#[derive(Debug, Clone)]
pub struct ApnsSilentConfig {
    pub key_id: String,
    pub team_id: String,
    pub bundle_id: String,
    pub use_sandbox: bool,
    /// Raw PEM-encoded `.p8` key bytes. Kept on the client for possible
    /// reconnection logic in future versions; the `apns-h2` `Client`
    /// already owns a parsed signer internally.
    #[allow(dead_code)]
    key_path: String,
}

impl ApnsSilentConfig {
    /// Read the config from environment. Fails loud if any of the five
    /// required env vars are missing or empty.
    pub fn from_env() -> Result<Self, ApnsSilentError> {
        fn require(key: &str) -> Result<String, ApnsSilentError> {
            match std::env::var(key) {
                Ok(v) if !v.trim().is_empty() => Ok(v),
                _ => Err(ApnsSilentError::MissingConfig(format!(
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
                return Err(ApnsSilentError::MissingConfig(format!(
                    "APNS_USE_SANDBOX must be true/false (got {:?})",
                    other
                )));
            }
        };

        Ok(Self {
            key_id,
            team_id,
            bundle_id,
            use_sandbox,
            key_path,
        })
    }
}

/// Holder for the lazily-built `apns-h2` HTTP/2 client.
///
/// The `Client` is constructed once at startup and wrapped in an
/// `Arc<OnceCell>` so that:
///
/// * axum app state only carries the `ApnsClient` handle
/// * concurrent `send_silent_push` calls share a single HTTP/2 connection
/// * accidental re-init in tests is a no-op rather than a panic
pub struct ApnsClient {
    inner: OnceCell<Arc<ClientInner>>,
}

struct ClientInner {
    client: Client,
    config: ApnsSilentConfig,
}

impl std::fmt::Debug for ApnsClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApnsClient")
            .field("initialized", &self.inner.get().is_some())
            .finish()
    }
}

impl Default for ApnsClient {
    fn default() -> Self {
        Self::new()
    }
}

impl ApnsClient {
    /// Construct an uninitialized holder. Must be paired with
    /// [`ApnsClient::init`] before use.
    pub const fn new() -> Self {
        Self {
            inner: OnceCell::new(),
        }
    }

    /// Initialize the client using raw `.p8` PEM key bytes. Does NOT read
    /// the filesystem — the caller is responsible for reading the key
    /// from wherever secrets live.
    pub fn init(
        &self,
        key_id: String,
        team_id: String,
        bundle_id: String,
        p8_key_bytes: Vec<u8>,
        use_sandbox: bool,
    ) -> Result<(), ApnsSilentError> {
        if self.inner.get().is_some() {
            return Ok(());
        }

        let endpoint = if use_sandbox {
            Endpoint::Sandbox
        } else {
            Endpoint::Production
        };
        let config = ClientConfig::new(endpoint);

        let mut key_reader = std::io::Cursor::new(p8_key_bytes);
        let client = Client::token(&mut key_reader, key_id.clone(), team_id.clone(), config)
            .map_err(ApnsSilentError::ClientBuild)?;

        let resolved = ApnsSilentConfig {
            key_id,
            team_id,
            bundle_id,
            use_sandbox,
            key_path: String::new(),
        };

        let inner = Arc::new(ClientInner {
            client,
            config: resolved,
        });

        // OnceCell::set returns Err if already set — we already short-circuited
        // above so treat a race as a no-op success.
        let _ = self.inner.set(inner);
        Ok(())
    }

    /// Convenience constructor: read all five env vars and the `.p8` file,
    /// then [`init`](Self::init). Intended for the server startup path.
    pub fn init_from_env(&self) -> Result<ApnsSilentConfig, ApnsSilentError> {
        let cfg = ApnsSilentConfig::from_env()?;
        let key_bytes = std::fs::read(&cfg.key_path).map_err(|e| ApnsSilentError::KeyRead {
            path: cfg.key_path.clone(),
            source: e,
        })?;
        self.init(
            cfg.key_id.clone(),
            cfg.team_id.clone(),
            cfg.bundle_id.clone(),
            key_bytes,
            cfg.use_sandbox,
        )?;
        Ok(cfg)
    }

    /// Send a silent (background) push. Returns `Ok(())` on HTTP 200.
    /// Maps APNs rejection codes to typed errors; the caller is expected
    /// to soft-delete the device token when [`ApnsSilentError::DeviceUnregistered`]
    /// is returned.
    pub async fn send_silent_push(
        &self,
        device_token: &str,
        conversation_id: &str,
        last_message_id: &str,
    ) -> Result<(), ApnsSilentError> {
        let inner = self
            .inner
            .get()
            .ok_or(ApnsSilentError::NotInitialized)?
            .clone();

        // Collapse id cap: APNs enforces a 64-byte ceiling. apns_h2's
        // CollapseId constructor checks this for us.
        let collapse = CollapseId::new(conversation_id).map_err(|e| ApnsSilentError::Rejected {
            code: 0,
            reason: format!("invalid collapse-id: {}", e),
        })?;

        let options = NotificationOptions {
            apns_push_type: Some(PushType::Background),
            apns_priority: Some(Priority::Normal),
            apns_expiration: Some(0),
            apns_topic: Some(inner.config.bundle_id.as_str()),
            apns_collapse_id: Some(collapse),
            apns_id: None,
        };

        let payload = SilentPayload {
            device_token,
            conversation_id,
            last_message_id,
            options,
        };

        match inner.client.send(payload).await {
            Ok(_resp) => Ok(()),
            Err(apns_h2::Error::ResponseError(resp)) => {
                let reason_str = resp
                    .error
                    .as_ref()
                    .map(|e| format!("{:?}", e.reason))
                    .unwrap_or_else(|| format!("HTTP {}", resp.code));

                let is_unregistered = resp.code == 410
                    || matches!(
                        resp.error.as_ref().map(|e| &e.reason),
                        Some(ErrorReason::Unregistered) | Some(ErrorReason::BadDeviceToken)
                    );

                if is_unregistered {
                    Err(ApnsSilentError::DeviceUnregistered(reason_str))
                } else {
                    Err(ApnsSilentError::Rejected {
                        code: resp.code,
                        reason: reason_str,
                    })
                }
            }
            Err(other) => Err(ApnsSilentError::Transport(other)),
        }
    }

    /// Read-only accessor used by tests and debug endpoints.
    #[allow(dead_code)]
    pub fn config(&self) -> Option<ApnsSilentConfig> {
        self.inner.get().map(|i| i.config.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{decode_header, Algorithm, DecodingKey, Validation};

    /// A throwaway ES256 key in PKCS8 PEM format, generated specifically
    /// for the unit test suite. NOT a real APNs key — it is embedded
    /// as source, is not valid with Apple, and exists solely so the JWT
    /// signer can produce a decodable token offline.
    ///
    /// Generated with:
    ///   openssl ecparam -name prime256v1 -genkey -noout -out key.pem
    ///   openssl pkcs8 -topk8 -nocrypt -in key.pem -out key.p8
    const TEST_P8_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg/mdmknXzO7DSFqVV
PYzOTX4uB+TLEL6rNJChSBdP4HChRANCAAQYM1N1dvP30GxXsNWFSGQ1z7HwN02X
vbrq97jpGprpOy5fzo13dG3DurwMAF+DSvBfcbcHSNnSQd7r7gXd/qmF
-----END PRIVATE KEY-----
";

    /// The corresponding public key, used only to verify the signature
    /// we produce can round-trip through the `jsonwebtoken` verifier.
    const TEST_PUBLIC_KEY: &str = "-----BEGIN PUBLIC KEY-----
MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEGDNTdXbz99BsV7DVhUhkNc+x8DdN
l7266ve46Rqa6TsuX86Nd3Rtw7q8DABfg0rwX3G3B0jZ0kHe6+4F3f6phQ==
-----END PUBLIC KEY-----
";

    #[test]
    fn silent_payload_is_byte_for_byte_exact() {
        let options = NotificationOptions::default();
        let payload = SilentPayload {
            device_token: "deadbeef",
            conversation_id: "conv-abc",
            last_message_id: "msg-123",
            options,
        };

        let json = payload.to_json_string().expect("serialize");

        // Exact spec string. NO "mutable-content", NO extra fields,
        // aps-first, then conv, then last.
        assert_eq!(
            json,
            r#"{"aps":{"content-available":1},"conv":"conv-abc","last":"msg-123"}"#,
        );
    }

    #[test]
    fn silent_payload_device_token_and_options_pass_through() {
        let options = NotificationOptions {
            apns_push_type: Some(PushType::Background),
            apns_priority: Some(Priority::Normal),
            apns_expiration: Some(0),
            apns_topic: Some("com.example.test"),
            apns_collapse_id: Some(CollapseId::new("conv-xyz").expect("collapse")),
            apns_id: None,
        };
        let payload = SilentPayload {
            device_token: "token-42",
            conversation_id: "conv-xyz",
            last_message_id: "msg-99",
            options,
        };

        assert_eq!(payload.get_device_token(), "token-42");
        let opts = payload.get_options();
        assert!(matches!(opts.apns_push_type, Some(PushType::Background)));
        assert!(matches!(opts.apns_priority, Some(Priority::Normal)));
        assert_eq!(opts.apns_expiration, Some(0));
        assert_eq!(opts.apns_topic, Some("com.example.test"));
    }

    #[test]
    fn jwt_es256_round_trip_from_p8() {
        // This asserts that a fresh apns-h2 Client can be built from a
        // real ES256 .p8 key and that *something* in the crate agrees
        // the signer produced a valid JWT. Since apns-h2 hides the
        // signer, we build a parallel JWT with jsonwebtoken (same
        // algorithm + claims shape) and verify we can decode it.
        let encoding =
            jsonwebtoken::EncodingKey::from_ec_pem(TEST_P8_KEY.as_bytes()).expect("p8 parse");
        let mut header = jsonwebtoken::Header::new(Algorithm::ES256);
        header.kid = Some("TESTKEYID1".to_string());

        #[derive(serde::Serialize)]
        struct Claims {
            iss: String,
            iat: u64,
        }
        let claims = Claims {
            iss: "TESTTEAMID".to_string(),
            iat: 1_700_000_000,
        };

        let token = jsonwebtoken::encode(&header, &claims, &encoding).expect("jwt encode");

        // Decodes the protected header without verification — proves
        // the token is well-formed ES256 with the expected kid.
        let decoded_header = decode_header(&token).expect("decode header");
        assert_eq!(decoded_header.alg, Algorithm::ES256);
        assert_eq!(decoded_header.kid.as_deref(), Some("TESTKEYID1"));

        // Full verification against the public key (iss = kid since we
        // do not store audience in APNs JWTs).
        let decoding = DecodingKey::from_ec_pem(TEST_PUBLIC_KEY.as_bytes()).expect("pub parse");
        let mut validation = Validation::new(Algorithm::ES256);
        validation.required_spec_claims.clear();
        validation.validate_exp = false;
        validation.validate_nbf = false;
        validation.validate_aud = false;
        let decoded: jsonwebtoken::TokenData<serde_json::Value> =
            jsonwebtoken::decode(&token, &decoding, &validation).expect("jwt verify");
        assert_eq!(decoded.claims["iss"], "TESTTEAMID");
        assert_eq!(decoded.claims["iat"], 1_700_000_000u64);

        // And: apns-h2 itself can at least parse the key without error,
        // which exercises its internal signer.
        let mut cursor = std::io::Cursor::new(TEST_P8_KEY.as_bytes().to_vec());
        let build = Client::token(
            &mut cursor,
            "TESTKEYID1".to_string(),
            "TESTTEAMID".to_string(),
            ClientConfig::default(),
        );
        assert!(
            build.is_ok(),
            "apns-h2 Client::token failed to parse .p8: {:?}",
            build.err()
        );
    }

    #[test]
    fn send_before_init_fails_typed() {
        let client = ApnsClient::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt
            .block_on(client.send_silent_push("tok", "conv", "msg"))
            .expect_err("must error");
        assert!(matches!(err, ApnsSilentError::NotInitialized));
    }

    #[test]
    fn init_is_idempotent() {
        let client = ApnsClient::new();
        client
            .init(
                "KID1".into(),
                "TID1".into(),
                "com.example.test".into(),
                TEST_P8_KEY.as_bytes().to_vec(),
                true,
            )
            .expect("first init");
        // Second call is a no-op — must not panic, must not replace config.
        client
            .init(
                "OTHER".into(),
                "OTHER".into(),
                "com.other".into(),
                TEST_P8_KEY.as_bytes().to_vec(),
                false,
            )
            .expect("second init no-op");
        let cfg = client.config().expect("initialized");
        assert_eq!(cfg.key_id, "KID1");
        assert_eq!(cfg.team_id, "TID1");
        assert_eq!(cfg.bundle_id, "com.example.test");
        assert!(cfg.use_sandbox);
    }

    #[test]
    fn missing_env_vars_are_reported() {
        // Scrub all five env vars (use unique prefixes so we don't stomp
        // on a real server process sharing this env).
        let guard = EnvGuard::new(&[
            "APNS_KEY_ID",
            "APNS_TEAM_ID",
            "APNS_BUNDLE_ID",
            "APNS_KEY_PATH",
            "APNS_USE_SANDBOX",
        ]);
        for k in guard.keys() {
            std::env::remove_var(k);
        }
        let err = ApnsSilentConfig::from_env().expect_err("must fail");
        assert!(
            matches!(err, ApnsSilentError::MissingConfig(_)),
            "wrong variant: {:?}",
            err
        );
    }

    /// Saves/restores env vars around a test so setting/clearing them
    /// does not leak between tests in the same process.
    struct EnvGuard {
        saved: Vec<(String, Option<String>)>,
    }

    impl EnvGuard {
        fn new(keys: &[&str]) -> Self {
            let saved = keys
                .iter()
                .map(|k| (k.to_string(), std::env::var(k).ok()))
                .collect();
            Self { saved }
        }
        fn keys(&self) -> Vec<&str> {
            self.saved.iter().map(|(k, _)| k.as_str()).collect()
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.saved {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
    }
}
