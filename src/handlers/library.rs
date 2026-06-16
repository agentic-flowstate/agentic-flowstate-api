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
pub struct SearchQuery {
    pub q: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct LibraryArtifactSummary {
    pub artifact_id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub artifact_type: String,
    pub created_by: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_step_id: Option<String>,
    pub organization: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epic_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slice_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticket_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lifecycle_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    pub content_length: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Serialize)]
pub struct LibraryArtifact {
    #[serde(flatten)]
    pub summary: LibraryArtifactSummary,
    pub content: String,
}

/// GET /api/library/artifacts — list all artifacts for the org
pub async fn list_library_artifacts(
    State(pool): State<Arc<SqlitePool>>,
    headers: HeaderMap,
) -> Response {
    let org = get_organization(&headers);
    match list_visible_artifacts(&pool, &org).await {
        Ok(artifacts) => (StatusCode::OK, Json(json!(artifacts))).into_response(),
        Err(e) => {
            error!("Failed to list artifacts for org {}: {:?}", org, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to list artifacts" })),
            )
                .into_response()
        }
    }
}

/// GET /api/library/artifacts/search?q=<query> — FTS5 search artifacts
pub async fn search_library_artifacts(
    State(pool): State<Arc<SqlitePool>>,
    headers: HeaderMap,
    Query(query): Query<SearchQuery>,
) -> Response {
    let org = get_organization(&headers);
    let q = match query.q {
        Some(q) if !q.trim().is_empty() => q,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Missing search query 'q'" })),
            )
                .into_response()
        }
    };
    match search_visible_artifacts(&pool, &org, &q).await {
        Ok(artifacts) => (StatusCode::OK, Json(json!(artifacts))).into_response(),
        Err(e) => {
            error!("Failed to search artifacts: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to search artifacts" })),
            )
                .into_response()
        }
    }
}

/// GET /api/library/artifacts/:artifact_id — get a single artifact with content
pub async fn get_library_artifact(
    State(pool): State<Arc<SqlitePool>>,
    headers: HeaderMap,
    Path(artifact_id): Path<String>,
) -> Response {
    let org = get_organization(&headers);
    match get_visible_artifact(&pool, &org, &artifact_id).await {
        Ok(Some(artifact)) => (StatusCode::OK, Json(json!(artifact))).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Artifact not found" })),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to get artifact {}: {:?}", artifact_id, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to get artifact" })),
            )
                .into_response()
        }
    }
}

/// GET /api/library/documents — list all documents for the org
pub async fn list_library_documents(
    State(pool): State<Arc<SqlitePool>>,
    headers: HeaderMap,
) -> Response {
    let org = get_organization(&headers);
    match ticketing_system::documents::list_documents(&pool, &org, None, None, None).await {
        Ok(documents) => (StatusCode::OK, Json(json!(documents))).into_response(),
        Err(e) => {
            error!("Failed to list documents for org {}: {:?}", org, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to list documents" })),
            )
                .into_response()
        }
    }
}

/// GET /api/library/documents/search?q=<query> — FTS5 search documents
pub async fn search_library_documents(
    State(pool): State<Arc<SqlitePool>>,
    headers: HeaderMap,
    Query(query): Query<SearchQuery>,
) -> Response {
    let org = get_organization(&headers);
    let q = match query.q {
        Some(q) if !q.trim().is_empty() => q,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Missing search query 'q'" })),
            )
                .into_response()
        }
    };
    match ticketing_system::documents::search_documents(&pool, &q, Some(&org)).await {
        Ok(documents) => (StatusCode::OK, Json(json!(documents))).into_response(),
        Err(e) => {
            error!("Failed to search documents: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to search documents" })),
            )
                .into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct DownloadQuery {
    pub inline: Option<bool>,
}

