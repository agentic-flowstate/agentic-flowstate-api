use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Extension, Json,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{sqlite::SqliteRow, Row, SqlitePool};
use std::sync::Arc;
use ticketing_system::retrieval::{
    gather_context, search_artifact_memory, GatherContextRequest, RetrievalRequest,
};
use tracing::error;

use crate::auth_middleware::AuthenticatedUser;

use super::get_organization;

const DEFAULT_PACKET_LIMIT: usize = 25;
const MAX_PACKET_LIMIT: usize = 100;
const ALLOWED_FEEDBACK_LABELS: &[&str] = &[
    "relevant",
    "partially_relevant",
    "irrelevant",
    "missing",
    "missing_expected",
    "stale",
    "contradictory",
    "unauthorized",
    "unauthorized_should_not_appear",
    "useful_context",
    "too_verbose",
    "insufficient_context",
    "duplicative",
];
const ALLOWED_RESULT_TYPES: &[&str] = &[
    "artifact",
    "chunk",
    "knowledge",
    "ticket",
    "document",
    "entity",
    "namespace",
];

#[derive(Debug, Deserialize)]
pub struct ContextSearchRequest {
    pub query_text: String,
    pub work_summary: Option<String>,
    pub ticket_id: Option<String>,
    pub repository: Option<String>,
    pub max_results: Option<usize>,
    pub max_selected: Option<usize>,
    pub token_budget: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct ContextGatherRequest {
    pub query_text: String,
    pub work_summary: Option<String>,
    pub ticket_id: Option<String>,
    pub repository: Option<String>,
    pub max_results: Option<usize>,
    pub max_selected: Option<usize>,
    pub max_items: Option<usize>,
    pub token_budget: Option<usize>,
    pub created_by_agent: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ListContextPacketsQuery {
    pub ticket_id: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct ContextPacketDetailQuery {
    pub include_items: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct ContextPacketSummary {
    pub packet_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retrieval_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticket_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    pub work_summary: String,
    pub created_by: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by_agent: Option<String>,
    pub summary: String,
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_count: Option<i64>,
    pub created_at: i64,
    pub metadata: Value,
}

#[derive(Debug, Serialize)]
pub struct ContextPacketDetail {
    #[serde(flatten)]
    pub packet: ContextPacketSummary,
    pub items: Vec<ContextPacketItemSummary>,
}

#[derive(Debug, Serialize, Clone)]
pub struct ContextPacketItemSummary {
    pub rank: i64,
    pub item_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub knowledge_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticket_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub citation_label: Option<String>,
    pub relevance_reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub included_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_retrieval_rank: Option<i64>,
    pub metadata: Value,
}

#[derive(Debug, Serialize)]
pub struct RetrievalExplanation {
    pub event: RetrievalEventSummary,
    pub results: Vec<RetrievalResultSummary>,
    pub packets: Vec<ContextPacketSummary>,
}

#[derive(Debug, Serialize)]
pub struct RetrievalEventSummary {
    pub retrieval_id: String,
    pub organization: String,
    pub actor_type: String,
    pub actor_id: String,
    pub tool_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_summary: Option<String>,
    pub query_text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub normalized_query: Option<String>,
    pub filters: Value,
    pub authorization_filter: Value,
    pub strategy: String,
    pub started_at: i64,
    pub elapsed_ms: i64,
    pub result_count: i64,
    pub selected_count: i64,
    pub empty_result: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_token_count: Option<i64>,
    pub context_truncated: bool,
    pub warnings: Vec<String>,
    pub metadata: Value,
}

#[derive(Debug, Serialize, Clone)]
pub struct RetrievalResultSummary {
    pub rank: i64,
    pub result_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub knowledge_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticket_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    pub score_components: Value,
    pub matched_fields: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    pub selected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection_reason: Option<String>,
    pub authorization_decision: String,
}

#[derive(Debug, Deserialize)]
pub struct RecordRetrievalFeedbackRequest {
    pub retrieval_id: Option<String>,
    pub packet_id: Option<String>,
    pub result_rank: Option<i64>,
    pub packet_item_rank: Option<i64>,
    pub result_type: Option<String>,
    pub artifact_id: Option<String>,
    pub chunk_id: Option<String>,
    pub knowledge_id: Option<String>,
    pub label: String,
    pub notes: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RecordRetrievalFeedbackResponse {
    pub feedback_id: String,
}

#[derive(Debug, Clone)]
struct FeedbackTarget {
    result_type: String,
    artifact_id: Option<String>,
    chunk_id: Option<String>,
    knowledge_id: Option<String>,
}

/// POST /api/context/search
pub async fn search_context(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Json(req): Json<ContextSearchRequest>,
) -> Response {
    let org = get_organization(&headers);
    if let Err(response) = ensure_ticket_scope(&pool, &org, req.ticket_id.as_deref()).await {
        return response;
    }

    let retrieval_req = retrieval_request_from_search(&org, &user.user_id, req);
    match search_artifact_memory(&pool, retrieval_req).await {
        Ok(response) => (StatusCode::OK, Json(json!(response))).into_response(),
        Err(e) => {
            error!("Failed artifact-memory search for org {}: {:?}", org, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to search artifact memory" })),
            )
                .into_response()
        }
    }
}

/// POST /api/context/gather
pub async fn gather_context_packet(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Json(req): Json<ContextGatherRequest>,
) -> Response {
    let org = get_organization(&headers);
    if let Err(response) = ensure_ticket_scope(&pool, &org, req.ticket_id.as_deref()).await {
        return response;
    }

    let retrieval = RetrievalRequest {
        organization: org.clone(),
        query_text: req.query_text,
        actor_type: "user".to_string(),
        actor_id: user.user_id.clone(),
        tool_name: "gather_context".to_string(),
        work_summary: req.work_summary,
        ticket_id: req.ticket_id.and_then(non_empty_string),
        repository: req.repository.and_then(non_empty_string),
        max_results: req.max_results,
        max_selected: req.max_selected,
        token_budget: req.token_budget,
    };
    let gather_req = GatherContextRequest {
        retrieval,
        created_by: user.user_id,
        created_by_agent: req.created_by_agent.and_then(non_empty_string),
        max_items: req.max_items,
        token_budget: req.token_budget,
    };

    match gather_context(&pool, gather_req).await {
        Ok(response) => (StatusCode::CREATED, Json(json!(response))).into_response(),
        Err(e) => {
            error!("Failed to gather context packet for org {}: {:?}", org, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to gather context packet" })),
            )
                .into_response()
        }
    }
}

/// GET /api/context-packets?ticket_id=<id>
pub async fn list_context_packets(
    State(pool): State<Arc<SqlitePool>>,
    headers: HeaderMap,
    Query(query): Query<ListContextPacketsQuery>,
) -> Response {
    let org = get_organization(&headers);
    if let Err(response) = ensure_ticket_scope(&pool, &org, query.ticket_id.as_deref()).await {
        return response;
    }

    let limit = bounded_limit(
        query.limit.unwrap_or(DEFAULT_PACKET_LIMIT),
        1,
        MAX_PACKET_LIMIT,
    );
    match list_packet_summaries(&pool, &org, query.ticket_id.as_deref(), limit).await {
        Ok(packets) => (StatusCode::OK, Json(json!(packets))).into_response(),
        Err(e) => {
            error!("Failed to list context packets for org {}: {:?}", org, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to list context packets" })),
            )
                .into_response()
        }
    }
}

/// GET /api/context-packets/:packet_id
pub async fn get_context_packet(
    State(pool): State<Arc<SqlitePool>>,
    headers: HeaderMap,
    Path(packet_id): Path<String>,
    Query(query): Query<ContextPacketDetailQuery>,
) -> Response {
    let org = get_organization(&headers);
    let packet = match get_packet_summary(&pool, &org, &packet_id).await {
        Ok(Some(packet)) => packet,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Context packet not found" })),
            )
                .into_response();
        }
        Err(e) => {
            error!("Failed to get context packet {}: {:?}", packet_id, e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to get context packet" })),
            )
                .into_response();
        }
    };

    let items = if query.include_items.unwrap_or(true) {
        match list_visible_packet_items(&pool, &org, &packet_id).await {
            Ok(items) => items,
            Err(e) => {
                error!("Failed to list context packet items {}: {:?}", packet_id, e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "Failed to list context packet items" })),
                )
                    .into_response();
            }
        }
    } else {
        Vec::new()
    };

