use axum::{
    extract::{Extension, Query, State},
    response::sse::{Event, KeepAlive, Sse},
};
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::convert::Infallible;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;
use ticketing_system::{email_accounts, emails, Email, SqlitePool};

use crate::auth_middleware::AuthenticatedUser;

#[derive(Debug, Deserialize)]
pub struct EmailSubscribeQuery {
    pub mailbox: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct EmailEventPayload {
    pub folder: String,
    pub emails: Vec<Email>,
    pub total: i64,
    pub unread: i64,
}

fn hash_emails(emails: &[Email]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for e in emails {
        e.id.hash(&mut hasher);
        e.message_id.hash(&mut hasher);
        e.is_read.hash(&mut hasher);
        e.is_starred.hash(&mut hasher);
    }
    emails.len().hash(&mut hasher);
    hasher.finish()
}

/// GET /api/emails/subscribe?mailbox=X
/// SSE endpoint for real-time email updates, scoped to the authenticated user's accounts.
/// Polls the database every 3 seconds and pushes when data changes.
pub async fn subscribe_emails(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Query(params): Query<EmailSubscribeQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mailbox = params.mailbox;
    let user_id = user.user_id;

    let stream = async_stream::stream! {
        let mut last_inbox_hash: u64 = 0;
        let mut last_sent_hash: u64 = 0;
        let mut tick_count: u32 = 0;
        let mut user_mailboxes: Vec<String> = Vec::new();

        loop {
            // Refresh user's account list every 30 seconds (or on first tick)
            if tick_count % 10 == 0 {
                if let Ok(accounts) = email_accounts::list_email_accounts_for_user(&pool, &user_id, true).await {
                    user_mailboxes = accounts.into_iter().map(|a| a.email).collect();
                }
            }
            tick_count = tick_count.wrapping_add(1);

            for (folder, last_hash) in [("INBOX", &mut last_inbox_hash), ("Sent", &mut last_sent_hash)] {
                let email_list = if let Some(ref mb) = mailbox {
                    // Specific mailbox requested — verify it belongs to user
                    if user_mailboxes.contains(mb) {
                        emails::list_emails(&pool, mb, Some(folder), 100, 0).await
                    } else {
                        Ok(vec![])
                    }
                } else {
                    // All user's mailboxes
                    emails::list_emails_by_mailboxes(&pool, &user_mailboxes, folder, 100, 0).await
                };

                if let Ok(list) = email_list {
                    let hash = hash_emails(&list);
                    if hash != *last_hash {
                        *last_hash = hash;
                        let total = list.len() as i64;
                        let unread = list.iter().filter(|e| !e.is_read).count() as i64;
                        let payload = EmailEventPayload {
                            folder: folder.to_string(),
                            emails: list,
                            total,
                            unread,
                        };
                        if let Ok(json) = serde_json::to_string(&payload) {
                            yield Ok(Event::default().event("emails").data(json));
                        }
                    }
                }
            }

            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    };

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    )
}
