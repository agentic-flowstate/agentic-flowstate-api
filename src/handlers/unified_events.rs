//! Unified SSE endpoint — single multiplexed connection for all data topics.
//!
//! Replaces individual SSE endpoints (data/subscribe, my-tickets/subscribe,
//! emails/subscribe, daily-plan/subscribe, conversations/subscribe, dms/subscribe,
//! meetings/subscribe, library snapshots) with ONE connection that handles all topics.
//!
//! Benefits:
//! - Single TCP connection instead of 5-7 simultaneous ones
//! - One keepalive ping covers everything (radio sleeps between pings)
//! - One polling loop with staggered topic checks
//! - Client subscribes to only the topics it needs

use axum::{
    extract::{Extension, Query, State},
    http::{HeaderMap, StatusCode},
    response::sse::{Event, KeepAlive, Sse},
};
use chrono::{DateTime, Utc};
use futures::stream::Stream;
use serde::Deserialize;
use sqlx::Row;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::convert::Infallible;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;
use ticketing_system::SqlitePool;

use crate::auth_middleware::AuthenticatedUser;

use super::chat_client_manager::ChatClientManager;
use super::resume_cursor::{extract_cursor, CursorError, ResumeQuery};

#[derive(Debug, Deserialize)]
pub struct UnifiedEventsQuery {
    /// Comma-separated list of topics to subscribe to.
    /// Available: tickets, emails, daily_plan, conversations, data, dms, meetings, library
    /// Default: tickets,emails,daily_plan
    pub topics: Option<String>,
    /// Organization for org-scoped topics (data, conversations, library)
    pub organization: Option<String>,
    /// Date for daily_plan topic (defaults to today)
    pub date: Option<String>,
    /// Mailbox filter for emails topic
    pub mailbox: Option<String>,
    /// Optional resume cursor. Currently used for conversation snapshot events.
    pub starting_after: Option<i64>,
}

impl UnifiedEventsQuery {
    fn resume_query(&self) -> ResumeQuery {
        ResumeQuery {
            starting_after: self.starting_after,
        }
    }
}

