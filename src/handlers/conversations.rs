use anyhow::Context;
use async_stream::stream;
use axum::{
    extract::{Extension, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    Json,
};
use futures::stream::Stream;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use ticketing_system::{
    agent_runners, checkpoints, conversation_turn_jobs, conversations, AddMessageRequest,
    BranchConversationRequest, Conversation, ConversationHierarchyScope, ConversationMessage,
    ConversationReadState, ConversationScaleCockpit, ConversationScaleCockpitOptions,
    CreateChildConversationRequest as TicketingCreateChildConversationRequest,
    CreateConversationRequest, SqlitePool, UpdateConversationRequest,
};

use super::chat_client_manager::ChatClientManager;
use super::chat_stream::{
    encode_codex_options_for_job, get_broadcast_sender, validate_codex_options, ChatCodexOptions,
    ChatRuntime,
};
use super::conversation_handoff::{
    resolve_context_handoff, ContextHandoffRequest, ResolvedContextHandoff,
};
use super::conversation_worker_manager::WORKER_MANAGER;
use super::resume_cursor::{extract_cursor, CursorError, ResumeQuery};
use super::runner_capacity;
use crate::agents::AgentType;
use crate::auth_middleware::AuthenticatedUser;
use crate::observability::request::{
    maybe_log_economics_guardrail, observe_serialized_payload, record_db_operation,
    record_db_query_count, Outcome, ROUTE_CONVERSATIONS, ROUTE_CONVERSATION_EVENTS_PAGE,
    ROUTE_CONVERSATION_MESSAGES,
};
use crate::observability::streaming::{
    record_cursor_expired, record_stream_closed, record_stream_opened, DisconnectReason,
};
use crate::observability::{agent_lifecycle, cancellation};
use crate::runner_commands;
use tokio::sync::{broadcast, RwLock};

static CONVERSATION_STATUS_BROADCASTER: Lazy<
    RwLock<HashMap<String, broadcast::Sender<ConversationRunStatusResponse>>>,
> = Lazy::new(|| RwLock::new(HashMap::new()));

const DEFAULT_EVENT_PAGE_LIMIT: i64 = 100;
const MAX_EVENT_PAGE_LIMIT: i64 = 200;
const DEFAULT_EVENT_PAGE_PAYLOAD_BYTES: usize = 192 * 1024;
const MAX_EVENT_PAGE_PAYLOAD_BYTES: usize = 256 * 1024;
const MAX_SINGLE_EVENT_PAYLOAD_BYTES: usize = 96 * 1024;

#[derive(Debug, Deserialize)]
pub struct ListConversationsQuery {
    pub organization: Option<String>,
    pub agent: Option<String>,
    /// roots (default), children, or all
    pub hierarchy_scope: Option<String>,
    /// Direct parent id. When supplied, returns children for that parent.
    pub parent_conversation_id: Option<String>,
    /// Comma-separated status filter (e.g., "open,waiting"). Default: "open,waiting"
    pub status: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub updated_since: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ConversationScaleCockpitQuery {
    pub organization: Option<String>,
    pub agent: Option<String>,
    pub limit: Option<i64>,
    pub include_archived_parents: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct ConversationListResponse {
    pub conversations: Vec<ConversationListItem>,
    pub total: i64,
    pub meta: ConversationListResponseMeta,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConversationListResponseMeta {
    pub requested_hierarchy_scope: String,
    pub applied_hierarchy_scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_conversation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_limit: Option<i64>,
    pub applied_limit: i64,
    pub offset: i64,
    pub returned_count: i64,
    pub has_more: bool,
    pub truncated: bool,
    pub child_fanout_guardrail: bool,
    pub max_root_limit: i64,
    pub max_child_limit: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub truncation_reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConversationListItem {
    pub id: String,
    pub title: String,
    pub organization: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_conversation_id: Option<String>,
    pub conversation_role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_conversation_count: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_child_conversation_count: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unread_child_conversation_count: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_sort_order: Option<i32>,
    pub updated_at: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_count: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_event_index: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_read_event_index: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unread_event_count: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_active: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_count: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_started_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_tool_call_started_at_epoch: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConversationSummary {
    #[serde(flatten)]
    pub conversation: Conversation,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_count: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_started_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_tool_call_started_at_epoch: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConversationRunStatusResponse {
    pub conversation_id: String,
    /// App-facing normalized state: idle, running, completed, or failed.
    pub status: String,
    /// Raw checkpoint status for diagnostics: running, completed, interrupted, etc.
    pub checkpoint_status: Option<String>,
    pub is_processing: bool,
    pub should_fetch: bool,
    pub updated_at: i64,
    pub last_event_index: i32,
    pub tool_call_count: i32,
    pub queued_message_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_started_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_tool_call_started_at_epoch: Option<i64>,
    pub server_time: i64,
}

#[derive(Debug, Deserialize)]
pub struct MarkConversationReadBody {
    pub last_read_event_index: Option<i32>,
}

#[derive(Debug, Deserialize)]
pub struct ConversationReadStateUpdateBody {
    pub conversation_id: String,
    pub last_read_event_index: i32,
}

#[derive(Debug, Deserialize)]
pub struct SyncConversationReadStatesBody {
    pub states: Vec<ConversationReadStateUpdateBody>,
}

#[derive(Debug, Serialize)]
pub struct ConversationReadStatesResponse {
    pub states: Vec<ConversationReadState>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConversationQueuedMessage {
    pub id: String,
    pub conversation_id: String,
    pub message: String,
    pub agent_type: String,
    pub status: String,
    pub client_id: Option<String>,
    pub attachment_count: usize,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(sqlx::FromRow)]
struct ConversationQueuedMessageRow {
    id: String,
    conversation_id: String,
    message: String,
    agent_type: String,
    status: String,
    client_id: Option<String>,
    images: Option<String>,
    created_at: i64,
    updated_at: i64,
}

#[derive(Debug, Deserialize)]
struct ChatAttachmentMeta {
    filename: String,
    path: String,
    mime_type: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateChildConversationsBody {
    pub children: Vec<CreateChildConversationSpec>,
}

#[derive(Debug, Deserialize)]
pub struct CreateMultiAgentConversationRequest {
    pub organization: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation_type: Option<String>,
    #[serde(default)]
    pub children: Vec<CreateChildConversationSpec>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateChildConversationSpec {
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_sort_order: Option<i32>,
    #[serde(default, flatten)]
    handoff: ContextHandoffRequest,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initial_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct MultiAgentConversationResponse {
    pub parent: Conversation,
    pub children: Vec<Conversation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued_turns: Vec<QueuedChildTurnResponse>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_handoffs: Vec<ChildContextHandoffResponse>,
}

#[derive(Debug, Serialize)]
pub struct QueuedChildTurnResponse {
    pub child_conversation_id: String,
    pub job_id: String,
    pub agent: String,
    pub prompt_name: String,
    pub model: String,
    pub reasoning_effort: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_packet_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retrieval_ids: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ChildContextHandoffResponse {
    pub child_conversation_id: String,
    pub context_packet_ids: Vec<String>,
    pub retrieval_ids: Vec<String>,
}

const ACTIVE_CHECKPOINT_STALE_SECONDS: i64 = 60;

fn is_active_checkpoint_status(status: Option<&str>) -> bool {
    matches!(status, Some("running") | Some("pending") | Some("queued"))
}

fn is_terminal_checkpoint_status(status: Option<&str>) -> bool {
    matches!(
        status,
        Some("completed" | "interrupted" | "failed" | "cancelled" | "timeout")
    )
}

fn normalize_checkpoint_status(status: Option<&str>) -> String {
    match status {
        Some("running") | Some("pending") | Some("queued") => "running",
        Some("completed") => "completed",
        Some("none") | None => "idle",
        Some("interrupted") | Some("failed") | Some("cancelled") | Some("timeout") => "failed",
        Some(_) => "failed",
    }
    .to_string()
}

#[derive(sqlx::FromRow)]
struct LastToolCallStartedAtRow {
    conversation_id: String,
    last_started_at: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct RunStartedAtRow {
    started_at: Option<i64>,
}

#[derive(Debug, Default)]
struct ConversationRunMetadataMaps {
    tool_call_counts: HashMap<String, i32>,
    run_started_times: HashMap<String, i64>,
    last_tool_calls: HashMap<String, i64>,
}

#[derive(Debug, Clone, Copy, Default)]
struct ChildActivityCounts {
    active: i32,
    unread: i32,
}

#[derive(sqlx::FromRow)]
struct ChildActivityCountsRow {
    parent_conversation_id: String,
    active_child_conversation_count: i64,
    unread_child_conversation_count: i64,
}

async fn last_tool_call_started_at_epoch_map(
    pool: &SqlitePool,
    conversation_ids: &[String],
) -> anyhow::Result<HashMap<String, i64>> {
    if conversation_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let placeholders = std::iter::repeat("?")
        .take(conversation_ids.len())
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        r#"
        SELECT conversation_id, MAX(created_at) AS last_started_at
        FROM conversation_tool_calls
        WHERE conversation_id IN ({})
        GROUP BY conversation_id
        "#,
        placeholders
    );

    let mut query = sqlx::query_as::<_, LastToolCallStartedAtRow>(&sql);
    for id in conversation_ids {
        query = query.bind(id);
    }

    let rows = query.fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| Some((row.conversation_id, row.last_started_at?)))
        .collect())
}

async fn last_tool_call_started_at_epoch(
    pool: &SqlitePool,
    conversation_id: &str,
    earliest_started_at: Option<i64>,
) -> anyhow::Result<Option<i64>> {
    let value: Option<i64> = if let Some(earliest_started_at) = earliest_started_at {
        sqlx::query_scalar(
            r#"
            SELECT MAX(created_at)
            FROM conversation_tool_calls
            WHERE conversation_id = ?
              AND created_at >= ?
            "#,
        )
        .bind(conversation_id)
        .bind(earliest_started_at)
        .fetch_one(pool)
        .await?
    } else {
        sqlx::query_scalar(
            r#"
            SELECT MAX(created_at)
            FROM conversation_tool_calls
            WHERE conversation_id = ?
            "#,
        )
        .bind(conversation_id)
        .fetch_one(pool)
        .await?
    };

    Ok(value)
}

async fn active_run_tool_call_count(
    pool: &SqlitePool,
    conversation_id: &str,
    earliest_started_at: Option<i64>,
) -> anyhow::Result<i32> {
    let value: i64 = if let Some(earliest_started_at) = earliest_started_at {
        sqlx::query_scalar(
            r#"
            SELECT COUNT(*)
            FROM conversation_tool_calls
            WHERE conversation_id = ?
              AND created_at >= ?
            "#,
        )
        .bind(conversation_id)
        .bind(earliest_started_at)
        .fetch_one(pool)
        .await?
    } else {
        sqlx::query_scalar(
            r#"
            SELECT COUNT(*)
            FROM conversation_tool_calls
            WHERE conversation_id = ?
            "#,
        )
        .bind(conversation_id)
        .fetch_one(pool)
        .await?
    };

    Ok(value.min(i64::from(i32::MAX)) as i32)
}

async fn active_run_started_at(
    pool: &SqlitePool,
    conversation_id: &str,
    checkpoint: Option<&ticketing_system::AgentCheckpoint>,
) -> anyhow::Result<Option<i64>> {
    let running_job = sqlx::query_as::<_, RunStartedAtRow>(
        r#"
        SELECT COALESCE(started_at, updated_at, created_at) AS started_at
        FROM conversation_turn_jobs
        WHERE conversation_id = ?
          AND status = 'running'
        ORDER BY started_at DESC, updated_at DESC
        LIMIT 1
        "#,
    )
    .bind(conversation_id)
    .fetch_optional(pool)
    .await?;

    if let Some(row) = running_job {
        if row.started_at.is_some() {
            return Ok(row.started_at);
        }
    }

    let active_runner_turn = sqlx::query_as::<_, RunStartedAtRow>(
        r#"
        SELECT started_at
        FROM agent_runner_turns
        WHERE conversation_id = ?
          AND status IN ('queued', 'running')
        ORDER BY started_at DESC
        LIMIT 1
        "#,
    )
    .bind(conversation_id)
    .fetch_optional(pool)
    .await?;

    if let Some(row) = active_runner_turn {
        if row.started_at.is_some() {
            return Ok(row.started_at);
        }
    }

    Ok(checkpoint.map(|cp| cp.updated_at))
}

fn queued_message_attachment_count(attachments_json: Option<&str>) -> usize {
    let Some(attachments_json) = attachments_json else {
        return 0;
    };
    serde_json::from_str::<Vec<serde_json::Value>>(attachments_json)
        .map(|attachments| attachments.len())
        .unwrap_or(0)
}

async fn pending_queued_message_count(
    pool: &SqlitePool,
    conversation_id: &str,
) -> anyhow::Result<i64> {
    sqlx::query_scalar(
        r#"
        SELECT COUNT(*)
        FROM conversation_turn_jobs
        WHERE conversation_id = ?
          AND status = 'pending'
        "#,
    )
    .bind(conversation_id)
    .fetch_one(pool)
    .await
    .map_err(Into::into)
}

async fn list_pending_queued_messages(
    pool: &SqlitePool,
    conversation_id: &str,
) -> anyhow::Result<Vec<ConversationQueuedMessage>> {
    let rows = sqlx::query_as::<_, ConversationQueuedMessageRow>(
        r#"
        SELECT
            id,
            conversation_id,
            message,
            agent_type,
            status,
            client_id,
            images,
            created_at,
            updated_at
        FROM conversation_turn_jobs
        WHERE conversation_id = ?
          AND status = 'pending'
        ORDER BY created_at ASC, id ASC
        "#,
    )
    .bind(conversation_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| ConversationQueuedMessage {
            id: row.id,
            conversation_id: row.conversation_id,
            message: row.message,
            agent_type: row.agent_type,
            status: row.status,
            client_id: row.client_id,
            attachment_count: queued_message_attachment_count(row.images.as_deref()),
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
        .collect())
}

async fn conversation_run_metadata_maps(
    pool: &SqlitePool,
    conversations: &[Conversation],
) -> anyhow::Result<ConversationRunMetadataMaps> {
    let ids = conversations
        .iter()
        .map(|conv| conv.id.clone())
        .collect::<Vec<_>>();
    let mut last_tool_calls = last_tool_call_started_at_epoch_map(pool, &ids).await?;
    let mut tool_call_counts = HashMap::new();
    let mut run_started_times = HashMap::new();

    for conversation in conversations
        .iter()
        .filter(|conv| conv.is_active.unwrap_or(false))
    {
        let checkpoint = checkpoints::get_checkpoint(pool, &conversation.id).await?;
        let run_started_at =
            active_run_started_at(pool, &conversation.id, checkpoint.as_ref()).await?;
        let checkpoint_tool_call_count = checkpoint
            .as_ref()
            .map(|cp| cp.tool_call_count)
            .unwrap_or(0);
        let tool_call_count = checkpoint_tool_call_count
            .max(active_run_tool_call_count(pool, &conversation.id, run_started_at).await?);
        tool_call_counts.insert(conversation.id.clone(), tool_call_count);
        if let Some(epoch) = run_started_at {
            run_started_times.insert(conversation.id.clone(), epoch);
        }
        if let Some(epoch) =
            last_tool_call_started_at_epoch(pool, &conversation.id, run_started_at).await?
        {
            last_tool_calls.insert(conversation.id.clone(), epoch);
        } else {
            last_tool_calls.remove(&conversation.id);
        }
    }

    Ok(ConversationRunMetadataMaps {
        tool_call_counts,
        run_started_times,
        last_tool_calls,
    })
}

async fn child_activity_counts_map(
    pool: &SqlitePool,
    user_id: &str,
    parent_ids: &[String],
) -> anyhow::Result<HashMap<String, ChildActivityCounts>> {
    if parent_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let placeholders = std::iter::repeat("?")
        .take(parent_ids.len())
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        r#"
        SELECT
            child.parent_conversation_id,
            SUM(
                CASE
                    WHEN ac.status IN ('running', 'pending', 'queued') THEN 1
                    WHEN EXISTS (
                        SELECT 1
                        FROM conversation_turn_jobs j
                        WHERE j.conversation_id = child.id
                          AND j.status IN ('pending', 'running')
                    ) THEN 1
                    WHEN EXISTS (
                        SELECT 1
                        FROM agent_runner_turns t
                        WHERE t.conversation_id = child.id
                          AND t.status IN ('queued', 'running')
                    ) THEN 1
                    ELSE 0
                END
            ) AS active_child_conversation_count,
            SUM(
                CASE
                    WHEN child.last_event_index > COALESCE(crc.last_read_event_index, -1) THEN 1
                    ELSE 0
                END
            ) AS unread_child_conversation_count
        FROM conversations child
        LEFT JOIN agent_checkpoints ac ON ac.conversation_id = child.id
        LEFT JOIN conversation_read_cursors crc
               ON crc.conversation_id = child.id
              AND crc.user_id = ?
        WHERE child.parent_conversation_id IN ({})
          AND child.status <> 'archived'
        GROUP BY child.parent_conversation_id
        "#,
        placeholders
    );

    let mut query = sqlx::query_as::<_, ChildActivityCountsRow>(&sql).bind(user_id);
    for id in parent_ids {
        query = query.bind(id);
    }

    let rows = query.fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            (
                row.parent_conversation_id,
                ChildActivityCounts {
                    active: row.active_child_conversation_count.min(i64::from(i32::MAX)) as i32,
                    unread: row.unread_child_conversation_count.min(i64::from(i32::MAX)) as i32,
                },
            )
        })
        .collect())
}

impl ConversationListItem {
    fn from_conversation(
        conversation: Conversation,
        metadata: &ConversationRunMetadataMaps,
        child_activity: Option<ChildActivityCounts>,
    ) -> Self {
        let tool_call_count = metadata.tool_call_counts.get(&conversation.id).copied();
        let run_started_at = metadata.run_started_times.get(&conversation.id).copied();
        let last_tool_call_started_at_epoch =
            metadata.last_tool_calls.get(&conversation.id).copied();
        let has_children = conversation.child_conversation_count.unwrap_or(0) > 0;

        ConversationListItem {
            id: conversation.id,
            title: conversation.title,
            organization: conversation.organization,
            agent: conversation.agent,
            conversation_type: conversation.conversation_type,
            parent_conversation_id: conversation.parent_conversation_id,
            conversation_role: conversation.conversation_role,
            child_conversation_count: conversation.child_conversation_count,
            active_child_conversation_count: child_activity
                .filter(|_| has_children)
                .map(|activity| activity.active),
            unread_child_conversation_count: child_activity
                .filter(|_| has_children)
                .map(|activity| activity.unread),
            child_sort_order: conversation.child_sort_order,
            updated_at: conversation.updated_at,
            status: conversation.status,
            message_count: conversation.message_count,
            last_event_index: conversation.last_event_index,
            last_read_event_index: conversation.last_read_event_index,
            unread_event_count: conversation.unread_event_count,
            is_active: conversation.is_active,
            tool_call_count,
            run_started_at,
            last_tool_call_started_at_epoch,
        }
    }
}

pub(crate) async fn conversation_list_items(
    pool: &SqlitePool,
    user_id: &str,
    conversations: Vec<Conversation>,
) -> anyhow::Result<Vec<ConversationListItem>> {
    // The permanent coordinator is an Alex-surface singleton, not a Chat
    // conversation. Keep this invariant at the shared list-enrichment
    // boundary so REST, conversation SSE, and unified SSE can never publish
    // its intentionally verbose transcript into Chat. The dedicated
    // `/api/alex/coordinator` endpoint remains its only discovery surface.
    let conversations = conversations
        .into_iter()
        .filter(is_global_chat_conversation)
        .collect::<Vec<_>>();
    let metadata = conversation_run_metadata_maps(pool, &conversations).await?;
    let parent_ids = conversations
        .iter()
        .filter(|conversation| {
            conversation.parent_conversation_id.is_none()
                && conversation.child_conversation_count.unwrap_or(0) > 0
        })
        .map(|conversation| conversation.id.clone())
        .collect::<Vec<_>>();
    let mut child_activity_counts = child_activity_counts_map(pool, user_id, &parent_ids).await?;

    Ok(conversations
        .into_iter()
        .map(|conversation| {
            let child_activity = child_activity_counts.remove(&conversation.id);
            ConversationListItem::from_conversation(conversation, &metadata, child_activity)
        })
        .collect())
}

fn is_global_chat_conversation(conversation: &Conversation) -> bool {
    conversation.agent.as_deref() != Some(crate::fable_coordinator::FABLE_AGENT)
        && conversation.conversation_type.as_deref()
            != Some(crate::fable_coordinator::FABLE_CONVERSATION_TYPE)
}

fn project_fable_worker_as_chat_root(mut conversation: Conversation) -> Conversation {
    // This is a response-only projection. Durable parentage stays attached to
    // the permanent coordinator so completion wakes and orchestration remain
    // correct, while Chat can monitor each worker without exposing the
    // coordinator transcript itself.
    conversation.parent_conversation_id = None;
    conversation.conversation_role = "standard".to_string();
    conversation
}

fn hierarchy_scope_label(scope: ConversationHierarchyScope) -> &'static str {
    match scope {
        ConversationHierarchyScope::Roots => "roots",
        ConversationHierarchyScope::Children => "children",
        ConversationHierarchyScope::All => "all",
    }
}

fn resolve_conversation_list_scope(
    requested_scope: ConversationHierarchyScope,
    parent_conversation_id: Option<&str>,
) -> (ConversationHierarchyScope, Vec<String>, bool) {
    if parent_conversation_id.is_some() {
        return (ConversationHierarchyScope::Children, Vec::new(), false);
    }

    match requested_scope {
        ConversationHierarchyScope::Roots => (ConversationHierarchyScope::Roots, Vec::new(), false),
        ConversationHierarchyScope::Children => (
            ConversationHierarchyScope::Roots,
            vec!["children_scope_requires_parent_conversation_id".to_string()],
            true,
        ),
        ConversationHierarchyScope::All => (
            ConversationHierarchyScope::Roots,
            vec!["all_hierarchy_clamped_to_root_summaries".to_string()],
            true,
        ),
    }
}

fn conversation_list_page_options(
    applied_scope: ConversationHierarchyScope,
    requested_limit: Option<i64>,
    offset: Option<i64>,
) -> conversations::ConversationListPageOptions {
    match applied_scope {
        ConversationHierarchyScope::Children => {
            conversations::ConversationListPageOptions::children(requested_limit, offset)
        }
        ConversationHierarchyScope::Roots | ConversationHierarchyScope::All => {
            conversations::ConversationListPageOptions::roots(requested_limit, offset)
        }
    }
}

fn conversation_list_response_meta(
    requested_scope: ConversationHierarchyScope,
    applied_scope: ConversationHierarchyScope,
    parent_conversation_id: Option<String>,
    page: &conversations::ConversationListPage,
    child_fanout_guardrail: bool,
    mut truncation_reasons: Vec<String>,
) -> ConversationListResponseMeta {
    if page.has_more {
        truncation_reasons.push("page_limit_reached".to_string());
    }
    if page
        .requested_limit
        .map(|requested| requested > page.applied_limit)
        .unwrap_or(false)
    {
        truncation_reasons.push("requested_limit_capped".to_string());
    }

    ConversationListResponseMeta {
        requested_hierarchy_scope: hierarchy_scope_label(requested_scope).to_string(),
        applied_hierarchy_scope: hierarchy_scope_label(applied_scope).to_string(),
        parent_conversation_id,
        requested_limit: page.requested_limit,
        applied_limit: page.applied_limit,
        offset: page.offset,
        returned_count: page.returned_count,
        has_more: page.has_more,
        truncated: page.truncated || child_fanout_guardrail,
        child_fanout_guardrail,
        max_root_limit: conversations::MAX_ROOT_CONVERSATION_PAGE_LIMIT,
        max_child_limit: conversations::MAX_CHILD_CONVERSATION_PAGE_LIMIT,
        truncation_reasons,
    }
}

pub(crate) async fn conversation_summaries(
    pool: &SqlitePool,
    conversations: Vec<Conversation>,
) -> anyhow::Result<Vec<ConversationSummary>> {
    let metadata = conversation_run_metadata_maps(pool, &conversations).await?;

    Ok(conversations
        .into_iter()
        .map(|conversation| {
            let tool_call_count = metadata.tool_call_counts.get(&conversation.id).copied();
            let run_started_at = metadata.run_started_times.get(&conversation.id).copied();
            let last_tool_call_started_at_epoch =
                metadata.last_tool_calls.get(&conversation.id).copied();
            ConversationSummary {
                conversation,
                tool_call_count,
                run_started_at,
                last_tool_call_started_at_epoch,
            }
        })
        .collect())
}

async fn repair_checkpoint_from_active_durable_work(
    pool: &SqlitePool,
    conversation_id: &str,
    checkpoint_status: Option<&str>,
) -> anyhow::Result<bool> {
    if is_active_checkpoint_status(checkpoint_status) {
        return Ok(false);
    }
    if is_terminal_checkpoint_status(checkpoint_status) {
        return Ok(false);
    }

    let has_active_job =
        conversation_turn_jobs::has_active_job_for_conversation(pool, conversation_id).await?;
    let has_active_turn =
        agent_runners::has_active_turn_for_conversation(pool, conversation_id).await?;

    if !has_active_job && !has_active_turn {
        return Ok(false);
    }

    let repaired_status = if has_active_turn { "running" } else { "queued" };
    tracing::warn!(
        "[CHAT-STATUS] Repairing checkpoint from active durable work: conv={} checkpoint_status={} repaired_status={} has_active_job={} has_active_turn={}",
        conversation_id,
        checkpoint_status.unwrap_or("none"),
        repaired_status,
        has_active_job,
        has_active_turn
    );
    checkpoints::upsert_checkpoint(pool, conversation_id, repaired_status, 0).await?;
    Ok(true)
}

pub(crate) async fn conversation_run_status_snapshot(
    pool: &SqlitePool,
    conversation_id: &str,
    manager: Option<&ChatClientManager>,
) -> anyhow::Result<ConversationRunStatusResponse> {
    let mut checkpoint = checkpoints::get_checkpoint(pool, conversation_id).await?;
    let mut checkpoint_status = checkpoint.as_ref().map(|cp| cp.status.clone());
    let mut status = normalize_checkpoint_status(checkpoint_status.as_deref());
    let mut is_processing = status == "running";

    if !is_processing
        && repair_checkpoint_from_active_durable_work(
            pool,
            conversation_id,
            checkpoint_status.as_deref(),
        )
        .await?
    {
        checkpoint = checkpoints::get_checkpoint(pool, conversation_id).await?;
        checkpoint_status = checkpoint.as_ref().map(|cp| cp.status.clone());
        status = normalize_checkpoint_status(checkpoint_status.as_deref());
        is_processing = status == "running";
    }

    if is_processing {
        let recovered = agent_runners::recover_stale_active_work_for_conversation(
            pool,
            conversation_id,
            ACTIVE_CHECKPOINT_STALE_SECONDS,
        )
        .await?;
        if recovered.any() {
            tracing::warn!(
                "[CHAT-STATUS] Recovered stale active work: conv={} turns_failed={} jobs_failed={} checkpoints_interrupted={}",
                conversation_id,
                recovered.turns_failed,
                recovered.jobs_failed,
                recovered.checkpoints_interrupted
            );
            checkpoint = checkpoints::get_checkpoint(pool, conversation_id).await?;
            checkpoint_status = checkpoint.as_ref().map(|cp| cp.status.clone());
            status = normalize_checkpoint_status(checkpoint_status.as_deref());
            is_processing = status == "running";
        }
    }

    if is_processing {
        if let (Some(manager), Some(checkpoint_row)) = (manager, checkpoint.as_ref()) {
            let has_live_turn = manager.has_runtime_turn(conversation_id).await;
            let has_worker = WORKER_MANAGER.has_worker(conversation_id).await;
            let has_runner_turn =
                agent_runners::has_active_turn_for_conversation(pool, conversation_id)
                    .await
                    .unwrap_or(false);
            let has_active_job =
                conversation_turn_jobs::has_active_job_for_conversation(pool, conversation_id)
                    .await
                    .unwrap_or(false);
            let checkpoint_age = chrono::Utc::now()
                .timestamp()
                .saturating_sub(checkpoint_row.updated_at);

            if !has_live_turn
                && !has_worker
                && !has_runner_turn
                && !has_active_job
                && checkpoint_age > ACTIVE_CHECKPOINT_STALE_SECONDS
                && is_active_checkpoint_status(checkpoint_status.as_deref())
            {
                tracing::warn!(
                    "[CHAT-STATUS] Marking stale checkpoint interrupted: conv={} status={} age={}s",
                    conversation_id,
                    checkpoint_status.as_deref().unwrap_or("none"),
                    checkpoint_age
                );
                checkpoints::mark_interrupted(pool, conversation_id).await?;
                checkpoint = checkpoints::get_checkpoint(pool, conversation_id).await?;
                checkpoint_status = checkpoint.as_ref().map(|cp| cp.status.clone());
                status = normalize_checkpoint_status(checkpoint_status.as_deref());
                is_processing = false;
            }
        }
    }

    let last_event_index = conversations::get_max_event_index(pool, conversation_id)
        .await
        .unwrap_or(-1);
    let queued_message_count = pending_queued_message_count(pool, conversation_id).await?;
    let run_started_at = if is_processing {
        active_run_started_at(pool, conversation_id, checkpoint.as_ref()).await?
    } else {
        None
    };
    let last_tool_call_started_at_epoch =
        last_tool_call_started_at_epoch(pool, conversation_id, run_started_at).await?;
    let checkpoint_tool_call_count = checkpoint
        .as_ref()
        .map(|cp| cp.tool_call_count)
        .unwrap_or(0);
    let tool_call_count = if is_processing {
        checkpoint_tool_call_count
            .max(active_run_tool_call_count(pool, conversation_id, run_started_at).await?)
    } else {
        checkpoint_tool_call_count
    };

    Ok(ConversationRunStatusResponse {
        conversation_id: conversation_id.to_string(),
        status,
        checkpoint_status,
        is_processing,
        should_fetch: !is_processing,
        updated_at: checkpoint.as_ref().map(|cp| cp.updated_at).unwrap_or(0),
        last_event_index,
        tool_call_count,
        queued_message_count,
        run_started_at,
        last_tool_call_started_at_epoch,
        server_time: chrono::Utc::now().timestamp(),
    })
}

fn apply_run_status_to_conversation(
    mut conv: Conversation,
    status: &ConversationRunStatusResponse,
) -> Conversation {
    conv.is_active = Some(status.is_processing);
    conv
}

async fn ensure_agent_allowed(
    pool: &SqlitePool,
    user_id: &str,
    agent: Option<&str>,
) -> Result<(), (StatusCode, String)> {
    let requires_admin =
        agent.and_then(AgentType::from_chat_agent_key) == Some(AgentType::FullAccess);
    if !requires_admin {
        return Ok(());
    }

    let is_admin = ticketing_system::system_logs::is_admin(pool, user_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !is_admin {
        return Err((
            StatusCode::FORBIDDEN,
            "Admin access required for full-access conversations".to_string(),
        ));
    }

    Ok(())
}

async fn require_user_conversation(
    pool: &SqlitePool,
    user_id: &str,
    conversation_id: &str,
) -> Result<Conversation, (StatusCode, String)> {
    let conversation = conversations::get_conversation(pool, conversation_id, false)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Conversation not found".to_string()))?;

    if conversation.user_id != user_id {
        return Err((StatusCode::NOT_FOUND, "Conversation not found".to_string()));
    }

    Ok(conversation)
}

fn child_conversation_requests(
    specs: &[CreateChildConversationSpec],
) -> Result<Vec<TicketingCreateChildConversationRequest>, (StatusCode, String)> {
    specs
        .iter()
        .map(|child| {
            if requests_fable_designation(
                child.agent.as_deref(),
                child.conversation_type.as_deref(),
            ) {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "The permanent Fable coordinator cannot be created as a child conversation"
                        .to_string(),
                ));
            }
            let has_initial_message = child
                .initial_message
                .as_deref()
                .map(str::trim)
                .is_some_and(|message| !message.is_empty());
            if has_initial_message {
                let agent = child.agent.as_deref().ok_or_else(|| {
                    (
                        StatusCode::BAD_REQUEST,
                        format!(
                            "Child '{}' must set agent when initial_message is provided",
                            child.title
                        ),
                    )
                })?;
                let _ = parse_child_agent_type(agent)?;
            }
            Ok(TicketingCreateChildConversationRequest {
                title: child.title.clone(),
                agent: child
                    .agent
                    .as_deref()
                    .map(canonical_child_agent_key_for_storage),
                conversation_type: child.conversation_type.clone(),
                child_sort_order: child.child_sort_order,
            })
        })
        .collect()
}

fn canonical_child_agent_key_for_storage(agent: &str) -> String {
    AgentType::from_chat_agent_key(agent)
        .map(|agent_type| agent_type.as_str().to_string())
        .unwrap_or_else(|| agent.to_string())
}

fn requests_fable_designation(agent: Option<&str>, conversation_type: Option<&str>) -> bool {
    conversation_type == Some(crate::fable_coordinator::FABLE_CONVERSATION_TYPE)
        || agent.and_then(AgentType::from_chat_agent_key) == Some(AgentType::FableCoordinator)
}

async fn resolve_child_context_handoffs(
    pool: &SqlitePool,
    organization: &str,
    children: &[CreateChildConversationSpec],
) -> Result<Vec<Option<ResolvedContextHandoff>>, (StatusCode, String)> {
    let mut resolved = Vec::with_capacity(children.len());
    for child in children {
        if child.handoff.has_handles()
            && child
                .initial_message
                .as_deref()
                .map(str::trim)
                .filter(|message| !message.is_empty())
                .is_none()
        {
            return Err((
                StatusCode::BAD_REQUEST,
                format!(
                    "Child '{}' provided context_packet_ids/retrieval_ids but no initial_message; packet handoff must be attached to a queued child turn.",
                    child.title
                ),
            ));
        }

        let handoff = resolve_context_handoff(pool, organization, &child.handoff)
            .await
            .map_err(|e| {
                (
                    StatusCode::BAD_REQUEST,
                    format!(
                        "Invalid artifact-memory handoff for child '{}': {}",
                        child.title, e
                    ),
                )
            })?;
        resolved.push(handoff);
    }
    Ok(resolved)
}

fn context_handoff_responses(
    children: &[Conversation],
    handoffs: &[Option<ResolvedContextHandoff>],
) -> Vec<ChildContextHandoffResponse> {
    children
        .iter()
        .zip(handoffs.iter())
        .filter_map(|(child, handoff)| {
            let handoff = handoff.as_ref()?;
            Some(ChildContextHandoffResponse {
                child_conversation_id: child.id.clone(),
                context_packet_ids: handoff.packet_ids(),
                retrieval_ids: handoff.retrieval_ids(),
            })
        })
        .collect()
}

#[derive(Debug, Clone)]
struct PreparedChildTurn {
    request: conversations::InitialChildTurnJobRequest,
    agent: String,
    prompt_name: String,
    codex_options: ChatCodexOptions,
    context_packet_ids: Vec<String>,
    retrieval_ids: Vec<String>,
}

async fn resolve_initial_child_codex_options(
    children: &[CreateChildConversationSpec],
) -> Result<Vec<Option<ChatCodexOptions>>, Response> {
    let mut resolved = Vec::with_capacity(children.len());
    for child in children {
        let has_initial_message = has_initial_child_message(child);
        if !has_initial_message {
            if child.model.is_some() || child.reasoning_effort.is_some() {
                return Err(text_error_response((
                    StatusCode::BAD_REQUEST,
                    format!(
                        "Child '{}' cannot set model or reasoning_effort without initial_message",
                        child.title
                    ),
                )));
            }
            resolved.push(None);
            continue;
        }

        let agent = child.agent.as_deref().ok_or_else(|| {
            text_error_response((
                StatusCode::BAD_REQUEST,
                format!(
                    "Child '{}' must set agent when initial_message is provided",
                    child.title
                ),
            ))
        })?;
        let agent_type = parse_child_agent_type(agent).map_err(text_error_response)?;
        let defaults = ChatCodexOptions::default_for_agent(&agent_type);
        let requested = if child.model.is_some() || child.reasoning_effort.is_some() {
            Some(ChatCodexOptions {
                model: child.model.clone().unwrap_or(defaults.model),
                reasoning_effort: child
                    .reasoning_effort
                    .clone()
                    .unwrap_or(defaults.reasoning_effort),
            })
        } else {
            None
        };
        resolved.push(Some(validate_codex_options(&agent_type, requested).await?));
    }
    Ok(resolved)
}

fn prepare_initial_child_turns(
    user_id: &str,
    children: &[CreateChildConversationSpec],
    handoffs: &[Option<ResolvedContextHandoff>],
    codex_options: &[Option<ChatCodexOptions>],
) -> Result<Vec<PreparedChildTurn>, (StatusCode, String)> {
    let mut prepared = Vec::new();
    let child_batch_size = children
        .iter()
        .filter(|child| has_initial_child_message(child))
        .count();
    let child_batch_id =
        (child_batch_size > 0).then(|| format!("child-batch-{}", uuid::Uuid::new_v4()));
    let child_batch_report_id = child_batch_id
        .as_ref()
        .map(|_| agent_lifecycle::new_report_id());
    let mut child_batch_index = 0usize;

    for (child_index, (spec, handoff)) in children.iter().zip(handoffs).enumerate() {
        let Some(initial_message) = spec
            .initial_message
            .as_deref()
            .map(str::trim)
            .filter(|message| !message.is_empty())
        else {
            continue;
        };
        let agent = spec.agent.as_deref().ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                format!(
                    "Child '{}' must set agent when initial_message is provided",
                    spec.title
                ),
            )
        })?;
        let agent_type = parse_child_agent_type(agent)?;
        let agent = agent_type.as_str();
        let prompt_name = spec
            .prompt_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(canonical_child_prompt_name)
            .unwrap_or_else(|| default_child_prompt_name(agent));
        let working_dir = spec
            .working_dir
            .as_deref()
            .map(str::trim)
            .filter(|dir| !dir.is_empty())
            .unwrap_or("/Users/jarvisgpt/projects");
        let codex_options = codex_options
            .get(child_index)
            .and_then(Option::as_ref)
            .ok_or_else(|| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Missing resolved Codex options for child '{}'", spec.title),
                )
            })?;

        let mut prompt_vars = HashMap::new();
        prompt_vars.insert(
            "CHILD_CONVERSATION_ID".to_string(),
            conversations::CHILD_CONVERSATION_ID_PLACEHOLDER.to_string(),
        );
        prompt_vars.insert(
            "PARENT_CONVERSATION_ID".to_string(),
            conversations::PARENT_CONVERSATION_ID_PLACEHOLDER.to_string(),
        );
        prompt_vars.insert(
            "SUPPORT_CONTEXT_TOOL_ARGS".to_string(),
            child_support_context_tool_args(
                conversations::CHILD_CONVERSATION_ID_PLACEHOLDER,
                user_id,
                handoff.as_ref(),
            )
            .map_err(internal_error)?,
        );
        if let Some(handoff) = handoff.as_ref() {
            prompt_vars.insert(
                "ARTIFACT_MEMORY_HANDOFF".to_string(),
                handoff.prompt_json().map_err(internal_error)?,
            );
        }
        add_file_backed_worker_prompt_vars(prompt_name, &mut prompt_vars)
            .map_err(internal_error)?;

        child_batch_index += 1;
        let metadata = orchestrated_child_turn_metadata(
            "api",
            agent,
            handoff.as_ref(),
            child_batch_id.as_deref(),
            child_batch_size,
            child_batch_index,
            child_batch_report_id.as_deref(),
        )?;
        let prompt_vars = encode_codex_options_for_job(prompt_vars, codex_options);
        prepared.push(PreparedChildTurn {
            request: conversations::InitialChildTurnJobRequest {
                child_index,
                payload: conversation_turn_jobs::ConversationTurnJobPayload {
                    user_id: user_id.to_string(),
                    message: initial_message.to_string(),
                    agent_type: agent.to_string(),
                    runtime: ChatRuntime::CodexAppServer.as_job_runtime().to_string(),
                    prompt_name: prompt_name.to_string(),
                    working_dir: working_dir.to_string(),
                    prompt_vars,
                    images_json: None,
                    client_id: spec.client_id.clone(),
                    message_metadata: Some(metadata),
                },
            },
            agent: agent.to_string(),
            prompt_name: prompt_name.to_string(),
            codex_options: codex_options.clone(),
            context_packet_ids: handoff
                .as_ref()
                .map(ResolvedContextHandoff::packet_ids)
                .unwrap_or_default(),
            retrieval_ids: handoff
                .as_ref()
                .map(ResolvedContextHandoff::retrieval_ids)
                .unwrap_or_default(),
        });
    }
    Ok(prepared)
}

