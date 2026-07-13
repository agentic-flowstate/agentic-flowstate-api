//! BallotRadar commercial-outreach compliance configuration and token crypto.
//!
//! No secret, raw token, recipient address, or postal-address value is logged
//! by this module. Configuration is deliberately fail-closed and is loaded only
//! when the public unsubscribe or dedicated outreach boundary is used, so the
//! unrelated Agentic Flowstate API remains available while outreach is paused.

use std::collections::HashMap;

use anyhow::{anyhow, bail, Context, Result};
use aws_sdk_dynamodb::types::{AttributeValue, ConditionCheck, Put, TransactWriteItem, Update};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use hmac::{Hmac, Mac};
use rand::{rngs::OsRng, RngCore};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use url::Url;

use ticketing_system::outreach::{
    MAX_UNSUBSCRIBE_TOKEN_TTL_SECONDS, MIN_UNSUBSCRIBE_TOKEN_TTL_SECONDS,
};

type HmacSha256 = Hmac<Sha256>;

const TOKEN_VERSION: &str = "u1";
const CONFIRMATION_VERSION: &str = "c1";
const TOKEN_PURPOSE: &str = "ballotradar-commercial";
const CONFIRMATION_PURPOSE: &str = "ballotradar-commercial-confirmation";
const RANDOM_ID_BYTES: usize = 16;

#[derive(Debug, Clone)]
pub struct OutreachComplianceConfig {
    pub public_base_url: Url,
    pub public_audience: String,
    pub postal_address: String,
    pub from_address: String,
    pub from_name: String,
    pub reply_to: String,
    pub unsubscribe_mailbox: String,
    pub configuration_set: String,
    pub contact_list: String,
    pub contact_topic: String,
    pub token_ttl_seconds: i64,
    pub confirmation_ttl_seconds: i64,
    secret_id: String,
    compliance_table: String,
    aws_profile: String,
    aws_region: String,
}

#[derive(Debug, Clone)]
pub struct OutreachSecrets {
    active_key_id: String,
    keys: HashMap<String, Vec<u8>>,
    recipient_hash_key: Vec<u8>,
}

