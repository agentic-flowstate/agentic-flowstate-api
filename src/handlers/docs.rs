use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{Row, SqlitePool};
use std::sync::Arc;
use tracing::error;

use super::get_organization;

#[derive(Debug, Deserialize)]
pub struct DocContentQuery {
    pub artifact_id: String,
}

/// GET /api/tickets/:ticket_id/docs/content?artifact_id=<id>
///
/// Serves markdown content for an artifact attached to a ticket.
/// Normalized documentation refs are authoritative; legacy
/// tickets.documentation arrays are used only when no normalized refs exist.
pub async fn serve_document_content(
    State(pool): State<Arc<SqlitePool>>,
    headers: HeaderMap,
    Path(ticket_id): Path<String>,
    Query(query): Query<DocContentQuery>,
) -> Response {
    let org = get_organization(&headers);
    let ticket = match ticketing_system::tickets::get_ticket_by_id(&pool, &ticket_id).await {
        Ok(Some(t)) if t.organization == org => t,
        Ok(Some(_)) | Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Ticket not found" })),
            )
                .into_response();
        }
        Err(e) => {
            error!("Failed to look up ticket {}: {:?}", ticket_id, e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to look up ticket" })),
            )
                .into_response();
        }
    };

    match get_normalized_document_artifact_content(&pool, &org, &ticket_id, &query.artifact_id)
        .await
    {
        Ok(Some(content)) => return markdown_response(content),
        Ok(None) => {}
        Err(e) => {
            error!(
                "Failed to fetch normalized document artifact {} for ticket {}: {:?}",
                query.artifact_id, ticket_id, e
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to fetch artifact" })),
            )
                .into_response();
        }
    }

    match normalized_documentation_ref_count(&pool, &ticket_id).await {
        Ok(0) => {}
        Ok(_) => {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": "Artifact not attached to this ticket" })),
            )
                .into_response();
        }
        Err(e) => {
            error!(
                "Failed to count normalized docs for ticket {}: {:?}",
                ticket_id, e
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to verify ticket documentation" })),
            )
                .into_response();
        }
    }

    let docs = ticket.documentation.unwrap_or_default();
    if !docs.contains(&query.artifact_id) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "Artifact not attached to this ticket" })),
        )
            .into_response();
    }

    match get_legacy_document_artifact_content(&pool, &org, &query.artifact_id).await {
        Ok(Some(content)) => markdown_response(content),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Artifact not found in database" })),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to fetch artifact {}: {:?}", query.artifact_id, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to fetch artifact" })),
            )
                .into_response()
        }
    }
}

#[derive(Debug, Serialize)]
pub struct TicketDocSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document_id: Option<String>,
    pub title: String,
    pub relationship_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
}

/// GET /api/tickets/:ticket_id/docs
///
/// Returns summaries of normalized documentation refs attached to a ticket.
pub async fn list_ticket_docs(
    State(pool): State<Arc<SqlitePool>>,
    headers: HeaderMap,
    Path(ticket_id): Path<String>,
) -> Response {
    let org = get_organization(&headers);
    let ticket = match ticketing_system::tickets::get_ticket_by_id(&pool, &ticket_id).await {
        Ok(Some(t)) if t.organization == org => t,
        Ok(Some(_)) | Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Ticket not found" })),
            )
                .into_response();
        }
        Err(e) => {
            error!("Failed to look up ticket {}: {:?}", ticket_id, e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to look up ticket" })),
            )
                .into_response();
        }
    };

    match list_normalized_ticket_docs(&pool, &org, &ticket_id).await {
        Ok(refs) if !refs.is_empty() => (StatusCode::OK, Json(json!(refs))).into_response(),
        Ok(_) => {
            let doc_ids = ticket.documentation.unwrap_or_default();
            match list_legacy_ticket_docs(&pool, &org, &doc_ids).await {
                Ok(summaries) => (StatusCode::OK, Json(json!(summaries))).into_response(),
                Err(e) => {
                    error!(
                        "Failed to list legacy docs for ticket {}: {:?}",
                        ticket_id, e
                    );
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": "Failed to list ticket documentation" })),
                    )
                        .into_response()
                }
            }
        }
        Err(e) => {
            error!(
                "Failed to list normalized docs for ticket {}: {:?}",
                ticket_id, e
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to list ticket documentation" })),
            )
                .into_response()
        }
    }
}

fn markdown_response(content: String) -> Response {
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/markdown; charset=utf-8",
        )],
        content,
    )
        .into_response()
}

