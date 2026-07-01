use anyhow::{Context, Result};
use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::Arc;
use ticketing_system::retrieval::{gather_context, GatherContextRequest, RetrievalRequest};
use ticketing_system::{
    checkpoints, conversation_turn_jobs, conversations, Conversation, ConversationHierarchyScope,
    CreateChildConversationRequest,
};

use crate::agents::AgentType;
use crate::auth_middleware::AuthenticatedUser;
use crate::handlers::chat_stream::{self, ChatCodexOptions, ChatRuntime};
use crate::handlers::conversation_handoff::{
    resolve_context_handoff, ContextHandoffRequest, ResolvedContextHandoff,
};

#[derive(Debug, Deserialize)]
pub struct LaunchConversationChildAgentRequest {
    pub kind: String,
    #[serde(default)]
    pub target_message_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct LaunchConversationChildAgentResponse {
    pub parent: Conversation,
    pub child: Conversation,
    pub did_enqueue_initial_turn: bool,
}

#[derive(Clone, Copy, Debug)]
enum ChildAgentKind {
    Evaluator,
    Feedback,
}

impl ChildAgentKind {
    fn parse(value: &str) -> Result<Self, (StatusCode, String)> {
        match value.trim() {
            "evaluator" | "evaluation" | "conversation-evaluator" => Ok(Self::Evaluator),
            "feedback" => Ok(Self::Feedback),
            other => Err((
                StatusCode::BAD_REQUEST,
                format!("Unsupported child agent kind: {other}"),
            )),
        }
    }

    fn agent_key(self) -> &'static str {
        match self {
            Self::Evaluator => "conversation-evaluator",
            Self::Feedback => "feedback",
        }
    }

    fn agent_type(self) -> AgentType {
        match self {
            Self::Evaluator => AgentType::ConversationEvaluator,
            Self::Feedback => AgentType::Feedback,
        }
    }

    fn conversation_type(self) -> &'static str {
        match self {
            Self::Evaluator => "evaluation",
            Self::Feedback => "feedback",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Evaluator => "Evaluator",
            Self::Feedback => "Feedback",
        }
    }

    fn child_sort_order(self) -> i32 {
        match self {
            Self::Evaluator => 0,
            Self::Feedback => 1,
        }
    }

    fn prompt_name(self) -> &'static str {
        match self {
            Self::Evaluator => "conversation-evaluator-system",
            Self::Feedback => "feedback",
        }
    }

    fn visible_initial_message(self) -> &'static str {
        match self {
            Self::Evaluator => "Run the evaluator for the parent conversation.",
            Self::Feedback => "Start a feedback review for the parent conversation.",
        }
    }
}

/// Create or reuse an evaluator/feedback child conversation and queue its first
/// seeded turn when the child is still empty.
pub async fn launch_conversation_child_agent(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
    Json(req): Json<LaunchConversationChildAgentRequest>,
) -> Result<Json<LaunchConversationChildAgentResponse>, (StatusCode, String)> {
    let kind = ChildAgentKind::parse(&req.kind)?;
    let parent = resolve_launch_parent(&pool, &user.user_id, &id).await?;

    if conversation_has_active_turn(&pool, &parent.id)
        .await
        .map_err(internal_error)?
    {
        return Err((
            StatusCode::CONFLICT,
            "Parent conversation is still processing; wait for the active turn to finish first."
                .to_string(),
        ));
    }

    let mut child = match find_existing_child(&pool, &user.user_id, &parent, kind)
        .await
        .map_err(internal_error)?
    {
        Some(child) => child,
        None => create_child(&pool, &user.user_id, &parent, kind)
            .await
            .map_err(internal_error)?,
    };

    let did_enqueue_initial_turn = maybe_enqueue_initial_turn(
        &pool,
        &user.user_id,
        &parent,
        &mut child,
        kind,
        req.target_message_id.as_deref(),
    )
    .await
    .map_err(internal_error)?;

    let parent = conversations::get_conversation(&pool, &parent.id, false)
        .await
        .map_err(internal_error)?
        .ok_or((StatusCode::NOT_FOUND, "Conversation not found".to_string()))?;

    Ok(Json(LaunchConversationChildAgentResponse {
        parent,
        child,
        did_enqueue_initial_turn,
    }))
}