/// GET /api/events/subscribe?topics=tickets,emails,daily_plan&organization=X
///
/// Single multiplexed SSE connection. The client specifies which topics to
/// subscribe to. Events are tagged with an SSE event type matching the topic
/// name so the client can dispatch them to the right handler.
///
/// Event format:
///   event: tickets
///   data: {"type":"tickets","tickets":[...]}
///
///   event: emails
///   data: {"folder":"INBOX","emails":[...],"total":5,"unread":2}
///
///   event: daily_plan
///   data: {"type":"sync","plan":{...}}
///
///   event: badge_update
///   data: {"unread_emails":3}
pub async fn subscribe_unified_events(
    State(pool): State<Arc<SqlitePool>>,
    State(manager): State<Arc<ChatClientManager>>,
    Extension(user): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Query(params): Query<UnifiedEventsQuery>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, (StatusCode, String)> {
    let cursor = match extract_cursor(&headers, &params.resume_query()) {
        Ok(cursor) => cursor,
        Err(CursorError::Malformed(detail)) => return Err((StatusCode::BAD_REQUEST, detail)),
        Err(CursorError::Retention { .. }) => {
            unreachable!("extract_cursor never produces CursorError::Retention")
        }
    };

    let topics: HashSet<String> = params
        .topics
        .unwrap_or_else(|| "tickets,emails,daily_plan".to_string())
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();

    let user_id = user.user_id;
    let org = params.organization;
    let date = params.date;
    let mailbox = params.mailbox;
    let poll_interval = unified_events_poll_interval(&topics);
    let conversation_resume_after = if topics.contains("conversations") && cursor.is_resume() {
        Some(cursor.event_index)
    } else {
        None
    };

    tracing::info!(
        "[UNIFIED-SSE] user={} topics={:?} org={:?}",
        user_id,
        topics,
        org
    );

    let stream = async_stream::stream! {
        // Per-topic hash trackers
        let mut hash_tickets: u64 = 0;
        let mut hash_inbox: u64 = 0;
        let mut hash_sent: u64 = 0;
        let mut hash_daily_plan_val: u64 = 0;
        let mut hash_conversations: u64 = 0;
        let mut hash_dms: u64 = 0;
        let mut hash_meetings: u64 = 0;
        let mut hash_epics: u64 = 0;
        let mut hash_slices: u64 = 0;
        let mut hash_data_tickets: u64 = 0;
        let mut hash_library: u64 = 0;
        let mut conversations_caught_up = conversation_resume_after.is_none();

        // Cache org memberships (rarely change during session)
        let user_orgs = ticketing_system::memberships::list_user_organizations(&pool, &user_id)
            .await
            .unwrap_or_default();

        // Validate org parameter against memberships — only allow org-scoped
        // topics (data, conversations) for orgs the user is a member of
        let validated_org: Option<String> = org.as_ref().and_then(|o| {
            if user_orgs.iter().any(|m| m.organization == *o) {
                Some(o.clone())
            } else {
                tracing::warn!(
                    "[UNIFIED-SSE] user={} denied access to org={} (not a member)",
                    user_id, o
                );
                None
            }
        });

        // Cache email accounts
        let mut user_mailboxes: Vec<String> = Vec::new();
        let mut email_refresh_counter: u32 = 0;

        // Resolve daily plan date
        let plan_date = date.unwrap_or_else(|| chrono::Utc::now().format("%Y-%m-%d").to_string());

        // Track last unread count for badge_update events
        let mut last_unread_emails: i64 = -1;

        loop {
            // ── TICKETS (my tickets across all orgs) ─────────────────────
            if topics.contains("tickets") {
                let mut all_tickets = Vec::new();
                for membership in &user_orgs {
                    if let Ok(org_tickets) = ticketing_system::tickets::list_tickets_by_organization(
                        &pool,
                        &membership.organization,
                    ).await {
                        all_tickets.extend(org_tickets);
                    }
                }

                let hash = hash_ticket_list(&all_tickets);
                if hash != hash_tickets {
                    hash_tickets = hash;
                    let payload = serde_json::json!({
                        "type": "tickets",
                        "tickets": all_tickets,
                    });
                    if let Ok(json) = serde_json::to_string(&payload) {
                        yield Ok(Event::default().event("tickets").data(json));
                    }
                }
            }

            // ── EMAILS ───────────────────────────────────────────────────
            if topics.contains("emails") {
                // Refresh account list periodically
                if email_refresh_counter % 10 == 0 {
                    if let Ok(accounts) = ticketing_system::email_accounts::list_email_accounts_for_user(
                        &pool, &user_id, true,
                    ).await {
                        user_mailboxes = accounts.into_iter().map(|a| a.email).collect();
                    }
                }
                email_refresh_counter = email_refresh_counter.wrapping_add(1);

                for (folder, last_hash) in [("INBOX", &mut hash_inbox), ("Sent", &mut hash_sent)] {
                    let email_list = if let Some(ref mb) = mailbox {
                        if user_mailboxes.contains(mb) {
                            ticketing_system::emails::list_emails(&pool, mb, Some(folder), 100, 0).await
                        } else {
                            Ok(vec![])
                        }
                    } else {
                        ticketing_system::emails::list_emails_by_mailboxes(
                            &pool, &user_mailboxes, folder, 100, 0,
                        ).await
                    };

                    if let Ok(list) = email_list {
                        let hash = hash_email_list(&list);
                        if hash != *last_hash {
                            *last_hash = hash;
                            let total = list.len() as i64;
                            let unread = list.iter().filter(|e| !e.is_read).count() as i64;

                            let payload = serde_json::json!({
                                "folder": folder,
                                "emails": list,
                                "total": total,
                                "unread": unread,
                            });
                            if let Ok(json) = serde_json::to_string(&payload) {
                                yield Ok(Event::default().event("emails").data(json));
                            }

                            // Emit badge_update for INBOX unread changes
                            if folder == "INBOX" && unread != last_unread_emails {
                                last_unread_emails = unread;
                                let badge = serde_json::json!({
                                    "unread_emails": unread,
                                });
                                if let Ok(json) = serde_json::to_string(&badge) {
                                    yield Ok(Event::default().event("badge_update").data(json));
                                }
                            }
                        }
                    }
                }
            }

            // ── DAILY PLAN ───────────────────────────────────────────────
            if topics.contains("daily_plan") {
                if let Ok(plan) = ticketing_system::daily_plan::get_plan_for_date(
                    &pool, &user_id, &plan_date,
                ).await {
                    let hash = hash_daily_plan(&plan);
                    if hash != hash_daily_plan_val {
                        hash_daily_plan_val = hash;
                        let payload = serde_json::json!({
                            "type": "sync",
                            "plan": plan,
                        });
                        if let Ok(json) = serde_json::to_string(&payload) {
                            yield Ok(Event::default().event("daily_plan").data(json));
                        }
                    }
                }
            }

            // ── CONVERSATIONS ────────────────────────────────────────────
            if topics.contains("conversations") {
                if let Ok(mut convs) = ticketing_system::conversations::list_conversations(
                    &pool, validated_org.as_deref(), Some(&user_id), None, None, None, None,
                ).await {
                    for conv in &mut convs {
                        if conv.is_active == Some(true) {
                            match crate::handlers::conversations::conversation_run_status_snapshot(
                                &pool,
                                &conv.id,
                                Some(manager.as_ref()),
                            ).await {
                                Ok(status) => {
                                    conv.is_active = Some(status.is_processing);
                                }
                                Err(e) => {
                                    tracing::error!(
                                        conversation_id = %conv.id,
                                        "Failed to normalize conversation run status for unified SSE: {}",
                                        e
                                    );
                                }
                            }
                        }
                    }
                    let summaries = match crate::handlers::conversations::conversation_summaries(&pool, convs).await {
                        Ok(summaries) => summaries,
                        Err(e) => {
                            tracing::error!("Failed to summarize conversations for unified SSE: {}", e);
                            tokio::time::sleep(Duration::from_secs(10)).await;
                            continue;
                        }
                    };
                    let hash = hash_conversation_list(&summaries);
                    let event_id = conversation_snapshot_event_id(&summaries);
                    let should_emit = should_emit_conversation_snapshot(
                        hash,
                        event_id,
                        conversation_resume_after,
                        &mut conversations_caught_up,
                        &mut hash_conversations,
                    );
                    if should_emit {
                        hash_conversations = hash;
                        let payload = serde_json::json!({
                            "type": "sync",
                            "conversations": summaries,
                            "updated_at": chrono::Utc::now().timestamp(),
                        });
                        if let Ok(json) = serde_json::to_string(&payload) {
                            yield Ok(Event::default().id(event_id.to_string()).event("conversations").data(json));
                        }
                    }
                }
            }

            // ── DMS ──────────────────────────────────────────────────────
            if topics.contains("dms") {
                if let Ok(dms) = ticketing_system::dms::list_user_dms(&pool, &user_id).await {
                    let hash = hash_dm_list(&dms);
                    if hash != hash_dms {
                        hash_dms = hash;
                        let payload = serde_json::json!({
                            "type": "dms",
                            "dms": dms,
                        });
                        if let Ok(json) = serde_json::to_string(&payload) {
                            yield Ok(Event::default().event("dms").data(json));
                        }
                    }
                }
            }

            // ── MEETINGS ─────────────────────────────────────────────────
            if topics.contains("meetings") {
                if let Ok(meetings) = ticketing_system::meetings::list_meetings(&pool, false).await {
                    let mut hasher = DefaultHasher::new();
                    for m in &meetings {
                        m.room_id.hash(&mut hasher);
                        m.status.hash(&mut hasher);
                    }
                    meetings.len().hash(&mut hasher);
                    let hash = hasher.finish();

                    if hash != hash_meetings {
                        hash_meetings = hash;
                        let payload = serde_json::json!({
                            "type": "meetings",
                            "meetings": meetings,
                        });
                        if let Ok(json) = serde_json::to_string(&payload) {
                            yield Ok(Event::default().event("meetings").data(json));
                        }
                    }
                }
            }

            // ── LIBRARY (artifacts/documents for one org) ───────────────
            if topics.contains("library") {
                if let Some(ref org_name) = validated_org {
                    let artifacts_result = list_visible_library_artifacts(&pool, org_name).await;
                    let documents_result = ticketing_system::documents::list_documents(
                        &pool, org_name, None, None, None,
                    ).await;

                    if let (Ok(artifacts), Ok(documents)) = (artifacts_result, documents_result) {
                        let hash = hash_library_lists(&artifacts, &documents);
                        if hash != hash_library {
                            hash_library = hash;
                            let payload = serde_json::json!({
                                "type": "library",
                                "artifacts": artifacts,
                                "documents": documents,
                            });
                            if let Ok(json) = serde_json::to_string(&payload) {
                                yield Ok(Event::default().event("library").data(json));
                            }
                        }
                    }
                }
            }

            // ── DATA (workspace: epics/slices/tickets for one org) ───────
            if topics.contains("data") {
                if let Some(ref org_name) = validated_org {
                    if let Ok(epic_list) = ticketing_system::epics::list_epics(&pool, Some(org_name)).await {
                        let eh = hash_epic_list(&epic_list);
                        if eh != hash_epics {
                            hash_epics = eh;
                            let payload = serde_json::json!({
                                "type": "epics",
                                "epics": epic_list,
                            });
                            if let Ok(json) = serde_json::to_string(&payload) {
                                yield Ok(Event::default().event("data").data(json));
                            }
                        }

                        let mut all_slices = Vec::new();
                        for epic in &epic_list {
                            if let Ok(slice_list) = ticketing_system::slices::list_slices(&pool, org_name, &epic.epic_id).await {
                                all_slices.extend(slice_list);
                            }
                        }
                        let sh = hash_slice_list(&all_slices);
                        if sh != hash_slices {
                            hash_slices = sh;
                            let payload = serde_json::json!({
                                "type": "slices",
                                "slices": all_slices,
                            });
                            if let Ok(json) = serde_json::to_string(&payload) {
                                yield Ok(Event::default().event("data").data(json));
                            }
                        }

                        let mut all_tickets = Vec::new();
                        for slice in &all_slices {
                            if let Ok(ticket_list) = ticketing_system::tickets::list_tickets(
                                &pool, org_name, &slice.epic_id, &slice.slice_id,
                            ).await {
                                all_tickets.extend(ticket_list);
                            }
                        }
                        let th = hash_ticket_list(&all_tickets);
                        if th != hash_data_tickets {
                            hash_data_tickets = th;
                            let payload = serde_json::json!({
                                "type": "tickets",
                                "tickets": all_tickets,
                            });
                            if let Ok(json) = serde_json::to_string(&payload) {
                                yield Ok(Event::default().event("data").data(json));
                            }
                        }
                    }
                }
            }

            // ── RESTART PENDING (check flag file) ────────────────────
            // Zero-cost addition to existing poll loop — just a file stat.
            // Pushes restart notification to active clients immediately.
            {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/jarvisgpt".to_string());
                let pending_path = format!("{}/.agentic-flowstate/pending_restart.json", home);
                if let Ok(contents) = tokio::fs::read_to_string(&pending_path).await {
                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&contents) {
                        let restart_type = parsed.get("type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("restart");
                        let payload = serde_json::json!({
                            "pending": true,
                            "type": restart_type,
                        });
                        if let Ok(json) = serde_json::to_string(&payload) {
                            yield Ok(Event::default().event("restart_pending").data(json));
                        }
                    }
                }
            }

            // Conversation-only desktop streams need to reflect cross-device
            // agent starts quickly. Keep broad multi-topic streams conservative.
            tokio::time::sleep(poll_interval).await;
        }
    };

    // Single keepalive ping for all topics
    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(30))
            .text("ping"),
    ))
}