async fn normalized_documentation_ref_count(
    pool: &SqlitePool,
    ticket_id: &str,
) -> sqlx::Result<i64> {
    sqlx::query_scalar(
        r#"
        SELECT COUNT(*)
        FROM ticket_documentation_refs
        WHERE ticket_id = ?
          AND relationship_type IN ('documentation', 'recovered_note')
        "#,
    )
    .bind(ticket_id)
    .fetch_one(pool)
    .await
}

async fn get_normalized_document_artifact_content(
    pool: &SqlitePool,
    org: &str,
    ticket_id: &str,
    artifact_id: &str,
) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar(
        r#"
        SELECT a.content
        FROM ticket_documentation_refs r
        JOIN artifacts a ON a.artifact_id = r.artifact_id
        WHERE r.ticket_id = ?
          AND r.artifact_id = ?
          AND r.relationship_type IN ('documentation', 'recovered_note')
          AND a.organization = ?
          AND a.lifecycle_status = 'active'
          AND a.visibility IN ('organization', 'system')
        LIMIT 1
        "#,
    )
    .bind(ticket_id)
    .bind(artifact_id)
    .bind(org)
    .fetch_optional(pool)
    .await
}

async fn get_legacy_document_artifact_content(
    pool: &SqlitePool,
    org: &str,
    artifact_id: &str,
) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar(
        r#"
        SELECT content
        FROM artifacts
        WHERE artifact_id = ?
          AND organization = ?
          AND lifecycle_status = 'active'
          AND visibility IN ('organization', 'system')
        LIMIT 1
        "#,
    )
    .bind(artifact_id)
    .bind(org)
    .fetch_optional(pool)
    .await
}

async fn list_normalized_ticket_docs(
    pool: &SqlitePool,
    org: &str,
    ticket_id: &str,
) -> sqlx::Result<Vec<TicketDocSummary>> {
    let rows = sqlx::query(
        r#"
        SELECT r.artifact_id, r.document_id, r.relationship_type, r.position, r.created_at,
               a.title AS artifact_title, a.artifact_type,
               d.filename AS document_title, d.document_type
        FROM ticket_documentation_refs r
        LEFT JOIN artifacts a ON a.artifact_id = r.artifact_id
        LEFT JOIN documents d ON d.document_id = r.document_id
        WHERE r.ticket_id = ?
          AND r.relationship_type IN ('documentation', 'recovered_note')
          AND (
              (r.artifact_id IS NOT NULL
               AND a.organization = ?
               AND a.lifecycle_status = 'active'
               AND a.visibility IN ('organization', 'system'))
              OR
              (r.document_id IS NOT NULL AND d.organization = ?)
          )
        ORDER BY COALESCE(r.position, 999999), r.created_at, r.artifact_id, r.document_id
        "#,
    )
    .bind(ticket_id)
    .bind(org)
    .bind(org)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| {
            let artifact_title: Option<String> = row.get("artifact_title");
            let document_title: Option<String> = row.get("document_title");
            TicketDocSummary {
                artifact_id: row.get("artifact_id"),
                document_id: row.get("document_id"),
                title: artifact_title
                    .or(document_title)
                    .unwrap_or_else(|| "Untitled documentation reference".to_string()),
                relationship_type: row.get("relationship_type"),
                artifact_type: row.get("artifact_type"),
                document_type: row.get("document_type"),
                position: row.get("position"),
                created_at: row.get("created_at"),
            }
        })
        .collect())
}

async fn list_legacy_ticket_docs(
    pool: &SqlitePool,
    org: &str,
    doc_ids: &[String],
) -> sqlx::Result<Vec<TicketDocSummary>> {
    let mut summaries = Vec::with_capacity(doc_ids.len());
    for (position, id) in doc_ids.iter().enumerate() {
        let row = sqlx::query(
            r#"
            SELECT artifact_id, title, artifact_type, created_at
            FROM artifacts
            WHERE artifact_id = ?
              AND organization = ?
              AND lifecycle_status = 'active'
              AND visibility IN ('organization', 'system')
            LIMIT 1
            "#,
        )
        .bind(id)
        .bind(org)
        .fetch_optional(pool)
        .await?;

        if let Some(row) = row {
            summaries.push(TicketDocSummary {
                artifact_id: Some(row.get("artifact_id")),
                document_id: None,
                title: row.get("title"),
                relationship_type: "documentation".to_string(),
                artifact_type: row.get("artifact_type"),
                document_type: None,
                position: Some(i64::try_from(position).unwrap_or(i64::MAX)),
                created_at: row.get("created_at"),
            });
        }
    }
    Ok(summaries)
}