async fn resolve_launch_parent(
    pool: &SqlitePool,
    user_id: &str,
    id: &str,
) -> Result<Conversation, (StatusCode, String)> {
    let conversation = conversations::get_conversation(pool, id, false)
        .await
        .map_err(internal_error)?
        .ok_or((StatusCode::NOT_FOUND, "Conversation not found".to_string()))?;
    if conversation.user_id != user_id {
        return Err((StatusCode::NOT_FOUND, "Conversation not found".to_string()));
    }

    let Some(parent_id) = conversation.parent_conversation_id.as_deref() else {
        return Ok(conversation);
    };

    let parent = conversations::get_conversation(pool, parent_id, false)
        .await
        .map_err(internal_error)?
        .ok_or((
            StatusCode::NOT_FOUND,
            "Parent conversation not found".to_string(),
        ))?;
    if parent.user_id != user_id {
        return Err((
            StatusCode::NOT_FOUND,
            "Parent conversation not found".to_string(),
        ));
    }
    Ok(parent)
}

async fn find_existing_child(
    pool: &SqlitePool,
    user_id: &str,
    parent: &Conversation,
    kind: ChildAgentKind,
) -> Result<Option<Conversation>> {
    let children = conversations::list_conversations_with_hierarchy(
        pool,
        Some(parent.organization.as_str()),
        Some(user_id),
        Some(kind.agent_key()),
        Some("open,waiting"),
        None,
        None,
        ConversationHierarchyScope::Children,
        Some(parent.id.as_str()),
    )
    .await?;

    Ok(children.into_iter().find(|child| {
        child.parent_conversation_id.as_deref() == Some(parent.id.as_str())
            && child.agent.as_deref() == Some(kind.agent_key())
            && child.conversation_type.as_deref() == Some(kind.conversation_type())
    }))
}

async fn create_child(
    pool: &SqlitePool,
    user_id: &str,
    parent: &Conversation,
    kind: ChildAgentKind,
) -> Result<Conversation> {
    let children = conversations::create_child_conversations(
        pool,
        user_id,
        &parent.id,
        vec![CreateChildConversationRequest {
            title: kind.title().to_string(),
            agent: Some(kind.agent_key().to_string()),
            conversation_type: Some(kind.conversation_type().to_string()),
            child_sort_order: Some(kind.child_sort_order()),
        }],
    )
    .await?;

    children
        .into_iter()
        .next()
        .context("child conversation was not created")
}

async fn maybe_enqueue_initial_turn(
    pool: &SqlitePool,
    user_id: &str,
    parent: &Conversation,
    child: &mut Conversation,
    kind: ChildAgentKind,
    target_message_id: Option<&str>,
) -> Result<bool> {
    if count_conversation_messages(pool, &child.id).await? > 0 {
        return Ok(false);
    }
    if conversation_has_active_turn(pool, &child.id).await? {
        child.is_active = Some(true);
        return Ok(false);
    }

    ensure_parent_has_transcript(pool, &parent.id, kind).await?;

    let handoff = gather_parent_context_handoff(pool, parent, kind).await?;

    enqueue_child_turn(
        pool,
        user_id,
        &child.id,
        kind,
        target_message_id,
        handoff.as_ref(),
    )
    .await?;
    child.is_active = Some(true);
    Ok(true)
}