    (
        StatusCode::OK,
        Json(json!(ContextPacketDetail { packet, items })),
    )
        .into_response()
}

/// GET /api/retrievals/:retrieval_id/explain
pub async fn explain_retrieval(
    State(pool): State<Arc<SqlitePool>>,
    headers: HeaderMap,
    Path(retrieval_id): Path<String>,
) -> Response {
    let org = get_organization(&headers);
    let event = match get_retrieval_event(&pool, &org, &retrieval_id).await {
        Ok(Some(event)) => event,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Retrieval not found" })),
            )
                .into_response();
        }
        Err(e) => {
            error!("Failed to get retrieval {}: {:?}", retrieval_id, e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to get retrieval" })),
            )
                .into_response();
        }
    };

    let results = match list_visible_retrieval_results(&pool, &org, &retrieval_id).await {
        Ok(results) => results,
        Err(e) => {
            error!("Failed to list retrieval results {}: {:?}", retrieval_id, e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to list retrieval results" })),
            )
                .into_response();
        }
    };

    let packets = match list_packets_for_retrieval(&pool, &org, &retrieval_id).await {
        Ok(packets) => packets,
        Err(e) => {
            error!("Failed to list retrieval packets {}: {:?}", retrieval_id, e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to list retrieval packets" })),
            )
                .into_response();
        }
    };

    (
        StatusCode::OK,
        Json(json!(RetrievalExplanation {
            event,
            results,
            packets,
        })),
    )
        .into_response()
}