fn queued_child_turn_responses(
    prepared_turns: &[PreparedChildTurn],
    queued_jobs: &[conversations::CreatedChildTurnJob],
) -> Vec<QueuedChildTurnResponse> {
    queued_jobs
        .iter()
        .filter_map(|queued| {
            let prepared = prepared_turns
                .iter()
                .find(|prepared| prepared.request.child_index == queued.child_index)?;
            Some(QueuedChildTurnResponse {
                child_conversation_id: queued.child_conversation_id.clone(),
                job_id: queued.job_id.clone(),
                agent: prepared.agent.clone(),
                prompt_name: prepared.prompt_name.clone(),
                model: prepared.codex_options.model.clone(),
                reasoning_effort: prepared.codex_options.reasoning_effort.clone(),
                status: "queued".to_string(),
                context_packet_ids: prepared.context_packet_ids.clone(),
                retrieval_ids: prepared.retrieval_ids.clone(),
            })
        })
        .collect()
}

async fn record_prepared_child_batch_pending(
    pool: &SqlitePool,
    parent_conversation_id: &str,
    prepared_turns: &[PreparedChildTurn],
    queued_jobs: &[conversations::CreatedChildTurnJob],
) {
    let Some(context) = prepared_turns.iter().find_map(|turn| {
        agent_lifecycle::child_batch_context_from_metadata(
            turn.request.payload.message_metadata.as_deref(),
        )
    }) else {
        return;
    };

    agent_lifecycle::record_child_batch_pending(
        pool,
        parent_conversation_id,
        &context,
        queued_jobs.len(),
    )
    .await;
    agent_lifecycle::refresh_queue_metrics(pool).await;
}