#[derive(Debug, Deserialize)]
struct StoredSecretBundle {
    active_key_id: String,
    keys: HashMap<String, String>,
    recipient_hash_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedToken {
    pub raw: String,
    pub token_id_hash: String,
    pub key_id: String,
}

#[derive(Debug, Clone)]
pub struct GlobalComplianceRegistry {
    client: aws_sdk_dynamodb::Client,
    ses_client: aws_sdk_sesv2::Client,
    table_name: String,
    contact_list: String,
    contact_topic: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteTokenRecord {
    pub recipient_normalized: String,
    pub recipient_hash: String,
    pub outreach_message_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSuppressionResult {
    pub token: RemoteTokenRecord,
    pub already_suppressed: bool,
}

impl OutreachComplianceConfig {
    pub fn from_env() -> Result<Self> {
        let public_base_url_raw = required_env("BALLOTRADAR_OUTREACH_PUBLIC_BASE_URL")?;
        let mut public_base_url = Url::parse(&public_base_url_raw)
            .context("BALLOTRADAR_OUTREACH_PUBLIC_BASE_URL must be an absolute URL")?;
        if public_base_url.scheme() != "https" || public_base_url.host_str().is_none() {
            bail!("BALLOTRADAR_OUTREACH_PUBLIC_BASE_URL must use public HTTPS");
        }
        if public_base_url.query().is_some() || public_base_url.fragment().is_some() {
            bail!("BALLOTRADAR_OUTREACH_PUBLIC_BASE_URL cannot contain query or fragment data");
        }
        let normalized_path = public_base_url.path().trim_end_matches('/');
        if normalized_path != "/u" {
            bail!("BALLOTRADAR_OUTREACH_PUBLIC_BASE_URL path must be /u");
        }
        public_base_url.set_path("/u");

        let public_audience = public_base_url
            .host_str()
            .ok_or_else(|| anyhow!("public unsubscribe audience is missing"))?
            .to_ascii_lowercase();
        let postal_address = required_env("BALLOTRADAR_OUTREACH_POSTAL_ADDRESS")?;
        validate_postal_address(&postal_address)?;
        let from_address = validate_ballotradar_mailbox(
            "BALLOTRADAR_OUTREACH_FROM_ADDRESS",
            &required_env("BALLOTRADAR_OUTREACH_FROM_ADDRESS")?,
        )?;
        let reply_to = validate_ballotradar_mailbox(
            "BALLOTRADAR_OUTREACH_REPLY_TO",
            &required_env("BALLOTRADAR_OUTREACH_REPLY_TO")?,
        )?;
        let unsubscribe_mailbox = validate_ballotradar_mailbox(
            "BALLOTRADAR_OUTREACH_UNSUBSCRIBE_MAILBOX",
            &required_env("BALLOTRADAR_OUTREACH_UNSUBSCRIBE_MAILBOX")?,
        )?;
        let from_name = required_env("BALLOTRADAR_OUTREACH_FROM_NAME")?;
        reject_header_injection("BALLOTRADAR_OUTREACH_FROM_NAME", &from_name)?;
        let configuration_set = required_env("BALLOTRADAR_OUTREACH_CONFIGURATION_SET")?;
        reject_header_injection("BALLOTRADAR_OUTREACH_CONFIGURATION_SET", &configuration_set)?;
        let contact_list = required_env("BALLOTRADAR_OUTREACH_CONTACT_LIST")?;
        reject_header_injection("BALLOTRADAR_OUTREACH_CONTACT_LIST", &contact_list)?;
        let contact_topic = required_env("BALLOTRADAR_OUTREACH_CONTACT_TOPIC")?;
        reject_header_injection("BALLOTRADAR_OUTREACH_CONTACT_TOPIC", &contact_topic)?;
        let token_ttl_seconds = parse_days(
            "BALLOTRADAR_OUTREACH_TOKEN_TTL_DAYS",
            &required_env("BALLOTRADAR_OUTREACH_TOKEN_TTL_DAYS")?,
        )?;
        if !(MIN_UNSUBSCRIBE_TOKEN_TTL_SECONDS..=MAX_UNSUBSCRIBE_TOKEN_TTL_SECONDS)
            .contains(&token_ttl_seconds)
        {
            bail!("BALLOTRADAR_OUTREACH_TOKEN_TTL_DAYS must be between 30 and 1825");
        }
        let confirmation_ttl_seconds =
            required_env("BALLOTRADAR_OUTREACH_CONFIRMATION_TTL_SECONDS")?
                .parse::<i64>()
                .context("BALLOTRADAR_OUTREACH_CONFIRMATION_TTL_SECONDS must be an integer")?;
        if !(60..=1_800).contains(&confirmation_ttl_seconds) {
            bail!("BALLOTRADAR_OUTREACH_CONFIRMATION_TTL_SECONDS must be between 60 and 1800");
        }

        Ok(Self {
            public_base_url,
            public_audience,
            postal_address,
            from_address,
            from_name,
            reply_to,
            unsubscribe_mailbox,
            configuration_set,
            contact_list,
            contact_topic,
            token_ttl_seconds,
            confirmation_ttl_seconds,
            secret_id: required_env("BALLOTRADAR_OUTREACH_SECRET_ID")?,
            compliance_table: required_env("BALLOTRADAR_OUTREACH_COMPLIANCE_TABLE")?,
            aws_profile: required_env("BALLOTRADAR_OUTREACH_AWS_PROFILE")?,
            aws_region: required_env("BALLOTRADAR_OUTREACH_AWS_REGION")?,
        })
    }

    pub async fn load_secrets(&self) -> Result<OutreachSecrets> {
        let sdk_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(self.aws_region.clone()))
            .profile_name(&self.aws_profile)
            .load()
            .await;
        let response = aws_sdk_secretsmanager::Client::new(&sdk_config)
            .get_secret_value()
            .secret_id(&self.secret_id)
            .send()
            .await
            .context("failed to load BallotRadar outreach signing keys")?;
        let secret = response
            .secret_string()
            .ok_or_else(|| anyhow!("BallotRadar outreach signing secret must be JSON text"))?;
        OutreachSecrets::from_secret_json(secret)
    }

    pub async fn load_registry(&self) -> Result<GlobalComplianceRegistry> {
        let sdk_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(self.aws_region.clone()))
            .profile_name(&self.aws_profile)
            .load()
            .await;
        Ok(GlobalComplianceRegistry {
            client: aws_sdk_dynamodb::Client::new(&sdk_config),
            ses_client: aws_sdk_sesv2::Client::new(&sdk_config),
            table_name: self.compliance_table.clone(),
            contact_list: self.contact_list.clone(),
            contact_topic: self.contact_topic.clone(),
        })
    }