/// POST /api/retrieval-feedback
pub async fn record_retrieval_feedback(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Json(req): Json<RecordRetrievalFeedbackRequest>,
) -> Response {
    let org = get_organization(&headers);
    if !ALLOWED_FEEDBACK_LABELS.contains(&req.label.as_str()) {
        return json_error(StatusCode::BAD_REQUEST, "Unsupported feedback label");
    }
    if req.retrieval_id.is_none() && req.packet_id.is_none() {
        return json_error(
            StatusCode::BAD_REQUEST,
            "retrieval_id or packet_id is required",
        );
    }

    if let Some(retrieval_id) = req.retrieval_id.as_deref() {
        match retrieval_exists_in_org(&pool, &org, retrieval_id).await {
            Ok(true) => {}
            Ok(false) => return json_error(StatusCode::NOT_FOUND, "Retrieval not found"),
            Err(e) => {
                error!("Failed to validate retrieval {}: {:?}", retrieval_id, e);
                return json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to validate retrieval",
                );
            }
        }
    }
    if let Some(packet_id) = req.packet_id.as_deref() {
        match packet_exists_in_org(&pool, &org, packet_id).await {
            Ok(true) => {}
            Ok(false) => return json_error(StatusCode::NOT_FOUND, "Context packet not found"),
            Err(e) => {
                error!("Failed to validate packet {}: {:?}", packet_id, e);
                return json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to validate context packet",
                );
            }
        }
    }

    let target = match derive_feedback_target(&pool, &org, &req).await {
        Ok(target) => target,
        Err(response) => return response,
    };
    if !ALLOWED_RESULT_TYPES.contains(&target.result_type.as_str()) {
        return json_error(StatusCode::BAD_REQUEST, "Unsupported feedback result_type");
    }

    let feedback_id = generated_feedback_id();
    let now = Utc::now().timestamp();
    let result = sqlx::query(
        r#"
        INSERT INTO retrieval_feedback (
            feedback_id, retrieval_id, packet_id, result_rank, packet_item_rank,
            result_type, artifact_id, chunk_id, knowledge_id, label, created_by,
            notes, created_at
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(&feedback_id)
    .bind(&req.retrieval_id)
    .bind(&req.packet_id)
    .bind(req.result_rank)
    .bind(req.packet_item_rank)
    .bind(&target.result_type)
    .bind(&target.artifact_id)
    .bind(&target.chunk_id)
    .bind(&target.knowledge_id)
    .bind(&req.label)
    .bind(&user.user_id)
    .bind(&req.notes)
    .bind(now)
    .execute(&*pool)
    .await;

    if let Err(e) = result {
        error!("Failed to record retrieval feedback: {:?}", e);
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to record retrieval feedback",
        );
    }

    let subject_type = if req.packet_id.is_some() {
        "context_packet"
    } else {
        "retrieval"
    };
    let subject_id = req
        .packet_id
        .as_deref()
        .or(req.retrieval_id.as_deref())
        .unwrap_or(&feedback_id);
    if let Err(e) = ticketing_system::artifact_memory::record_memory_event(
        &pool,
        &org,
        "feedback_applied",
        subject_type,
        subject_id,
        "user",
        &user.user_id,
        json!({
            "feedback_id": feedback_id,
            "label": req.label,
            "result_type": target.result_type,
            "artifact_id": target.artifact_id,
            "chunk_id": target.chunk_id,
            "knowledge_id": target.knowledge_id,
            "result_rank": req.result_rank,
            "packet_item_rank": req.packet_item_rank,
        }),
    )
    .await
    {
        error!("Failed to record feedback memory event: {:?}", e);
    }

    (
        StatusCode::CREATED,
        Json(json!(RecordRetrievalFeedbackResponse { feedback_id })),
    )
        .into_response()
}

