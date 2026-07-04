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
    let requested_organization = header_organization(&headers);
    let conversation_id = normalized_optional(query.conversation_id);
    let mut conversation_entries = Vec::new();
    let mut organization_entries = Vec::new();

    if let Some(conversation_id) = conversation_id.as_deref() {
        let conversation =
            match require_user_conversation(&pool, &user.user_id, conversation_id).await {
                Ok(conversation) => conversation,
                Err(response) => return response,
            };

        let entries = match ticketing_system::quick_reference::list_entries(
            &pool,
            &conversation.organization,
            Some(conversation_id),
        )
        .await
        {
            Ok(entries) => entries,
            Err(e) => {
                return server_error("Failed to list conversation quick-reference entries", e)
            }
        };
        conversation_entries = entries.conversation;

        if let Some(organization) = requested_organization.as_deref() {
            match has_organization_access(&pool, &user.user_id, organization).await {
                Ok(true) => {
                    let entries = match ticketing_system::quick_reference::list_entries(
                        &pool,
                        organization,
                        None,
                    )
                    .await
                    {
                        Ok(entries) => entries,
                        Err(e) => {
                            return server_error(
                                "Failed to list organization quick-reference entries",
                                e,
                            )
                        }
                    };
                    organization_entries = entries.organization;
                }
                Ok(false) => {}
                Err(response) => return response,
            }
        }
    } else {
        let organization =
            match require_header_organization_access(&pool, &user.user_id, &headers).await {
                Ok(organization) => organization,
                Err(response) => return response,
            };

        let entries =
            match ticketing_system::quick_reference::list_entries(&pool, &organization, None).await
            {
                Ok(entries) => entries,
                Err(e) => return server_error("Failed to list quick-reference entries", e),
            };
        organization_entries = entries.organization;
    }

    (
        StatusCode::OK,
        Json(json!(ticketing_system::QuickReferenceList {
            conversation: conversation_entries,
            organization: organization_entries,
        })),
    )
        .into_response()
}

