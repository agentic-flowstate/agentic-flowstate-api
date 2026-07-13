use anyhow::{anyhow, bail, Context, Result};
use async_native_tls::TlsConnector;
use async_std::net::TcpStream;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use futures::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use mail_builder::MessageBuilder;
use ticketing_system::{email_accounts, ResolvedEmailIdentity, SqlitePool};
use uuid::Uuid;

const PURELYMAIL_IMAP_HOST: &str = "imap.purelymail.com";
const PURELYMAIL_SMTP_HOST: &str = "smtp.purelymail.com";
const PURELYMAIL_SMTP_PORT: u16 = 465;

#[derive(Debug, Clone)]
pub struct OutboundEmail {
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body_text: Option<String>,
    pub body_html: Option<String>,
    pub reply_to: Option<String>,
    pub in_reply_to: Option<String>,
    /// Additional deterministic MIME headers. Header names and values are
    /// validated against CR/LF injection before serialization.
    pub headers: Vec<(String, String)>,
    /// Opaque SES message tags. Recipient addresses and other PII are forbidden.
    pub ses_tags: Vec<(String, String)>,
    /// Fail unless the resolved mailbox uses this exact SES configuration set.
    pub required_configuration_set: Option<String>,
    /// Durable outreach reservation rechecked immediately before the SES SDK
    /// submission call. Required whenever required_configuration_set is set.
    pub outreach_message_id: Option<String>,
    /// Keyed, non-PII recipient identifier used for the independent global
    /// suppression authority recheck immediately before SES submission.
    pub outreach_recipient_hash: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DeliveryResult {
    pub message_id: String,
    pub provider: Option<String>,
    pub provider_message_id: Option<String>,
    pub configuration_set: Option<String>,
    pub source_mailbox: String,
}

pub async fn send_outbound_email(
    pool: &SqlitePool,
    user_id: &str,
    req: &OutboundEmail,
) -> Result<DeliveryResult> {
    let identity =
        email_accounts::resolve_email_identity_for_user(pool, user_id, &req.from).await?;
    let message_id = format!("<{}@agentic-flowstate.local>", Uuid::new_v4());

    match identity.account.outbound_transport.as_str() {
        "smtp" => {
            if req.required_configuration_set.is_some() {
                bail!("commercial outreach requires the approved SES configuration set");
            }
            if identity.account.imap_host != PURELYMAIL_IMAP_HOST {
                bail!(
                    "SMTP outbound transport is not configured for IMAP host '{}'",
                    identity.account.imap_host
                );
            }
            send_purelymail_smtp(&identity, req, &message_id).await?;
            Ok(DeliveryResult {
                message_id,
                provider: None,
                provider_message_id: None,
                configuration_set: None,
                source_mailbox: identity.account.email,
            })
        }
        "ses" => {
            if req.required_configuration_set.is_some()
                && (req.outreach_message_id.is_none() || req.outreach_recipient_hash.is_none())
            {
                bail!("commercial outreach requires a durable reservation and recipient suppression key");
            }
            let configuration_set = identity
                .account
                .ses_configuration_set
                .clone()
                .ok_or_else(|| anyhow!("SES configuration set is required for '{}'", req.from))?;
            if let Some(required) = req.required_configuration_set.as_deref() {
                if configuration_set != required {
                    bail!(
                        "SES configuration set '{}' does not match required commercial-outreach set",
                        configuration_set
                    );
                }
            }
            let provider_message_id =
                send_ses_raw(pool, &identity, req, &message_id, &configuration_set).await?;
            Ok(DeliveryResult {
                message_id,
                provider: Some("ses".to_string()),
                provider_message_id: Some(provider_message_id),
                configuration_set: Some(configuration_set),
                source_mailbox: identity.account.email,
            })
        }
        other => bail!("Unsupported outbound transport '{}'", other),
    }
}

async fn send_ses_raw(
    pool: &SqlitePool,
    identity: &ResolvedEmailIdentity,
    req: &OutboundEmail,
    message_id: &str,
    configuration_set: &str,
) -> Result<String> {
    let profile = identity
        .account
        .aws_profile
        .as_deref()
        .ok_or_else(|| anyhow!("AWS profile is required for SES sender '{}'", req.from))?;
    let config_loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(identity.account.aws_region.clone()))
        .profile_name(profile);
    let config = config_loader.load().await;
    let ses_client = aws_sdk_sesv2::Client::new(&config);
    let raw_bytes = build_raw_message(req, message_id)?;