async fn publish_queued_child_turn_statuses(
    pool: &SqlitePool,
    queued_jobs: &[conversations::CreatedChildTurnJob],
) -> Result<(), (StatusCode, String)> {
    for queued in queued_jobs {
        publish_conversation_run_status(pool, &queued.child_conversation_id)
            .await
            .map_err(internal_error)?;
    }
    Ok(())
}

fn has_initial_child_message(child: &CreateChildConversationSpec) -> bool {
    child
        .initial_message
        .as_deref()
        .map(str::trim)
        .is_some_and(|message| !message.is_empty())
}

fn default_child_prompt_name(agent: &str) -> &str {
    match agent {
        "codex" => "full-access",
        "conversation-evaluator" => "conversation-evaluator-system",
        _ => agent,
    }
}

fn add_file_backed_worker_prompt_vars(
    prompt_name: &str,
    prompt_vars: &mut HashMap<String, String>,
) -> anyhow::Result<()> {
    let prompt_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("_prompts")
        .join(format!("{prompt_name}.txt"));
    let prompt = std::fs::read_to_string(&prompt_path)
        .with_context(|| format!("read worker prompt template {}", prompt_path.display()))?;
    if prompt.contains("{{AGENTS_MD}}") {
        let agents_md = std::fs::read_to_string("/Users/jarvisgpt/projects/AGENTS.md")
            .context("read /Users/jarvisgpt/projects/AGENTS.md for worker prompt")?;
        prompt_vars.insert("AGENTS_MD".to_string(), agents_md);
    }
    Ok(())
}

fn canonical_child_prompt_name(prompt_name: &str) -> &str {
    match prompt_name {
        "codex" => "full-access",
        other => other,
    }
}

fn parse_child_agent_type(agent: &str) -> Result<AgentType, (StatusCode, String)> {
    AgentType::from_chat_agent_key(agent)
        .or_else(|| serde_json::from_value(serde_json::Value::String(agent.to_string())).ok())
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                format!("Unsupported child agent for queued turn: {agent}"),
            )
        })
}

fn orchestrated_child_turn_metadata(
    orchestrated_by: &str,
    agent: &str,
    handoff: Option<&ResolvedContextHandoff>,
    child_batch_id: Option<&str>,
    child_batch_size: usize,
    child_batch_index: usize,
    report_id: Option<&str>,
) -> Result<String, (StatusCode, String)> {
    let mut value = serde_json::json!({
        "origin": "agent_orchestrated",
        "orchestrated_by": orchestrated_by,
        "orchestration": "child_initial_turn",
        "agent": agent,
    });
    if let Some(child_batch_id) = child_batch_id {
        value["child_batch_id"] = serde_json::Value::String(child_batch_id.to_string());
        value["child_batch_size"] = serde_json::json!(child_batch_size);
        value["child_batch_index"] = serde_json::json!(child_batch_index);
    }
    if let Some(report_id) = report_id {
        value["report_id"] = serde_json::Value::String(report_id.to_string());
    }
    if agent == "conversation-evaluator" {
        value["suppress_parent_completion_relay"] = serde_json::Value::Bool(true);
    }
    if let Some(handoff) = handoff {
        value["artifact_memory_handoff"] = handoff.metadata_json();
    }
    serde_json::to_string(&value).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

fn child_support_context_tool_args(
    child_id: &str,
    user_id: &str,
    handoff: Option<&ResolvedContextHandoff>,
) -> Result<String, serde_json::Error> {
    let mut args = serde_json::json!({
        "child_conversation_id": child_id,
        "user_id": user_id,
    });
    if let Some(handoff) = handoff {
        args["artifact_memory_handoff"] = serde_json::to_value(handoff)?;
    }
    serde_json::to_string(&args)
}

fn internal_error(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

fn text_error_response((status, message): (StatusCode, String)) -> Response {
    (status, message).into_response()
}

async fn guard_runner_queue_admission(
    pool: &SqlitePool,
    children: &[CreateChildConversationSpec],
    context: &str,
) -> Result<(), Response> {
    let requested_jobs = children
        .iter()
        .filter(|child| has_initial_child_message(child))
        .count();
    if requested_jobs == 0 {
        return Ok(());
    }

    let admission = runner_capacity::admit_enqueue(pool, requested_jobs, context)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to inspect runner queue capacity: {e}"),
            )
                .into_response()
        })?;
    if admission.accepted {
        Ok(())
    } else {
        Err(runner_capacity::queue_admission_rejection_response(
            admission,
        ))
    }
}