// ── Hash helpers ─────────────────────────────────────────────────────────────

fn hash_ticket_list(tickets: &[ticketing_system::Ticket]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for t in tickets {
        t.ticket_id.hash(&mut hasher);
        t.updated_at_iso.hash(&mut hasher);
        t.status.hash(&mut hasher);
        t.title.hash(&mut hasher);
    }
    tickets.len().hash(&mut hasher);
    hasher.finish()
}

fn hash_email_list(emails: &[ticketing_system::Email]) -> u64 {
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

fn hash_daily_plan(plan: &ticketing_system::DailyPlanView) -> u64 {
    let mut hasher = DefaultHasher::new();
    plan.items.len().hash(&mut hasher);
    for item in &plan.items {
        item.item_id.hash(&mut hasher);
        item.title.hash(&mut hasher);
        item.scheduled_time.hash(&mut hasher);
        item.checked.hash(&mut hasher);
        item.sort_order.hash(&mut hasher);
    }
    hasher.finish()
}

fn hash_conversation_list(convs: &[crate::handlers::conversations::ConversationSummary]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for summary in convs {
        summary.conversation.updated_at.hash(&mut hasher);
        summary.conversation.id.hash(&mut hasher);
        summary
            .conversation
            .parent_conversation_id
            .hash(&mut hasher);
        summary.conversation.conversation_role.hash(&mut hasher);
        summary
            .conversation
            .child_conversation_count
            .hash(&mut hasher);
        summary.conversation.status.hash(&mut hasher);
        summary.conversation.message_count.hash(&mut hasher);
        summary.conversation.last_event_index.hash(&mut hasher);
        summary.conversation.last_read_event_index.hash(&mut hasher);
        summary.conversation.unread_event_count.hash(&mut hasher);
        summary.conversation.is_active.hash(&mut hasher);
        summary.tool_call_count.hash(&mut hasher);
        summary.run_started_at.hash(&mut hasher);
        summary.last_tool_call_started_at_epoch.hash(&mut hasher);
    }
    convs.len().hash(&mut hasher);
    hasher.finish()
}

fn conversation_snapshot_event_id(
    convs: &[crate::handlers::conversations::ConversationSummary],
) -> i64 {
    convs.iter().fold(0_i64, |max_seen, summary| {
        let updated_at = parse_conversation_timestamp_millis(&summary.conversation.updated_at);
        let last_event = summary
            .conversation
            .last_event_index
            .map(i64::from)
            .unwrap_or(0);
        let run_started_at = summary
            .run_started_at
            .map(epoch_seconds_to_millis)
            .unwrap_or(0);
        let last_tool_call = summary
            .last_tool_call_started_at_epoch
            .map(epoch_seconds_to_millis)
            .unwrap_or(0);
        max_seen
            .max(updated_at)
            .max(last_event)
            .max(run_started_at)
            .max(last_tool_call)
    })
}

fn should_emit_conversation_snapshot(
    hash: u64,
    event_id: i64,
    resume_after: Option<i64>,
    caught_up: &mut bool,
    last_hash: &mut u64,
) -> bool {
    if !*caught_up {
        *caught_up = true;
        if resume_after.is_some_and(|cursor| event_id <= cursor) {
            *last_hash = hash;
            return false;
        }
    }

    hash != *last_hash
}

fn parse_conversation_timestamp_millis(value: &str) -> i64 {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc).timestamp_millis())
        .unwrap_or(0)
}