async fn enqueue_child_turn(
    pool: &SqlitePool,
    user_id: &str,
    child_id: &str,
    kind: ChildAgentKind,
    target_message_id: Option<&str>,
    handoff: Option<&ResolvedContextHandoff>,
) -> Result<()> {
    checkpoints::upsert_checkpoint(pool, child_id, "queued", 0)
        .await
        .context("queue child-agent checkpoint")?;

    let agent_type = kind.agent_type();
    let codex_options = ChatCodexOptions::default_for_agent(&agent_type);
    let mut prompt_vars = HashMap::new();
    prompt_vars.insert(
        "SUPPORT_CONTEXT_TOOL_ARGS".to_string(),
        support_context_tool_args(child_id, user_id, target_message_id, handoff)?,
    );
    if let Some(handoff) = handoff {
        prompt_vars.insert(
            "ARTIFACT_MEMORY_HANDOFF".to_string(),
            handoff.prompt_json()?,
        );
    }
    let payload = conversation_turn_jobs::ConversationTurnJobPayload {
        user_id: user_id.to_string(),
        message: kind.visible_initial_message().to_string(),
        agent_type: agent_type.as_str().to_string(),
        runtime: ChatRuntime::CodexAppServer.as_job_runtime().to_string(),
        prompt_name: kind.prompt_name().to_string(),
        working_dir: "/Users/jarvisgpt/projects".to_string(),
        prompt_vars: chat_stream::encode_codex_options_for_job(prompt_vars, &codex_options),
        images_json: None,
        client_id: None,
        message_metadata: Some(orchestrated_message_metadata(
            "api",
            kind.agent_type().as_str(),
            handoff,
        )?),
    };

    conversation_turn_jobs::enqueue_job(pool, child_id, payload)
        .await
        .context("enqueue child-agent turn")?;
    super::conversations::publish_conversation_run_status(pool, child_id)
        .await
        .context("publish child-agent run status")?;
    Ok(())
}

fn orchestrated_message_metadata(
    orchestrated_by: &str,
    agent: &str,
    handoff: Option<&ResolvedContextHandoff>,
) -> Result<String> {
    let mut value = serde_json::json!({
        "origin": "agent_orchestrated",
        "orchestrated_by": orchestrated_by,
        "orchestration": "child_initial_turn",
        "agent": agent,
    });
    if agent == "conversation-evaluator" {
        value["suppress_parent_completion_relay"] = serde_json::Value::Bool(true);
    }
    if let Some(handoff) = handoff {
        value["artifact_memory_handoff"] = handoff.metadata_json();
    }
    serde_json::to_string(&value).context("serialize child-agent kickoff metadata")
}

async fn ensure_parent_has_transcript(
    pool: &SqlitePool,
    parent_id: &str,
    kind: ChildAgentKind,
) -> Result<()> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM conversation_messages \
         WHERE conversation_id = ? \
           AND role IN ('user', 'assistant') \
           AND trim(content) != ''",
    )
    .bind(parent_id)
    .fetch_one(pool)
    .await
    .context("count parent transcript messages")?;

    if count == 0 {
        let label = match kind {
            ChildAgentKind::Evaluator => "evaluate",
            ChildAgentKind::Feedback => "use for feedback",
        };
        anyhow::bail!("Conversation has no completed user/assistant messages to {label}.");
    }

    Ok(())
}

async fn gather_parent_context_handoff(
    pool: &SqlitePool,
    parent: &Conversation,
    kind: ChildAgentKind,
) -> Result<Option<ResolvedContextHandoff>> {
    let Some(ticket_id) = parent.router_ticket_id.as_deref() else {
        return Ok(None);
    };
    let organization = parent
        .router_organization
        .as_deref()
        .unwrap_or(parent.organization.as_str());
    let actor_id = format!("api-child-agent-handoff:{}", parent.id);
    let query_text = format!(
        "{} child-agent context for parent conversation `{}` linked to ticket `{}`: {}",
        kind.conversation_type(),
        parent.id,
        ticket_id,
        parent.title
    );
    let packet = gather_context(
        pool,
        GatherContextRequest {
            retrieval: RetrievalRequest {
                organization: organization.to_string(),
                query_text,
                actor_type: "agent".to_string(),
                actor_id: actor_id.clone(),
                tool_name: "gather_context".to_string(),
                work_summary: Some(format!(
                    "{} child-agent handoff for parent conversation {}",
                    kind.conversation_type(),
                    parent.id
                )),
                ticket_id: Some(ticket_id.to_string()),
                repository: None,
                max_results: Some(8),
                max_selected: Some(4),
                token_budget: Some(2_000),
            },
            created_by: actor_id,
            created_by_agent: Some("conversation-child-agent".to_string()),
            max_items: Some(4),
            token_budget: Some(2_000),
        },
    )
    .await
    .with_context(|| {
        format!(
            "assemble artifact-memory packet for {} child of parent {}",
            kind.conversation_type(),
            parent.id
        )
    })?;

    resolve_context_handoff(
        pool,
        organization,
        &ContextHandoffRequest {
            context_packet_ids: vec![packet.packet_id],
            retrieval_ids: vec![packet.retrieval_id],
        },
    )
    .await
    .context("resolve child-agent artifact-memory packet handoff")
}