async fn get_status_sender(
    conversation_id: &str,
) -> broadcast::Sender<ConversationRunStatusResponse> {
    {
        let map = CONVERSATION_STATUS_BROADCASTER.read().await;
        if let Some(tx) = map.get(conversation_id) {
            return tx.clone();
        }
    }
    let mut map = CONVERSATION_STATUS_BROADCASTER.write().await;
    if let Some(tx) = map.get(conversation_id) {
        return tx.clone();
    }
    let (tx, _) = broadcast::channel(32);
    map.insert(conversation_id.to_string(), tx.clone());
    tx
}

async fn remove_status_sender(conversation_id: &str) {
    let mut map = CONVERSATION_STATUS_BROADCASTER.write().await;
    map.remove(conversation_id);
}

pub async fn publish_conversation_run_status(
    pool: &SqlitePool,
    conversation_id: &str,
) -> anyhow::Result<()> {
    let snapshot = conversation_run_status_snapshot(pool, conversation_id, None).await?;
    let sender = {
        let map = CONVERSATION_STATUS_BROADCASTER.read().await;
        map.get(conversation_id).cloned()
    };

    if let Some(tx) = sender {
        let _ = tx.send(snapshot.clone());
    }
    if !snapshot.is_processing {
        remove_status_sender(conversation_id).await;
    }
    Ok(())
}

/// List conversations (GET /api/conversations)
pub async fn list_conversations(
    State(pool): State<Arc<SqlitePool>>,
    State(manager): State<Arc<ChatClientManager>>,
    Extension(user): Extension<AuthenticatedUser>,
    Query(params): Query<ListConversationsQuery>,
) -> Result<Json<ConversationListResponse>, (StatusCode, String)> {
    let requested_scope = ConversationHierarchyScope::parse(params.hierarchy_scope.as_deref())
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let (applied_scope, truncation_reasons, child_fanout_guardrail) =
        resolve_conversation_list_scope(requested_scope, params.parent_conversation_id.as_deref());
    let page_options = conversation_list_page_options(applied_scope, params.limit, params.offset);

    let mut estimated_query_count = 0_u64;
    let list_started = Instant::now();
    let page_result = conversations::list_conversations_with_hierarchy_page(
        &pool,
        params.organization.as_deref(),
        Some(&user.user_id),
        params.agent.as_deref(),
        params.status.as_deref(),
        params.updated_since.as_deref(),
        applied_scope,
        params.parent_conversation_id.as_deref(),
        page_options,
    )
    .await;
    record_db_operation(
        ROUTE_CONVERSATIONS,
        "conversation.list_with_hierarchy",
        list_started.elapsed(),
        Outcome::from_result(&page_result),
    );
    estimated_query_count += 1;
    let mut page = page_result.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut list = std::mem::take(&mut page.conversations);

    // Fable's durable children are the implementation/research conversations
    // Alex expects to monitor in Chat. Since their parent is intentionally
    // excluded from Chat, project those workers as standalone list rows. The
    // stored relationship is untouched and remains available to orchestration.
    if applied_scope == ConversationHierarchyScope::Roots
        && params.parent_conversation_id.is_none()
        && params.agent.is_none()
    {
        let coordinator = list
            .iter()
            .find(|conversation| !is_global_chat_conversation(conversation))
            .map(|conversation| (conversation.id.clone(), conversation.organization.clone()));
        if let Some((coordinator_id, coordinator_organization)) = coordinator {
            let workers_started = Instant::now();
            let workers_result = conversations::list_conversations_with_hierarchy_page(
                &pool,
                Some(coordinator_organization.as_str()),
                Some(&user.user_id),
                None,
                params.status.as_deref(),
                params.updated_since.as_deref(),
                ConversationHierarchyScope::Children,
                Some(&coordinator_id),
                conversations::ConversationListPageOptions::children(None, None),
            )
            .await;
            record_db_operation(
                ROUTE_CONVERSATIONS,
                "conversation.list_fable_workers",
                workers_started.elapsed(),
                Outcome::from_result(&workers_result),
            );
            estimated_query_count += 1;
            let workers =
                workers_result.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            list.extend(
                workers
                    .conversations
                    .into_iter()
                    .map(project_fable_worker_as_chat_root),
            );
        }
    }

    let active_count = list
        .iter()
        .filter(|conv| conv.is_active == Some(true))
        .count() as u64;
    let active_status_started = Instant::now();
    for conv in &mut list {
        if conv.is_active == Some(true) {
            let status =
                conversation_run_status_snapshot(&pool, &conv.id, Some(manager.as_ref())).await;
            if let Err(e) = status {
                record_db_operation(
                    ROUTE_CONVERSATIONS,
                    "conversation.active_status_snapshots",
                    active_status_started.elapsed(),
                    Outcome::Error,
                );
                return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
            }
            let status = status.expect("status result checked above");
            conv.is_active = Some(status.is_processing);
        }
    }
    if active_count > 0 {
        record_db_operation(
            ROUTE_CONVERSATIONS,
            "conversation.active_status_snapshots",
            active_status_started.elapsed(),
            Outcome::Success,
        );
        estimated_query_count += active_count.saturating_mul(8);
    }

    let enrichment_started = Instant::now();
    let conversations_result = conversation_list_items(&pool, &user.user_id, list).await;
    record_db_operation(
        ROUTE_CONVERSATIONS,
        "conversation.summary_enrichment",
        enrichment_started.elapsed(),
        Outcome::from_result(&conversations_result),
    );
    estimated_query_count += 2 + active_count.saturating_mul(4);
    let conversations =
        conversations_result.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let total = conversations.len() as i64;
    page.returned_count = conversations.len() as i64;

    let response = ConversationListResponse {
        conversations,
        total,
        meta: conversation_list_response_meta(
            requested_scope,
            applied_scope,
            params.parent_conversation_id.clone(),
            &page,
            child_fanout_guardrail,
            truncation_reasons,
        ),
    };
    let payload_bytes = observe_serialized_payload(
        ROUTE_CONVERSATIONS,
        "conversation_list",
        &response,
        response.conversations.len() as u64,
    )
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    record_db_query_count(
        ROUTE_CONVERSATIONS,
        "request.estimated_total",
        estimated_query_count,
        Outcome::Success,
    );
    maybe_log_economics_guardrail(
        pool.clone(),
        "chat",
        ROUTE_CONVERSATIONS,
        "list_conversations",
        estimated_query_count,
        payload_bytes,
        response.conversations.len() as u64,
    );

    Ok(Json(response))
}

/// Conversation scale cockpit (GET /api/conversations/scale-cockpit)
pub async fn get_conversation_scale_cockpit(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Query(params): Query<ConversationScaleCockpitQuery>,
) -> Result<Json<ConversationScaleCockpit>, (StatusCode, String)> {
    let snapshot = conversations::conversation_scale_cockpit(
        &pool,
        ConversationScaleCockpitOptions {
            organization: params.organization,
            user_id: Some(user.user_id),
            agent: params.agent,
            limit: params.limit.unwrap_or(10),
            include_archived_parents: params.include_archived_parents.unwrap_or(true),
        },
    )
    .await
    .map_err(|e| {
        metrics::counter!(
            "af_conversation_scale_cockpit_requests_total",
            "outcome" => "error",
            "health_status" => "unknown"
        )
        .increment(1);
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;

    metrics::counter!(
        "af_conversation_scale_cockpit_requests_total",
        "outcome" => "success",
        "health_status" => snapshot.health_status.clone()
    )
    .increment(1);
    metrics::histogram!(
        "af_conversation_scale_cockpit_response_payload_bytes",
        "health_status" => snapshot.health_status.clone()
    )
    .record(snapshot.estimated_response_payload_bytes as f64);
    metrics::histogram!(
        "af_conversation_scale_cockpit_query_count",
        "health_status" => snapshot.health_status.clone()
    )
    .record(snapshot.query_count_estimate as f64);

    Ok(Json(snapshot))
}

/// Get single conversation by ID (GET /api/conversations/:id)
pub async fn get_conversation(
    State(pool): State<Arc<SqlitePool>>,
    State(manager): State<Arc<ChatClientManager>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
) -> Result<Json<ConversationSummary>, (StatusCode, String)> {
    let conv = conversations::get_conversation(&pool, &id, true)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Conversation not found".to_string()))?;

    if conv.user_id != user.user_id {
        return Err((StatusCode::NOT_FOUND, "Conversation not found".to_string()));
    }

    let status = conversation_run_status_snapshot(&pool, &id, Some(manager.as_ref()))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let conversation = apply_run_status_to_conversation(conv, &status);
    let summary = conversation_summaries(&pool, vec![conversation])
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .into_iter()
        .next()
        .ok_or((
            StatusCode::INTERNAL_SERVER_ERROR,
            "Conversation summary missing".to_string(),
        ))?;

    Ok(Json(summary))
}

/// POST /api/conversations/:id/read
pub async fn mark_conversation_read(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
    Json(body): Json<MarkConversationReadBody>,
) -> Result<Json<ConversationReadState>, (StatusCode, String)> {
    conversations::mark_conversation_read(&pool, &user.user_id, &id, body.last_read_event_index)
        .await
        .map(Json)
        .map_err(conversation_read_error)
}

/// POST /api/conversations/read-states
pub async fn sync_conversation_read_states(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(body): Json<SyncConversationReadStatesBody>,
) -> Result<Json<ConversationReadStatesResponse>, (StatusCode, String)> {
    let states = body
        .states
        .into_iter()
        .map(|state| (state.conversation_id, state.last_read_event_index))
        .collect::<Vec<_>>();

    conversations::sync_conversation_read_states(&pool, &user.user_id, states)
        .await
        .map(|states| Json(ConversationReadStatesResponse { states }))
        .map_err(conversation_read_error)
}

fn conversation_read_error(error: anyhow::Error) -> (StatusCode, String) {
    let message = error.to_string();
    if message == "Conversation not found" {
        return (StatusCode::NOT_FOUND, message);
    }
    if message.contains("last_read_event_index") {
        return (StatusCode::BAD_REQUEST, message);
    }
    (StatusCode::INTERNAL_SERVER_ERROR, message)
}

/// Create a conversation (POST /api/conversations)
pub async fn create_conversation(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(mut req): Json<CreateConversationRequest>,
) -> Result<(StatusCode, Json<Conversation>), (StatusCode, String)> {
    if requests_fable_designation(req.agent.as_deref(), req.conversation_type.as_deref()) {
        return Err((
            StatusCode::CONFLICT,
            "Use GET /api/alex/coordinator for the permanent Fable coordinator".to_string(),
        ));
    }
    ensure_agent_allowed(&pool, &user.user_id, req.agent.as_deref()).await?;
    if req.parent_conversation_id.is_some() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Use POST /api/conversations/:id/children to create child conversations".to_string(),
        ));
    }
    if req.conversation_role.as_deref() == Some("sub_agent") {
        return Err((
            StatusCode::BAD_REQUEST,
            "Root conversations cannot use conversation_role=sub_agent".to_string(),
        ));
    }

    req.user_id = user.user_id;
    let conv = conversations::create_conversation(&pool, req)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok((StatusCode::CREATED, Json(conv)))
}

/// Create a multi-agent parent conversation and optional child conversations.
/// POST /api/conversations/multi-agent
pub async fn create_multi_agent_conversation(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(req): Json<CreateMultiAgentConversationRequest>,
) -> Result<(StatusCode, Json<MultiAgentConversationResponse>), Response> {
    if requests_fable_designation(req.agent.as_deref(), req.conversation_type.as_deref()) {
        return Err(text_error_response((
            StatusCode::CONFLICT,
            "Use GET /api/alex/coordinator for the permanent Fable coordinator".to_string(),
        )));
    }
    ensure_agent_allowed(&pool, &user.user_id, req.agent.as_deref())
        .await
        .map_err(text_error_response)?;
    let children_specs = req.children;
    for child in &children_specs {
        ensure_agent_allowed(&pool, &user.user_id, child.agent.as_deref())
            .await
            .map_err(text_error_response)?;
    }
    let resolved_handoffs =
        resolve_child_context_handoffs(&pool, &req.organization, &children_specs)
            .await
            .map_err(text_error_response)?;
    let child_requests =
        child_conversation_requests(&children_specs).map_err(text_error_response)?;
    let resolved_codex_options = resolve_initial_child_codex_options(&children_specs).await?;
    guard_runner_queue_admission(&pool, &children_specs, "create_multi_agent_conversation").await?;
    let prepared_turns = prepare_initial_child_turns(
        &user.user_id,
        &children_specs,
        &resolved_handoffs,
        &resolved_codex_options,
    )
    .map_err(text_error_response)?;
    let initial_turn_jobs = prepared_turns
        .iter()
        .map(|turn| turn.request.clone())
        .collect::<Vec<_>>();

    let parent = CreateConversationRequest {
        user_id: user.user_id,
        organization: req.organization,
        title: req.title,
        session_id: req.session_id,
        agent: req.agent,
        conversation_type: req.conversation_type,
        parent_conversation_id: None,
        conversation_role: Some("multi_agent_parent".to_string()),
        child_sort_order: None,
    };
    let created = conversations::create_multi_agent_conversation_with_initial_turn_jobs(
        &pool,
        parent,
        child_requests,
        initial_turn_jobs,
    )
    .await
    .map_err(|e| text_error_response((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())))?;
    publish_queued_child_turn_statuses(&pool, &created.queued_jobs)
        .await
        .map_err(text_error_response)?;
    record_prepared_child_batch_pending(
        &pool,
        &created.parent.id,
        &prepared_turns,
        &created.queued_jobs,
    )
    .await;
    let queued_turns = queued_child_turn_responses(&prepared_turns, &created.queued_jobs);
    let parent = created.parent;
    let children = created.children;
    let context_handoffs = context_handoff_responses(&children, &resolved_handoffs);

    Ok((
        StatusCode::CREATED,
        Json(MultiAgentConversationResponse {
            parent,
            children,
            queued_turns,
            context_handoffs,
        }),
    ))
}

