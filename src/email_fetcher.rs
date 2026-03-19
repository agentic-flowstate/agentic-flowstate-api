use anyhow::{Context, Result};
use async_imap::extensions::idle::IdleResponse;
use async_native_tls::TlsConnector;
use async_std::net::TcpStream;
use futures::StreamExt;
use mail_parser::{MessageParser, MimeHeaders};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use ticketing_system::{email_accounts, emails, CreateEmailRequest, EmailAccountInternal, SqlitePool};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

type ImapSession = async_imap::Session<async_native_tls::TlsStream<TcpStream>>;

/// Shared signal that account handlers can use to wake the account manager immediately.
/// Call `notify_one()` after creating or deleting an email account.
static ACCOUNT_CHANGE_NOTIFY: once_cell::sync::Lazy<Arc<Notify>> =
    once_cell::sync::Lazy::new(|| Arc::new(Notify::new()));

/// Get the shared notify handle for signaling account changes.
pub fn account_change_signal() -> Arc<Notify> {
    ACCOUNT_CHANGE_NOTIFY.clone()
}

/// Start the IMAP IDLE email fetcher system.
/// Spawns a manager task that tracks active accounts and maintains per-account IDLE watchers.
pub fn start_email_fetcher(db_pool: Arc<SqlitePool>, shutdown: CancellationToken) {
    tokio::spawn(account_manager(db_pool, shutdown));
}