fn epoch_seconds_to_millis(value: i64) -> i64 {
    value.saturating_mul(1_000)
}

fn hash_dm_list(dms: &[ticketing_system::DmConversation]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for dm in dms {
        dm.id.hash(&mut hasher);
        dm.updated_at.hash(&mut hasher);
        dm.unread_count.hash(&mut hasher);
    }
    dms.len().hash(&mut hasher);
    hasher.finish()
}

fn hash_epic_list(epics: &[ticketing_system::Epic]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for e in epics {
        e.epic_id.hash(&mut hasher);
        e.updated_at_iso.hash(&mut hasher);
        e.title.hash(&mut hasher);
    }
    epics.len().hash(&mut hasher);
    hasher.finish()
}

fn hash_slice_list(slices: &[ticketing_system::Slice]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for s in slices {
        s.slice_id.hash(&mut hasher);
        s.updated_at_iso.hash(&mut hasher);
        s.title.hash(&mut hasher);
    }
    slices.len().hash(&mut hasher);
    hasher.finish()
}

fn hash_library_lists(
    artifacts: &[ticketing_system::ArtifactSummary],
    documents: &[ticketing_system::DocumentSummary],
) -> u64 {
    let mut hasher = DefaultHasher::new();
    for artifact in artifacts {
        artifact.artifact_id.hash(&mut hasher);
        artifact.title.hash(&mut hasher);
        artifact.artifact_type.hash(&mut hasher);
        artifact.ticket_id.hash(&mut hasher);
        artifact.updated_at_iso.hash(&mut hasher);
        artifact.content_length.hash(&mut hasher);
    }
    artifacts.len().hash(&mut hasher);

    for document in documents {
        document.document_id.hash(&mut hasher);
        document.filename.hash(&mut hasher);
        document.mime_type.hash(&mut hasher);
        document.size_bytes.hash(&mut hasher);
        document.ticket_id.hash(&mut hasher);
        document.updated_at_iso.hash(&mut hasher);
    }
    documents.len().hash(&mut hasher);
    hasher.finish()
}

