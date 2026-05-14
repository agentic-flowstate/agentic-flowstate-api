use anyhow::{Context, Result};
use async_imap::extensions::idle::IdleResponse;
use async_native_tls::TlsConnector;
use async_std::net::TcpStream;
use futures::StreamExt;
use mail_parser::{MessageParser, MimeHeaders};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use ticketing_system::{
    email_accounts, email_intake, emails, CreateEmailRequest, Email, EmailAccountInternal,
    SqlitePool,
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::email_attachment_safety::unique_attachment_filename;

type ImapSession = async_imap::Session<async_native_tls::TlsStream<TcpStream>>;
const EMAIL_FETCH_WINDOW: u32 = 500;
const SPAM_FOLDER: &str = "Junk";

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
                crate::system_log_helper::log_error(
                    &db_pool,
                    "email",
                    &format!("Failed to load email accounts: {}", e),
                    None,
                )
                .await;
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        };

        let active_emails: HashSet<String> =
            active_accounts.iter().map(|a| a.email.clone()).collect();

        // Stop watchers for removed accounts
        let removed: Vec<String> = watchers
            .keys()
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
                tracing::warn!(
                    "[IDLE] Restarting watchers for {}: some tasks died",
                    account.email
                );
                if let Some(old_handles) = watchers.remove(&account.email) {
                    for h in old_handles {
                        h.abort();
                    }
                }
            }

            tracing::info!("[IDLE] Starting watchers for account: {}", account.email);

            let folders = vec!["INBOX", "Sent"];

            let mut handles = Vec::new();
            for db_folder in folders {
                let pool = db_pool.clone();
                let acct = account.clone();
                let db_f = db_folder.to_string();

                let handle = tokio::spawn(async move {
                    idle_watcher(pool, acct, db_f).await;
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
async fn idle_watcher(db_pool: Arc<SqlitePool>, account: EmailAccountInternal, db_folder: String) {
    let mut backoff = Duration::from_secs(1);
    let max_backoff = Duration::from_secs(60);
    // IDLE timeout: 14 minutes to stay within NAT gateway timeouts (typically 15 min)
    // and well within the 29-minute RFC 2177 recommendation
    let idle_timeout = Duration::from_secs(14 * 60);

    loop {
        match idle_loop(&db_pool, &account, &db_folder, idle_timeout).await {
            Ok(()) => {
                // Clean exit (shouldn't happen in normal operation)
                tracing::info!(
                    "[IDLE] {} {} watcher exited cleanly",
                    account.email,
                    db_folder
                );
                backoff = Duration::from_secs(1);
            }
            Err(e) => {
                tracing::warn!(
                    "[IDLE] {} {} error: {:?} — reconnecting in {:?}",
                    account.email,
                    db_folder,
                    e,
                    backoff
                );
                email_accounts::update_fetch_status(
                    &db_pool,
                    &account.email,
                    "error",
                    Some(&format!("IDLE {}: {:?}", db_folder, e)),
                )
                .await
                .ok();
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

    let imap_folder = match select_logical_folder(&mut session, account, db_folder).await? {
        Some(folder) => folder,
        None => {
            session.logout().await.ok();
            tokio::time::sleep(Duration::from_secs(300)).await;
            return Ok(());
        }
    };

    // Initial full fetch to sync any missed messages
    if let Err(e) = fetch_folder(&mut session, db_pool, account, &imap_folder, db_folder).await {
        tracing::warn!(
            "[IDLE] Initial fetch failed for {} {}: {:?}",
            account.email,
            db_folder,
            e
        );
    }
    email_accounts::update_fetch_status(db_pool, &account.email, "ok", None)
        .await
        .ok();

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
                    account.email,
                    db_folder,
                    data.parsed()
                );
                // Fetch new messages
                if let Err(e) =
                    fetch_folder(&mut session, db_pool, account, &imap_folder, db_folder).await
                {
                    tracing::warn!(
                        "[IDLE] Fetch after event failed for {} {}: {:?}",
                        account.email,
                        db_folder,
                        e
                    );
                }
                email_accounts::update_fetch_status(db_pool, &account.email, "ok", None)
                    .await
                    .ok();
            }
            IdleResponse::Timeout => {
                // Normal — re-issue IDLE to keep connection alive
                tracing::debug!(
                    "[IDLE] {} {} timeout, re-issuing IDLE",
                    account.email,
                    db_folder
                );
            }
            IdleResponse::ManualInterrupt => {
                tracing::debug!(
                    "[IDLE] {} {} manually interrupted",
                    account.email,
                    db_folder
                );
                break;
            }
        }
    }

    session.logout().await.ok();
    Ok(())
}

/// Connect and fetch emails for a single account (both INBOX and Sent).
/// Used by the force sync endpoint — independent of the IDLE system.
pub async fn fetch_emails_for_account(
    db_pool: &SqlitePool,
    account: &EmailAccountInternal,
) -> Result<()> {
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

    let folders = vec!["INBOX", "Sent"];

    for db_folder in folders {
        if let Err(e) = fetch_logical_folder(&mut session, db_pool, account, db_folder).await {
            tracing::warn!(
                "Failed to fetch {} for {}: {:?}",
                db_folder,
                account.email,
                e
            );
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
) -> Result<bool> {
    let spam_rules = if db_folder == "INBOX" {
        match active_email_block_rules(db_pool, &account.email).await {
            Ok(rules) => rules,
            Err(e) => {
                tracing::warn!(
                    "Failed to load spam block rules for {}: {:?}",
                    account.email,
                    e
                );
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    // Select folder (re-select to get fresh EXISTS count)
    let mailbox = match session.select(imap_folder).await {
        Ok(m) => m,
        Err(e) => {
            tracing::debug!("Could not select folder {}: {:?}", imap_folder, e);
            return Ok(false);
        }
    };

    tracing::debug!(
        "{} has {} messages for {}",
        imap_folder,
        mailbox.exists,
        account.email
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
        let local_emails =
            emails::get_local_message_ids(db_pool, &account.email, db_folder).await?;
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
            tracing::info!(
                "Deleted {} stale emails from {} {}",
                count,
                account.email,
                db_folder
            );
        }
    }

    // Fetch a bounded recent window for new email ingestion. This keeps reconnects
    // cheap while avoiding the old 50-message ceiling that hid recent setup mail.
    let fetch_count = std::cmp::min(mailbox.exists, EMAIL_FETCH_WINDOW);
    if fetch_count == 0 {
        return Ok(true);
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
                        tracing::info!(
                            "Stored new email in {} from {}",
                            db_folder,
                            req.from_address
                        );

                        let mut was_auto_junked = false;
                        if let Some(rule) = matching_block_rule(&stored_email, &spam_rules) {
                            match move_blocked_email_to_junk(
                                session,
                                db_pool,
                                &stored_email,
                                uid,
                                rule,
                            )
                            .await
                            {
                                Ok(()) => was_auto_junked = true,
                                Err(e) => {
                                    tracing::warn!(
                                        "Failed to auto-move blocked email {} from {} to Junk: {:?}",
                                        stored_email.id,
                                        stored_email.from_address,
                                        e
                                    );
                                }
                            }
                        }

                        let attachment_count = parsed.attachment_count();
                        if attachment_count > 0 && !was_auto_junked {
                            let attachments_dir = dirs::home_dir()
                                .unwrap_or_default()
                                .join(".agentic-flowstate")
                                .join("attachments")
                                .join(stored_email.id.to_string());

                            if let Err(e) = std::fs::create_dir_all(&attachments_dir) {
                                tracing::warn!("Failed to create attachments dir: {:?}", e);
                            } else {
                                let mut used_filenames = HashSet::new();
                                for (index, attachment) in parsed.attachments().enumerate() {
                                    let raw_filename = attachment
                                        .attachment_name()
                                        .unwrap_or("unnamed")
                                        .to_string();
                                    let filename = unique_attachment_filename(
                                        &raw_filename,
                                        index + 1,
                                        &mut used_filenames,
                                    );
                                    let content_type = attachment
                                        .content_type()
                                        .map(|ct: &mail_parser::ContentType| ct.ctype().to_string())
                                        .unwrap_or_else(|| "application/octet-stream".to_string());
                                    let contents = attachment.contents();
                                    let size_bytes = contents.len() as i64;
                                    let file_path = attachments_dir.join(&filename);

                                    if let Err(e) = std::fs::write(&file_path, contents) {
                                        tracing::warn!(
                                            "Failed to write attachment {}: {:?}",
                                            filename,
                                            e
                                        );
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
                                    )
                                    .await
                                    {
                                        tracing::warn!(
                                            "Failed to store attachment record for {}: {:?}",
                                            filename,
                                            e
                                        );
                                    }
                                }
                            }
                        }

                        if let Err(e) = email_intake::process_email_intake(
                            db_pool,
                            stored_email.id,
                            "email_fetcher",
                        )
                        .await
                        {
                            tracing::warn!(
                                "Failed to run email intake for email {}: {:?}",
                                stored_email.id,
                                e
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to store email: {:?}", e);
                    }
                }
            }
        }
    }

    Ok(true)
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct EmailBlockRule {
    id: i64,
    rule_type: String,
    pattern: String,
}

async fn active_email_block_rules(pool: &SqlitePool, mailbox: &str) -> Result<Vec<EmailBlockRule>> {
    ensure_spam_schema(pool).await?;

    let rules = sqlx::query_as::<_, EmailBlockRule>(
        r#"
        SELECT id, rule_type, pattern
        FROM email_block_rules
        WHERE is_active = 1
          AND action = 'junk'
          AND mailbox IN ('*', ?)
        ORDER BY mailbox = ? DESC, updated_at DESC, id DESC
        "#,
    )
    .bind(mailbox)
    .bind(mailbox)
    .fetch_all(pool)
    .await
    .context("Failed to load active email block rules")?;

    Ok(rules)
}

async fn ensure_spam_schema(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS email_block_rules (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            mailbox TEXT NOT NULL DEFAULT '*',
            rule_type TEXT NOT NULL CHECK(rule_type IN ('sender', 'domain')),
            pattern TEXT NOT NULL,
            action TEXT NOT NULL DEFAULT 'junk' CHECK(action IN ('junk')),
            reason TEXT,
            is_active INTEGER NOT NULL DEFAULT 1,
            created_by TEXT NOT NULL DEFAULT 'system',
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            UNIQUE(mailbox, rule_type, pattern)
        )
        "#,
    )
    .execute(pool)
    .await
    .context("Failed to create email_block_rules")?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS email_spam_actions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            email_id INTEGER NOT NULL,
            mailbox TEXT NOT NULL,
            message_id TEXT NOT NULL,
            from_address TEXT NOT NULL,
            subject TEXT,
            action TEXT NOT NULL,
            rule_id INTEGER REFERENCES email_block_rules(id) ON DELETE SET NULL,
            provider_action TEXT,
            status TEXT NOT NULL,
            error TEXT,
            created_by TEXT NOT NULL DEFAULT 'system',
            created_at INTEGER NOT NULL
        )
        "#,
    )
    .execute(pool)
    .await
    .context("Failed to create email_spam_actions")?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_email_block_rules_active ON email_block_rules(is_active, mailbox, rule_type, pattern)",
    )
    .execute(pool)
    .await
    .context("Failed to index email_block_rules")?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_email_spam_actions_mailbox ON email_spam_actions(mailbox, created_at DESC)",
    )
    .execute(pool)
    .await
    .context("Failed to index email_spam_actions mailbox")?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_email_spam_actions_email ON email_spam_actions(email_id, created_at DESC)",
    )
    .execute(pool)
    .await
    .context("Failed to index email_spam_actions email")?;

    Ok(())
}