fn retrieval_request_from_search(
    org: &str,
    user_id: &str,
    req: ContextSearchRequest,
) -> RetrievalRequest {
    RetrievalRequest {
        organization: org.to_string(),
        query_text: req.query_text,
        actor_type: "user".to_string(),
        actor_id: user_id.to_string(),
        tool_name: "api_context_search".to_string(),
        work_summary: req.work_summary,
        ticket_id: req.ticket_id.and_then(non_empty_string),
        repository: req.repository.and_then(non_empty_string),
        max_results: req.max_results,
        max_selected: req.max_selected,
        token_budget: req.token_budget,
    }
}

async fn ensure_ticket_scope(
    pool: &SqlitePool,
    org: &str,
    ticket_id: Option<&str>,
) -> Result<(), Response> {
    let Some(ticket_id) = ticket_id.and_then(non_empty_str) else {
        return Ok(());
    };
    match sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM tickets WHERE ticket_id = ? AND organization = ? LIMIT 1",
    )
    .bind(ticket_id)
    .bind(org)
    .fetch_optional(pool)
    .await
    {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(json_error(StatusCode::NOT_FOUND, "Ticket not found")),
        Err(e) => {
            error!("Failed to validate ticket scope {}: {:?}", ticket_id, e);
            Err(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to validate ticket scope",
            ))
        }
    }
}

async fn list_packet_summaries(
    pool: &SqlitePool,
    org: &str,
    ticket_id: Option<&str>,
    limit: usize,
) -> sqlx::Result<Vec<ContextPacketSummary>> {
    let rows = sqlx::query(
        r#"
        SELECT packet_id, retrieval_id, ticket_id, repository, work_summary,
               created_by, created_by_agent, summary, warnings_json,
               token_budget, token_count, created_at, metadata_json
        FROM context_packets
        WHERE organization = ?
          AND (? IS NULL OR ticket_id = ?)
        ORDER BY created_at DESC
        LIMIT ?
        "#,
    )
    .bind(org)
    .bind(ticket_id)
    .bind(ticket_id)
    .bind(i64::try_from(limit).unwrap_or(i64::MAX))
    .fetch_all(pool)
    .await?;

    Ok(rows.iter().map(row_to_packet_summary).collect())
}