/// List direct child conversations for a parent.
/// GET /api/conversations/:id/children
pub async fn list_child_conversations(
    State(pool): State<Arc<SqlitePool>>,
    State(manager): State<Arc<ChatClientManager>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
    Query(params): Query<ListConversationsQuery>,
) -> Result<Json<ConversationListResponse>, (StatusCode, String)> {
    let parent = require_user_conversation(&pool, &user.user_id, &id).await?;
    let page_options =
        conversations::ConversationListPageOptions::children(params.limit, params.offset);

    let mut page = conversations::list_conversations_with_hierarchy_page(
        &pool,
        params
            .organization
            .as_deref()
            .or(Some(parent.organization.as_str())),
        Some(&user.user_id),
        params.agent.as_deref(),
        params.status.as_deref(),
        params.updated_since.as_deref(),
        ConversationHierarchyScope::Children,
        Some(&id),
        page_options,
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut list = std::mem::take(&mut page.conversations);

    for conv in &mut list {
        if conv.is_active == Some(true) {
            let status = conversation_run_status_snapshot(&pool, &conv.id, Some(manager.as_ref()))
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            conv.is_active = Some(status.is_processing);
        }
    }

    let total = list.len() as i64;
    let conversations = conversation_list_items(&pool, &user.user_id, list)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    page.returned_count = conversations.len() as i64;

    Ok(Json(ConversationListResponse {
        conversations,
        total,
        meta: conversation_list_response_meta(
            ConversationHierarchyScope::Children,
            ConversationHierarchyScope::Children,
            Some(id),
            &page,
            false,
            Vec::new(),
        ),
    }))
}

/// Create child conversations under an existing parent.
/// POST /api/conversations/:id/children
pub async fn create_child_conversations(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
    Json(req): Json<CreateChildConversationsBody>,
) -> Result<(StatusCode, Json<MultiAgentConversationResponse>), Response> {
    let children_specs = req.children;
    for child in &children_specs {
        ensure_agent_allowed(&pool, &user.user_id, child.agent.as_deref())
            .await
            .map_err(text_error_response)?;
    }
    let parent_before = require_user_conversation(&pool, &user.user_id, &id)
        .await
        .map_err(text_error_response)?;
    let resolved_handoffs =
        resolve_child_context_handoffs(&pool, &parent_before.organization, &children_specs)
            .await
            .map_err(text_error_response)?;
    let child_requests =
        child_conversation_requests(&children_specs).map_err(text_error_response)?;
    let resolved_codex_options = resolve_initial_child_codex_options(&children_specs).await?;
    guard_runner_queue_admission(&pool, &children_specs, "create_child_conversations").await?;
    let prepared_turns = prepare_initial_child_turns(
        &user.user_id,
        &children_specs,
        &resolved_handoffs,
        &resolved_codex_options,
    )
    .map_err(text_error_response)?;
    let initial_turn_jobs = prepared_turns
        .iter()
        .map(|turn| turn.request.clone())
        .collect::<Vec<_>>();

    let created = conversations::create_child_conversations_with_initial_turn_jobs(
        &pool,
        &user.user_id,
        &id,
        child_requests,
        initial_turn_jobs,
    )
    .await
    .map_err(|e| text_error_response((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())))?;
    publish_queued_child_turn_statuses(&pool, &created.queued_jobs)
        .await
        .map_err(text_error_response)?;
    record_prepared_child_batch_pending(&pool, &id, &prepared_turns, &created.queued_jobs).await;
    let queued_turns = queued_child_turn_responses(&prepared_turns, &created.queued_jobs);
    let children = created.children;
    let context_handoffs = context_handoff_responses(&children, &resolved_handoffs);

    let parent = require_user_conversation(&pool, &user.user_id, &id)
        .await
        .map_err(text_error_response)?;

    Ok((
        StatusCode::CREATED,
        Json(MultiAgentConversationResponse {
            parent,
            children,
            queued_turns,
            context_handoffs,
        }),
    ))
}

/// Create a top-level branch from an existing conversation.
/// POST /api/conversations/:id/branch
pub async fn branch_conversation(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
    Json(req): Json<BranchConversationRequest>,
) -> Result<(StatusCode, Json<Conversation>), (StatusCode, String)> {
    reject_permanent_fable_mutation(&pool, &user.user_id, &id).await?;
    let conv = conversations::branch_conversation(&pool, &user.user_id, &id, req)
        .await
        .map_err(branch_conversation_error)?;

    Ok((StatusCode::CREATED, Json(conv)))
}

fn branch_conversation_error(error: anyhow::Error) -> (StatusCode, String) {
    let message = error.to_string();
    if message.starts_with("Source conversation not found:") {
        return (StatusCode::NOT_FOUND, "Conversation not found".to_string());
    }
    (StatusCode::INTERNAL_SERVER_ERROR, message)
}

/// Update a conversation (PATCH /api/conversations/:id)
pub async fn update_conversation(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
    Json(req): Json<UpdateConversationRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
    reject_permanent_fable_mutation(&pool, &user.user_id, &id).await?;
    conversations::update_conversation(&pool, &user.user_id, &id, req)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Set conversation to waiting (POST /api/conversations/:id/wait)
pub async fn wait_conversation(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    reject_permanent_fable_mutation(&pool, &user.user_id, &id).await?;
    conversations::wait_conversation(&pool, &user.user_id, &id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Activate a conversation — move from waiting/archived back to open (POST /api/conversations/:id/activate)
pub async fn activate_conversation(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    reject_permanent_fable_mutation(&pool, &user.user_id, &id).await?;
    conversations::activate_conversation(&pool, &user.user_id, &id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Archive a conversation (DELETE /api/conversations/:id)
/// Sets status='archived' and archived_at timestamp. Conversation data is preserved.
pub async fn delete_conversation(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    reject_permanent_fable_mutation(&pool, &user.user_id, &id).await?;
    conversations::archive_conversation(&pool, &user.user_id, &id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

async fn reject_permanent_fable_mutation(
    pool: &SqlitePool,
    user_id: &str,
    conversation_id: &str,
) -> Result<(), (StatusCode, String)> {
    let conversation = conversations::get_conversation(pool, conversation_id, false)
        .await
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Conversation not found".to_string()))?;
    if conversation.user_id != user_id {
        return Err((StatusCode::NOT_FOUND, "Conversation not found".to_string()));
    }
    if crate::fable_coordinator::is_fable_conversation(&conversation) {
        return Err((
            StatusCode::CONFLICT,
            "The permanent Fable coordinator cannot be branched, edited, archived, or moved out of open state"
                .to_string(),
        ));
    }
    Ok(())
}

/// Cancel a running conversation's agent (POST /api/conversations/:id/cancel)
pub async fn cancel_conversation(
    State(pool): State<Arc<SqlitePool>>,
    State(manager): State<Arc<ChatClientManager>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let cancel_requested_ms = chrono::Utc::now().timestamp_millis();
    // Verify conversation belongs to user
    let conv = conversations::get_conversation(&pool, &id, false)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Conversation not found".to_string()))?;
    if conv.user_id != user.user_id {
        return Err((StatusCode::NOT_FOUND, "Conversation not found".to_string()));
    }

    cancellation::record_request_received(&id, &user.user_id, cancel_requested_ms);
    manager.mark_cancelled_turn(&id).await;
    match agent_runners::request_cancel(&pool, &id).await {
        Ok(()) => {
            cancellation::record_durable_marker_written(
                &id,
                cancel_requested_ms,
                chrono::Utc::now().timestamp_millis(),
            );
        }
        Err(e) => {
            tracing::warn!(
                target: "agentic_api::cancel",
                cancel_phase = "durable_marker_write_failed",
                conversation_id = %id,
                cancel_requested_at_ms = cancel_requested_ms,
                error = %e,
                "[CANCEL] failed to persist cancellation request"
            );
        }
    }
    let cancelled_pending_jobs =
        match conversation_turn_jobs::cancel_pending_jobs_for_conversation(&pool, &id).await {
            Ok(count) => count,
            Err(e) => {
                tracing::warn!("[CANCEL] Failed to cancel pending jobs for {}: {}", id, e);
                0
            }
        };
    let worker_exists = WORKER_MANAGER.has_worker(&id).await;
    let runner_turn_exists = agent_runners::has_active_turn_for_conversation(&pool, &id)
        .await
        .unwrap_or(false);
    let mut runner_command_delivered = false;
    let mut runner_command_interrupted = false;
    let mut runner_command_error: Option<String> = None;
    let mut runner_command_observability_recorded = false;
    if runner_turn_exists {
        let command_started_ms = chrono::Utc::now().timestamp_millis();
        match runner_commands::send_cancel_conversation_command(&id).await {
            Ok(result) => {
                runner_command_delivered = true;
                runner_command_interrupted = result.interrupted;
                let command_elapsed_ms = chrono::Utc::now()
                    .timestamp_millis()
                    .saturating_sub(command_started_ms);
                let command_delivered_ms = chrono::Utc::now().timestamp_millis();
                cancellation::record_runner_command_delivered(
                    &id,
                    cancel_requested_ms,
                    command_started_ms,
                    command_delivered_ms,
                    worker_exists,
                    runner_turn_exists,
                    cancelled_pending_jobs,
                );
                runner_command_observability_recorded = true;
                if let Some(error) = result.error {
                    tracing::warn!(
                        "[CANCEL] Runner command delivered with interrupt error for conversation {} command_elapsed_ms={} marker_set={} interrupted={} error={}",
                        id,
                        command_elapsed_ms,
                        result.marker_set,
                        result.interrupted,
                        error
                    );
                    runner_command_error = Some(error);
                } else {
                    tracing::info!(
                        "[CANCEL] Runner command delivered for conversation {} command_elapsed_ms={} marker_set={} interrupted={}",
                        id,
                        command_elapsed_ms,
                        result.marker_set,
                        result.interrupted
                    );
                }
            }
            Err(e) => {
                let command_checked_ms = chrono::Utc::now().timestamp_millis();
                let command_elapsed_ms = command_checked_ms.saturating_sub(command_started_ms);
                cancellation::record_runner_command_unavailable(
                    &id,
                    cancel_requested_ms,
                    command_checked_ms,
                    worker_exists,
                    runner_turn_exists,
                    cancelled_pending_jobs,
                );
                runner_command_observability_recorded = true;
                tracing::warn!(
                    "[CANCEL] Runner command unavailable for conversation {} command_elapsed_ms={} error={} durable_marker_written=true",
                    id,
                    command_elapsed_ms,
                    e
                );
                runner_command_error = Some(e.to_string());
            }
        }
    }
    let runner_command_error = runner_command_error.unwrap_or_else(|| "none".to_string());

    // Try to interrupt the running agent
    let interrupt_started_ms = chrono::Utc::now().timestamp_millis();
    match manager.interrupt(&id).await {
        Ok(true) => {
            let interrupted_ms = chrono::Utc::now().timestamp_millis();
            if !runner_command_observability_recorded {
                cancellation::record_runner_command_delivered(
                    &id,
                    cancel_requested_ms,
                    interrupt_started_ms,
                    interrupted_ms,
                    worker_exists,
                    runner_turn_exists,
                    cancelled_pending_jobs,
                );
            }
            tracing::info!(
                "[CANCEL] Interrupted agent for conversation {} request_to_interrupt_ms={} interrupt_elapsed_ms={} worker_exists={} runner_turn_exists={} cancelled_pending_jobs={} runner_command_delivered={} runner_command_interrupted={} runner_command_error={}",
                id,
                interrupted_ms.saturating_sub(cancel_requested_ms),
                interrupted_ms.saturating_sub(interrupt_started_ms),
                worker_exists,
                runner_turn_exists,
                cancelled_pending_jobs,
                runner_command_delivered,
                runner_command_interrupted,
                runner_command_error
            );
            // Broadcast cancelled status so SSE clients get notified
            let event = crate::agents::StreamEvent::Status {
                status: "cancelled".to_string(),
                message: Some("Cancelled by user".to_string()),
            };
            if let Ok(json) = serde_json::to_string(&event) {
                let broadcast_tx = get_broadcast_sender(&id).await;
                let _ = broadcast_tx.send((-1, json));
            }
        }
        Ok(false) => {
            let no_live_turn_ms = chrono::Utc::now().timestamp_millis();
            if !runner_command_observability_recorded {
                cancellation::record_runner_command_unavailable(
                    &id,
                    cancel_requested_ms,
                    no_live_turn_ms,
                    worker_exists,
                    runner_turn_exists,
                    cancelled_pending_jobs,
                );
            }
            if worker_exists || runner_turn_exists {
                tracing::info!(
                    "[CANCEL] Marked active/queued turn cancelled for conversation {} request_to_marker_ms={} worker_exists={} runner_turn_exists={} cancelled_pending_jobs={} runner_command_delivered={} runner_command_interrupted={} runner_command_error={}",
                    id,
                    no_live_turn_ms.saturating_sub(cancel_requested_ms),
                    worker_exists,
                    runner_turn_exists,
                    cancelled_pending_jobs,
                    runner_command_delivered,
                    runner_command_interrupted,
                    runner_command_error
                );
            } else {
                tracing::info!(
                    "[CANCEL] No active runner turn for conversation {} request_to_idle_ms={} cancelled_pending_jobs={} runner_command_delivered={} runner_command_interrupted={} runner_command_error={}",
                    id,
                    no_live_turn_ms.saturating_sub(cancel_requested_ms),
                    cancelled_pending_jobs,
                    runner_command_delivered,
                    runner_command_interrupted,
                    runner_command_error
                );
                let _ = manager.consume_cancelled_turn(&id).await;
                match checkpoints::get_checkpoint(&pool, &id).await {
                    Ok(Some(checkpoint))
                        if is_active_checkpoint_status(Some(checkpoint.status.as_str())) =>
                    {
                        if let Err(e) = checkpoints::mark_interrupted(&pool, &id).await {
                            tracing::warn!(
                                "[CANCEL] Failed to clear stale checkpoint for {}: {}",
                                id,
                                e
                            );
                        }
                        if let Err(e) = publish_conversation_run_status(&pool, &id).await {
                            tracing::warn!(
                                "[CANCEL] Failed to publish stale checkpoint cleanup for {}: {}",
                                id,
                                e
                            );
                        }
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!(
                        "[CANCEL] Failed to inspect checkpoint for stale cleanup {}: {}",
                        id,
                        e
                    ),
                }
            }
        }
        Err(e) => {
            tracing::warn!(
                "[CANCEL] Interrupt failed for {} after {}ms: {}",
                id,
                chrono::Utc::now()
                    .timestamp_millis()
                    .saturating_sub(interrupt_started_ms),
                e
            );
            // Don't fail the request — the agent might have already finished
        }
    }

    Ok(StatusCode::OK)
}

/// List queued user messages waiting behind a running conversation turn.
/// These are durable `conversation_turn_jobs` rows with `status='pending'`.
pub async fn list_queued_messages(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
) -> Result<Json<Vec<ConversationQueuedMessage>>, (StatusCode, String)> {
    require_user_conversation(&pool, &user.user_id, &id).await?;

    let queued = list_pending_queued_messages(&pool, &id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(queued))
}

/// Cancel one queued message before the runner claims it.
pub async fn cancel_queued_message(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path((id, job_id)): Path<(String, String)>,
) -> Result<StatusCode, (StatusCode, String)> {
    require_user_conversation(&pool, &user.user_id, &id).await?;

    let status: Option<String> = sqlx::query_scalar(
        r#"
        SELECT status
        FROM conversation_turn_jobs
        WHERE id = ?
          AND conversation_id = ?
        "#,
    )
    .bind(&job_id)
    .bind(&id)
    .fetch_optional(&*pool)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    match status.as_deref() {
        Some("pending") => {}
        Some(_) => {
            return Err((
                StatusCode::CONFLICT,
                "Queued message is no longer pending".to_string(),
            ));
        }
        None => {
            return Err((
                StatusCode::NOT_FOUND,
                "Queued message not found".to_string(),
            ));
        }
    }

    let now = chrono::Utc::now().timestamp();
    let result = sqlx::query(
        r#"
        UPDATE conversation_turn_jobs
        SET status = 'cancelled',
            updated_at = ?,
            completed_at = ?,
            error_message = 'Cancelled by user from chat queue'
        WHERE id = ?
          AND conversation_id = ?
          AND status = 'pending'
        "#,
    )
    .bind(now)
    .bind(now)
    .bind(&job_id)
    .bind(&id)
    .execute(&*pool)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if result.rows_affected() == 0 {
        return Err((
            StatusCode::CONFLICT,
            "Queued message was already claimed".to_string(),
        ));
    }

    if let Err(e) = publish_conversation_run_status(&pool, &id).await {
        tracing::warn!(
            "[QUEUE] Failed to publish queue cancellation status for {}: {}",
            id,
            e
        );
    }

    Ok(StatusCode::NO_CONTENT)
}

/// Add a message to a conversation (POST /api/conversations/:id/messages)
pub async fn add_message(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
    Json(req): Json<AddMessageRequest>,
) -> Result<(StatusCode, Json<ConversationMessage>), (StatusCode, String)> {
    // Verify conversation exists and belongs to user
    let conv = conversations::get_conversation(&pool, &id, false)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Conversation not found".to_string()))?;
    if conv.user_id != user.user_id {
        return Err((StatusCode::NOT_FOUND, "Conversation not found".to_string()));
    }

    let msg = conversations::add_message(&pool, &id, req)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok((StatusCode::CREATED, Json(msg)))
}

#[derive(Debug, Deserialize)]
pub struct UpdateMessageRequest {
    pub content: String,
}

/// Update a message (PATCH /api/conversations/:id/messages/:message_id)
pub async fn update_message(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path((conv_id, message_id)): Path<(String, String)>,
    Json(req): Json<UpdateMessageRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
    // Verify conversation exists and belongs to user
    let conv = conversations::get_conversation(&pool, &conv_id, false)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Conversation not found".to_string()))?;
    if conv.user_id != user.user_id {
        return Err((StatusCode::NOT_FOUND, "Conversation not found".to_string()));
    }

    conversations::update_message(&pool, &message_id, &req.content)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Query params for `GET /api/conversations/:id/messages`. Clients that only
/// render a recent window pass a small `limit` to avoid downloading the full
/// conversation on cold start. Omitted `limit` = all messages.
///
/// `before` is a `message_index` cursor: when set, returns up to `limit`
/// messages strictly older than that index (chronological). Used by the
/// iOS app for pull-to-load-older pagination — pass the smallest known
/// `message_index` to load the previous page of older history.
///
/// `defer_active=true` is the ChatLab completed-turn reveal mode. While a
/// backend run is active, the endpoint omits the latest assistant row from the
/// response so clients can show only the user's message plus a processing
/// indicator. Once the run is terminal the same request returns the full
/// completed turn, including tool/thinking blocks.
#[derive(Debug, Deserialize)]
pub struct ListMessagesQuery {
    pub limit: Option<i64>,
    pub before: Option<i64>,
    pub defer_active: Option<bool>,
}

/// List messages for a conversation (GET /api/conversations/:id/messages)
pub async fn list_messages(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
    Query(params): Query<ListMessagesQuery>,
) -> Result<Json<Vec<ConversationMessage>>, (StatusCode, String)> {
    let mut estimated_query_count = 0_u64;

    // Verify conversation exists and belongs to user
    let conversation_started = Instant::now();
    let conv_result = conversations::get_conversation(&pool, &id, false).await;
    record_db_operation(
        ROUTE_CONVERSATION_MESSAGES,
        "conversation.lookup",
        conversation_started.elapsed(),
        Outcome::from_result(&conv_result),
    );
    estimated_query_count += 1;
    let conv = conv_result
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Conversation not found".to_string()))?;
    if conv.user_id != user.user_id {
        return Err((StatusCode::NOT_FOUND, "Conversation not found".to_string()));
    }

    let messages_started = Instant::now();
    let messages_result =
        conversations::list_messages(&pool, &id, params.limit, params.before).await;
    record_db_operation(
        ROUTE_CONVERSATION_MESSAGES,
        "conversation.messages_page",
        messages_started.elapsed(),
        Outcome::from_result(&messages_result),
    );
    estimated_query_count += 1;
    let mut messages =
        messages_result.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if params.defer_active.unwrap_or(false) {
        let defer_started = Instant::now();
        if let Err(e) = agent_runners::recover_stale_active_work_for_conversation(
            &pool,
            &id,
            ACTIVE_CHECKPOINT_STALE_SECONDS,
        )
        .await
        {
            tracing::warn!(
                "[CHAT-MESSAGES] Failed to recover stale active work for {} before defer_active: {}",
                id,
                e
            );
        }
        estimated_query_count += 1;
        let mut checkpoint = checkpoints::get_checkpoint(&pool, &id)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        estimated_query_count += 1;
        let checkpoint_status = checkpoint.as_ref().map(|cp| cp.status.clone());
        if repair_checkpoint_from_active_durable_work(&pool, &id, checkpoint_status.as_deref())
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        {
            estimated_query_count += 3;
            checkpoint = checkpoints::get_checkpoint(&pool, &id)
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            estimated_query_count += 1;
        }
        let is_processing = matches!(
            checkpoint.as_ref().map(|cp| cp.status.as_str()),
            Some("running") | Some("pending") | Some("queued")
        );
        if is_processing {
            if let Some(idx) = messages.iter().rposition(|m| {
                m.role == "assistant"
                    && !super::child_completion_status::is_child_completion_status_message(m)
            }) {
                messages.remove(idx);
            }
        }
        record_db_operation(
            ROUTE_CONVERSATION_MESSAGES,
            "conversation.defer_active_state",
            defer_started.elapsed(),
            Outcome::Success,
        );
    }

    let messages = messages
        .into_iter()
        .filter(|message| !super::child_completion_status::is_hidden_from_chat_display(message))
        .map(super::child_completion_status::sanitize_message_for_display)
        .collect::<Vec<_>>();
    let payload_bytes = observe_serialized_payload(
        ROUTE_CONVERSATION_MESSAGES,
        "message_page",
        &messages,
        messages.len() as u64,
    )
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    record_db_query_count(
        ROUTE_CONVERSATION_MESSAGES,
        "request.estimated_total",
        estimated_query_count,
        Outcome::Success,
    );
    maybe_log_economics_guardrail(
        pool.clone(),
        "chat",
        ROUTE_CONVERSATION_MESSAGES,
        "list_messages",
        estimated_query_count,
        payload_bytes,
        messages.len() as u64,
    );

    Ok(Json(messages))
}

/// Get checkpoint status for a conversation (GET /api/conversations/:id/checkpoint)
/// Returns whether an agent is actively processing this conversation.
///
/// On top of the basic `status/tool_call_count/updated_at` used by the
/// background-task path, this returns a richer snapshot for the iOS chat UI
/// to hydrate optimistic state on conversation re-entry:
///
///   * `last_event_index` — latest SSE event id persisted. Client uses this
///     as the `starting_after` cursor instead of whatever stale cursor it
///     had cached locally.
///   * `server_time` — wall-clock time on the server. Clients diff this
///     against `updated_at` to decide whether to show a "catching up…" pill
///     or discard the checkpoint as stale.
///   * `recent_events` — every event since the most recent terminator
///     (result / status=completed / cancelled / failed / timeout), minus
///     heartbeats. Capped at 200. Feeds through the same parseEvent pipeline
///     the SSE stream uses, so the UI can show the current tool card and
///     partial text BEFORE the SSE handshake completes. See
///     `conversations::get_active_run_events` for the selection logic.
pub async fn get_conversation_checkpoint(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
) -> Result<Json<ConversationCheckpointResponse>, (StatusCode, String)> {
    // Verify conversation belongs to user
    let conv = conversations::get_conversation(&pool, &id, false)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Conversation not found".to_string()))?;
    if conv.user_id != user.user_id {
        return Err((StatusCode::NOT_FOUND, "Conversation not found".to_string()));
    }

    if let Err(e) = agent_runners::recover_stale_active_work_for_conversation(
        &pool,
        &id,
        ACTIVE_CHECKPOINT_STALE_SECONDS,
    )
    .await
    {
        tracing::warn!(
            "[CHAT-CHECKPOINT] Failed to recover stale active work for {}: {}",
            id,
            e
        );
    }

    let mut checkpoint = ticketing_system::checkpoints::get_checkpoint(&pool, &id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let checkpoint_status = checkpoint.as_ref().map(|cp| cp.status.clone());
    if repair_checkpoint_from_active_durable_work(&pool, &id, checkpoint_status.as_deref())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    {
        checkpoint = ticketing_system::checkpoints::get_checkpoint(&pool, &id)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }

    let last_event_index = conversations::get_max_event_index(&pool, &id)
        .await
        .unwrap_or(-1);

    let server_time = chrono::Utc::now().timestamp();

    // Only bother shipping recent events when the agent is actually running
    // (or just finished) — no point paying the query cost for idle chats
    // where the client has all the history via /messages.
    let include_active = matches!(
        checkpoint.as_ref().map(|c| c.status.as_str()),
        Some("running") | Some("pending") | Some("queued")
    );

    let recent_events: Vec<RecentEvent> = if include_active {
        let raw = conversations::get_active_run_events(&pool, &id, 200)
            .await
            .unwrap_or_default();
        // Materialize payloads via load_event_payload_str so blob-offloaded
        // events (T-E184E642) surface the real JSON to the client, not the
        // `{"$blob":...}` sentinel. Inline rows round-trip as a cheap clone.
        let mut out = Vec::with_capacity(raw.len());
        for e in raw {
            let event_data = match conversations::load_event_payload_str(&pool, &e).await {
                Ok(s) => s,
                Err(err) => {
                    tracing::error!(
                        "Failed to materialize payload for event {}/{}: {}",
                        e.conversation_id,
                        e.event_index,
                        err
                    );
                    continue;
                }
            };
            out.push(RecentEvent {
                event_index: e.event_index,
                event_type: e.event_type,
                event_data,
                created_at: e.created_at,
            });
        }
        out
    } else {
        Vec::new()
    };

    match checkpoint {
        Some(cp) => Ok(Json(ConversationCheckpointResponse {
            status: cp.status,
            tool_call_count: cp.tool_call_count,
            updated_at: cp.updated_at,
            last_event_index,
            server_time,
            recent_events,
        })),
        None => Ok(Json(ConversationCheckpointResponse {
            status: "none".to_string(),
            tool_call_count: 0,
            updated_at: 0,
            last_event_index,
            server_time,
            recent_events,
        })),
    }
}

#[derive(Debug, Serialize)]
pub struct ConversationCheckpointResponse {
    pub status: String,
    pub tool_call_count: i32,
    pub updated_at: i64,
    /// Max event_index persisted to conversation_events. -1 if none.
    /// Clients use this as the `starting_after` cursor on SSE reconnect.
    pub last_event_index: i32,
    /// Server wall-clock time at response generation, for client staleness checks.
    pub server_time: i64,
    /// Snapshot of events in the currently-running turn, ordered by event_index.
    /// Empty when the agent isn't running. Heartbeats are filtered.
    pub recent_events: Vec<RecentEvent>,
}

#[derive(Debug, Serialize)]
pub struct RecentEvent {
    pub event_index: i32,
    pub event_type: String,
    /// Raw JSON string (same shape the SSE `data:` field carries). Ship it
    /// as-is so the client can reuse its SSE parser with zero divergence.
    pub event_data: String,
    pub created_at: i64,
}

/// GET /api/conversations/:id/agent-status
///
/// Bare-bones lifecycle endpoint for ChatLab. This intentionally returns no
/// assistant content and no recent event tail; clients use it only to decide
/// whether to show the single "agent is processing" state or fetch persisted
/// conversation output.
pub async fn get_conversation_run_status(
    State(pool): State<Arc<SqlitePool>>,
    State(manager): State<Arc<ChatClientManager>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
) -> Result<Json<ConversationRunStatusResponse>, (StatusCode, String)> {
    let conv = conversations::get_conversation(&pool, &id, false)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Conversation not found".to_string()))?;
    if conv.user_id != user.user_id {
        return Err((StatusCode::NOT_FOUND, "Conversation not found".to_string()));
    }

    let snapshot = conversation_run_status_snapshot(&pool, &id, Some(manager.as_ref()))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(snapshot))
}

/// GET /api/conversations/:id/agent-status/stream
///
/// Status-only SSE for the simplified app flow. The stream sends an initial
/// status snapshot, then only sends another snapshot when the backend publishes
/// a lifecycle transition. It never carries assistant tokens, tool progress, or
/// replayed content frames.
pub async fn stream_conversation_run_status(
    State(pool): State<Arc<SqlitePool>>,
    State(manager): State<Arc<ChatClientManager>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
) -> Result<Sse<Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>>, (StatusCode, String)>
{
    let conv = conversations::get_conversation(&pool, &id, false)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Conversation not found".to_string()))?;
    if conv.user_id != user.user_id {
        return Err((StatusCode::NOT_FOUND, "Conversation not found".to_string()));
    }

    let status_tx = get_status_sender(&id).await;
    let mut status_rx = status_tx.subscribe();
    drop(status_tx);

    let initial = conversation_run_status_snapshot(&pool, &id, Some(manager.as_ref()))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let stream_pool = pool.clone();
    let stream_manager = manager.clone();
    let stream_conversation_id = id.clone();

    let out = stream! {
        yield Ok(status_sse_event(&initial));
        if initial.is_processing {
            let mut poll = tokio::time::interval(Duration::from_secs(5));
            loop {
                tokio::select! {
                    recv = status_rx.recv() => {
                        match recv {
                            Ok(snapshot) => {
                                yield Ok(status_sse_event(&snapshot));
                                if !snapshot.is_processing {
                                    break;
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => {
                                match conversation_run_status_snapshot(
                                    &stream_pool,
                                    &stream_conversation_id,
                                    Some(stream_manager.as_ref()),
                                )
                                .await
                                {
                                    Ok(snapshot) => {
                                        yield Ok(status_sse_event(&snapshot));
                                        if !snapshot.is_processing {
                                            break;
                                        }
                                    }
                                    Err(e) => {
                                        tracing::error!(
                                            "Failed to rebuild chat run status after lag for {}: {}",
                                            stream_conversation_id,
                                            e
                                        );
                                        break;
                                    }
                                }
                            }
                            Err(broadcast::error::RecvError::Closed) => {
                                break;
                            }
                        }
                    }
                    _ = poll.tick() => {
                        match conversation_run_status_snapshot(
                            &stream_pool,
                            &stream_conversation_id,
                            Some(stream_manager.as_ref()),
                        )
                        .await
                        {
                            Ok(snapshot) => {
                                yield Ok(status_sse_event(&snapshot));
                                if !snapshot.is_processing {
                                    break;
                                }
                            }
                            Err(e) => {
                                tracing::error!(
                                    "Failed to poll chat run status for {}: {}",
                                    stream_conversation_id,
                                    e
                                );
                                break;
                            }
                        }
                    }
                }
            }
        } else {
            remove_status_sender(&stream_conversation_id).await;
        }
    };

    Ok(
        Sse::new(Box::pin(out) as Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>)
            .keep_alive(
                KeepAlive::new()
                    .interval(Duration::from_secs(30))
                    .text("ping"),
            ),
    )
}

fn status_sse_event(snapshot: &ConversationRunStatusResponse) -> Event {
    let payload = serde_json::to_string(snapshot)
        .expect("ConversationRunStatusResponse serialization should not fail");
    Event::default()
        .event("conversation_status")
        .id(snapshot.last_event_index.to_string())
        .data(payload)
}

#[derive(Debug, Deserialize)]
pub struct ConversationEventsPageQuery {
    pub starting_after: Option<i32>,
    pub limit: Option<i64>,
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct ConversationEventsPageResponse {
    pub events: Vec<RecentEvent>,
    pub last_event_index: i32,
    pub meta: ConversationEventsPageMeta,
}

#[derive(Debug, Serialize)]
pub struct ConversationEventsPageMeta {
    pub starting_after: i32,
    pub next_starting_after: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_limit: Option<i64>,
    pub applied_limit: i64,
    pub returned_count: usize,
    pub has_more: bool,
    pub truncated: bool,
    pub payload_bytes: usize,
    pub max_payload_bytes: usize,
    pub max_event_payload_bytes: usize,
    pub skipped_oversized_event_count: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped_oversized_event_indexes: Vec<i32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub truncation_reasons: Vec<String>,
}

fn applied_event_page_limit(requested: Option<i64>) -> i64 {
    requested
        .unwrap_or(DEFAULT_EVENT_PAGE_LIMIT)
        .clamp(1, MAX_EVENT_PAGE_LIMIT)
}

fn applied_event_page_payload_bytes(requested: Option<usize>) -> usize {
    requested
        .unwrap_or(DEFAULT_EVENT_PAGE_PAYLOAD_BYTES)
        .clamp(MAX_SINGLE_EVENT_PAYLOAD_BYTES, MAX_EVENT_PAGE_PAYLOAD_BYTES)
}

/// GET /api/v1/conversations/:id/events/page?starting_after=N&limit=M
/// JSON event replay endpoint used by iOS delta-sync. Unlike `/checkpoint`,
/// this returns the actual rows after the supplied cursor for both active and
/// idle conversations, so foreground repair does not depend on a capped active
/// checkpoint tail.
pub async fn list_conversation_events_page(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
    Query(query): Query<ConversationEventsPageQuery>,
) -> Result<Json<ConversationEventsPageResponse>, (StatusCode, String)> {
    let mut estimated_query_count = 0_u64;

    let conversation_started = Instant::now();
    let conv_result = conversations::get_conversation(&pool, &id, false).await;
    record_db_operation(
        ROUTE_CONVERSATION_EVENTS_PAGE,
        "conversation.lookup",
        conversation_started.elapsed(),
        Outcome::from_result(&conv_result),
    );
    estimated_query_count += 1;
    let conv = conv_result
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Conversation not found".to_string()))?;
    if conv.user_id != user.user_id {
        return Err((StatusCode::NOT_FOUND, "Conversation not found".to_string()));
    }

    let after = query.starting_after.unwrap_or(-1);
    let limit = applied_event_page_limit(query.limit);
    let max_payload_bytes = applied_event_page_payload_bytes(query.max_bytes);
    let max_event_started = Instant::now();
    let last_event_index_result = conversations::get_max_event_index(&pool, &id).await;
    record_db_operation(
        ROUTE_CONVERSATION_EVENTS_PAGE,
        "conversation.max_event_index",
        max_event_started.elapsed(),
        Outcome::from_result(&last_event_index_result),
    );
    estimated_query_count += 1;
    let last_event_index = last_event_index_result.unwrap_or(-1);

    let events_started = Instant::now();
    let raw_result = conversations::get_events_after_limited(&pool, &id, after, limit + 1).await;
    record_db_operation(
        ROUTE_CONVERSATION_EVENTS_PAGE,
        "conversation.events_page",
        events_started.elapsed(),
        Outcome::from_result(&raw_result),
    );
    estimated_query_count += 1;
    let raw = raw_result.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut events = Vec::with_capacity(raw.len());
    let mut payload_bytes = 0_usize;
    let mut next_starting_after = after;
    let mut has_more = false;
    let mut truncated = query
        .limit
        .map(|requested| requested > limit)
        .unwrap_or(false);
    let mut truncation_reasons = Vec::new();
    if truncated {
        truncation_reasons.push("requested_limit_capped".to_string());
    }
    let mut skipped_oversized_event_indexes = Vec::new();
    let materialize_started = Instant::now();
    for e in raw {
        if events.len() as i64 >= limit {
            has_more = true;
            truncated = true;
            truncation_reasons.push("page_limit_reached".to_string());
            break;
        }
        let event_data = conversations::load_event_payload_str(&pool, &e)
            .await
            .map_err(|err| {
                tracing::error!(
                    "Failed to materialize payload for event {}/{}: {}",
                    e.conversation_id,
                    e.event_index,
                    err
                );
                (StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
            })?;
        let event_bytes = event_data.len();
        if event_bytes > MAX_SINGLE_EVENT_PAYLOAD_BYTES {
            truncated = true;
            next_starting_after = e.event_index;
            skipped_oversized_event_indexes.push(e.event_index);
            if !truncation_reasons
                .iter()
                .any(|reason| reason == "oversized_event_skipped")
            {
                truncation_reasons.push("oversized_event_skipped".to_string());
            }
            tracing::warn!(
                conversation_id = %id,
                event_index = e.event_index,
                event_type = %e.event_type,
                event_bytes,
                max_event_payload_bytes = MAX_SINGLE_EVENT_PAYLOAD_BYTES,
                "Skipping oversized conversation event from page response"
            );
            continue;
        }
        if !events.is_empty() && payload_bytes.saturating_add(event_bytes) > max_payload_bytes {
            has_more = true;
            truncated = true;
            truncation_reasons.push("payload_byte_budget_reached".to_string());
            break;
        }
        payload_bytes = payload_bytes.saturating_add(event_bytes);
        next_starting_after = e.event_index;
        events.push(RecentEvent {
            event_index: e.event_index,
            event_type: e.event_type,
            event_data,
            created_at: e.created_at,
        });
    }
    if !events.is_empty() {
        record_db_operation(
            ROUTE_CONVERSATION_EVENTS_PAGE,
            "conversation.event_payload_materialization",
            materialize_started.elapsed(),
            Outcome::Success,
        );
        estimated_query_count += events.len() as u64;
    }
    if last_event_index > next_starting_after {
        has_more = true;
    }

    let returned_count = events.len();
    let skipped_oversized_event_count = skipped_oversized_event_indexes.len();
    let response = ConversationEventsPageResponse {
        events,
        last_event_index,
        meta: ConversationEventsPageMeta {
            starting_after: after,
            next_starting_after,
            requested_limit: query.limit,
            applied_limit: limit,
            returned_count,
            has_more,
            truncated,
            payload_bytes,
            max_payload_bytes,
            max_event_payload_bytes: MAX_SINGLE_EVENT_PAYLOAD_BYTES,
            skipped_oversized_event_count,
            skipped_oversized_event_indexes,
            truncation_reasons,
        },
    };
    let payload_bytes = observe_serialized_payload(
        ROUTE_CONVERSATION_EVENTS_PAGE,
        "conversation_events_page",
        &response,
        response.events.len() as u64,
    )
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    record_db_query_count(
        ROUTE_CONVERSATION_EVENTS_PAGE,
        "request.estimated_total",
        estimated_query_count,
        Outcome::Success,
    );
    maybe_log_economics_guardrail(
        pool.clone(),
        "chat",
        ROUTE_CONVERSATION_EVENTS_PAGE,
        "list_conversation_events_page",
        estimated_query_count,
        payload_bytes,
        response.events.len() as u64,
    );

    Ok(Json(response))
}

/// GET /api/v1/conversations/:id/events
/// SSE reconnection endpoint: replays stored events after the supplied cursor, then tails live events while an agent is running.
pub async fn reconnect_conversation_stream(
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ResumeQuery>,
    State(db): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<Sse<Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>>, (StatusCode, String)>
{
    // Verify the authenticated user owns this conversation
    let conv = conversations::get_conversation(&db, &id, false).await;
    match conv {
        Ok(Some(c)) if c.user_id == user.user_id => {}
        _ => {
            // Not found or not owned — return an empty stream that closes immediately
            let empty = futures::stream::empty();
            return Ok(Sse::new(
                Box::pin(empty) as Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>
            )
            .keep_alive(
                KeepAlive::new()
                    .interval(Duration::from_secs(30))
                    .text("ping"),
            ));
        }
    }

    let checkpoint_status = match checkpoints::get_checkpoint(&db, &id).await {
        Ok(Some(cp)) => cp.status,
        _ => "none".to_string(),
    };

    let cursor = match extract_cursor(&headers, &query) {
        Ok(cursor) => cursor,
        Err(CursorError::Malformed(detail)) => return Err((StatusCode::BAD_REQUEST, detail)),
        Err(CursorError::Retention { .. }) => {
            unreachable!("extract_cursor never produces CursorError::Retention")
        }
    };
    let after = i32::try_from(cursor.event_index).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            format!(
                "resume cursor is outside the supported event index range: {}",
                cursor.event_index
            ),
        )
    })?;
    let resume = cursor.is_resume();

    let events = if resume {
        conversations::get_events_after(&db, &id, after)
            .await
            .unwrap_or_default()
    } else {
        conversations::get_events(&db, &id)
            .await
            .unwrap_or_default()
    };

    // Cursor-expired detection: the client supplied a resume cursor
    // but either (a) the allocator has already moved past N and no event
    // at index N+1 is retained in the log (first returned event has an
    // index well above N+1).
    // We fold this into the normal SSE response (no 410 frame on the
    // wire — iOS re-syncs via full message fetch) and only emit the
    // metric so operators can chart the rate. Feature retention work
    // (T-future) will convert this into an actual HTTP 410.
    let mut cursor_expired = false;
    if resume {
        let oldest_retained = match conversations::get_events(&db, &id).await {
            Ok(ref all) if !all.is_empty() => Some(all[0].event_index),
            _ => None,
        };
        if let Some(oldest) = oldest_retained {
            if oldest > after + 1 {
                record_cursor_expired(&id, after, oldest);
                cursor_expired = true;
            }
        }
    }

    // Record the stream-opened metric BEFORE we hand the body off so the
    // gauge/counter reflect the connection immediately, not after the
    // first event flushes. The matching close is emitted by the
    // StreamCloseGuard wrapper below when the stream is dropped.
    record_stream_opened(&id, &user.user_id, resume);

    let inner = super::chat_stream::create_conversation_reconnect_stream(
        db,
        id.clone(),
        events,
        checkpoint_status,
    );

    // Box-pin the inner stream so the drop-guard wrapper has a
    // concrete `Unpin`-able handle. The underlying async_stream
    // `AsyncStream` is not itself `Unpin`, so we must pin on the heap.
    let inner_boxed: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> =
        Box::pin(inner);

    // Drop-guard adapter: wraps the inner stream, records close when the
    // stream terminates (natural end-of-stream OR client disconnect —
    // axum drops the stream on both paths). If the cursor was detected
    // as expired above, pre-seed the close reason so the close metric
    // fires with `reason="cursor_expired"` instead of the default
    // `client_disconnect`.
    let mut guarded = StreamCloseGuard::new(id, inner_boxed);
    if cursor_expired {
        guarded.reason = DisconnectReason::CursorExpired;
    }

    Ok(Sse::new(Box::pin(guarded) as Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(30))
                .text("ping"),
        ))
}

/// RAII stream wrapper that records the matching `stream_closed_total`
/// counter + `stream_duration_ms` histogram observation when the stream
/// is dropped.
///
/// The close reason is inferred from whether the underlying stream ever
/// ran to completion (`Normal`) or was dropped mid-flight
/// (`ClientDisconnect`). More specific reasons — idle timeouts or
/// cursor-expired rejections — are emitted at the call site and
/// suppress the generic close recorded here via `set_reason`.
struct StreamCloseGuard {
    conversation_id: String,
    opened_at: std::time::Instant,
    reason: DisconnectReason,
    inner: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>,
    closed: bool,
}

impl StreamCloseGuard {
    fn new(
        conversation_id: String,
        inner: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>,
    ) -> Self {
        Self {
            conversation_id,
            opened_at: std::time::Instant::now(),
            reason: DisconnectReason::ClientDisconnect,
            inner,
            closed: false,
        }
    }
}

impl Stream for StreamCloseGuard {
    type Item = Result<Event, Infallible>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let poll = self.inner.as_mut().poll_next(cx);
        if matches!(poll, std::task::Poll::Ready(None)) {
            // Natural end-of-stream: server drained everything and the
            // generator yielded Poll::Ready(None). Mark the close as
            // normal so the drop handler emits `reason="normal"`.
            self.reason = DisconnectReason::Normal;
        }
        poll
    }
}

impl Drop for StreamCloseGuard {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        let duration_ms = self.opened_at.elapsed().as_millis().min(u64::MAX as u128) as u64;
        record_stream_closed(&self.conversation_id, duration_ms, self.reason);
    }
}

/// SSE event types for conversation updates
#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum ConversationStreamEvent {
    /// Full list of conversations (sent on connect and when changes detected)
    #[serde(rename = "sync")]
    Sync {
        conversations: Vec<ConversationListItem>,
        updated_at: i64,
    },
}

/// GET /api/conversations/subscribe
/// SSE endpoint for real-time conversation list updates
pub async fn subscribe_conversations(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Query(params): Query<ListConversationsQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let user_id = user.user_id.clone();
    let stream = async_stream::stream! {
        // Track the last update time we've seen
        let mut last_sync_hash: u64 = 0;

        loop {
            // Get current conversations for this user
            match conversations::list_conversations(&pool, params.organization.as_deref(), Some(&user_id), params.agent.as_deref(), params.status.as_deref(), None, None).await {
                Ok(convs) => {
                    let summaries = match conversation_list_items(&pool, &user_id, convs).await {
                        Ok(summaries) => summaries,
                        Err(e) => {
                            tracing::error!("Failed to summarize conversations for SSE: {}", e);
                            continue;
                        }
                    };
                    // Simple change detection: hash the updated_at timestamps
                    use std::hash::{Hash, Hasher};
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    for summary in &summaries {
                        summary.updated_at.hash(&mut hasher);
                        summary.id.hash(&mut hasher);
                        summary.parent_conversation_id.hash(&mut hasher);
                        summary.conversation_role.hash(&mut hasher);
                        summary.child_conversation_count.hash(&mut hasher);
                        summary.active_child_conversation_count.hash(&mut hasher);
                        summary.unread_child_conversation_count.hash(&mut hasher);
                        summary.last_event_index.hash(&mut hasher);
                        summary.last_read_event_index.hash(&mut hasher);
                        summary.unread_event_count.hash(&mut hasher);
                        summary.is_active.hash(&mut hasher);
                        summary.tool_call_count.hash(&mut hasher);
                        summary.run_started_at.hash(&mut hasher);
                        summary.last_tool_call_started_at_epoch.hash(&mut hasher);
                    }
                    summaries.len().hash(&mut hasher);
                    let current_hash = hasher.finish();

                    // Only send if changed
                    if current_hash != last_sync_hash {
                        last_sync_hash = current_hash;
                        let event = ConversationStreamEvent::Sync {
                            conversations: summaries,
                            updated_at: chrono::Utc::now().timestamp(),
                        };
                        if let Ok(json) = serde_json::to_string(&event) {
                            yield Ok(Event::default().data(json));
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to list conversations for SSE: {}", e);
                }
            }

            // Poll every 10 seconds (was 2s — reduced to save battery/radio on mobile)
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    };

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(30))
            .text("ping"),
    )
}

/// GET /api/chat-attachments/:conversation_id/:filename
/// Serve a chat attachment stored for a conversation the user owns.
pub async fn get_chat_attachment(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path((conversation_id, filename)): Path<(String, String)>,
) -> Response {
    match conversations::get_conversation(&pool, &conversation_id, false).await {
        Ok(Some(conv)) if conv.user_id == user.user_id => {}
        _ => return StatusCode::NOT_FOUND.into_response(),
    }

    let Some((path, mime)) = find_chat_attachment(&pool, &conversation_id, &filename).await else {
        return StatusCode::NOT_FOUND.into_response();
    };

    match tokio::fs::read(&path).await {
        Ok(data) => ([(header::CONTENT_TYPE, mime)], data).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn find_chat_attachment(
    pool: &SqlitePool,
    conversation_id: &str,
    filename: &str,
) -> Option<(PathBuf, String)> {
    if filename.contains('/') || filename.contains('\\') {
        return None;
    }

    let rows: Vec<(Option<String>,)> = sqlx::query_as(
        r#"
        SELECT attachments
        FROM conversation_messages
        WHERE conversation_id = ?
          AND attachments IS NOT NULL
        "#,
    )
    .bind(conversation_id)
    .fetch_all(pool)
    .await
    .ok()?;

    let home = dirs::home_dir()?;
    let attachment_root = home
        .join(".agentic-flowstate")
        .join("chat-attachments")
        .join(conversation_id);
    let legacy_image_root = home
        .join(".agentic-flowstate")
        .join("chat-images")
        .join(conversation_id);

    for (raw,) in rows {
        let Some(raw) = raw else { continue };
        let metas: Vec<ChatAttachmentMeta> = match serde_json::from_str(&raw) {
            Ok(metas) => metas,
            Err(_) => continue,
        };
        for meta in metas {
            if meta.filename != filename {
                continue;
            }
            let path = PathBuf::from(&meta.path);
            if path.starts_with(&attachment_root) || path.starts_with(&legacy_image_root) {
                return Some((path, meta.mime_type));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::context_packets::{
        ContextPacketItemSummary, ContextPacketSummary, RetrievalEventSummary,
    };
    use crate::handlers::conversation_handoff::{ResolvedContextHandoff, ResolvedContextPacket};
    use serde_json::{json, Value};

    const PACKET_ID: &str = "CP-API1234";
    const RETRIEVAL_ID: &str = "R-API1234";
    const BOUNDED_SNIPPET: &str = "Bounded multi-agent handoff snippet.";
    const RAW_PARENT_TRANSCRIPT_SENTINEL: &str = "RAW_PARENT_TRANSCRIPT_SENTINEL";
    const FULL_OUTPUT_SENTINEL: &str = "FULL_OUTPUT_SENTINEL";

    #[test]
    fn evaluator_child_defaults_to_context_aware_prompt() {
        assert_eq!(
            default_child_prompt_name("conversation-evaluator"),
            "conversation-evaluator-system"
        );
        assert_eq!(default_child_prompt_name("codex"), "full-access");
        assert_eq!(canonical_child_prompt_name("codex"), "full-access");
        assert_eq!(default_child_prompt_name("feedback"), "feedback");
    }

    #[test]
    fn child_request_stores_codex_alias_as_full_access() {
        let requests = child_conversation_requests(&[CreateChildConversationSpec {
            title: "Child".to_string(),
            agent: Some("codex".to_string()),
            conversation_type: None,
            child_sort_order: None,
            handoff: ContextHandoffRequest::default(),
            initial_message: Some("Do work".to_string()),
            prompt_name: Some("codex".to_string()),
            working_dir: None,
            client_id: None,
            model: None,
            reasoning_effort: None,
        }])
        .expect("child request");

        assert_eq!(requests[0].agent.as_deref(), Some("full-access"));
    }

    #[test]
    fn child_request_rejects_fable_coordinator() {
        let error = child_conversation_requests(&[CreateChildConversationSpec {
            title: "Invalid coordinator child".to_string(),
            agent: Some("fable-coordinator".to_string()),
            conversation_type: None,
            child_sort_order: None,
            handoff: ContextHandoffRequest::default(),
            initial_message: Some("Coordinate".to_string()),
            prompt_name: None,
            working_dir: None,
            client_id: None,
            model: None,
            reasoning_effort: None,
        }])
        .expect_err("Fable must remain a singleton parent");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert!(error.1.contains("cannot be created as a child"));
    }

    fn conversation_with_activity(is_active: Option<bool>) -> Conversation {
        Conversation {
            id: "conv-1".to_string(),
            user_id: "alex".to_string(),
            session_id: None,
            organization: "agentic-flowstate".to_string(),
            agent: Some("full-access".to_string()),
            conversation_type: Some("bug".to_string()),
            parent_conversation_id: None,
            conversation_role: "standard".to_string(),
            child_conversation_count: Some(0),
            child_sort_order: None,
            title: "Conversation Error Investigation".to_string(),
            started_at: "2026-05-11T22:22:17Z".to_string(),
            updated_at: "2026-05-11T22:39:19Z".to_string(),
            status: "open".to_string(),
            archived_at: None,
            router_ticket_id: None,
            router_organization: None,
            message_count: Some(2),
            last_event_index: Some(128),
            last_read_event_index: None,
            unread_event_count: None,
            is_active,
            messages: Some(vec![]),
        }
    }

    #[test]
    fn conversation_list_item_serializes_compact_sidebar_shape() {
        let mut conversation = conversation_with_activity(Some(true));
        conversation.child_conversation_count = Some(3);
        conversation.router_ticket_id = Some("T-12345678".to_string());
        conversation.router_organization = Some("agentic-flowstate".to_string());

        let mut metadata = ConversationRunMetadataMaps::default();
        metadata.tool_call_counts.insert(conversation.id.clone(), 7);
        metadata
            .run_started_times
            .insert(conversation.id.clone(), 1_783_305_000);
        metadata
            .last_tool_calls
            .insert(conversation.id.clone(), 1_783_305_100);

        let item = ConversationListItem::from_conversation(
            conversation,
            &metadata,
            Some(ChildActivityCounts {
                active: 1,
                unread: 2,
            }),
        );
        let value = serde_json::to_value(&item).expect("serialize list item");

        assert_eq!(value["id"], "conv-1");
        assert_eq!(value["child_conversation_count"], 3);
        assert_eq!(value["active_child_conversation_count"], 1);
        assert_eq!(value["unread_child_conversation_count"], 2);
        assert_eq!(value["tool_call_count"], 7);
        assert_eq!(value["run_started_at"], 1_783_305_000);
        assert_eq!(value["last_tool_call_started_at_epoch"], 1_783_305_100);
        assert!(value.get("user_id").is_none());
        assert!(value.get("session_id").is_none());
        assert!(value.get("started_at").is_none());
        assert!(value.get("archived_at").is_none());
        assert!(value.get("router_ticket_id").is_none());
        assert!(value.get("router_organization").is_none());
        assert!(value.get("messages").is_none());
    }

    #[test]
    fn global_chat_list_excludes_the_permanent_fable_coordinator() {
        let mut coordinator = conversation_with_activity(Some(false));
        coordinator.agent = Some(crate::fable_coordinator::FABLE_AGENT.to_string());
        coordinator.conversation_type =
            Some(crate::fable_coordinator::FABLE_CONVERSATION_TYPE.to_string());

        assert!(!is_global_chat_conversation(&coordinator));
        assert!(is_global_chat_conversation(&conversation_with_activity(
            Some(false)
        )));
    }

    #[test]
    fn global_chat_list_fails_closed_for_partial_coordinator_designation() {
        let mut agent_only = conversation_with_activity(Some(false));
        agent_only.agent = Some(crate::fable_coordinator::FABLE_AGENT.to_string());
        let mut type_only = conversation_with_activity(Some(false));
        type_only.conversation_type =
            Some(crate::fable_coordinator::FABLE_CONVERSATION_TYPE.to_string());

        assert!(!is_global_chat_conversation(&agent_only));
        assert!(!is_global_chat_conversation(&type_only));
    }

    #[test]
    fn fable_workers_are_projected_as_standalone_chat_rows() {
        let mut worker = conversation_with_activity(Some(true));
        worker.id = "worker-1".to_string();
        worker.parent_conversation_id = Some("alex-coordinator".to_string());
        worker.conversation_role = "sub_agent".to_string();

        let projected = project_fable_worker_as_chat_root(worker.clone());

        assert_eq!(projected.id, worker.id);
        assert_eq!(projected.agent, worker.agent);
        assert!(projected.parent_conversation_id.is_none());
        assert_eq!(projected.conversation_role, "standard");
    }

    fn run_status(is_processing: bool) -> ConversationRunStatusResponse {
        ConversationRunStatusResponse {
            conversation_id: "conv-1".to_string(),
            status: if is_processing {
                "running"
            } else {
                "completed"
            }
            .to_string(),
            checkpoint_status: Some(
                if is_processing {
                    "running"
                } else {
                    "completed"
                }
                .to_string(),
            ),
            is_processing,
            should_fetch: !is_processing,
            updated_at: 1_778_539_159,
            last_event_index: 128,
            tool_call_count: 3,
            queued_message_count: 0,
            run_started_at: Some(1_778_539_000),
            last_tool_call_started_at_epoch: Some(1_778_539_100),
            server_time: 1_778_539_160,
        }
    }

    fn resolved_handoff() -> ResolvedContextHandoff {
        ResolvedContextHandoff {
            packets: vec![ResolvedContextPacket {
                summary: ContextPacketSummary {
                    packet_id: PACKET_ID.to_string(),
                    retrieval_id: Some(RETRIEVAL_ID.to_string()),
                    ticket_id: Some("T-API1234".to_string()),
                    repository: Some("agentic-flowstate-api".to_string()),
                    work_summary: "multi-agent packet handoff".to_string(),
                    created_by: "api-handoff-test".to_string(),
                    created_by_agent: Some("workspace-manager".to_string()),
                    summary: "Context packet for multi-agent packet handoff.".to_string(),
                    warnings: vec!["packet_truncated".to_string()],
                    token_budget: Some(2_000),
                    token_count: Some(52),
                    created_at: 1_781_662_300,
                    metadata: json!({"source": "api-context-gather"}),
                },
                items: vec![ContextPacketItemSummary {
                    rank: 1,
                    item_type: "chunk".to_string(),
                    artifact_id: Some("A-API1234".to_string()),
                    chunk_id: Some("C-API1234-1".to_string()),
                    knowledge_id: None,
                    ticket_id: Some("T-API1234".to_string()),
                    document_id: None,
                    entity_id: None,
                    citation_label: Some("A-API1234#C-API1234-1".to_string()),
                    relevance_reason: "selected retrieval chunk".to_string(),
                    included_text: Some(BOUNDED_SNIPPET.to_string()),
                    token_count: Some(10),
                    source_retrieval_rank: Some(1),
                    metadata: json!({"matched_fields": ["content"]}),
                }],
            }],
            retrievals: vec![RetrievalEventSummary {
                retrieval_id: RETRIEVAL_ID.to_string(),
                organization: "agentic-flowstate".to_string(),
                actor_type: "agent".to_string(),
                actor_id: "api-handoff-test".to_string(),
                tool_name: "gather_context".to_string(),
                work_summary: Some("multi-agent packet handoff".to_string()),
                query_text: "multi-agent packet handoff query".to_string(),
                normalized_query: Some("multi-agent packet handoff query".to_string()),
                filters: json!({"ticket_id": "T-API1234"}),
                authorization_filter: json!({
                    "organization": "agentic-flowstate",
                    "visibility": ["organization", "system"]
                }),
                strategy: "fts_facets_links_v1".to_string(),
                started_at: 1_781_662_300,
                elapsed_ms: 21,
                result_count: 4,
                selected_count: 1,
                empty_result: false,
                context_token_count: Some(52),
                context_truncated: true,
                warnings: vec!["packet_truncated".to_string()],
                metadata: json!({"query_terms": ["multi-agent", "handoff"]}),
            }],
        }
    }

    #[test]
    fn apply_run_status_clears_stale_selected_conversation_activity() {
        let conv = conversation_with_activity(Some(true));
        let conv = apply_run_status_to_conversation(conv, &run_status(false));

        assert_eq!(conv.is_active, Some(false));
    }

    #[test]
    fn apply_run_status_sets_selected_conversation_activity() {
        let conv = conversation_with_activity(None);
        let conv = apply_run_status_to_conversation(conv, &run_status(true));

        assert_eq!(conv.is_active, Some(true));
    }

    #[test]
    fn child_spec_accepts_packet_handoff_fields_at_api_boundary() {
        let spec: CreateChildConversationSpec = serde_json::from_value(json!({
            "title": "Focused child",
            "agent": "workspace-manager",
            "initial_message": "Run child task",
            "context_packet_ids": [PACKET_ID],
            "retrieval_ids": [RETRIEVAL_ID]
        }))
        .expect("deserialize child spec");

        assert!(spec.handoff.has_handles());
        assert_eq!(spec.handoff.context_packet_ids, vec![PACKET_ID.to_string()]);
        assert_eq!(spec.handoff.retrieval_ids, vec![RETRIEVAL_ID.to_string()]);
    }

    #[test]
    fn multi_agent_response_and_runner_metadata_keep_handoff_compact() {
        let mut child = conversation_with_activity(None);
        child.id = "child-1".to_string();
        let handoff = resolved_handoff();
        let responses = context_handoff_responses(&[child], &[Some(handoff)]);

        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].child_conversation_id, "child-1");
        assert_eq!(responses[0].context_packet_ids, vec![PACKET_ID.to_string()]);
        assert_eq!(responses[0].retrieval_ids, vec![RETRIEVAL_ID.to_string()]);

        let handoff = resolved_handoff();
        let metadata = orchestrated_child_turn_metadata(
            "api",
            "workspace-manager",
            Some(&handoff),
            Some("child-batch-test"),
            2,
            1,
            Some("af-report-test"),
        )
        .expect("child turn metadata");
        let value: Value = serde_json::from_str(&metadata).expect("metadata json");
        assert_eq!(
            value["artifact_memory_handoff"]["context_packet_ids"],
            json!([PACKET_ID])
        );
        assert_eq!(
            value["artifact_memory_handoff"]["retrieval_ids"],
            json!([RETRIEVAL_ID])
        );
        assert_eq!(value["artifact_memory_handoff"]["packet_count"], 1);
        assert_eq!(value["artifact_memory_handoff"]["retrieval_count"], 1);
        assert_eq!(value["child_batch_id"], "child-batch-test");
        assert_eq!(value["child_batch_size"], 2);
        assert_eq!(value["child_batch_index"], 1);
        assert_eq!(value["report_id"], "af-report-test");

        let handoff_metadata = &value["artifact_memory_handoff"];
        assert!(handoff_metadata.get("packets").is_none());
        assert!(handoff_metadata.get("retrievals").is_none());
        assert!(handoff_metadata.get("items").is_none());
        assert!(!metadata.contains(BOUNDED_SNIPPET));
        assert!(!metadata.contains("multi-agent packet handoff query"));
        assert!(!metadata.contains(RAW_PARENT_TRANSCRIPT_SENTINEL));
        assert!(!metadata.contains(FULL_OUTPUT_SENTINEL));
    }

    #[test]
    fn evaluator_child_metadata_suppresses_parent_completion_relay() {
        let metadata = orchestrated_child_turn_metadata(
            "api",
            "conversation-evaluator",
            None,
            Some("child-batch-test"),
            1,
            1,
            Some("af-report-test"),
        )
        .expect("child turn metadata");
        let value: Value = serde_json::from_str(&metadata).expect("metadata json");

        assert_eq!(value["agent"], "conversation-evaluator");
        assert_eq!(value["suppress_parent_completion_relay"], true);
    }

    #[test]
    fn child_context_tool_args_include_child_id_user_and_handoff() {
        let handoff = resolved_handoff();
        let args = child_support_context_tool_args("child-1", "alex", Some(&handoff))
            .expect("context args");
        let value: Value = serde_json::from_str(&args).expect("context args json");

        assert_eq!(value["child_conversation_id"], "child-1");
        assert_eq!(value["user_id"], "alex");
        assert_eq!(
            value["artifact_memory_handoff"]["packets"][0]["packet_id"],
            PACKET_ID
        );
        assert_eq!(
            value["artifact_memory_handoff"]["retrievals"][0]["retrieval_id"],
            RETRIEVAL_ID
        );
    }

    #[test]
    fn all_hierarchy_list_requests_are_clamped_to_root_summaries() {
        let (applied_scope, reasons, guardrail) =
            resolve_conversation_list_scope(ConversationHierarchyScope::All, None);
        let page = conversations::ConversationListPage {
            conversations: Vec::new(),
            applied_limit: conversations::DEFAULT_ROOT_CONVERSATION_PAGE_LIMIT,
            requested_limit: None,
            offset: 0,
            returned_count: 100,
            has_more: true,
            truncated: true,
        };
        let meta = conversation_list_response_meta(
            ConversationHierarchyScope::All,
            applied_scope,
            None,
            &page,
            guardrail,
            reasons,
        );

        assert_eq!(applied_scope, ConversationHierarchyScope::Roots);
        assert!(meta.child_fanout_guardrail);
        assert!(meta.truncated);
        assert_eq!(meta.applied_hierarchy_scope, "roots");
        assert!(meta
            .truncation_reasons
            .iter()
            .any(|reason| reason == "all_hierarchy_clamped_to_root_summaries"));
        assert!(meta
            .truncation_reasons
            .iter()
            .any(|reason| reason == "page_limit_reached"));
    }

    #[test]
    fn event_page_limits_are_capped_to_guardrail_policy() {
        assert_eq!(applied_event_page_limit(Some(10_000)), MAX_EVENT_PAGE_LIMIT);
        assert_eq!(
            applied_event_page_payload_bytes(Some(1)),
            MAX_SINGLE_EVENT_PAYLOAD_BYTES
        );
        assert_eq!(
            applied_event_page_payload_bytes(Some(usize::MAX)),
            MAX_EVENT_PAGE_PAYLOAD_BYTES
        );
    }
}