    pub fn unsubscribe_url(&self, token: &str) -> Result<Url> {
        reject_path_segment(token)?;
        self.public_base_url
            .join(&format!("u/{token}"))
            .or_else(|_| {
                let mut base = self.public_base_url.clone();
                base.set_path("/u/");
                base.join(token)
            })
            .context("failed to build public unsubscribe URL")
    }

    pub fn mailto_url(&self) -> String {
        format!("mailto:{}?subject=unsubscribe", self.unsubscribe_mailbox)
    }
}

impl GlobalComplianceRegistry {
    pub async fn register_token(
        &self,
        token: &IssuedToken,
        recipient_normalized: &str,
        recipient_hash: &str,
        outreach_message_id: &str,
        issued_at: i64,
        expires_at: i64,
    ) -> Result<()> {
        self.client
            .put_item()
            .table_name(&self.table_name)
            .item("pk", av_s(format!("TOKEN#{}", token.token_id_hash)))
            .item("sk", av_s("METADATA"))
            .item("record_type", av_s("unsubscribe_token"))
            .item("token_version", av_s(TOKEN_VERSION))
            .item("key_id", av_s(&token.key_id))
            .item("recipient_normalized", av_s(recipient_normalized))
            .item("recipient_hash", av_s(recipient_hash))
            .item("outreach_message_id", av_s(outreach_message_id))
            .item("issued_at", av_n(issued_at))
            .item("expires_at", av_n(expires_at))
            .condition_expression("attribute_not_exists(pk)")
            .send()
            .await
            .context("failed to register public unsubscribe token authority")?;
        Ok(())
    }

    pub async fn active_token(
        &self,
        token_id_hash: &str,
        checked_at: i64,
    ) -> Result<Option<RemoteTokenRecord>> {
        let item = self
            .client
            .get_item()
            .table_name(&self.table_name)
            .key("pk", av_s(format!("TOKEN#{token_id_hash}")))
            .key("sk", av_s("METADATA"))
            .consistent_read(true)
            .send()
            .await
            .context("failed to read public unsubscribe token authority")?
            .item;
        let Some(item) = item else {
            return Ok(None);
        };
        let issued_at = item_i64(&item, "issued_at")?;
        let expires_at = item_i64(&item, "expires_at")?;
        if issued_at > checked_at || expires_at <= checked_at {
            return Ok(None);
        }
        Ok(Some(RemoteTokenRecord {
            recipient_normalized: item_string(&item, "recipient_normalized")?,
            recipient_hash: item_string(&item, "recipient_hash")?,
            outreach_message_id: item_string(&item, "outreach_message_id")?,
        }))
    }

    pub async fn is_suppressed(&self, recipient_hash: &str) -> Result<bool> {
        let response = self
            .client
            .get_item()
            .table_name(&self.table_name)
            .key("pk", av_s(format!("SUPPRESSION#{recipient_hash}")))
            .key("sk", av_s("GLOBAL"))
            .consistent_read(true)
            .projection_expression("pk")
            .send()
            .await
            .context("failed to check global public suppression authority")?;
        Ok(response.item.is_some())
    }

