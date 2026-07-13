use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Extension, Json,
};
use serde::Deserialize;
use serde_json::json;
use sqlx::SqlitePool;
use std::sync::Arc;
use ticketing_system::schedule_offsets::{
    self, ActorType, ScheduleOffsetActor, ScheduleOffsetError, ScheduleOffsetErrorCode,
    ScheduleOffsetRequest, ROLLOUT_GATE_EXTERNAL_APPLY,
};

use crate::{auth_middleware::AuthenticatedUser, system_log_helper};

use super::get_organization;

const IDEMPOTENCY_KEY_HEADER: &str = "Idempotency-Key";

#[derive(Debug, Deserialize)]
pub struct PreviewPageQuery {
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct ApplyScheduleOffsetRequest {
    pub preview_hash: String,
}

pub async fn preview_schedule_offset(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Json(request): Json<ScheduleOffsetRequest>,
) -> Response {
    let organization = get_organization(&headers);
    if request.organization != organization {
        return error_response(ScheduleOffsetError::new(
            ScheduleOffsetErrorCode::OrganizationForbidden,
            "The request organization must match X-Organization.",
            json!({}),
        ));
    }

    let mode = request.day_mode.as_str();
    tracing::info!(
        component = "ticket_schedule_offsets_api",
        operation = "preview",
        organization,
        actor_type = "user",
        actor_id = %user.user_id,
        mode,
        offset_days = request.offset_days,
        "schedule offset preview request received"
    );
    let actor = user_actor(&user);
    match schedule_offsets::preview_schedule_offset(&pool, request, &actor).await {
        Ok(response) => {
            metrics::counter!(
                "api_ticket_schedule_offset_requests_total",
                "phase" => "preview",
                "outcome" => if response.applicable { "applicable" } else { "not_applicable" },
            )
            .increment(1);
            (StatusCode::OK, Json(response)).into_response()
        }
        Err(error) => {
            record_error_metric("preview", &error);
            log_internal_error(&pool, "preview", &error).await;
            error_response(error)
        }
    }
}

pub async fn get_schedule_offset_preview(
    State(pool): State<Arc<SqlitePool>>,
    headers: HeaderMap,
    Path(operation_id): Path<String>,
    Query(query): Query<PreviewPageQuery>,
) -> Response {
    let organization = get_organization(&headers);
    match schedule_offsets::get_schedule_offset_preview(
        &pool,
        &operation_id,
        query.cursor.as_deref(),
        query.limit,
    )
    .await
    {
        Ok(page) if page.organization == organization => {
            metrics::counter!(
                "api_ticket_schedule_offset_requests_total",
                "phase" => "read",
                "outcome" => "success",
            )
            .increment(1);
            (StatusCode::OK, Json(page)).into_response()
        }
        Ok(_) => {
            let error = preview_not_found(&operation_id);
            record_error_metric("read", &error);
            error_response(error)
        }
        Err(error) => {
            record_error_metric("read", &error);
            log_internal_error(&pool, "read", &error).await;
            error_response(error)
        }
    }
}

pub async fn apply_schedule_offset(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Path(operation_id): Path<String>,
    Json(request): Json<ApplyScheduleOffsetRequest>,
) -> Response {
    let organization = get_organization(&headers);

    // Resolve the persisted preview before disclosing rollout state so an
    // authenticated member cannot probe another organization's operation IDs.
    match schedule_offsets::get_schedule_offset_preview(&pool, &operation_id, None, Some(1)).await {
        Ok(page) if page.organization == organization => {}
        Ok(_) => return error_response(preview_not_found(&operation_id)),
        Err(error) => return error_response(error),
    }

    let external_apply_enabled =
        match schedule_offsets::rollout_gate_enabled(&pool, ROLLOUT_GATE_EXTERNAL_APPLY).await {
            Ok(enabled) => enabled,
            Err(error) => {
                record_error_metric("apply", &error);
                log_internal_error(&pool, "apply_gate", &error).await;
                return error_response(error);
            }
        };
    if !external_apply_enabled {
        metrics::counter!(
            "api_ticket_schedule_offset_requests_total",
            "phase" => "apply",
            "outcome" => "disabled",
        )
        .increment(1);
        tracing::warn!(
            component = "ticket_schedule_offsets_api",
            operation = "apply",
            organization,
            operation_id,
            actor_type = "user",
            actor_id = %user.user_id,
            gate = ROLLOUT_GATE_EXTERNAL_APPLY,
            "schedule offset apply rejected by rollout gate"
        );
        system_log_helper::log_event(
            &pool,
            "warn",
            "ticket_schedule_offsets",
            "Schedule offset apply rejected by disabled rollout gate",
            Some(&format!(
                "operation_id={operation_id};gate={ROLLOUT_GATE_EXTERNAL_APPLY}"
            )),
            Some(&user.user_id),
            None,
        )
        .await;
        return error_response(ScheduleOffsetError::new(
            ScheduleOffsetErrorCode::OperationAlreadyRejected,
            "Schedule-offset apply is disabled until the external_apply rollout gate is enabled.",
            json!({"gate": ROLLOUT_GATE_EXTERNAL_APPLY, "enabled": false}),
        ));
    }

    let idempotency_key = headers
        .get(IDEMPOTENCY_KEY_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let actor = user_actor(&user);
    match schedule_offsets::apply_schedule_offset(
        &pool,
        &operation_id,
        &request.preview_hash,
        idempotency_key,
        &actor,
    )
    .await
    {
        Ok(result) => {
            metrics::counter!(
                "api_ticket_schedule_offset_requests_total",
                "phase" => "apply",
                "outcome" => if result.replayed { "replayed" } else { "applied" },
            )
            .increment(1);
            (StatusCode::OK, Json(result)).into_response()
        }
        Err(error) => {
            record_error_metric("apply", &error);
            log_internal_error(&pool, "apply", &error).await;
            error_response(error)
        }
    }
}

pub(crate) fn error_response(error: ScheduleOffsetError) -> Response {
    let status =
        StatusCode::from_u16(error.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(json!({"ok": false, "error": error}))).into_response()
}

fn preview_not_found(operation_id: &str) -> ScheduleOffsetError {
    ScheduleOffsetError::new(
        ScheduleOffsetErrorCode::PreviewNotFound,
        "The schedule offset preview was not found.",
        json!({"operation_id": operation_id}),
    )
}

fn user_actor(user: &AuthenticatedUser) -> ScheduleOffsetActor {
    ScheduleOffsetActor {
        actor_type: ActorType::User,
        actor_id: user.user_id.clone(),
    }
}

fn record_error_metric(phase: &'static str, error: &ScheduleOffsetError) {
    let outcome = if error.code == ScheduleOffsetErrorCode::ScheduleOffsetInternal {
        "internal_error"
    } else {
        "rejected"
    };
    metrics::counter!(
        "api_ticket_schedule_offset_requests_total",
        "phase" => phase,
        "outcome" => outcome,
    )
    .increment(1);
}

async fn log_internal_error(pool: &Arc<SqlitePool>, operation: &str, error: &ScheduleOffsetError) {
    if error.code != ScheduleOffsetErrorCode::ScheduleOffsetInternal {
        return;
    }
    system_log_helper::log_error(
        pool,
        "ticket_schedule_offsets",
        "Schedule offset API request failed internally",
        Some(&format!(
            "operation={operation};error_code={}",
            error.code.as_str()
        )),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use sqlx::sqlite::SqlitePoolOptions;

    #[test]
    fn typed_error_response_preserves_contract_status() {
        let response = error_response(ScheduleOffsetError::new(
            ScheduleOffsetErrorCode::TicketDateAnchored,
            "anchored",
            json!({}),
        ));
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn page_query_rejects_non_numeric_limit_at_transport_boundary() {
        assert!(serde_urlencoded::from_str::<PreviewPageQuery>("limit=abc").is_err());
    }

    #[tokio::test]
    async fn apply_route_fails_closed_while_external_gate_is_disabled() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect in-memory sqlite");
        schedule_offsets::ensure_schema(&pool)
            .await
            .expect("install schedule schema");
        sqlx::query(
            r#"INSERT INTO ticket_schedule_offset_operations (
                   operation_id, organization, status, request_json, request_hash,
                   preview_hash, dependency_snapshot_json, actor_type, actor_id,
                   changed_count, skipped_anchored_count, skipped_filter_count,
                   conflict_count, created_at, expires_at
               ) VALUES ('SO-TEST', 'org-a', 'previewed', '{}', 'request-hash',
                   'preview-hash', '[]', 'user', 'alex', 1, 0, 0, 0, 1, 4102444800)"#,
        )
        .execute(&pool)
        .await
        .expect("seed preview");

        let mut wrong_org_headers = HeaderMap::new();
        wrong_org_headers.insert("X-Organization", HeaderValue::from_static("org-b"));
        let hidden = get_schedule_offset_preview(
            State(Arc::new(pool.clone())),
            wrong_org_headers,
            Path("SO-TEST".to_string()),
            Query(PreviewPageQuery {
                cursor: None,
                limit: Some(1),
            }),
        )
        .await;
        assert_eq!(hidden.status(), StatusCode::NOT_FOUND);

        let mut headers = HeaderMap::new();
        headers.insert("X-Organization", HeaderValue::from_static("org-a"));
        headers.insert(
            IDEMPOTENCY_KEY_HEADER,
            HeaderValue::from_static("request-1"),
        );
        let response = apply_schedule_offset(
            State(Arc::new(pool.clone())),
            Extension(AuthenticatedUser {
                user_id: "alex".to_string(),
            }),
            headers,
            Path("SO-TEST".to_string()),
            Json(ApplyScheduleOffsetRequest {
                preview_hash: "preview-hash".to_string(),
            }),
        )
        .await;

        assert_eq!(response.status(), StatusCode::CONFLICT);
        let status: String = sqlx::query_scalar(
            "SELECT status FROM ticket_schedule_offset_operations WHERE operation_id = 'SO-TEST'",
        )
        .fetch_one(&pool)
        .await
        .expect("read operation status");
        assert_eq!(status, "previewed");
        assert!(
            !schedule_offsets::rollout_gate_enabled(&pool, ROLLOUT_GATE_EXTERNAL_APPLY)
                .await
                .expect("read gate")
        );
    }
}