/// Manager task: periodically checks for account changes and spawns/kills IDLE watchers.
async fn account_manager(db_pool: Arc<SqlitePool>, shutdown: CancellationToken) {
    let notify = account_change_signal();
    // Map of email -> abort handle for running IDLE watchers
    let mut watchers: HashMap<String, Vec<tokio::task::JoinHandle<()>>> = HashMap::new();

    loop {
        // Load current active accounts
        let active_accounts = match email_accounts::get_active_accounts_internal(&db_pool).await {
            Ok(accounts) => accounts,
            Err(e) => {
                tracing::error!("Failed to load email accounts: {:?}", e);
                crate::system_log_helper::log_error(&db_pool, "email", &format!("Failed to load email accounts: {}", e), None).await;
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        };

        let active_emails: HashSet<String> = active_accounts.iter().map(|a| a.email.clone()).collect();

        // Stop watchers for removed accounts
        let removed: Vec<String> = watchers.keys()
            .filter(|email| !active_emails.contains(*email))
            .cloned()
            .collect();
        for email in removed {
            tracing::info!("[IDLE] Stopping watchers for removed account: {}", email);
            if let Some(handles) = watchers.remove(&email) {
                for h in handles {
                    h.abort();
                }
            }
        }

        // Start watchers for new accounts
        for account in &active_accounts {
            if watchers.contains_key(&account.email) {
                // Check if any watcher tasks have finished (crashed) and need restart
                let handles = watchers.get(&account.email).unwrap();
                let any_finished = handles.iter().any(|h| h.is_finished());
                if !any_finished {
                    continue; // All watchers still running
                }
                // Some watcher died, kill all and restart
                tracing::warn!("[IDLE] Restarting watchers for {}: some tasks died", account.email);
                if let Some(old_handles) = watchers.remove(&account.email) {
                    for h in old_handles {
                        h.abort();
                    }
                }
            }

            tracing::info!("[IDLE] Starting watchers for account: {}", account.email);

            let folders = vec![
                ("INBOX", "INBOX"),
                ("Sent Items", "Sent"),
            ];

            let mut handles = Vec::new();
            for (imap_folder, db_folder) in folders {
                let pool = db_pool.clone();
                let acct = account.clone();
                let imap_f = imap_folder.to_string();
                let db_f = db_folder.to_string();

                let handle = tokio::spawn(async move {
                    idle_watcher(pool, acct, imap_f, db_f).await;
                });
                handles.push(handle);
            }

            watchers.insert(account.email.clone(), handles);
        }

        // Wait for either a signal, a periodic check (every 60s), or shutdown
        tokio::select! {
            _ = shutdown.cancelled() => {
                tracing::info!("[IDLE] Shutdown signal received, stopping all watchers");
                for (email, handles) in watchers.drain() {
                    tracing::info!("[IDLE] Aborting watchers for {}", email);
                    for h in handles {
                        h.abort();
                    }
                }
                break;
            }
            _ = notify.notified() => {
                tracing::debug!("[IDLE] Account change signaled, refreshing watchers");
            }
            _ = tokio::time::sleep(Duration::from_secs(60)) => {
                tracing::debug!("[IDLE] Periodic watcher check");
            }
        }
    }
}

/// IDLE watcher for a single folder on a single account.
/// Maintains a persistent IMAP connection with IDLE, reconnects on failure.
async fn idle_watcher(
    db_pool: Arc<SqlitePool>,
    account: EmailAccountInternal,
    imap_folder: String,
    db_folder: String,
) {
    let mut backoff = Duration::from_secs(1);
    let max_backoff = Duration::from_secs(60);
    // IDLE timeout: 14 minutes to stay within NAT gateway timeouts (typically 15 min)
    // and well within the 29-minute RFC 2177 recommendation
    let idle_timeout = Duration::from_secs(14 * 60);

    loop {
        match idle_loop(&db_pool, &account, &imap_folder, &db_folder, idle_timeout).await {
            Ok(()) => {
                // Clean exit (shouldn't happen in normal operation)
                tracing::info!("[IDLE] {} {} watcher exited cleanly", account.email, db_folder);
                backoff = Duration::from_secs(1);
            }
            Err(e) => {
                tracing::warn!(
                    "[IDLE] {} {} error: {:?} — reconnecting in {:?}",
                    account.email, db_folder, e, backoff
                );
                email_accounts::update_fetch_status(
                    &db_pool, &account.email, "error",
                    Some(&format!("IDLE {}: {:?}", db_folder, e)),
                ).await.ok();
            }
        }

        tokio::time::sleep(backoff).await;
        backoff = std::cmp::min(backoff * 2, max_backoff);
    }
}

/// Single IDLE session loop: connect, select folder, initial fetch, then IDLE loop.
/// Returns Err on any connection/protocol error (caller will reconnect).
async fn idle_loop(
    db_pool: &SqlitePool,
    account: &EmailAccountInternal,
    imap_folder: &str,
    db_folder: &str,
    idle_timeout: Duration,
) -> Result<()> {
    // Connect
    let tcp_stream = TcpStream::connect(format!("{}:{}", account.imap_host, account.imap_port))
        .await
        .context("Failed to connect to IMAP server")?;

    let tls = TlsConnector::new();
    let tls_stream = tls
        .connect(&account.imap_host, tcp_stream)
        .await
        .context("TLS handshake failed")?;

    let client = async_imap::Client::new(tls_stream);

    let mut session = client
        .login(&account.email, &account.password)
        .await
        .map_err(|e| anyhow::anyhow!("IMAP login failed: {:?}", e.0))?;

    // Select folder
    let _mailbox = match session.select(imap_folder).await {
        Ok(m) => m,
        Err(e) => {
            tracing::debug!("[IDLE] Could not select {} for {}: {:?}", imap_folder, account.email, e);
            session.logout().await.ok();
            // Folder doesn't exist — sleep long and retry (maybe it gets created later)
            tokio::time::sleep(Duration::from_secs(300)).await;
            return Ok(());
        }
    };

    // Initial full fetch to sync any missed messages
    if let Err(e) = fetch_folder(&mut session, db_pool, account, imap_folder, db_folder).await {
        tracing::warn!("[IDLE] Initial fetch failed for {} {}: {:?}", account.email, db_folder, e);
    }
    email_accounts::update_fetch_status(db_pool, &account.email, "ok", None).await.ok();

    tracing::info!("[IDLE] {} {} entering IDLE loop", account.email, db_folder);

    // IDLE loop: take session ownership, wait for events, get session back
    loop {
        let mut idle_handle = session.idle();
        idle_handle.init().await.context("IDLE init failed")?;

        let (wait_future, _interrupt) = idle_handle.wait_with_timeout(idle_timeout);
        let idle_result = wait_future.await.context("IDLE wait failed")?;

        // Get session back
        session = idle_handle.done().await.context("IDLE done failed")?;

        match idle_result {
            IdleResponse::NewData(data) => {
                tracing::debug!(
                    "[IDLE] {} {} got event: {:?}",
                    account.email, db_folder,
                    data.parsed()
                );
                // Fetch new messages
                if let Err(e) = fetch_folder(&mut session, db_pool, account, imap_folder, db_folder).await {
                    tracing::warn!("[IDLE] Fetch after event failed for {} {}: {:?}", account.email, db_folder, e);
                }
                email_accounts::update_fetch_status(db_pool, &account.email, "ok", None).await.ok();
            }
            IdleResponse::Timeout => {
                // Normal — re-issue IDLE to keep connection alive
                tracing::debug!("[IDLE] {} {} timeout, re-issuing IDLE", account.email, db_folder);
            }
            IdleResponse::ManualInterrupt => {
                tracing::debug!("[IDLE] {} {} manually interrupted", account.email, db_folder);
                break;
            }
        }
    }

    session.logout().await.ok();
    Ok(())
}

/// Connect and fetch emails for a single account (both INBOX and Sent).
/// Used by the force sync endpoint — independent of the IDLE system.
pub async fn fetch_emails_for_account(db_pool: &SqlitePool, account: &EmailAccountInternal) -> Result<()> {
    tracing::debug!("Force-syncing emails for {}", account.email);

    let tcp_stream = TcpStream::connect(format!("{}:{}", account.imap_host, account.imap_port))
        .await
        .context("Failed to connect to IMAP server")?;

    let tls = TlsConnector::new();
    let tls_stream = tls
        .connect(&account.imap_host, tcp_stream)
        .await
        .context("TLS handshake failed")?;

    let client = async_imap::Client::new(tls_stream);

    let mut session = client
        .login(&account.email, &account.password)
        .await
        .map_err(|e| anyhow::anyhow!("IMAP login failed: {:?}", e.0))?;

    let folders = vec![
        ("INBOX", "INBOX"),
        ("Sent Items", "Sent"),
    ];

    for (imap_folder, db_folder) in folders {
        if let Err(e) = fetch_folder(&mut session, db_pool, account, imap_folder, db_folder).await {
            tracing::warn!("Failed to fetch {} for {}: {:?}", imap_folder, account.email, e);
        }
    }

    session.logout().await.ok();
    tracing::debug!("Force-sync completed for {}", account.email);
    Ok(())
}

/// Fetch emails from a specific IMAP folder — handles both deletion sync and new message ingestion.
async fn fetch_folder(
    session: &mut ImapSession,
    db_pool: &SqlitePool,
    account: &EmailAccountInternal,
    imap_folder: &str,
    db_folder: &str,
) -> Result<()> {
    // Select folder (re-select to get fresh EXISTS count)
    let mailbox = match session.select(imap_folder).await {
        Ok(m) => m,
        Err(e) => {
            tracing::debug!("Could not select folder {}: {:?}", imap_folder, e);
            return Ok(());
        }
    };

    tracing::debug!(
        "{} has {} messages for {}",
        imap_folder, mailbox.exists, account.email
    );

    // Get all UIDs on server via UID SEARCH ALL (lightweight — no bodies)
    let server_uids: HashSet<u32> = match session.uid_search("ALL").await {
        Ok(uids) => uids.into_iter().collect(),
        Err(e) => {
            tracing::warn!("UID SEARCH ALL failed for {}: {:?}", imap_folder, e);
            HashSet::new()
        }
    };

    // Sync deletions: remove local emails whose UIDs are no longer on server
    if !server_uids.is_empty() {
        let local_emails = emails::get_local_message_ids(db_pool, &account.email, db_folder).await?;
        let mut stale_ids = Vec::new();
        for (db_id, message_id) in &local_emails {
            if let Some(uid_str) = message_id.rsplit(':').next() {
                if let Ok(uid) = uid_str.parse::<u32>() {
                    if !server_uids.contains(&uid) {
                        stale_ids.push(*db_id);
                    }
                }
            }
        }
        if !stale_ids.is_empty() {
            let count = emails::delete_emails_by_ids(db_pool, &stale_ids).await?;
            tracing::info!("Deleted {} stale emails from {} {}", count, account.email, db_folder);
        }
    }

    // Fetch the last 50 messages for new email ingestion
    let fetch_count = std::cmp::min(mailbox.exists, 50);
    if fetch_count == 0 {
        return Ok(());
    }

    let start = mailbox.exists.saturating_sub(fetch_count) + 1;
    let range = format!("{}:*", start);

    let messages_stream = session
        .fetch(&range, "(UID RFC822 INTERNALDATE)")
        .await
        .context("Failed to fetch messages")?;

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

        if emails::email_exists(db_pool, &message_id).await? {
            continue;
        }

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

                match emails::create_email(db_pool, &req).await {
                    Ok(stored_email) => {
                        tracing::info!("Stored new email in {} from {}", db_folder, req.from_address);

                        let attachment_count = parsed.attachment_count();
                        if attachment_count > 0 {
                            let attachments_dir = dirs::home_dir()
                                .unwrap_or_default()
                                .join(".agentic-flowstate")
                                .join("attachments")
                                .join(stored_email.id.to_string());

                            if let Err(e) = std::fs::create_dir_all(&attachments_dir) {
                                tracing::warn!("Failed to create attachments dir: {:?}", e);
                            } else {
                                for attachment in parsed.attachments() {
                                    let filename = attachment.attachment_name()
                                        .unwrap_or("unnamed")
                                        .to_string();
                                    let content_type = attachment.content_type()
                                        .map(|ct: &mail_parser::ContentType| ct.ctype().to_string())
                                        .unwrap_or_else(|| "application/octet-stream".to_string());
                                    let contents = attachment.contents();
                                    let size_bytes = contents.len() as i64;
                                    let file_path = attachments_dir.join(&filename);

                                    if let Err(e) = std::fs::write(&file_path, contents) {
                                        tracing::warn!("Failed to write attachment {}: {:?}", filename, e);
                                        continue;
                                    }

                                    let stored_path = file_path.to_string_lossy().to_string();
                                    if let Err(e) = emails::create_attachment(
                                        db_pool,
                                        stored_email.id,
                                        &filename,
                                        &content_type,
                                        size_bytes,
                                        Some(&stored_path),
                                    ).await {
                                        tracing::warn!("Failed to store attachment record for {}: {:?}", filename, e);
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to store email: {:?}", e);
                    }
                }
            }
        }
    }

    Ok(())
}