/// POST /api/quick-reference
pub async fn upsert_quick_reference(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Json(req): Json<UpsertQuickReferenceRequest>,
) -> Response {
    let scope = req.scope.trim().to_ascii_lowercase();
    let organization: String;
    let conversation_id: Option<String>;

    match scope.as_str() {
        "conversation" => {
            let Some(req_conversation_id) = normalized_optional(req.conversation_id.clone()) else {
                return bad_request("conversation_id is required for conversation scope");
            };
            let conversation =
                match require_user_conversation(&pool, &user.user_id, &req_conversation_id).await {
                    Ok(conversation) => conversation,
                    Err(response) => return response,
                };
            organization = conversation.organization;
            conversation_id = Some(req_conversation_id);
        }
        "organization" => {
            if normalized_optional(req.conversation_id.clone()).is_some() {
                return bad_request("conversation_id must be omitted for organization scope");
            }
            organization =
                match require_header_organization_access(&pool, &user.user_id, &headers).await {
                    Ok(organization) => organization,
                    Err(response) => return response,
                };
            conversation_id = None;
        }
        _ => return bad_request("scope must be one of: conversation, organization"),
    }

    let storage_req = ticketing_system::UpsertQuickReferenceEntry {
        organization,
        scope,
        conversation_id,
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
    Path(id): Path<String>,
    Json(req): Json<PatchQuickReferenceRequest>,
) -> Response {
    let entry = match require_entry_access(&pool, &user.user_id, &id).await {
        Ok(entry) => entry,
        Err(response) => return response,
    };

    match ticketing_system::quick_reference::update_entry(
        &pool,
        &entry.organization,
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
    Json(req): Json<ReorderQuickReferenceRequest>,
) -> Response {
    if req.entries.is_empty() {
        return (
            StatusCode::OK,
            Json(json!(Vec::<ticketing_system::QuickReferenceEntry>::new())),
        )
            .into_response();
    }

    let mut organization: Option<String> = None;
    for item in &req.entries {
        let entry = match require_entry_access(&pool, &user.user_id, &item.id).await {
            Ok(entry) => entry,
            Err(response) => return response,
        };
        match organization.as_deref() {
            Some(existing) if existing != entry.organization => {
                return bad_request("Cannot reorder entries across organizations")
            }
            Some(_) => {}
            None => organization = Some(entry.organization),
        }
    }
    let organization = organization.expect("non-empty quick-reference reorder has organization");

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
    Path(id): Path<String>,
) -> Response {
    let entry = match require_entry_access(&pool, &user.user_id, &id).await {
        Ok(entry) => entry,
        Err(response) => return response,
    };

    match ticketing_system::quick_reference::delete_entry(&pool, &entry.organization, &entry.id)
        .await
    {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => not_found("Quick-reference entry not found"),
        Err(e) => server_error("Failed to delete quick-reference entry", e),
    }
}

async fn require_entry_access(
    pool: &SqlitePool,
    user_id: &str,
    id: &str,
) -> Result<ticketing_system::QuickReferenceEntry, Response> {
    let entry = get_entry_by_id_any_org(pool, id)
        .await?
        .ok_or_else(|| not_found("Quick-reference entry not found"))?;

    if entry.scope == "conversation" {
        let Some(conversation_id) = entry.conversation_id.clone() else {
            return Err(server_error_message(
                "Conversation-scoped quick-reference entry is missing conversation_id",
            ));
        };
        require_user_conversation(pool, user_id, &conversation_id).await?;
    } else {
        require_organization_access(pool, user_id, &entry.organization).await?;
    }

    Ok(entry)
}

async fn require_user_conversation(
    pool: &SqlitePool,
    user_id: &str,
    conversation_id: &str,
) -> Result<ticketing_system::Conversation, Response> {
    let conversation =
        ticketing_system::conversations::get_conversation(pool, conversation_id, false)
            .await
            .map_err(|e| server_error("Failed to fetch conversation", e))?
            .ok_or_else(|| not_found("Conversation not found"))?;

    if conversation.user_id != user_id {
        return Err(not_found("Conversation not found"));
    }

    Ok(conversation)
}

async fn require_header_organization_access(
    pool: &SqlitePool,
    user_id: &str,
    headers: &HeaderMap,
) -> Result<String, Response> {
    let Some(organization) = header_organization(headers) else {
        return Err(bad_request("X-Organization header is required"));
    };
    require_organization_access(pool, user_id, &organization).await?;
    Ok(organization)
}

async fn require_organization_access(
    pool: &SqlitePool,
    user_id: &str,
    organization: &str,
) -> Result<(), Response> {
    match has_organization_access(pool, user_id, organization).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(forbidden(&format!(
            "No access to organization: {organization}"
        ))),
        Err(response) => Err(response),
    }
}

async fn has_organization_access(
    pool: &SqlitePool,
    user_id: &str,
    organization: &str,
) -> Result<bool, Response> {
    ticketing_system::memberships::check_membership(pool, user_id, organization)
        .await
        .map_err(|e| server_error("Organization access check failed", e))
}

async fn get_entry_by_id_any_org(
    pool: &SqlitePool,
    id: &str,
) -> Result<Option<ticketing_system::QuickReferenceEntry>, Response> {
    let id = id.trim();
    if id.is_empty() {
        return Err(bad_request("Quick-reference entry id is required"));
    }
    let organization: Option<String> =
        sqlx::query_scalar("SELECT organization FROM quick_references WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await
            .map_err(|e| server_error("Failed to fetch quick-reference entry", e.into()))?;

    match organization {
        Some(organization) => ticketing_system::quick_reference::get_entry(pool, &organization, id)
            .await
            .map_err(|e| server_error("Failed to fetch quick-reference entry", e)),
        None => Ok(None),
    }
}

fn header_organization(headers: &HeaderMap) -> Option<String> {
    headers
        .get("X-Organization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| normalized_optional(Some(value.to_string())))
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

fn forbidden(message: &str) -> Response {
    (StatusCode::FORBIDDEN, Json(json!({ "error": message }))).into_response()
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