    pub async fn suppress_with_token(
        &self,
        token_id_hash: &str,
        idempotency_key: &str,
        request_id: &str,
        source: &str,
        event_at: i64,
    ) -> Result<Option<RemoteSuppressionResult>> {
        let Some(token) = self.active_token(token_id_hash, event_at).await? else {
            return Ok(None);
        };
        let audit_key_hash = sha256_hex(idempotency_key.as_bytes());
        let condition = ConditionCheck::builder()
            .table_name(&self.table_name)
            .key("pk", av_s(format!("TOKEN#{token_id_hash}")))
            .key("sk", av_s("METADATA"))
            .condition_expression("issued_at <= :now AND expires_at > :now")
            .expression_attribute_values(":now", av_n(event_at))
            .build()?;
        let update = Update::builder()
            .table_name(&self.table_name)
            .key("pk", av_s(format!("SUPPRESSION#{}", token.recipient_hash)))
            .key("sk", av_s("GLOBAL"))
            .update_expression("SET record_type = :record_type, first_suppressed_at = if_not_exists(first_suppressed_at, :now), last_suppressed_at = :now, suppression_source = :source")
            .expression_attribute_values(":record_type", av_s("global_suppression"))
            .expression_attribute_values(":now", av_n(event_at))
            .expression_attribute_values(":source", av_s(source))
            .build()?;
        let audit = Put::builder()
            .table_name(&self.table_name)
            .item("pk", av_s(format!("AUDIT#{audit_key_hash}")))
            .item("sk", av_s("EVENT"))
            .item("record_type", av_s("compliance_audit"))
            .item("event_type", av_s("global_unsubscribe"))
            .item("token_id_hash", av_s(token_id_hash))
            .item("recipient_hash", av_s(&token.recipient_hash))
            .item("outreach_message_id", av_s(&token.outreach_message_id))
            .item("request_id", av_s(request_id))
            .item("event_source", av_s(source))
            .item("event_at", av_n(event_at))
            .condition_expression("attribute_not_exists(pk)")
            .build()?;
        let result = self
            .client
            .transact_write_items()
            .transact_items(
                TransactWriteItem::builder()
                    .condition_check(condition)
                    .build(),
            )
            .transact_items(TransactWriteItem::builder().update(update).build())
            .transact_items(TransactWriteItem::builder().put(audit).build())
            .send()
            .await;
        if result.is_ok() {
            return Ok(Some(RemoteSuppressionResult {
                token,
                already_suppressed: false,
            }));
        }

        let audit_exists = self
            .client
            .get_item()
            .table_name(&self.table_name)
            .key("pk", av_s(format!("AUDIT#{audit_key_hash}")))
            .key("sk", av_s("EVENT"))
            .consistent_read(true)
            .projection_expression("pk")
            .send()
            .await
            .context("global suppression transaction failed and audit lookup failed")?
            .item
            .is_some();
        if audit_exists && self.is_suppressed(&token.recipient_hash).await? {
            return Ok(Some(RemoteSuppressionResult {
                token,
                already_suppressed: true,
            }));
        }
        result.context("failed to commit global public suppression transaction")?;
        unreachable!("error result returned above")
    }

    pub async fn suppress_recipient(
        &self,
        recipient_normalized: &str,
        recipient_hash: &str,
        idempotency_key: &str,
        request_id: &str,
        source: &str,
        event_at: i64,
    ) -> Result<bool> {
        let audit_key_hash = sha256_hex(idempotency_key.as_bytes());
        let update = Update::builder()
            .table_name(&self.table_name)
            .key("pk", av_s(format!("SUPPRESSION#{recipient_hash}")))
            .key("sk", av_s("GLOBAL"))
            .update_expression("SET record_type = :record_type, first_suppressed_at = if_not_exists(first_suppressed_at, :now), last_suppressed_at = :now, suppression_source = :source")
            .expression_attribute_values(":record_type", av_s("global_suppression"))
            .expression_attribute_values(":now", av_n(event_at))
            .expression_attribute_values(":source", av_s(source))
            .build()?;
        let audit = Put::builder()
            .table_name(&self.table_name)
            .item("pk", av_s(format!("AUDIT#{audit_key_hash}")))
            .item("sk", av_s("EVENT"))
            .item("record_type", av_s("compliance_audit"))
            .item("event_type", av_s("inbound_opt_out"))
            .item("recipient_hash", av_s(recipient_hash))
            .item("request_id", av_s(request_id))
            .item("event_source", av_s(source))
            .item("event_at", av_n(event_at))
            .condition_expression("attribute_not_exists(pk)")
            .build()?;
        let result = self
            .client
            .transact_write_items()
            .transact_items(TransactWriteItem::builder().update(update).build())
            .transact_items(TransactWriteItem::builder().put(audit).build())
            .send()
            .await;
        let already_suppressed = if result.is_ok() {
            false
        } else {
            let audit_exists = self
                .client
                .get_item()
                .table_name(&self.table_name)
                .key("pk", av_s(format!("AUDIT#{audit_key_hash}")))
                .key("sk", av_s("EVENT"))
                .consistent_read(true)
                .projection_expression("pk")
                .send()
                .await
                .context("inbound suppression transaction failed and audit lookup failed")?
                .item
                .is_some();
            if !audit_exists || !self.is_suppressed(recipient_hash).await? {
                result.context("failed to commit inbound global suppression transaction")?;
            }
            true
        };
        self.sync_ses_suppression(recipient_normalized).await?;
        Ok(already_suppressed)
    }