fn support_context_tool_args(
    child_id: &str,
    user_id: &str,
    target_message_id: Option<&str>,
    handoff: Option<&ResolvedContextHandoff>,
) -> Result<String> {
    let mut args = serde_json::json!({
        "child_conversation_id": child_id,
        "user_id": user_id,
    });
    if let Some(target_message_id) = target_message_id {
        args["target_message_id"] = serde_json::Value::String(target_message_id.to_string());
    }
    if let Some(handoff) = handoff {
        args["artifact_memory_handoff"] = serde_json::to_value(handoff)
            .context("serialize child-agent artifact-memory handoff")?;
    }
    Ok(args.to_string())
}

async fn count_conversation_messages(pool: &SqlitePool, conversation_id: &str) -> Result<i64> {
    sqlx::query_scalar("SELECT COUNT(*) FROM conversation_messages WHERE conversation_id = ?")
        .bind(conversation_id)
        .fetch_one(pool)
        .await
        .context("count conversation messages")
}

async fn conversation_has_active_turn(pool: &SqlitePool, conversation_id: &str) -> Result<bool> {
    let active_jobs =
        conversation_turn_jobs::has_active_job_for_conversation(pool, conversation_id)
            .await
            .context("check active child-agent jobs")?;
    if active_jobs {
        return Ok(true);
    }

    let active_runner_turns: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_runner_turns \
         WHERE conversation_id = ? AND status IN ('queued', 'running')",
    )
    .bind(conversation_id)
    .fetch_one(pool)
    .await
    .context("check active child-agent runner turns")?;

    Ok(active_runner_turns > 0)
}