async fn list_packets_for_retrieval(
    pool: &SqlitePool,
    org: &str,
    retrieval_id: &str,
) -> sqlx::Result<Vec<ContextPacketSummary>> {
    let rows = sqlx::query(
        r#"
        SELECT packet_id, retrieval_id, ticket_id, repository, work_summary,
               created_by, created_by_agent, summary, warnings_json,
               token_budget, token_count, created_at, metadata_json
        FROM context_packets
        WHERE organization = ?
          AND retrieval_id = ?
        ORDER BY created_at DESC
        "#,
    )
    .bind(org)
    .bind(retrieval_id)
    .fetch_all(pool)
    .await?;

    Ok(rows.iter().map(row_to_packet_summary).collect())
}

async fn get_packet_summary(
    pool: &SqlitePool,
    org: &str,
    packet_id: &str,
) -> sqlx::Result<Option<ContextPacketSummary>> {
    let row = sqlx::query(
        r#"
        SELECT packet_id, retrieval_id, ticket_id, repository, work_summary,
               created_by, created_by_agent, summary, warnings_json,
               token_budget, token_count, created_at, metadata_json
        FROM context_packets
        WHERE organization = ?
          AND packet_id = ?
        LIMIT 1
        "#,
    )
    .bind(org)
    .bind(packet_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.as_ref().map(row_to_packet_summary))
}

async fn list_visible_packet_items(
    pool: &SqlitePool,
    org: &str,
    packet_id: &str,
) -> sqlx::Result<Vec<ContextPacketItemSummary>> {
    let rows = sqlx::query(
        r#"
        SELECT i.rank, i.item_type, i.artifact_id, i.chunk_id, i.knowledge_id,
               i.ticket_id, i.document_id, i.entity_id, i.citation_label,
               i.relevance_reason, i.included_text, i.token_count,
               i.source_retrieval_rank, i.metadata_json
        FROM context_packet_items i
        LEFT JOIN artifacts a ON a.artifact_id = i.artifact_id
        LEFT JOIN artifact_chunks c ON c.chunk_id = i.chunk_id
        LEFT JOIN artifacts ca ON ca.artifact_id = c.artifact_id
        LEFT JOIN documents d ON d.document_id = i.document_id
        LEFT JOIN tickets t ON t.ticket_id = i.ticket_id
        LEFT JOIN knowledge_items k ON k.knowledge_id = i.knowledge_id
        LEFT JOIN memory_entities e ON e.entity_id = i.entity_id
        WHERE i.packet_id = ?
          AND (
              i.item_type IN ('warning', 'follow_up_query')
              OR (i.chunk_id IS NOT NULL
                  AND ca.organization = ?
                  AND ca.lifecycle_status = 'active'
                  AND ca.visibility IN ('organization', 'system')
                  AND c.lifecycle_status = 'active')
              OR (i.chunk_id IS NULL
                  AND i.artifact_id IS NOT NULL
                  AND a.organization = ?
                  AND a.lifecycle_status = 'active'
                  AND a.visibility IN ('organization', 'system'))
              OR (i.document_id IS NOT NULL AND d.organization = ?)
              OR (i.ticket_id IS NOT NULL AND t.organization = ?)
              OR (i.knowledge_id IS NOT NULL
                  AND k.organization = ?
                  AND k.lifecycle_status NOT IN ('invalid', 'archived')
                  AND k.visibility IN ('organization', 'system'))
              OR (i.entity_id IS NOT NULL AND e.organization = ?)
          )
        ORDER BY i.rank ASC
        "#,
    )
    .bind(packet_id)
    .bind(org)
    .bind(org)
    .bind(org)
    .bind(org)
    .bind(org)
    .bind(org)
    .fetch_all(pool)
    .await?;

    Ok(rows.iter().map(row_to_packet_item).collect())
}