async fn list_visible_library_artifacts(
    pool: &SqlitePool,
    org: &str,
) -> anyhow::Result<Vec<ticketing_system::ArtifactSummary>> {
    let rows = sqlx::query(
        r#"
        SELECT artifact_id, title, length(content) AS content_length, artifact_type, created_by,
               source_step_id, organization, epic_id, slice_id, ticket_id,
               agent_run_id, owner_agent, produced_by_agent, source_uri,
               source_conversation_id, source_message_id, source_document_id,
               repository, metadata_json, created_at, updated_at
        FROM artifacts
        WHERE organization = ?
          AND lifecycle_status = 'active'
          AND visibility IN ('organization', 'system')
        ORDER BY created_at DESC
        "#,
    )
    .bind(org)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| ticketing_system::ArtifactSummary {
            artifact_id: row.get("artifact_id"),
            title: row.get("title"),
            artifact_type: row.get("artifact_type"),
            created_by: row.get("created_by"),
            source_step_id: row.get("source_step_id"),
            organization: row.get("organization"),
            epic_id: row.get("epic_id"),
            slice_id: row.get("slice_id"),
            ticket_id: row.get("ticket_id"),
            agent_run_id: row.get("agent_run_id"),
            owner_agent: row.get("owner_agent"),
            produced_by_agent: row.get("produced_by_agent"),
            source_uri: row.get("source_uri"),
            source_conversation_id: row.get("source_conversation_id"),
            source_message_id: row.get("source_message_id"),
            source_document_id: row.get("source_document_id"),
            repository: row.get("repository"),
            metadata: row
                .try_get::<String, _>("metadata_json")
                .ok()
                .and_then(|raw| serde_json::from_str(&raw).ok())
                .filter(serde_json::Value::is_object)
                .unwrap_or_else(|| serde_json::json!({})),
            content_length: usize::try_from(row.get::<i64, _>("content_length")).unwrap_or(0),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
            created_at_iso: timestamp_to_iso(row.get("created_at")),
            updated_at_iso: timestamp_to_iso(row.get("updated_at")),
        })
        .collect())
}

