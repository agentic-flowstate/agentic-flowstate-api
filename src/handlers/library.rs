use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;
use sqlx::SqlitePool;
use std::sync::Arc;
use tracing::error;

use super::get_organization;

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub q: Option<String>,
}

/// GET /api/library/artifacts — list all artifacts for the org
pub async fn list_library_artifacts(
    State(pool): State<Arc<SqlitePool>>,
    headers: HeaderMap,
) -> Response {
    let org = get_organization(&headers);
    match ticketing_system::artifacts::list_by_org(&pool, &org).await {
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
    match ticketing_system::artifacts::search(&pool, &q, Some(&org)).await {
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
    match ticketing_system::artifacts::get_artifact(&pool, &artifact_id).await {
        Ok(Some(artifact)) if artifact.organization == org => {
            (StatusCode::OK, Json(json!(artifact))).into_response()
        }
        Ok(Some(_)) | Ok(None) => (
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