async fn get_retrieval_event(
    pool: &SqlitePool,
    org: &str,
    retrieval_id: &str,
) -> sqlx::Result<Option<RetrievalEventSummary>> {
    let row = sqlx::query(
        r#"
        SELECT retrieval_id, organization, actor_type, actor_id, tool_name,
               work_summary, query_text, normalized_query, filters_json,
               authorization_filter_json, strategy, started_at, elapsed_ms,
               result_count, selected_count, empty_result, context_token_count,
               context_truncated, warnings_json, metadata_json
        FROM retrieval_events
        WHERE organization = ?
          AND retrieval_id = ?
        LIMIT 1
        "#,
    )
    .bind(org)
    .bind(retrieval_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.as_ref().map(row_to_retrieval_event))
}

async fn list_visible_retrieval_results(
    pool: &SqlitePool,
    org: &str,
    retrieval_id: &str,
) -> sqlx::Result<Vec<RetrievalResultSummary>> {
    let rows = sqlx::query(
        r#"
        SELECT r.rank, r.result_type, r.artifact_id, r.chunk_id, r.knowledge_id,
               r.ticket_id, r.document_id, r.entity_id, r.namespace_id, r.score,
               r.score_components_json, r.matched_fields_json, r.snippet,
               r.selected, r.selection_reason, r.authorization_decision
        FROM retrieval_event_results r
        LEFT JOIN artifacts a ON a.artifact_id = r.artifact_id
        LEFT JOIN artifact_chunks c ON c.chunk_id = r.chunk_id
        LEFT JOIN artifacts ca ON ca.artifact_id = c.artifact_id
        LEFT JOIN documents d ON d.document_id = r.document_id
        LEFT JOIN tickets t ON t.ticket_id = r.ticket_id
        LEFT JOIN knowledge_items k ON k.knowledge_id = r.knowledge_id
        LEFT JOIN memory_entities e ON e.entity_id = r.entity_id
        LEFT JOIN artifact_namespaces ns ON ns.namespace_id = r.namespace_id
        WHERE r.retrieval_id = ?
          AND (
              (r.chunk_id IS NOT NULL
               AND ca.organization = ?
               AND ca.lifecycle_status = 'active'
               AND ca.visibility IN ('organization', 'system')
               AND c.lifecycle_status = 'active')
              OR (r.chunk_id IS NULL
                  AND r.artifact_id IS NOT NULL
                  AND a.organization = ?
                  AND a.lifecycle_status = 'active'
                  AND a.visibility IN ('organization', 'system'))
              OR (r.document_id IS NOT NULL AND d.organization = ?)
              OR (r.ticket_id IS NOT NULL AND t.organization = ?)
              OR (r.knowledge_id IS NOT NULL
                  AND k.organization = ?
                  AND k.lifecycle_status NOT IN ('invalid', 'archived')
                  AND k.visibility IN ('organization', 'system'))
              OR (r.entity_id IS NOT NULL AND e.organization = ?)
              OR (r.namespace_id IS NOT NULL
                  AND ns.organization = ?
                  AND ns.visibility IN ('organization', 'system'))
          )
        ORDER BY r.rank ASC
        "#,
    )
    .bind(retrieval_id)
    .bind(org)
    .bind(org)
    .bind(org)
    .bind(org)
    .bind(org)
    .bind(org)
    .bind(org)
    .fetch_all(pool)
    .await?;

    Ok(rows.iter().map(row_to_retrieval_result).collect())
}

async fn retrieval_exists_in_org(
    pool: &SqlitePool,
    org: &str,
    retrieval_id: &str,
) -> sqlx::Result<bool> {
    let exists: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM retrieval_events WHERE retrieval_id = ? AND organization = ? LIMIT 1",
    )
    .bind(retrieval_id)
    .bind(org)
    .fetch_optional(pool)
    .await?;
    Ok(exists.is_some())
}

async fn packet_exists_in_org(pool: &SqlitePool, org: &str, packet_id: &str) -> sqlx::Result<bool> {
    let exists: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM context_packets WHERE packet_id = ? AND organization = ? LIMIT 1",
    )
    .bind(packet_id)
    .bind(org)
    .fetch_optional(pool)
    .await?;
    Ok(exists.is_some())
}

