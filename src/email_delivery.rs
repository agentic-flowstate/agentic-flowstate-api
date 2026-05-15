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
}

#[derive(Debug, Clone)]
pub struct DeliveryResult {
    pub message_id: String,
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

    if identity.account.imap_host == PURELYMAIL_IMAP_HOST {
        send_purelymail_smtp(&identity, req, &message_id).await?;
        return Ok(DeliveryResult {
            message_id,
            source_mailbox: identity.account.email,
        });
    }

    let ses_message_id = send_ses_raw(&identity, req, &message_id).await?;
    Ok(DeliveryResult {
        message_id: ses_message_id,
        source_mailbox: identity.account.email,
    })
}

async fn send_ses_raw(
    identity: &ResolvedEmailIdentity,
    req: &OutboundEmail,
    message_id: &str,
) -> Result<String> {
    let mut config_loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(identity.account.aws_region.clone()));
    if let Some(ref profile) = identity.account.aws_profile {
        config_loader = config_loader.profile_name(profile);
    }
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

    let result = ses_client
        .send_email()
        .from_email_address(&req.from)
        .content(content)
        .send()
        .await
        .context("SES send failed")?;

    if let Some(provider_message_id) = result.message_id() {
        tracing::debug!(
            "SES accepted email with provider message id {} for RFC Message-ID {}",
            provider_message_id,
            message_id
        );
    }

    Ok(message_id.to_string())
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

    match (&req.body_text, &req.body_html) {
        (Some(text), Some(html)) => {
            builder = builder.text_body(text).html_body(html);
        }
        (Some(text), None) => {
            builder = builder.text_body(text);
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