    pub async fn sync_ses_suppression(&self, recipient_normalized: &str) -> Result<()> {
        let preference = aws_sdk_sesv2::types::TopicPreference::builder()
            .topic_name(&self.contact_topic)
            .subscription_status(aws_sdk_sesv2::types::SubscriptionStatus::OptOut)
            .build()?;
        let updated = self
            .ses_client
            .update_contact()
            .contact_list_name(&self.contact_list)
            .email_address(recipient_normalized)
            .topic_preferences(preference.clone())
            .unsubscribe_all(true)
            .send()
            .await;
        let update_error = match updated {
            Ok(_) => return Ok(()),
            Err(error) => error,
        };
        self.ses_client
            .create_contact()
            .contact_list_name(&self.contact_list)
            .email_address(recipient_normalized)
            .topic_preferences(preference)
            .unsubscribe_all(true)
            .send()
            .await
            .with_context(|| {
                format!(
                    "global suppression committed but SES contact-list synchronization failed after update error: {}",
                    update_error
                )
            })?;
        Ok(())
    }
}

fn av_s(value: impl Into<String>) -> AttributeValue {
    AttributeValue::S(value.into())
}

fn av_n(value: i64) -> AttributeValue {
    AttributeValue::N(value.to_string())
}

fn item_string(item: &HashMap<String, AttributeValue>, name: &str) -> Result<String> {
    item.get(name)
        .and_then(|value| value.as_s().ok())
        .cloned()
        .ok_or_else(|| anyhow!("public compliance record is missing {name}"))
}

fn item_i64(item: &HashMap<String, AttributeValue>, name: &str) -> Result<i64> {
    item.get(name)
        .and_then(|value| value.as_n().ok())
        .ok_or_else(|| anyhow!("public compliance record is missing {name}"))?
        .parse::<i64>()
        .with_context(|| format!("public compliance record has invalid {name}"))
}

impl OutreachSecrets {
    fn from_secret_json(secret: &str) -> Result<Self> {
        let stored: StoredSecretBundle = serde_json::from_str(secret)
            .context("BallotRadar outreach signing secret has invalid JSON")?;
        validate_key_id(&stored.active_key_id)?;
        if stored.keys.is_empty() {
            bail!("BallotRadar outreach signing secret contains no verification keys");
        }
        let mut keys = HashMap::with_capacity(stored.keys.len());
        for (key_id, encoded) in stored.keys {
            validate_key_id(&key_id)?;
            let key = URL_SAFE_NO_PAD
                .decode(encoded.as_bytes())
                .context("BallotRadar outreach signing secret contains invalid key encoding")?;
            if key.len() < 32 {
                bail!("BallotRadar outreach signing keys must contain at least 256 bits");
            }
            keys.insert(key_id, key);
        }
        if !keys.contains_key(&stored.active_key_id) {
            bail!("active BallotRadar outreach key is missing from the key ring");
        }
        let recipient_hash_key = URL_SAFE_NO_PAD
            .decode(stored.recipient_hash_key.as_bytes())
            .context("BallotRadar outreach recipient hash key has invalid encoding")?;
        if recipient_hash_key.len() < 32 {
            bail!("BallotRadar outreach recipient hash key must contain at least 256 bits");
        }
        Ok(Self {
            active_key_id: stored.active_key_id,
            keys,
            recipient_hash_key,
        })
    }

    pub fn issue_token(&self, config: &OutreachComplianceConfig) -> Result<IssuedToken> {
        let mut random_id = [0_u8; RANDOM_ID_BYTES];
        OsRng.fill_bytes(&mut random_id);
        let random = URL_SAFE_NO_PAD.encode(random_id);
        let key_id = self.active_key_id.clone();
        let mac = self.token_mac(&key_id, &random, &config.public_audience)?;
        let raw = format!(
            "{TOKEN_VERSION}.{key_id}.{random}.{}",
            URL_SAFE_NO_PAD.encode(mac)
        );
        Ok(IssuedToken {
            token_id_hash: sha256_hex(raw.as_bytes()),
            raw,
            key_id,
        })
    }