async fn derive_feedback_target(
    pool: &SqlitePool,
    org: &str,
    req: &RecordRetrievalFeedbackRequest,
) -> Result<FeedbackTarget, Response> {
    let mut target = None;
    if let (Some(packet_id), Some(rank)) = (req.packet_id.as_deref(), req.packet_item_rank) {
        let items = list_visible_packet_items(pool, org, packet_id)
            .await
            .map_err(|e| {
                error!(
                    "Failed to load packet target {}:{}: {:?}",
                    packet_id, rank, e
                );
                json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to validate packet item",
                )
            })?;
        target = items
            .into_iter()
            .find(|item| item.rank == rank)
            .map(|item| FeedbackTarget {
                result_type: item.item_type,
                artifact_id: item.artifact_id,
                chunk_id: item.chunk_id,
                knowledge_id: item.knowledge_id,
            });
    }
    if target.is_none() {
        if let (Some(retrieval_id), Some(rank)) = (req.retrieval_id.as_deref(), req.result_rank) {
            let results = list_visible_retrieval_results(pool, org, retrieval_id)
                .await
                .map_err(|e| {
                    error!(
                        "Failed to load retrieval target {}:{}: {:?}",
                        retrieval_id, rank, e
                    );
                    json_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Failed to validate retrieval result",
                    )
                })?;
            target = results
                .into_iter()
                .find(|result| result.rank == rank)
                .map(|result| FeedbackTarget {
                    result_type: result.result_type,
                    artifact_id: result.artifact_id,
                    chunk_id: result.chunk_id,
                    knowledge_id: result.knowledge_id,
                });
        }
    }

    let mut target = target.unwrap_or_else(|| FeedbackTarget {
        result_type: req.result_type.clone().unwrap_or_default(),
        artifact_id: req.artifact_id.clone(),
        chunk_id: req.chunk_id.clone(),
        knowledge_id: req.knowledge_id.clone(),
    });

    if let Some(result_type) = req.result_type.as_ref().and_then(|s| non_empty_str(s)) {
        target.result_type = result_type.to_string();
    }
    if target.result_type.is_empty() {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "result_type is required",
        ));
    }
    if let Some(artifact_id) = req.artifact_id.as_ref().and_then(|s| non_empty_str(s)) {
        target.artifact_id = Some(artifact_id.to_string());
    }
    if let Some(chunk_id) = req.chunk_id.as_ref().and_then(|s| non_empty_str(s)) {
        target.chunk_id = Some(chunk_id.to_string());
    }
    if let Some(knowledge_id) = req.knowledge_id.as_ref().and_then(|s| non_empty_str(s)) {
        target.knowledge_id = Some(knowledge_id.to_string());
    }
    Ok(target)
}

fn row_to_packet_summary(row: &SqliteRow) -> ContextPacketSummary {
    let work_summary: String = row.get("work_summary");
    let token_count: Option<i64> = row.get("token_count");
    ContextPacketSummary {
        packet_id: row.get("packet_id"),
        retrieval_id: row.get("retrieval_id"),
        ticket_id: row.get("ticket_id"),
        repository: row.get("repository"),
        summary: packet_read_summary(&work_summary, token_count),
        work_summary,
        created_by: row.get("created_by"),
        created_by_agent: row.get("created_by_agent"),
        warnings: parse_string_vec(row.get::<String, _>("warnings_json").as_str()),
        token_budget: row.get("token_budget"),
        token_count,
        created_at: row.get("created_at"),
        metadata: parse_json(row.get::<String, _>("metadata_json").as_str(), json!({})),
    }
}

fn packet_read_summary(work_summary: &str, token_count: Option<i64>) -> String {
    match token_count {
        Some(count) if count > 0 => {
            format!("Context packet for `{work_summary}` with {count} estimated tokens.")
        }
        _ => format!("Context packet for `{work_summary}`."),
    }
}