fn timestamp_to_iso(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_default()
}

fn unified_events_poll_interval(topics: &HashSet<String>) -> Duration {
    if topics.len() == 1 && topics.contains("conversations") {
        Duration::from_secs(2)
    } else {
        Duration::from_secs(15)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::handlers::conversations::ConversationSummary;
    use ticketing_system::{ArtifactSummary, Conversation, DocumentSummary};

    fn conversation_summary(
        updated_at: &str,
        last_event_index: Option<i32>,
        run_started_at: Option<i64>,
        last_tool_call_started_at_epoch: Option<i64>,
    ) -> ConversationSummary {
        ConversationSummary {
            conversation: Conversation {
                id: "conv-1".to_string(),
                user_id: "alex".to_string(),
                session_id: None,
                organization: "agentic-flowstate".to_string(),
                agent: Some("full-access".to_string()),
                conversation_type: Some("research".to_string()),
                parent_conversation_id: None,
                conversation_role: "standard".to_string(),
                child_conversation_count: Some(0),
                child_sort_order: None,
                title: "Conversation".to_string(),
                started_at: "2026-06-08T21:59:00Z".to_string(),
                updated_at: updated_at.to_string(),
                status: "open".to_string(),
                archived_at: None,
                router_ticket_id: None,
                router_organization: None,
                message_count: Some(1),
                last_event_index,
                last_read_event_index: None,
                unread_event_count: None,
                is_active: Some(false),
                messages: Some(vec![]),
            },
            tool_call_count: None,
            run_started_at,
            last_tool_call_started_at_epoch,
        }
    }

    fn artifact_summary(updated_at_iso: &str) -> ArtifactSummary {
        ArtifactSummary {
            artifact_id: "A-12345678".to_string(),
            title: "Research note".to_string(),
            artifact_type: "research".to_string(),
            created_by: "codex".to_string(),
            source_step_id: None,
            organization: "agentic-flowstate".to_string(),
            epic_id: Some("frontend".to_string()),
            slice_id: Some("ios-app".to_string()),
            ticket_id: Some("T-12345678".to_string()),
            agent_run_id: None,
            content_length: 512,
            created_at: 1_780_956_000,
            updated_at: 1_780_956_001,
            created_at_iso: "2026-06-08T22:00:00Z".to_string(),
            updated_at_iso: updated_at_iso.to_string(),
        }
    }

    fn document_summary(size_bytes: i64) -> DocumentSummary {
        DocumentSummary {
            document_id: "D-12345678".to_string(),
            filename: "brief.pdf".to_string(),
            mime_type: "application/pdf".to_string(),
            size_bytes,
            description: Some("Brief".to_string()),
            document_type: "pdf".to_string(),
            organization: "agentic-flowstate".to_string(),
            epic_id: Some("frontend".to_string()),
            slice_id: Some("ios-app".to_string()),
            ticket_id: Some("T-12345678".to_string()),
            created_by: "codex".to_string(),
            created_at: 1_780_956_000,
            updated_at: 1_780_956_001,
            created_at_iso: "2026-06-08T22:00:00Z".to_string(),
            updated_at_iso: "2026-06-08T22:00:01Z".to_string(),
        }
    }

    #[test]
    fn conversation_snapshot_event_id_uses_timestamp_millis() {
        let summaries = vec![conversation_summary(
            "2026-06-08T22:00:01.234Z",
            Some(128),
            None,
            None,
        )];

        assert_eq!(
            conversation_snapshot_event_id(&summaries),
            1_780_956_001_234
        );
    }

    #[test]
    fn conversation_snapshot_event_id_includes_active_run_times() {
        let summaries = vec![conversation_summary(
            "2026-06-08T22:00:01Z",
            Some(128),
            Some(1_780_956_700),
            Some(1_780_956_800),
        )];

        assert_eq!(
            conversation_snapshot_event_id(&summaries),
            1_780_956_800_000
        );
    }

    #[test]
    fn conversation_hash_includes_read_state() {
        let unread = vec![conversation_summary(
            "2026-06-08T22:00:01Z",
            Some(128),
            None,
            None,
        )];
        let mut read = unread.clone();
        read[0].conversation.last_read_event_index = Some(128);
        read[0].conversation.unread_event_count = Some(0);

        assert_ne!(
            hash_conversation_list(&unread),
            hash_conversation_list(&read)
        );
    }

    #[test]
    fn fresh_conversation_snapshot_emits_when_hash_changes() {
        let mut caught_up = true;
        let mut last_hash = 0;

        assert!(should_emit_conversation_snapshot(
            42,
            1_780_956_001_234,
            None,
            &mut caught_up,
            &mut last_hash,
        ));
    }

    #[test]
    fn up_to_date_resume_skips_initial_conversation_snapshot() {
        let mut caught_up = false;
        let mut last_hash = 0;

        assert!(!should_emit_conversation_snapshot(
            42,
            1_780_956_001_234,
            Some(1_780_956_001_234),
            &mut caught_up,
            &mut last_hash,
        ));
        assert!(caught_up);
        assert_eq!(last_hash, 42);
    }

    #[test]
    fn stale_resume_emits_current_conversation_snapshot() {
        let mut caught_up = false;
        let mut last_hash = 0;

        assert!(should_emit_conversation_snapshot(
            42,
            1_780_956_001_234,
            Some(1_780_956_001_000),
            &mut caught_up,
            &mut last_hash,
        ));
        assert!(caught_up);
        assert_eq!(last_hash, 0);
    }

    #[test]
    fn conversation_only_stream_uses_fast_poll_interval() {
        let topics = HashSet::from(["conversations".to_string()]);
        assert_eq!(
            unified_events_poll_interval(&topics),
            Duration::from_secs(2)
        );
    }

    #[test]
    fn mixed_topic_stream_keeps_conservative_poll_interval() {
        let topics = HashSet::from(["conversations".to_string(), "emails".to_string()]);
        assert_eq!(
            unified_events_poll_interval(&topics),
            Duration::from_secs(15)
        );
    }

    #[test]
    fn library_hash_changes_when_artifact_or_document_metadata_changes() {
        let artifacts = vec![artifact_summary("2026-06-08T22:00:01Z")];
        let documents = vec![document_summary(1024)];

        let first = hash_library_lists(&artifacts, &documents);
        assert_eq!(first, hash_library_lists(&artifacts, &documents));

        let changed_artifacts = vec![artifact_summary("2026-06-08T22:05:01Z")];
        assert_ne!(first, hash_library_lists(&changed_artifacts, &documents));

        let changed_documents = vec![document_summary(2048)];
        assert_ne!(first, hash_library_lists(&artifacts, &changed_documents));
    }
}