    let raw_message = aws_sdk_sesv2::types::RawMessage::builder()
        .data(aws_sdk_sesv2::primitives::Blob::new(raw_bytes))
        .build()
        .map_err(|e| anyhow!(e.to_string()))?;

    let content = aws_sdk_sesv2::types::EmailContent::builder()
        .raw(raw_message)
        .build();

    let email_tags = req
        .ses_tags
        .iter()
        .map(|(name, value)| {
            validate_ses_tag(name, value)?;
            aws_sdk_sesv2::types::MessageTag::builder()
                .name(name)
                .value(value)
                .build()
                .map_err(|error| anyhow!(error.to_string()))
        })
        .collect::<Result<Vec<_>>>()?;

    if let Some(outreach_message_id) = req.outreach_message_id.as_deref() {
        let recipient_hash = req
            .outreach_recipient_hash
            .as_deref()
            .ok_or_else(|| anyhow!("commercial outreach recipient suppression key is missing"))?;
        let compliance_config = crate::outreach_compliance::OutreachComplianceConfig::from_env()
            .context("final public suppression configuration recheck failed")?;
        let registry = compliance_config
            .load_registry()
            .await
            .context("final public suppression authority recheck failed")?;
        if registry.is_suppressed(recipient_hash).await? {
            bail!("final global public suppression recheck blocked SES submission");
        }
        let decision = ticketing_system::outreach::revalidate_reservation(
            pool,
            outreach_message_id,
            chrono::Utc::now().timestamp(),
        )
        .await
        .context("final commercial-outreach eligibility recheck failed")?;
        if !decision.eligible {
            bail!(
                "final commercial-outreach eligibility recheck blocked SES submission: {}",
                decision.reasons.join(", ")
            );
        }
    }

    let result = ses_client
        .send_email()
        .from_email_address(&req.from)
        .configuration_set_name(configuration_set)
        .content(content)
        .set_email_tags((!email_tags.is_empty()).then_some(email_tags))
        .send()
        .await
        .context("SES send failed")?;

    let provider_message_id = result
        .message_id()
        .ok_or_else(|| anyhow!("SES response did not include MessageId"))?
        .to_string();
    tracing::info!(
        component = "email_delivery",
        provider = "ses",
        provider_message_id,
        rfc_message_id = message_id,
        configuration_set,
        "SES accepted outbound email"
    );
    Ok(provider_message_id)
}

async fn send_purelymail_smtp(
    identity: &ResolvedEmailIdentity,
    req: &OutboundEmail,
    message_id: &str,
) -> Result<()> {
    let raw_bytes = build_raw_message(req, message_id)?;
    let stream = TcpStream::connect((PURELYMAIL_SMTP_HOST, PURELYMAIL_SMTP_PORT))
        .await
        .context("Failed to connect to Purely Mail SMTP")?;
    let tls = TlsConnector::new()
        .connect(PURELYMAIL_SMTP_HOST, stream)
        .await
        .context("Failed to negotiate Purely Mail SMTP TLS")?;
    let mut reader = BufReader::new(tls);

    read_smtp_response(&mut reader, &[220]).await?;
    smtp_command(&mut reader, "EHLO agentic-flowstate.local\r\n", &[250]).await?;
    smtp_command(&mut reader, "AUTH LOGIN\r\n", &[334]).await?;
    smtp_command(
        &mut reader,
        &format!("{}\r\n", STANDARD.encode(identity.account.email.as_bytes())),
        &[334],
    )
    .await?;
    smtp_command(
        &mut reader,
        &format!(
            "{}\r\n",
            STANDARD.encode(identity.account.password.as_bytes())
        ),
        &[235],
    )
    .await?;
    smtp_command(
        &mut reader,
        &format!("MAIL FROM:<{}>\r\n", req.from),
        &[250],
    )
    .await?;
    for recipient in req.to.iter().chain(req.cc.iter()).chain(req.bcc.iter()) {
        smtp_command(
            &mut reader,
            &format!("RCPT TO:<{}>\r\n", recipient),
            &[250, 251],
        )
        .await?;
    }
    smtp_command(&mut reader, "DATA\r\n", &[354]).await?;
    write_smtp_data(&mut reader, &raw_bytes).await?;
    read_smtp_response(&mut reader, &[250]).await?;
    smtp_command(&mut reader, "QUIT\r\n", &[221]).await?;

    Ok(())
}