    pub fn verify_token(&self, config: &OutreachComplianceConfig, raw: &str) -> Result<String> {
        reject_path_segment(raw)?;
        let parts = raw.split('.').collect::<Vec<_>>();
        if parts.len() != 4 || parts[0] != TOKEN_VERSION {
            bail!("invalid unsubscribe token");
        }
        let key_id = parts[1];
        validate_key_id(key_id).map_err(|_| anyhow!("invalid unsubscribe token"))?;
        let random = URL_SAFE_NO_PAD
            .decode(parts[2].as_bytes())
            .map_err(|_| anyhow!("invalid unsubscribe token"))?;
        if random.len() != RANDOM_ID_BYTES || URL_SAFE_NO_PAD.encode(&random) != parts[2] {
            bail!("invalid unsubscribe token");
        }
        let provided_mac = URL_SAFE_NO_PAD
            .decode(parts[3].as_bytes())
            .map_err(|_| anyhow!("invalid unsubscribe token"))?;
        if provided_mac.len() != 32 || URL_SAFE_NO_PAD.encode(&provided_mac) != parts[3] {
            bail!("invalid unsubscribe token");
        }
        let key = self
            .keys
            .get(key_id)
            .ok_or_else(|| anyhow!("invalid unsubscribe token"))?;
        let material = token_material(key_id, parts[2], &config.public_audience);
        let mut verifier =
            HmacSha256::new_from_slice(key).map_err(|_| anyhow!("invalid unsubscribe token"))?;
        verifier.update(&material);
        verifier
            .verify_slice(&provided_mac)
            .map_err(|_| anyhow!("invalid unsubscribe token"))?;
        Ok(sha256_hex(raw.as_bytes()))
    }

    pub fn recipient_hash(&self, recipient: &str) -> Result<String> {
        let mut mac = HmacSha256::new_from_slice(&self.recipient_hash_key)
            .map_err(|_| anyhow!("outreach signing key is invalid"))?;
        mac.update(b"ballotradar-recipient-audit\0");
        mac.update(recipient.as_bytes());
        Ok(hex::encode(mac.finalize().into_bytes()))
    }

    pub fn issue_confirmation_nonce(
        &self,
        config: &OutreachComplianceConfig,
        token_id_hash: &str,
        issued_at: i64,
    ) -> Result<String> {
        let expires_at = issued_at
            .checked_add(config.confirmation_ttl_seconds)
            .ok_or_else(|| anyhow!("confirmation expiration overflow"))?;
        let mut random_id = [0_u8; RANDOM_ID_BYTES];
        OsRng.fill_bytes(&mut random_id);
        let random = URL_SAFE_NO_PAD.encode(random_id);
        let material = confirmation_material(
            &self.active_key_id,
            token_id_hash,
            &config.public_audience,
            issued_at,
            expires_at,
            &random,
        );
        let key = self
            .keys
            .get(&self.active_key_id)
            .ok_or_else(|| anyhow!("active outreach signing key is unavailable"))?;
        let mut mac = HmacSha256::new_from_slice(key)?;
        mac.update(&material);
        Ok(format!(
            "{CONFIRMATION_VERSION}.{}.{issued_at}.{expires_at}.{random}.{}",
            self.active_key_id,
            URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
        ))
    }