fn row_to_packet_item(row: &SqliteRow) -> ContextPacketItemSummary {
    ContextPacketItemSummary {
        rank: row.get("rank"),
        item_type: row.get("item_type"),
        artifact_id: row.get("artifact_id"),
        chunk_id: row.get("chunk_id"),
        knowledge_id: row.get("knowledge_id"),
        ticket_id: row.get("ticket_id"),
        document_id: row.get("document_id"),
        entity_id: row.get("entity_id"),
        citation_label: row.get("citation_label"),
        relevance_reason: row.get("relevance_reason"),
        included_text: row.get("included_text"),
        token_count: row.get("token_count"),
        source_retrieval_rank: row.get("source_retrieval_rank"),
        metadata: parse_json(row.get::<String, _>("metadata_json").as_str(), json!({})),
    }
}

fn row_to_retrieval_event(row: &SqliteRow) -> RetrievalEventSummary {
    RetrievalEventSummary {
        retrieval_id: row.get("retrieval_id"),
        organization: row.get("organization"),
        actor_type: row.get("actor_type"),
        actor_id: row.get("actor_id"),
        tool_name: row.get("tool_name"),
        work_summary: row.get("work_summary"),
        query_text: row.get("query_text"),
        normalized_query: row.get("normalized_query"),
        filters: parse_json(row.get::<String, _>("filters_json").as_str(), json!({})),
        authorization_filter: parse_json(
            row.get::<String, _>("authorization_filter_json").as_str(),
            json!({}),
        ),
        strategy: row.get("strategy"),
        started_at: row.get("started_at"),
        elapsed_ms: row.get("elapsed_ms"),
        result_count: row.get("result_count"),
        selected_count: row.get("selected_count"),
        empty_result: row.get::<i64, _>("empty_result") != 0,
        context_token_count: row.get("context_token_count"),
        context_truncated: row.get::<i64, _>("context_truncated") != 0,
        warnings: parse_string_vec(row.get::<String, _>("warnings_json").as_str()),
        metadata: parse_json(row.get::<String, _>("metadata_json").as_str(), json!({})),
    }
}

fn row_to_retrieval_result(row: &SqliteRow) -> RetrievalResultSummary {
    RetrievalResultSummary {
        rank: row.get("rank"),
        result_type: row.get("result_type"),
        artifact_id: row.get("artifact_id"),
        chunk_id: row.get("chunk_id"),
        knowledge_id: row.get("knowledge_id"),
        ticket_id: row.get("ticket_id"),
        document_id: row.get("document_id"),
        entity_id: row.get("entity_id"),
        namespace_id: row.get("namespace_id"),
        score: row.get("score"),
        score_components: parse_json(
            row.get::<String, _>("score_components_json").as_str(),
            json!({}),
        ),
        matched_fields: parse_json(
            row.get::<String, _>("matched_fields_json").as_str(),
            json!([]),
        ),
        snippet: row.get("snippet"),
        selected: row.get::<i64, _>("selected") != 0,
        selection_reason: row.get("selection_reason"),
        authorization_decision: row.get("authorization_decision"),
    }
}

fn parse_json(raw: &str, default: Value) -> Value {
    serde_json::from_str(raw).unwrap_or(default)
}

fn parse_string_vec(raw: &str) -> Vec<String> {
    serde_json::from_str(raw).unwrap_or_default()
}

fn non_empty_string(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn non_empty_str(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn bounded_limit(value: usize, min: usize, max: usize) -> usize {
    value.max(min).min(max)
}

fn generated_feedback_id() -> String {
    let hex = uuid::Uuid::new_v4()
        .simple()
        .to_string()
        .to_ascii_uppercase();
    format!("FB-{}", &hex[..12])
}

fn json_error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_limit_clamps_to_configured_range() {
        assert_eq!(bounded_limit(0, 1, 100), 1);
        assert_eq!(bounded_limit(25, 1, 100), 25);
        assert_eq!(bounded_limit(500, 1, 100), 100);
    }

    #[test]
    fn non_empty_string_trims_and_discards_empty_values() {
        assert_eq!(
            non_empty_string("  T-12345678  ".to_string()).as_deref(),
            Some("T-12345678")
        );
        assert_eq!(non_empty_string("   ".to_string()), None);
    }
}
