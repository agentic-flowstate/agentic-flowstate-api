use crate::{auth_middleware::AuthenticatedUser, secret_cipher};
use axum::{
    extract::{Extension, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;
use sqlx::SqlitePool;
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct UpsertSecretReferenceRequest {
    pub scope: String,
    pub conversation_id: Option<String>,
    pub key: String,
    pub label: String,
    pub value: String,
    pub description: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ListSecretReferenceQuery {
    pub conversation_id: Option<String>,
}

pub async fn list_secret_references(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Query(query): Query<ListSecretReferenceQuery>,
) -> Response {
    let organization = match require_org(&pool, &user.user_id, &headers).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    match ticketing_system::secret_references::list_metadata(
        &pool,
        &user.user_id,
        &organization,
        query.conversation_id.as_deref(),
    )
    .await
    {
        Ok(entries) => Json(entries).into_response(),
        Err(error) => server_error("Failed to list secret references", error),
    }
}

pub async fn upsert_secret_reference(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Json(request): Json<UpsertSecretReferenceRequest>,
) -> Response {
    let organization = match require_org(&pool, &user.user_id, &headers).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let key = request.key.trim().to_ascii_uppercase();
    let encryption_context = match ticketing_system::secret_references::encryption_context(
        &request.scope,
        &organization,
        request.conversation_id.as_deref(),
    ) {
        Ok(value) => value,
        Err(error) => return bad_request_error("Secure Key scope rejected", error),
    };
    let (encrypted_value, nonce) = match secret_cipher::encrypt(
        &request.value,
        &secret_cipher::aad(&user.user_id, &encryption_context, &key),
    ) {
        Ok(value) => value,
        Err(error) => return bad_request_error("Secret value rejected", error),
    };
    let storage_request = ticketing_system::UpsertSecretReference {
        user_id: user.user_id.clone(),
        scope: request.scope.clone(),
        organization: organization.clone(),
        conversation_id: request.conversation_id,
        key: key.clone(),
        label: request.label,
        encrypted_value,
        nonce,
        description: request.description,
    };
    match ticketing_system::secret_references::upsert(&pool, storage_request).await {
        Ok(entry) => {
            tracing::info!(component = "secret_references", operation = "upsert", user_id = %user.user_id, organization = %organization, scope = %request.scope, secret_key = %key, "secret reference stored");
            (StatusCode::OK, Json(entry)).into_response()
        }
        Err(error) => bad_request_error("Failed to store secret reference", error),
    }
}

pub async fn delete_secret_reference(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let organization = match require_org(&pool, &user.user_id, &headers).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    match ticketing_system::secret_references::delete(&pool, &user.user_id, &organization, &id)
        .await
    {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Secret reference not found"})),
        )
            .into_response(),
        Err(error) => server_error("Failed to delete secret reference", error),
    }
}

async fn require_org(
    pool: &SqlitePool,
    user_id: &str,
    headers: &HeaderMap,
) -> Result<String, Response> {
    let organization = headers
        .get("X-Organization")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "X-Organization header is required"})),
            )
                .into_response()
        })?
        .to_owned();
    match ticketing_system::memberships::check_membership(pool, user_id, &organization).await {
        Ok(true) => Ok(organization),
        Ok(false) => Err((
            StatusCode::FORBIDDEN,
            Json(json!({"error": "Organization access denied"})),
        )
            .into_response()),
        Err(error) => Err(server_error("Organization access check failed", error)),
    }
}

fn bad_request_error(context: &str, error: anyhow::Error) -> Response {
    tracing::warn!(component = "secret_references", operation = "validation", error = %error, "{context}");
    (StatusCode::BAD_REQUEST, Json(json!({"error": context}))).into_response()
}

fn server_error(context: &str, error: anyhow::Error) -> Response {
    tracing::error!(component = "secret_references", error = ?error, "{context}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": context})),
    )
        .into_response()
}