    pub fn verify_confirmation_nonce(
        &self,
        config: &OutreachComplianceConfig,
        token_id_hash: &str,
        nonce: &str,
        checked_at: i64,
    ) -> Result<()> {
        reject_path_segment(nonce)?;
        let parts = nonce.split('.').collect::<Vec<_>>();
        if parts.len() != 6 || parts[0] != CONFIRMATION_VERSION {
            bail!("invalid confirmation nonce");
        }
        let key_id = parts[1];
        validate_key_id(key_id).map_err(|_| anyhow!("invalid confirmation nonce"))?;
        let issued_at = parts[2]
            .parse::<i64>()
            .map_err(|_| anyhow!("invalid confirmation nonce"))?;
        let expires_at = parts[3]
            .parse::<i64>()
            .map_err(|_| anyhow!("invalid confirmation nonce"))?;
        if issued_at > checked_at || expires_at <= checked_at {
            bail!("invalid confirmation nonce");
        }
        let random = URL_SAFE_NO_PAD
            .decode(parts[4].as_bytes())
            .map_err(|_| anyhow!("invalid confirmation nonce"))?;
        if random.len() != RANDOM_ID_BYTES || URL_SAFE_NO_PAD.encode(&random) != parts[4] {
            bail!("invalid confirmation nonce");
        }
        let provided_mac = URL_SAFE_NO_PAD
            .decode(parts[5].as_bytes())
            .map_err(|_| anyhow!("invalid confirmation nonce"))?;
        if provided_mac.len() != 32 || URL_SAFE_NO_PAD.encode(&provided_mac) != parts[5] {
            bail!("invalid confirmation nonce");
        }
        let key = self
            .keys
            .get(key_id)
            .ok_or_else(|| anyhow!("invalid confirmation nonce"))?;
        let material = confirmation_material(
            key_id,
            token_id_hash,
            &config.public_audience,
            issued_at,
            expires_at,
            parts[4],
        );
        let mut verifier = HmacSha256::new_from_slice(key)?;
        verifier.update(&material);
        verifier
            .verify_slice(&provided_mac)
            .map_err(|_| anyhow!("invalid confirmation nonce"))
    }

    fn token_mac(&self, key_id: &str, random: &str, audience: &str) -> Result<Vec<u8>> {
        let key = self
            .keys
            .get(key_id)
            .ok_or_else(|| anyhow!("active outreach signing key is unavailable"))?;
        let mut mac = HmacSha256::new_from_slice(key)?;
        mac.update(&token_material(key_id, random, audience));
        Ok(mac.finalize().into_bytes().to_vec())
    }
}

fn token_material(key_id: &str, random: &str, audience: &str) -> Vec<u8> {
    [
        TOKEN_VERSION.as_bytes(),
        key_id.as_bytes(),
        random.as_bytes(),
        audience.as_bytes(),
        TOKEN_PURPOSE.as_bytes(),
    ]
    .join(&0)
}

fn confirmation_material(
    key_id: &str,
    token_id_hash: &str,
    audience: &str,
    issued_at: i64,
    expires_at: i64,
    random: &str,
) -> Vec<u8> {
    let issued_at = issued_at.to_string();
    let expires_at = expires_at.to_string();
    [
        CONFIRMATION_VERSION.as_bytes(),
        key_id.as_bytes(),
        token_id_hash.as_bytes(),
        audience.as_bytes(),
        issued_at.as_bytes(),
        expires_at.as_bytes(),
        random.as_bytes(),
        CONFIRMATION_PURPOSE.as_bytes(),
    ]
    .join(&0)
}

fn required_env(name: &str) -> Result<String> {
    std::env::var(name)
        .with_context(|| format!("required commercial-outreach configuration {name} is missing"))
        .and_then(|value| {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                bail!("required commercial-outreach configuration {name} is empty");
            }
            Ok(trimmed.to_string())
        })
}

fn parse_days(name: &str, value: &str) -> Result<i64> {
    let days = value
        .parse::<i64>()
        .with_context(|| format!("{name} must be an integer"))?;
    days.checked_mul(24 * 60 * 60)
        .ok_or_else(|| anyhow!("{name} is too large"))
}

fn validate_postal_address(value: &str) -> Result<()> {
    let lower = value.to_ascii_lowercase();
    if value.len() < 12
        || value.contains('[')
        || value.contains(']')
        || lower.contains("placeholder")
        || lower.contains("tbd")
        || !value.chars().any(|character| character.is_ascii_digit())
    {
        bail!("BALLOTRADAR_OUTREACH_POSTAL_ADDRESS is missing or unverified");
    }
    reject_header_injection("BALLOTRADAR_OUTREACH_POSTAL_ADDRESS", value)
}

fn validate_ballotradar_mailbox(name: &str, value: &str) -> Result<String> {
    reject_header_injection(name, value)?;
    let normalized = value.trim().to_ascii_lowercase();
    let (local, domain) = normalized
        .rsplit_once('@')
        .ok_or_else(|| anyhow!("{name} must be a BallotRadar mailbox"))?;
    if local.is_empty() || domain != "ballotradar.com" {
        bail!("{name} must be a BallotRadar mailbox");
    }
    Ok(normalized)
}