fn matching_block_rule<'a>(
    email: &Email,
    rules: &'a [EmailBlockRule],
) -> Option<&'a EmailBlockRule> {
    let sender = normalize_email_address(&email.from_address);
    let domain = email_domain(&sender);

    rules.iter().find(|rule| match rule.rule_type.as_str() {
        "sender" => sender == rule.pattern,
        "domain" => domain.as_deref() == Some(rule.pattern.as_str()),
        _ => false,
    })
}

async fn move_blocked_email_to_junk(
    session: &mut ImapSession,
    pool: &SqlitePool,
    email: &Email,
    uid: u32,
    rule: &EmailBlockRule,
) -> Result<()> {
    let now = chrono::Utc::now().timestamp();
    let mut provider_action = None;
    let mut status = "failed";
    let mut error = None;

    match session.uid_mv(uid.to_string(), SPAM_FOLDER).await {
        Ok(()) => {
            provider_action = Some(format!(
                "imap_uid_move:{}:{}->{}",
                email.mailbox, email.folder, SPAM_FOLDER
            ));
            if let Err(e) = emails::update_email_folders(pool, &[email.id], SPAM_FOLDER).await {
                error = Some(format!(
                    "Provider move succeeded but local update failed: {e:?}"
                ));
            } else {
                status = "success";
            }
        }
        Err(e) => {
            error = Some(format!("{e:?}"));
        }
    }

    sqlx::query(
        r#"
        INSERT INTO email_spam_actions
            (email_id, mailbox, message_id, from_address, subject, action, rule_id,
             provider_action, status, error, created_by, created_at)
        VALUES (?, ?, ?, ?, ?, 'rule_applied_auto', ?, ?, ?, ?, 'email_fetcher', ?)
        "#,
    )
    .bind(email.id)
    .bind(&email.mailbox)
    .bind(&email.message_id)
    .bind(&email.from_address)
    .bind(&email.subject)
    .bind(rule.id)
    .bind(&provider_action)
    .bind(status)
    .bind(&error)
    .bind(now)
    .execute(pool)
    .await
    .context("Failed to record automatic spam action")?;

    if status == "success" {
        tracing::info!(
            "Auto-moved blocked email {} from {} to Junk via {} rule {}",
            email.id,
            email.from_address,
            rule.rule_type,
            rule.id
        );
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "Automatic spam move failed for email {}: {}",
            email.id,
            error.unwrap_or_else(|| "unknown error".to_string())
        ))
    }
}