fn build_raw_message(req: &OutboundEmail, message_id: &str) -> Result<Vec<u8>> {
    let mut builder = MessageBuilder::new()
        .from(req.from.as_str())
        .subject(req.subject.as_str())
        .header(
            "Message-ID",
            mail_builder::headers::raw::Raw::new(message_id),
        );

    for to in &req.to {
        builder = builder.to(to.as_str());
    }
    for cc in &req.cc {
        builder = builder.cc(cc.as_str());
    }
    for bcc in &req.bcc {
        builder = builder.bcc(bcc.as_str());
    }

    if let Some(in_reply_to) = &req.in_reply_to {
        builder = builder.in_reply_to(in_reply_to.as_str()).header(
            "References",
            mail_builder::headers::raw::Raw::new(in_reply_to.as_str()),
        );
    }
    if let Some(reply_to) = &req.reply_to {
        builder = builder.header("Reply-To", mail_builder::headers::raw::Raw::new(reply_to));
    }
    for (name, value) in &req.headers {
        validate_mime_header(name, value)?;
        builder = builder.header(
            name.as_str(),
            mail_builder::headers::raw::Raw::new(value.as_str()),
        );
    }

    match (&req.body_text, &req.body_html) {
        (Some(text), Some(html)) => {
            builder = builder.text_body(text).html_body(html);
        }
        (Some(text), None) => {
            builder = builder
                .text_body(text)
                .html_body(plain_text_email_html(text));
        }
        (None, Some(html)) => {
            builder = builder.html_body(html);
        }
        (None, None) => {
            builder = builder.text_body("");
        }
    }

    builder
        .write_to_vec()
        .map_err(|e| anyhow!("Failed to build MIME message: {}", e))
}

fn validate_mime_header(name: &str, value: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        || value.contains('\r')
        || value.contains('\n')
    {
        bail!("outbound MIME header is invalid");
    }
    Ok(())
}

fn validate_ses_tag(name: &str, value: &str) -> Result<()> {
    let valid = |text: &str| {
        !text.is_empty()
            && text.len() <= 256
            && text.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':' | b'/')
            })
    };
    if !valid(name) || !valid(value) {
        bail!("SES message tag is invalid");
    }
    Ok(())
}

fn plain_text_email_html(body: &str) -> String {
    let escaped = body
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;");
    format!(
        "<!doctype html><html><body><div style=\"white-space:pre-wrap;font-family:system-ui,-apple-system,sans-serif\">{escaped}</div></body></html>"
    )
}

async fn smtp_command<S>(
    reader: &mut BufReader<S>,
    command: &str,
    expected: &[u16],
) -> Result<String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    reader
        .get_mut()
        .write_all(command.as_bytes())
        .await
        .context("Failed to write SMTP command")?;
    reader
        .get_mut()
        .flush()
        .await
        .context("Failed to flush SMTP command")?;
    read_smtp_response(reader, expected).await
}

