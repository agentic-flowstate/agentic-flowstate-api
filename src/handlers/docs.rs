use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::SqlitePool;
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
/// The artifact_id must exist in the ticket's documentation list.
pub async fn serve_document_content(
    State(pool): State<Arc<SqlitePool>>,
    headers: HeaderMap,
    Path(ticket_id): Path<String>,
    Query(query): Query<DocContentQuery>,
) -> Response {
    let org = get_organization(&headers);
    // Look up ticket
    let ticket = match ticketing_system::tickets::get_ticket_by_id(&pool, &ticket_id).await {
        Ok(Some(t)) if t.organization == org => t,
        Ok(Some(_)) => {
            return (
                StatusCode::NOT_FOUND,
                axum::Json(json!({ "error": "Ticket not found" })),
            )
                .into_response()
        }
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                axum::Json(json!({ "error": "Ticket not found" })),
            )
                .into_response();
        }
        Err(e) => {
            error!("Failed to look up ticket {}: {:?}", ticket_id, e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({ "error": "Failed to look up ticket" })),
            )
                .into_response();
        }
    };

    // Verify the requested artifact_id is in this ticket's documentation list
    let docs = ticket.documentation.unwrap_or_default();
    if !docs.contains(&query.artifact_id) {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(json!({ "error": "Artifact not attached to this ticket" })),
        )
            .into_response();
    }

    // Fetch artifact content from DB
    match ticketing_system::artifacts::get_artifact(&pool, &query.artifact_id).await {
        Ok(Some(artifact)) => (
            StatusCode::OK,
            [(
                axum::http::header::CONTENT_TYPE,
                "text/markdown; charset=utf-8",
            )],
            artifact.content,
        )
            .into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            axum::Json(json!({ "error": "Artifact not found in database" })),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to fetch artifact {}: {:?}", query.artifact_id, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({ "error": "Failed to fetch artifact" })),
            )
                .into_response()
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ArtifactDocSummary {
    pub artifact_id: String,
    pub title: String,
    pub artifact_type: String,
}

/// GET /api/tickets/:ticket_id/docs
///
/// Returns summaries of all artifacts attached to a ticket's documentation list.
pub async fn list_ticket_docs(
    State(pool): State<Arc<SqlitePool>>,
    headers: HeaderMap,
    Path(ticket_id): Path<String>,
) -> Response {
    let org = get_organization(&headers);
    // Look up ticket
    let ticket = match ticketing_system::tickets::get_ticket_by_id(&pool, &ticket_id).await {
        Ok(Some(t)) if t.organization == org => t,
        Ok(Some(_)) => {
            return (
                StatusCode::NOT_FOUND,
                axum::Json(json!({ "error": "Ticket not found" })),
            )
                .into_response()
        }
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                axum::Json(json!({ "error": "Ticket not found" })),
            )
                .into_response();
        }
        Err(e) => {
            error!("Failed to look up ticket {}: {:?}", ticket_id, e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({ "error": "Failed to look up ticket" })),
            )
                .into_response();
        }
    };

    let doc_ids = ticket.documentation.unwrap_or_default();
    let mut summaries: Vec<ArtifactDocSummary> = Vec::with_capacity(doc_ids.len());

    for id in &doc_ids {
        match ticketing_system::artifacts::get_artifact(&pool, id).await {
            Ok(Some(artifact)) => {
                summaries.push(ArtifactDocSummary {
                    artifact_id: artifact.artifact_id,
                    title: artifact.title,
                    artifact_type: artifact.artifact_type,
                });
            }
            Ok(None) => {
                // Artifact was deleted but ID still in documentation list — skip it
            }
            Err(e) => {
                error!(
                    "Failed to fetch artifact {} for ticket {}: {:?}",
                    id, ticket_id, e
                );
            }
        }
    }

    (StatusCode::OK, axum::Json(json!(summaries))).into_response()
}