fn normalize_email_address(value: &str) -> String {
    value
        .trim()
        .trim_matches('<')
        .trim_matches('>')
        .to_ascii_lowercase()
}

fn email_domain(address: &str) -> Option<String> {
    address
        .rsplit_once('@')
        .map(|(_, domain)| domain.trim().to_ascii_lowercase())
        .filter(|domain| !domain.is_empty())
}

async fn fetch_logical_folder(
    session: &mut ImapSession,
    db_pool: &SqlitePool,
    account: &EmailAccountInternal,
    db_folder: &str,
) -> Result<()> {
    let mut last_error: Option<String> = None;
    for imap_folder in folder_candidates(db_folder) {
        match fetch_folder(session, db_pool, account, imap_folder, db_folder).await {
            Ok(true) => return Ok(()),
            Ok(false) => continue,
            Err(e) => last_error = Some(format!("{e:?}")),
        }
    }

    if let Some(error) = last_error {
        return Err(anyhow::anyhow!(
            "No usable IMAP folder for logical folder {}: {}",
            db_folder,
            error
        ));
    }

    tracing::debug!(
        "No IMAP folder found for logical folder {} on {}",
        db_folder,
        account.email
    );
    Ok(())
}

async fn select_logical_folder(
    session: &mut ImapSession,
    account: &EmailAccountInternal,
    db_folder: &str,
) -> Result<Option<String>> {
    for imap_folder in folder_candidates(db_folder) {
        match session.select(imap_folder).await {
            Ok(_) => return Ok(Some(imap_folder.to_string())),
            Err(e) => {
                tracing::debug!(
                    "[IDLE] Could not select {} for {}: {:?}",
                    imap_folder,
                    account.email,
                    e
                );
            }
        }
    }

    Ok(None)
}

fn folder_candidates(db_folder: &str) -> Vec<&'static str> {
    match db_folder {
        "INBOX" => vec!["INBOX"],
        "Sent" => vec![
            "Sent Items",
            "Sent",
            "Sent Messages",
            "Sent Mail",
            "[Gmail]/Sent Mail",
            "INBOX.Sent",
        ],
        _ => vec![],
    }
}