fn internal_error(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::context_packets::{
        ContextPacketItemSummary, ContextPacketSummary, RetrievalEventSummary,
    };
    use crate::handlers::conversation_handoff::{ResolvedContextHandoff, ResolvedContextPacket};
    use serde_json::{json, Value};

    const PACKET_ID: &str = "CP-RUNNER1234";
    const RETRIEVAL_ID: &str = "R-RUNNER1234";
    const BOUNDED_SNIPPET: &str = "Bounded child-agent handoff snippet.";
    const RAW_PARENT_TRANSCRIPT_SENTINEL: &str = "RAW_PARENT_TRANSCRIPT_SENTINEL";
    const FULL_OUTPUT_SENTINEL: &str = "FULL_OUTPUT_SENTINEL";

    fn resolved_handoff() -> ResolvedContextHandoff {
        ResolvedContextHandoff {
            packets: vec![ResolvedContextPacket {
                summary: ContextPacketSummary {
                    packet_id: PACKET_ID.to_string(),
                    retrieval_id: Some(RETRIEVAL_ID.to_string()),
                    ticket_id: Some("T-RUNNER12".to_string()),
                    repository: Some("agentic-flowstate-api".to_string()),
                    work_summary: "runner handoff smoke".to_string(),
                    created_by: "api-child-agent-handoff:parent-1".to_string(),
                    created_by_agent: Some("conversation-child-agent".to_string()),
                    summary: "Context packet for runner handoff smoke.".to_string(),
                    warnings: vec!["packet_truncated".to_string()],
                    token_budget: Some(2_000),
                    token_count: Some(48),
                    created_at: 1_781_662_200,
                    metadata: json!({"source": "api-context-gather"}),
                },
                items: vec![ContextPacketItemSummary {
                    rank: 1,
                    item_type: "chunk".to_string(),
                    artifact_id: Some("A-RUNNER1234".to_string()),
                    chunk_id: Some("C-RUNNER1234-1".to_string()),
                    knowledge_id: None,
                    ticket_id: Some("T-RUNNER12".to_string()),
                    document_id: None,
                    entity_id: None,
                    citation_label: Some("A-RUNNER1234#C-RUNNER1234-1".to_string()),
                    relevance_reason: "selected retrieval chunk".to_string(),
                    included_text: Some(BOUNDED_SNIPPET.to_string()),
                    token_count: Some(9),
                    source_retrieval_rank: Some(1),
                    metadata: json!({"matched_fields": ["content"]}),
                }],
            }],
            retrievals: vec![RetrievalEventSummary {
                retrieval_id: RETRIEVAL_ID.to_string(),
                organization: "agentic-flowstate".to_string(),
                actor_type: "agent".to_string(),
                actor_id: "api-child-agent-handoff:parent-1".to_string(),
                tool_name: "gather_context".to_string(),
                work_summary: Some("runner handoff smoke".to_string()),
                query_text: "child packet handoff query".to_string(),
                normalized_query: Some("child packet handoff query".to_string()),
                filters: json!({"ticket_id": "T-RUNNER12"}),
                authorization_filter: json!({
                    "organization": "agentic-flowstate",
                    "visibility": ["organization", "system"]
                }),
                strategy: "fts_facets_links_v1".to_string(),
                started_at: 1_781_662_200,
                elapsed_ms: 19,
                result_count: 3,
                selected_count: 1,
                empty_result: false,
                context_token_count: Some(48),
                context_truncated: true,
                warnings: vec!["packet_truncated".to_string()],
                metadata: json!({"query_terms": ["child", "handoff"]}),
            }],
        }
    }

    #[test]
    fn child_agent_runner_args_carry_packet_snippets_and_trace_metadata_without_parent_transcript()
    {
        let handoff = resolved_handoff();
        let args = support_context_tool_args("child-1", "alex", Some("message-1"), Some(&handoff))
            .expect("support context args");
        let value: Value = serde_json::from_str(&args).expect("support args json");

        assert_eq!(value["child_conversation_id"], "child-1");
        assert_eq!(value["user_id"], "alex");
        assert_eq!(value["target_message_id"], "message-1");
        assert_eq!(
            value["artifact_memory_handoff"]["packets"][0]["packet_id"],
            PACKET_ID
        );
        assert_eq!(
            value["artifact_memory_handoff"]["packets"][0]["items"][0]["included_text"],
            BOUNDED_SNIPPET
        );
        assert_eq!(
            value["artifact_memory_handoff"]["retrievals"][0]["retrieval_id"],
            RETRIEVAL_ID
        );
        assert_eq!(
            value["artifact_memory_handoff"]["retrievals"][0]["filters"]["ticket_id"],
            "T-RUNNER12"
        );
        assert_eq!(
            value["artifact_memory_handoff"]["retrievals"][0]["warnings"][0],
            "packet_truncated"
        );

        let serialized = value.to_string();
        assert!(!serialized.contains(RAW_PARENT_TRANSCRIPT_SENTINEL));
        assert!(!serialized.contains(FULL_OUTPUT_SENTINEL));
        assert!(value.get("parent_transcript").is_none());
        assert!(value.get("transcript").is_none());
    }

    #[test]
    fn child_agent_runner_metadata_records_only_compact_handoff_handles() {
        let handoff = resolved_handoff();
        let metadata =
            orchestrated_message_metadata("api", "conversation-evaluator", Some(&handoff))
                .expect("runner metadata");
        let value: Value = serde_json::from_str(&metadata).expect("metadata json");

        assert_eq!(value["origin"], "agent_orchestrated");
        assert_eq!(value["orchestration"], "child_initial_turn");
        assert_eq!(value["suppress_parent_completion_relay"], true);
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

        let handoff_metadata = &value["artifact_memory_handoff"];
        assert!(handoff_metadata.get("packets").is_none());
        assert!(handoff_metadata.get("retrievals").is_none());
        assert!(handoff_metadata.get("items").is_none());
        assert!(!metadata.contains(BOUNDED_SNIPPET));
        assert!(!metadata.contains("child packet handoff query"));
        assert!(!metadata.contains(RAW_PARENT_TRANSCRIPT_SENTINEL));
        assert!(!metadata.contains(FULL_OUTPUT_SENTINEL));
    }
}
