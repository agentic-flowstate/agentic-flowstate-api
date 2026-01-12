use anyhow::{Context, Result};
use async_native_tls::TlsConnector;
use async_std::net::TcpStream;
use mail_parser::MessageParser;
use std::sync::Arc;
use std::time::Duration;
use ticketing_system::{emails, CreateEmailRequest, SqlitePool};

/// Email account configuration
#[derive(Debug, Clone)]
pub struct EmailAccount {
    pub email: String,
    pub password: String,
    pub imap_host: String,
    pub imap_port: u16,
}

/// Start the background email fetcher task
pub fn start_email_fetcher(db_pool: Arc<SqlitePool>, accounts: Vec<EmailAccount>) {
    tokio::spawn(async move {
        let poll_interval = Duration::from_secs(60); // Check every minute

        loop {
            for account in &accounts {
                if let Err(e) = fetch_emails_for_account(&db_pool, account).await {
                    tracing::error!(
                        "Failed to fetch emails for {}: {:?}",
                        account.email,
                        e
                    );
                }
            }

            tokio::time::sleep(poll_interval).await;
        }
    });
}

/// Fetch emails for a single account (both INBOX and Sent folders)
async fn fetch_emails_for_account(db_pool: &SqlitePool, account: &EmailAccount) -> Result<()> {
    tracing::debug!("Fetching emails for {}", account.email);

    // Connect to IMAP server using async-std TcpStream
    let tcp_stream = TcpStream::connect(format!("{}:{}", account.imap_host, account.imap_port))
        .await
        .context("Failed to connect to IMAP server")?;

    let tls = TlsConnector::new();
    let tls_stream = tls
        .connect(&account.imap_host, tcp_stream)
        .await
        .context("TLS handshake failed")?;

    let client = async_imap::Client::new(tls_stream);

    // Login
    let mut session = client
        .login(&account.email, &account.password)
        .await
        .map_err(|e| anyhow::anyhow!("IMAP login failed: {:?}", e.0))?;

    // Fetch from both INBOX and Sent folders
    let folders = vec![
        ("INBOX", "INBOX"),
        ("Sent Items", "Sent"),  // WorkMail uses "Sent Items"
    ];

    for (imap_folder, db_folder) in folders {
        if let Err(e) = fetch_folder(&mut session, db_pool, account, imap_folder, db_folder).await {
            tracing::warn!("Failed to fetch {} for {}: {:?}", imap_folder, account.email, e);
        }
    }

    session.logout().await.ok();
    tracing::debug!("Finished fetching emails for {}", account.email);

    Ok(())
}

/// Fetch emails from a specific IMAP folder
async fn fetch_folder(
    session: &mut async_imap::Session<async_native_tls::TlsStream<TcpStream>>,
    db_pool: &SqlitePool,
    account: &EmailAccount,
    imap_folder: &str,
    db_folder: &str,
) -> Result<()> {
    // Select folder
    let mailbox = match session.select(imap_folder).await {
        Ok(m) => m,
        Err(e) => {
            tracing::debug!("Could not select folder {}: {:?}", imap_folder, e);
            return Ok(()); // Folder might not exist, that's OK
        }
    };

    tracing::debug!(
        "{} has {} messages for {}",
        imap_folder,
        mailbox.exists,
        account.email
    );

    // Get UIDs of messages we haven't fetched yet
    // For now, fetch the last 50 messages
    let fetch_count = std::cmp::min(mailbox.exists, 50);
    if fetch_count == 0 {
        return Ok(());
    }

    let start = mailbox.exists.saturating_sub(fetch_count) + 1;
    let range = format!("{}:*", start);

    // Fetch messages
    let messages_stream = session
        .fetch(&range, "(UID RFC822 INTERNALDATE)")
        .await
        .context("Failed to fetch messages")?;

    // Collect messages from stream
    use futures::StreamExt;
    let messages: Vec<_> = messages_stream.collect().await;

    let parser = MessageParser::default();

    for message_result in messages {
        let message = match message_result {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("Failed to fetch message: {:?}", e);
                continue;
            }
        };

        let uid = message.uid.unwrap_or(0);
        let message_id = format!("{}:{}:{}", account.email, db_folder, uid);

        // Check if we already have this email
        if emails::email_exists(db_pool, &message_id).await? {
            continue;
        }

        // Parse the message body
        if let Some(body) = message.body() {
            if let Some(parsed) = parser.parse(body) {
                let from_addr = parsed
                    .from()
                    .and_then(|f| f.first())
                    .map(|a| a.address().unwrap_or_default().to_string())
                    .unwrap_or_default();

                let from_name = parsed
                    .from()
                    .and_then(|f| f.first())
                    .and_then(|a| a.name())
                    .map(|s| s.to_string());

                let to_addresses: Vec<String> = parsed
                    .to()
                    .map(|list| {
                        list.iter()
                            .filter_map(|a| a.address())
                            .map(|s| s.to_string())
                            .collect()
                    })
                    .unwrap_or_default();

                let cc_addresses: Option<Vec<String>> = parsed.cc().map(|list| {
                    list.iter()
                        .filter_map(|a| a.address())
                        .map(|s| s.to_string())
                        .collect()
                });

                let subject = parsed.subject().map(|s| s.to_string());
                let body_text = parsed.body_text(0).map(|s| s.to_string());
                let body_html = parsed.body_html(0).map(|s| s.to_string());

                let received_at = parsed
                    .date()
                    .map(|d| d.to_timestamp())
                    .unwrap_or_else(|| chrono::Utc::now().timestamp());

                let in_reply_to = parsed.in_reply_to().as_text().map(|s| s.to_string());

                let thread_id = parsed
                    .thread_name()
                    .map(|s| s.to_string())
                    .or_else(|| in_reply_to.clone());

                let req = CreateEmailRequest {
                    message_id,
                    mailbox: account.email.clone(),
                    folder: db_folder.to_string(),
                    from_address: from_addr,
                    from_name,
                    to_addresses,
                    cc_addresses,
                    subject,
                    body_text,
                    body_html,
                    received_at,
                    thread_id,
                    in_reply_to,
                };

                if let Err(e) = emails::create_email(db_pool, &req).await {
                    tracing::warn!("Failed to store email: {:?}", e);
                } else {
                    tracing::info!("Stored new email in {} from {}", db_folder, req.from_address);
                }
            }
        }
    }

    Ok(())
}

/// Load email accounts from config file
pub fn load_email_accounts() -> Result<Vec<EmailAccount>> {
    let config_path = dirs::home_dir()
        .context("Could not determine home directory")?
        .join(".agentic-flowstate")
        .join("email-accounts.json");

    if !config_path.exists() {
        tracing::warn!(
            "No email accounts config found at {}. Email fetching disabled.",
            config_path.display()
        );
        return Ok(vec![]);
    }

    let content = std::fs::read_to_string(&config_path)
        .context("Failed to read email accounts config")?;

    let accounts: Vec<EmailAccountConfig> = serde_json::from_str(&content)
        .context("Failed to parse email accounts config")?;

    Ok(accounts
        .into_iter()
        .map(|a| EmailAccount {
            email: a.email,
            password: a.password,
            imap_host: a.imap_host.unwrap_or_else(|| "imap.mail.us-east-1.awsapps.com".to_string()),
            imap_port: a.imap_port.unwrap_or(993),
        })
        .collect())
}

#[derive(serde::Deserialize)]
struct EmailAccountConfig {
    email: String,
    password: String,
    imap_host: Option<String>,
    imap_port: Option<u16>,
}