async fn read_smtp_response<S>(reader: &mut BufReader<S>, expected: &[u16]) -> Result<String>
where
    S: AsyncRead + Unpin,
{
    let mut response = String::new();
    loop {
        let mut line = String::new();
        let bytes = reader
            .read_line(&mut line)
            .await
            .context("Failed to read SMTP response")?;
        if bytes == 0 {
            bail!("SMTP server closed the connection");
        }
        let is_final = line.as_bytes().get(3) == Some(&b' ');
        response.push_str(&line);
        if is_final {
            break;
        }
    }

    let code = response
        .get(0..3)
        .ok_or_else(|| anyhow!("Malformed SMTP response: {}", response.trim_end()))?
        .parse::<u16>()
        .context("Malformed SMTP response code")?;
    if !expected.contains(&code) {
        bail!("Unexpected SMTP response {}: {}", code, response.trim_end());
    }

    Ok(response)
}

async fn write_smtp_data<S>(reader: &mut BufReader<S>, raw_bytes: &[u8]) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let writer = reader.get_mut();
    for raw_line in raw_bytes.split(|byte| *byte == b'\n') {
        let line = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
        if line.starts_with(b".") {
            writer
                .write_all(b".")
                .await
                .context("Failed to dot-stuff SMTP data")?;
        }
        writer
            .write_all(line)
            .await
            .context("Failed to write SMTP data")?;
        writer
            .write_all(b"\r\n")
            .await
            .context("Failed to terminate SMTP data line")?;
    }
    writer
        .write_all(b".\r\n")
        .await
        .context("Failed to finish SMTP data")?;
    writer.flush().await.context("Failed to flush SMTP data")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compliant_message() -> OutboundEmail {
        OutboundEmail {
            from: "Alex Lewis, BallotRadar <alex@ballotradar.com>".into(),
            to: vec!["recipient@example.com".into()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "A truthful subject".into(),
            body_text: Some("Advertisement — exact text footer".into()),
            body_html: Some("<p>Advertisement — exact HTML footer</p>".into()),
            reply_to: Some("alex@ballotradar.com".into()),
            in_reply_to: None,
            headers: vec![
                (
                    "List-Unsubscribe".into(),
                    "<https://email.ballotradar.com/u/opaque>, <mailto:unsubscribe@ballotradar.com?subject=unsubscribe>".into(),
                ),
                (
                    "List-Unsubscribe-Post".into(),
                    "List-Unsubscribe=One-Click".into(),
                ),
            ],
            ses_tags: vec![
                ("MessageClass".into(), "commercial-outreach".into()),
                ("outreach_message_id".into(), "OM-OPAQUE".into()),
            ],
            required_configuration_set: Some("outreach-reply-first".into()),
            outreach_message_id: Some("OM-OPAQUE".into()),
            outreach_recipient_hash: Some("recipient-hash".into()),
        }
    }

    #[test]
    fn raw_message_contains_rfc_8058_headers_exactly_once() {
        let raw = build_raw_message(&compliant_message(), "<message-id@agentic-flowstate.local>")
            .unwrap();
        let raw = String::from_utf8(raw).unwrap();
        assert_eq!(raw.matches("List-Unsubscribe:").count(), 1);
        assert_eq!(raw.matches("List-Unsubscribe-Post:").count(), 1);
        assert!(raw.contains("List-Unsubscribe=One-Click"));
        assert!(raw.contains("mailto:unsubscribe@ballotradar.com"));
    }

    #[test]
    fn custom_headers_and_ses_tags_reject_injection_or_pii_shaped_values() {
        let mut message = compliant_message();
        message.headers[0].1.push_str("\r\nBcc: victim@example.com");
        assert!(build_raw_message(&message, "<id@example.com>").is_err());
        assert!(validate_ses_tag("outreach_message_id", "OM-OPAQUE").is_ok());
        assert!(validate_ses_tag("outreach_message_id", "person name").is_err());
        assert!(validate_ses_tag("outreach_message_id", "person@example.com").is_err());
        assert!(validate_ses_tag("outreach_message_id", "").is_err());
    }
}
