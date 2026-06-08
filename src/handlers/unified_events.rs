//! Unified SSE endpoint — single multiplexed connection for all data topics.
//!
//! Replaces individual SSE endpoints (data/subscribe, my-tickets/subscribe,
//! emails/subscribe, daily-plan/subscribe, conversations/subscribe, dms/subscribe,
//! meetings/subscribe) with ONE connection that handles all topics.
//!
//! Benefits:
//! - Single TCP connection instead of 5-7 simultaneous ones
//! - One keepalive ping covers everything (radio sleeps between pings)
//! - One polling loop with staggered topic checks
//! - Client subscribes to only the topics it needs

use axum::{
    extract::{Extension, Query, State},
    response::sse::{Event, KeepAlive, Sse},
};
use futures::stream::Stream;
use serde::Deserialize;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::convert::Infallible;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;
use ticketing_system::SqlitePool;

use crate::auth_middleware::AuthenticatedUser;

#[derive(Debug, Deserialize)]
pub struct UnifiedEventsQuery {
    /// Comma-separated list of topics to subscribe to.
    /// Available: tickets, emails, daily_plan, conversations, data, dms, meetings
    /// Default: tickets,emails,daily_plan
    pub topics: Option<String>,
    /// Organization for org-scoped topics (data, conversations)
    pub organization: Option<String>,
    /// Date for daily_plan topic (defaults to today)
    pub date: Option<String>,
    /// Mailbox filter for emails topic
    pub mailbox: Option<String>,
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
    Extension(user): Extension<AuthenticatedUser>,
    Query(params): Query<UnifiedEventsQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
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
                if let Ok(convs) = ticketing_system::conversations::list_conversations(
                    &pool, validated_org.as_deref(), Some(&user_id), None, None, None, None,
                ).await {
                    let summaries = match crate::handlers::conversations::conversation_summaries(&pool, convs).await {
                        Ok(summaries) => summaries,
                        Err(e) => {
                            tracing::error!("Failed to summarize conversations for unified SSE: {}", e);
                            tokio::time::sleep(Duration::from_secs(10)).await;
                            continue;
                        }
                    };
                    let hash = hash_conversation_list(&summaries);
                    if hash != hash_conversations {
                        hash_conversations = hash;
                        let payload = serde_json::json!({
                            "type": "sync",
                            "conversations": summaries,
                            "updated_at": chrono::Utc::now().timestamp(),
                        });
                        if let Ok(json) = serde_json::to_string(&payload) {
                            yield Ok(Event::default().event("conversations").data(json));
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

            // Single sleep for all topics — one radio wakeup per cycle
            tokio::time::sleep(Duration::from_secs(15)).await;
        }
    };

    // Single keepalive ping for all topics
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(30))
            .text("ping"),
    )
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
        summary.tool_call_count.hash(&mut hasher);
        summary.run_started_at.hash(&mut hasher);
        summary.last_tool_call_started_at_epoch.hash(&mut hasher);
    }
    convs.len().hash(&mut hasher);
    hasher.finish()
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
