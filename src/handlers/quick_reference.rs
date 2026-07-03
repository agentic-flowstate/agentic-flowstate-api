//! Quick-reference drawer REST API.

use axum::{
    extract::{Extension, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::SqlitePool;
use std::sync::Arc;

use crate::auth_middleware::AuthenticatedUser;
use crate::handlers::get_organization;

#[derive(Debug, Deserialize)]
pub struct ListQuickReferenceQuery {
    pub conversation_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpsertQuickReferenceRequest {
    pub scope: String,
    pub conversation_id: Option<String>,
    pub key: String,
    pub label: String,
    pub value: String,
    pub value_type: String,
    pub sort_order: Option<i64>,
    pub description: Option<String>,
    pub source_message_id: Option<String>,
    pub ticket_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PatchQuickReferenceRequest {
    pub label: Option<String>,
    pub value: Option<String>,
    pub value_type: Option<String>,
    pub sort_order: Option<i64>,
    pub description: Option<String>,
    pub source_message_id: Option<String>,
    pub ticket_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ReorderQuickReferenceRequest {
    pub entries: Vec<ticketing_system::QuickReferenceReorderItem>,
}

#[derive(Debug, Serialize)]
pub struct UpsertQuickReferenceResponse {
    pub entry: ticketing_system::QuickReferenceEntry,
    pub created: bool,
}

/// GET /api/quick-reference?conversation_id=...
pub async fn list_quick_reference(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Query(query): Query<ListQuickReferenceQuery>,
) -> Response {
    let organization = get_organization(&headers);
    let conversation_id = normalized_optional(query.conversation_id);

    if let Some(conversation_id) = conversation_id.as_deref() {
        if let Err(response) =
            require_conversation_access(&pool, &user.user_id, &organization, conversation_id).await
        {
            return response;
        }
    }

    match ticketing_system::quick_reference::list_entries(
        &pool,
        &organization,
        conversation_id.as_deref(),
    )
    .await
    {
        Ok(entries) => (StatusCode::OK, Json(json!(entries))).into_response(),
        Err(e) => server_error("Failed to list quick-reference entries", e),
    }
}

/// POST /api/quick-reference
pub async fn upsert_quick_reference(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Json(req): Json<UpsertQuickReferenceRequest>,
) -> Response {
    let organization = get_organization(&headers);
    let scope = req.scope.trim().to_ascii_lowercase();

    match scope.as_str() {
        "conversation" => {
            let Some(conversation_id) = normalized_optional(req.conversation_id.clone()) else {
                return bad_request("conversation_id is required for conversation scope");
            };
            if let Err(response) =
                require_conversation_access(&pool, &user.user_id, &organization, &conversation_id)
                    .await
            {
                return response;
            }
        }
        "organization" => {
            if normalized_optional(req.conversation_id.clone()).is_some() {
                return bad_request("conversation_id must be omitted for organization scope");
            }
        }
        _ => return bad_request("scope must be one of: conversation, organization"),
    }

    let storage_req = ticketing_system::UpsertQuickReferenceEntry {
        organization,
        scope: req.scope,
        conversation_id: req.conversation_id,
        key: req.key,
        label: req.label,
        value: req.value,
        value_type: req.value_type,
        sort_order: req.sort_order,
        description: req.description,
        source_message_id: req.source_message_id,
        ticket_id: req.ticket_id,
        created_by: user.user_id.clone(),
        updated_by: user.user_id,
    };

    match ticketing_system::quick_reference::upsert_entry(&pool, storage_req).await {
        Ok(result) => {
            let status = if result.created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            (
                status,
                Json(json!(UpsertQuickReferenceResponse {
                    entry: result.entry,
                    created: result.created,
                })),
            )
                .into_response()
        }
        Err(e) => bad_request_error("Failed to upsert quick-reference entry", e),
    }
}

/// PATCH /api/quick-reference/:id
pub async fn patch_quick_reference(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<PatchQuickReferenceRequest>,
) -> Response {
    let organization = get_organization(&headers);
    let entry = match require_entry_access(&pool, &user.user_id, &organization, &id).await {
        Ok(entry) => entry,
        Err(response) => return response,
    };

    match ticketing_system::quick_reference::update_entry(
        &pool,
        &organization,
        &entry.id,
        ticketing_system::UpdateQuickReferenceEntry {
            label: req.label,
            value: req.value,
            value_type: req.value_type,
            sort_order: req.sort_order,
            description: req.description,
            source_message_id: req.source_message_id,
            ticket_id: req.ticket_id,
            updated_by: user.user_id,
        },
    )
    .await
    {
        Ok(Some(entry)) => (StatusCode::OK, Json(json!(entry))).into_response(),
        Ok(None) => not_found("Quick-reference entry not found"),
        Err(e) => bad_request_error("Failed to patch quick-reference entry", e),
    }
}

/// POST /api/quick-reference/reorder
pub async fn reorder_quick_reference(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Json(req): Json<ReorderQuickReferenceRequest>,
) -> Response {
    let organization = get_organization(&headers);

    for item in &req.entries {
        if let Err(response) =
            require_entry_access(&pool, &user.user_id, &organization, &item.id).await
        {
            return response;
        }
    }

    match ticketing_system::quick_reference::reorder_entries(
        &pool,
        &organization,
        req.entries,
        &user.user_id,
    )
    .await
    {
        Ok(entries) => (StatusCode::OK, Json(json!(entries))).into_response(),
        Err(e) => bad_request_error("Failed to reorder quick-reference entries", e),
    }
}

/// DELETE /api/quick-reference/:id
pub async fn delete_quick_reference(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let organization = get_organization(&headers);
    let entry = match require_entry_access(&pool, &user.user_id, &organization, &id).await {
        Ok(entry) => entry,
        Err(response) => return response,
    };

    match ticketing_system::quick_reference::delete_entry(&pool, &organization, &entry.id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => not_found("Quick-reference entry not found"),
        Err(e) => server_error("Failed to delete quick-reference entry", e),
    }
}

async fn require_entry_access(
    pool: &SqlitePool,
    user_id: &str,
    organization: &str,
    id: &str,
) -> Result<ticketing_system::QuickReferenceEntry, Response> {
    let entry = ticketing_system::quick_reference::get_entry(pool, organization, id)
        .await
        .map_err(|e| server_error("Failed to fetch quick-reference entry", e))?
        .ok_or_else(|| not_found("Quick-reference entry not found"))?;

    if entry.scope == "conversation" {
        let Some(conversation_id) = entry.conversation_id.clone() else {
            return Err(server_error_message(
                "Conversation-scoped quick-reference entry is missing conversation_id",
            ));
        };
        require_conversation_access(pool, user_id, organization, &conversation_id).await?;
    }

    Ok(entry)
}

async fn require_conversation_access(
    pool: &SqlitePool,
    user_id: &str,
    organization: &str,
    conversation_id: &str,
) -> Result<(), Response> {
    let conversation =
        ticketing_system::conversations::get_conversation(pool, conversation_id, false)
            .await
            .map_err(|e| server_error("Failed to fetch conversation", e))?
            .ok_or_else(|| not_found("Conversation not found"))?;

    let organization_matches = conversation.organization == organization
        || conversation.router_organization.as_deref() == Some(organization);
    if conversation.user_id != user_id || !organization_matches {
        return Err(not_found("Conversation not found"));
    }

    Ok(())
}

fn normalized_optional(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

fn bad_request(message: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response()
}

fn bad_request_error(context: &str, error: anyhow::Error) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": format!("{context}: {error}") })),
    )
        .into_response()
}

fn not_found(message: &str) -> Response {
    (StatusCode::NOT_FOUND, Json(json!({ "error": message }))).into_response()
}

fn server_error(context: &str, error: anyhow::Error) -> Response {
    tracing::error!("{context}: {error:?}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": context })),
    )
        .into_response()
}

fn server_error_message(message: &str) -> Response {
    tracing::error!("{message}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": message })),
    )
        .into_response()
}