/// GET /api/library/documents/:document_id/download — download or view document binary
/// Pass ?inline=true to serve for embedding (Content-Disposition: inline)
pub async fn download_library_document(
    State(pool): State<Arc<SqlitePool>>,
    headers: HeaderMap,
    Path(document_id): Path<String>,
    Query(query): Query<DownloadQuery>,
) -> Response {
    let org = get_organization(&headers);
    // Get metadata for Content-Type and filename
    let doc = match ticketing_system::documents::get_document(&pool, &document_id).await {
        Ok(Some(d)) if d.organization == org => d,
        Ok(Some(_)) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Document not found" })),
            )
                .into_response()
        }
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Document not found" })),
            )
                .into_response()
        }
        Err(e) => {
            error!("Failed to get document metadata {}: {:?}", document_id, e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to get document" })),
            )
                .into_response();
        }
    };

    // Get binary content
    let content = match ticketing_system::documents::get_document_content(&pool, &document_id).await
    {
        Ok(Some(c)) => c,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Document content not found" })),
            )
                .into_response()
        }
        Err(e) => {
            error!("Failed to get document content {}: {:?}", document_id, e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to get document content" })),
            )
                .into_response();
        }
    };

    let disposition = if query.inline.unwrap_or(false) {
        format!("inline; filename=\"{}\"", doc.filename.replace('"', "\\\""))
    } else {
        format!(
            "attachment; filename=\"{}\"",
            doc.filename.replace('"', "\\\"")
        )
    };

    (
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, doc.mime_type.as_str()),
            (
                axum::http::header::CONTENT_DISPOSITION,
                disposition.as_str(),
            ),
        ],
        content,
    )
        .into_response()
}

async fn list_visible_artifacts(
    pool: &SqlitePool,
    org: &str,
) -> sqlx::Result<Vec<LibraryArtifactSummary>> {
    let rows = sqlx::query(
        r#"
        SELECT artifact_id, title, summary, length(content) AS content_length,
               artifact_type, created_by, source_step_id, organization,
               epic_id, slice_id, ticket_id, agent_run_id, source_kind,
               lifecycle_status, visibility, repository, created_at, updated_at
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

    Ok(rows.into_iter().map(row_to_artifact_summary).collect())
}

async fn search_visible_artifacts(
    pool: &SqlitePool,
    org: &str,
    query: &str,
) -> anyhow::Result<Vec<LibraryArtifactSummary>> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }

    let rows = match search_visible_artifact_rows(pool, org, query).await {
        Ok(rows) => rows,
        Err(error) if ticketing_system::fts::should_retry_as_literal(&error) => {
            let literal_query = ticketing_system::fts::literalize_query(query);
            search_visible_artifact_rows(pool, org, &literal_query).await?
        }
        Err(error) => return Err(error.into()),
    };

    Ok(rows.into_iter().map(row_to_artifact_summary).collect())
}

async fn search_visible_artifact_rows(
    pool: &SqlitePool,
    org: &str,
    query: &str,
) -> sqlx::Result<Vec<sqlx::sqlite::SqliteRow>> {
    sqlx::query(
        r#"
        SELECT a.artifact_id, a.title, a.summary, length(a.content) AS content_length,
               a.artifact_type, a.created_by, a.source_step_id, a.organization,
               a.epic_id, a.slice_id, a.ticket_id, a.agent_run_id, a.source_kind,
               a.lifecycle_status, a.visibility, a.repository, a.created_at, a.updated_at
        FROM artifacts a
        JOIN artifacts_fts f ON a.artifact_id = f.artifact_id
        WHERE artifacts_fts MATCH ?
          AND a.organization = ?
          AND a.lifecycle_status = 'active'
          AND a.visibility IN ('organization', 'system')
        ORDER BY rank
        "#,
    )
    .bind(query)
    .bind(org)
    .fetch_all(pool)
    .await
}

async fn get_visible_artifact(
    pool: &SqlitePool,
    org: &str,
    artifact_id: &str,
) -> sqlx::Result<Option<LibraryArtifact>> {
    let row = sqlx::query(
        r#"
        SELECT artifact_id, title, summary, content, length(content) AS content_length,
               artifact_type, created_by, source_step_id, organization,
               epic_id, slice_id, ticket_id, agent_run_id, source_kind,
               lifecycle_status, visibility, repository, created_at, updated_at
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
    .await?;

    Ok(row.map(|row| {
        let content = row.get("content");
        LibraryArtifact {
            summary: row_to_artifact_summary(row),
            content,
        }
    }))
}

fn row_to_artifact_summary(row: sqlx::sqlite::SqliteRow) -> LibraryArtifactSummary {
    LibraryArtifactSummary {
        artifact_id: row.get("artifact_id"),
        title: row.get("title"),
        summary: row.get("summary"),
        artifact_type: row.get("artifact_type"),
        created_by: row.get("created_by"),
        source_step_id: row.get("source_step_id"),
        organization: row.get("organization"),
        epic_id: row.get("epic_id"),
        slice_id: row.get("slice_id"),
        ticket_id: row.get("ticket_id"),
        agent_run_id: row.get("agent_run_id"),
        source_kind: row.get("source_kind"),
        lifecycle_status: row.get("lifecycle_status"),
        visibility: row.get("visibility"),
        repository: row.get("repository"),
        content_length: row.get("content_length"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}