fn reject_header_injection(name: &str, value: &str) -> Result<()> {
    if value.contains('\r') || value.contains('\n') {
        bail!("{name} cannot contain newline characters");
    }
    Ok(())
}

fn reject_path_segment(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 512
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        bail!("invalid opaque token");
    }
    Ok(())
}

fn validate_key_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 32
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("outreach signing key ID is invalid");
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> OutreachComplianceConfig {
        OutreachComplianceConfig {
            public_base_url: Url::parse("https://email.ballotradar.com/u").unwrap(),
            public_audience: "email.ballotradar.com".into(),
            postal_address: "123 Verified Avenue, Conroe, TX 77301".into(),
            from_address: "alex@ballotradar.com".into(),
            from_name: "Alex Lewis, BallotRadar".into(),
            reply_to: "alex@ballotradar.com".into(),
            unsubscribe_mailbox: "unsubscribe@ballotradar.com".into(),
            configuration_set: "outreach-reply-first".into(),
            contact_list: "ballotradar-sales".into(),
            contact_topic: "commercial-outreach".into(),
            token_ttl_seconds: MIN_UNSUBSCRIBE_TOKEN_TTL_SECONDS,
            confirmation_ttl_seconds: 600,
            secret_id: "secret-id".into(),
            compliance_table: "compliance-table".into(),
            aws_profile: "profile".into(),
            aws_region: "us-east-1".into(),
        }
    }

    fn secrets() -> OutreachSecrets {
        let key = URL_SAFE_NO_PAD.encode([7_u8; 32]);
        OutreachSecrets::from_secret_json(&format!(
            r#"{{"active_key_id":"kid-1","keys":{{"kid-1":"{key}"}},"recipient_hash_key":"{key}"}}"#
        ))
        .unwrap()
    }

    #[test]
    fn signed_token_is_opaque_audience_bound_and_constant_time_verified() {
        let config = config();
        let secrets = secrets();
        let token = secrets.issue_token(&config).unwrap();
        assert_eq!(
            config.unsubscribe_url(&token.raw).unwrap().as_str(),
            format!("https://email.ballotradar.com/u/{}", token.raw)
        );
        assert_eq!(token.raw.split('.').count(), 4);
        assert_eq!(
            secrets.verify_token(&config, &token.raw).unwrap(),
            token.token_id_hash
        );
        let mut other_audience = config.clone();
        other_audience.public_audience = "other.ballotradar.com".into();
        assert!(secrets.verify_token(&other_audience, &token.raw).is_err());
        let tampered = format!("{}A", &token.raw[..token.raw.len() - 1]);
        assert!(secrets.verify_token(&config, &tampered).is_err());
        assert!(!token.raw.contains('@'));
    }

    #[test]
    fn human_confirmation_nonce_is_token_bound_and_expires() {
        let config = config();
        let secrets = secrets();
        let nonce = secrets
            .issue_confirmation_nonce(&config, &sha256_hex(b"token"), 1_000)
            .unwrap();
        secrets
            .verify_confirmation_nonce(&config, &sha256_hex(b"token"), &nonce, 1_001)
            .unwrap();
        assert!(secrets
            .verify_confirmation_nonce(&config, &sha256_hex(b"other"), &nonce, 1_001)
            .is_err());
        assert!(secrets
            .verify_confirmation_nonce(&config, &sha256_hex(b"token"), &nonce, 1_600)
            .is_err());
    }

    #[test]
    fn key_ring_rejects_short_or_missing_active_keys() {
        let short = URL_SAFE_NO_PAD.encode([1_u8; 16]);
        assert!(OutreachSecrets::from_secret_json(&format!(
            r#"{{"active_key_id":"kid-1","keys":{{"kid-1":"{short}"}}}}"#
        ))
        .is_err());
        let key = URL_SAFE_NO_PAD.encode([1_u8; 32]);
        assert!(OutreachSecrets::from_secret_json(&format!(
            r#"{{"active_key_id":"kid-2","keys":{{"kid-1":"{key}"}}}}"#
        ))
        .is_err());
    }
}
