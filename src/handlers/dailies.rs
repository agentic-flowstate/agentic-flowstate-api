use anyhow::Context;
use async_stream::stream;
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    Extension, Json,
};
use futures::Stream;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::SqlitePool;
use std::collections::HashSet;
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use ticketing_system::daily_action_executions::{
    self, DailyActionCompletionPolicy, DailyActionError, DailyActionErrorCode,
    DailyActionExecutionEvent, DailyActionExecutionSnapshot, DailyActionExecutionStatus,
    DailyOccurrenceAction, DailyOccurrenceWindow,
};

use crate::auth_middleware::AuthenticatedUser;
use crate::dailies_scheduler;
use crate::daily_actions::{self, DailyLaunchError, DailyLaunchRequest};
use crate::package_updates::{self, PackageUpdateScanReport};
use crate::system_log_helper;

use super::runner_capacity;

const IDEMPOTENCY_KEY_HEADER: &str = "Idempotency-Key";
const DEFAULT_WINDOW_DAYS: i64 = 7;
const DEFAULT_EVENT_LIMIT: i64 = 100;
const EVENT_POLL_INTERVAL: Duration = Duration::from_secs(1);

type DailyEventStream = Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>;

#[derive(Debug, Deserialize)]
pub struct RunsQuery {
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct DailyWindowQuery {
    pub start: String,
    pub days: Option<i64>,
    pub timezone: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchDailyExecutionBody {
    pub retry_of_execution_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DailyEventsQuery {
    pub starting_after: Option<i64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DailyOccurrenceDto {
    pub occurrence_id: String,
    pub daily_id: String,
    pub occurrence_date: String,
    pub action_key: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DailyConversationDto {
    pub id: String,
    pub parent_conversation_id: String,
    pub title: String,
    pub deep_link: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DailyJobDto {
    pub id: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DailyFailureDto {
    pub code: String,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DailyExecutionDto {
    pub execution_id: String,
    pub daily_id: String,
    pub occurrence_id: String,
    pub action_key: String,
    pub attempt: i64,
    pub retry_of_execution_id: Option<String>,
    pub completion_policy: DailyActionCompletionPolicy,
    pub status: DailyActionExecutionStatus,
    pub phase: Option<String>,
    pub conversation: DailyConversationDto,
    pub job: DailyJobDto,
    pub failure: Option<DailyFailureDto>,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub updated_at: i64,
    pub version: i64,
    pub sync_cursor: i64,
}

#[derive(Debug, Serialize)]
pub struct LaunchDailyExecutionResponse {
    #[serde(flatten)]
    pub execution: DailyExecutionDto,
    pub deduplicated: bool,
    pub replayed: bool,
}

#[derive(Debug, Serialize)]
pub struct DailyWindowItemDto {
    pub occurrence: DailyOccurrenceDto,
    pub source_organization: String,
    pub title: String,
    pub description: String,
    pub kind: String,
    pub tags: Vec<String>,
    pub daily_status: String,
    pub quest_points: i64,
    pub action: DailyOccurrenceAction,
    pub latest_execution: Option<DailyExecutionDto>,
}

#[derive(Debug, Serialize)]
pub struct DailyWindowDto {
    pub start_date: String,
    pub days: i64,
    pub items: Vec<DailyWindowItemDto>,
    pub materialized_count: u64,
    pub sync_cursor: i64,
    pub progression: DailyProgressionDto,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DailyProgressionDto {
    pub quest_points: i64,
    pub grant_count: i64,
    pub unlocks: Vec<QuestUnlockStatusDto>,
    pub mount: Option<QuestMountStateDto>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct QuestUnlockCatalogItemDto {
    pub unlock_id: String,
    pub unlock_type: String,
    pub name: String,
    pub description: String,
    pub required_points: i64,
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct QuestUnlockStatusDto {
    pub item: QuestUnlockCatalogItemDto,
    pub unlocked: bool,
    pub unlocked_at: Option<i64>,
    pub equipped: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct QuestMountStateDto {
    pub equipped_unlock_id: String,
    pub equipped_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Serialize)]
pub struct DailyExecutionEventDto {
    pub event_id: i64,
    pub event_type: String,
    pub execution: DailyExecutionDto,
    pub created_at: i64,
}

pub async fn get_daily_window(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Query(query): Query<DailyWindowQuery>,
) -> Response {
    let started = Instant::now();
    if let Err(error) = dailies_scheduler::ensure_package_update_daily(&pool, &user.user_id).await {
        return internal_daily_error(
            &pool,
            &user.user_id,
            "window",
            "Failed to ensure the default package update Daily",
            error,
        )
        .await;
    }

    let days = query.days.unwrap_or(DEFAULT_WINDOW_DAYS);
    match daily_action_executions::materialize_daily_occurrence_window(
        &pool,
        &user.user_id,
        &query.timezone,
        &query.start,
        days,
    )
    .await
    {
        Ok(window) => {
            let progression = match load_daily_progression(&pool, &user.user_id).await {
                Ok(progression) => progression,
                Err(error) => {
                    return internal_daily_error(
                        &pool,
                        &user.user_id,
                        "window_progression",
                        "Failed to load durable Daily progression",
                        error,
                    )
                    .await;
                }
            };
            record_daily_metric("window", "success", started.elapsed());
            (
                StatusCode::OK,
                Json(DailyWindowDto::from_window(window, progression)),
            )
                .into_response()
        }
        Err(error) => {
            record_daily_core_error("window", &error, started.elapsed());
            log_daily_core_error(&pool, &user.user_id, "window", &error).await;
            daily_core_error_response(error)
        }
    }
}

pub async fn equip_daily_mount(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(unlock_id): Path<String>,
) -> Response {
    let started = Instant::now();
    match ticketing_system::quest_economy::equip_quest_mount(&pool, &user.user_id, &unlock_id).await
    {
        Ok(mount) => {
            record_daily_metric("mount_equip", "success", started.elapsed());
            (StatusCode::OK, Json(QuestMountStateDto::from(mount))).into_response()
        }
        Err(error) if error.to_string() == "mount is not unlocked for this account" => {
            record_daily_metric("mount_equip", "rejected", started.elapsed());
            (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": {
                        "code": "mount_locked",
                        "message": "This mount is not unlocked for the account."
                    }
                })),
            )
                .into_response()
        }
        Err(error) => {
            record_daily_metric("mount_equip", "internal_error", started.elapsed());
            internal_daily_error(
                &pool,
                &user.user_id,
                "mount_equip",
                "Failed to equip Daily mount",
                error,
            )
            .await
        }
    }
}

pub async fn create_daily_execution(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Path((daily_id, occurrence_id, action_key)): Path<(String, String, String)>,
    Json(body): Json<LaunchDailyExecutionBody>,
) -> Response {
    let idempotency_key = match required_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let daily = match authorized_daily(&pool, &user.user_id, &daily_id).await {
        Ok(daily) => daily,
        Err(response) => return response,
    };

    launch_scoped_daily(
        &pool,
        &user,
        daily,
        occurrence_id,
        action_key,
        idempotency_key,
        body.retry_of_execution_id,
        "execute",
        "dailies_http",
    )
    .await
}

pub async fn get_daily_execution(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(execution_id): Path<String>,
) -> Response {
    let started = Instant::now();
    match daily_action_executions::get_daily_action_execution(&pool, &user.user_id, &execution_id)
        .await
    {
        Ok(Some(execution)) => {
            record_daily_metric("execution_read", "success", started.elapsed());
            (StatusCode::OK, Json(DailyExecutionDto::from(execution))).into_response()
        }
        Ok(None) => {
            record_daily_metric("execution_read", "not_found", started.elapsed());
            daily_not_found("execution_not_found", "The Daily execution was not found.")
        }
        Err(error) => {
            record_daily_core_error("execution_read", &error, started.elapsed());
            log_daily_core_error(&pool, &user.user_id, "execution_read", &error).await;
            daily_core_error_response(error)
        }
    }
}

pub async fn get_daily_execution_by_idempotency_key(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(key): Path<String>,
) -> Response {
    let started = Instant::now();
    match daily_action_executions::get_daily_action_execution_by_idempotency_key(
        &pool,
        &user.user_id,
        &key,
    )
    .await
    {
        Ok(Some(execution)) => {
            record_daily_metric("idempotency_read", "success", started.elapsed());
            (StatusCode::OK, Json(DailyExecutionDto::from(execution))).into_response()
        }
        Ok(None) => {
            record_daily_metric("idempotency_read", "not_found", started.elapsed());
            daily_not_found("execution_not_found", "The Daily execution was not found.")
        }
        Err(error) => {
            record_daily_core_error("idempotency_read", &error, started.elapsed());
            log_daily_core_error(&pool, &user.user_id, "idempotency_read", &error).await;
            daily_core_error_response(error)
        }
    }
}

pub async fn stream_daily_execution_events(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Query(query): Query<DailyEventsQuery>,
) -> Response {
    let header_cursor = match last_event_id(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let starting_after = query.starting_after.or(header_cursor).unwrap_or(0);
    if starting_after < 0 {
        return daily_bad_request(
            "invalid_cursor",
            "starting_after and Last-Event-ID must be non-negative integers.",
        );
    }

    let initial = match daily_action_executions::list_daily_action_execution_events(
        &pool,
        &user.user_id,
        starting_after,
        DEFAULT_EVENT_LIMIT,
    )
    .await
    {
        Ok(page) => page,
        Err(error) => {
            log_daily_core_error(&pool, &user.user_id, "events", &error).await;
            return daily_core_error_response(error);
        }
    };

    metrics::counter!(
        "api_daily_action_sse_connections_total",
        "outcome" => "opened"
    )
    .increment(1);
    tracing::info!(
        component = "dailies_api",
        operation = "daily_action.sse_opened",
        user_id = %user.user_id,
        starting_after,
        initial_event_count = initial.events.len(),
        sync_cursor = initial.sync_cursor,
        "opened account-global Daily execution event stream"
    );
    let pool = pool.clone();
    let user_id = user.user_id;
    let event_stream = stream! {
        let mut cursor = starting_after;
        if cursor > initial.sync_cursor {
            metrics::counter!(
                "api_daily_action_sse_connections_total",
                "outcome" => "reset"
            )
            .increment(1);
            let data = json!({
                "reason": "cursor_ahead",
                "sync_cursor": initial.sync_cursor,
            });
            yield Ok(Event::default().event("reset_required").data(data.to_string()));
            cursor = initial.sync_cursor;
        } else {
            let initial_count = initial.events.len() as i64;
            for event in initial.events {
                cursor = event.event_id;
                yield Ok(encode_daily_sse_event(event));
            }
            if initial_count < DEFAULT_EVENT_LIMIT {
                cursor = initial.sync_cursor;
            }
            let marker = json!({"sync_cursor": cursor});
            yield Ok(Event::default().event("daily_execution.cursor").data(marker.to_string()));
        }

        let mut interval = tokio::time::interval(EVENT_POLL_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            match daily_action_executions::list_daily_action_execution_events(
                &pool,
                &user_id,
                cursor,
                DEFAULT_EVENT_LIMIT,
            )
            .await
            {
                Ok(page) => {
                    let page_count = page.events.len() as i64;
                    for event in page.events {
                        cursor = event.event_id;
                        yield Ok(encode_daily_sse_event(event));
                    }
                    if page_count < DEFAULT_EVENT_LIMIT {
                        cursor = page.sync_cursor;
                    }
                }
                Err(error) => {
                    tracing::error!(
                        component = "dailies_api",
                        operation = "daily_action.sse_poll_failed",
                        error_code = error.code.as_str(),
                        "Daily execution SSE polling failed"
                    );
                    metrics::counter!(
                        "api_daily_action_sse_connections_total",
                        "outcome" => "poll_failed"
                    )
                    .increment(1);
                    system_log_helper::log_event(
                        &pool,
                        "error",
                        "dailies",
                        "Daily execution event stream failed",
                        Some(&format!("error_code={}", error.code.as_str())),
                        Some(&user_id),
                        None,
                    )
                    .await;
                    yield Ok(Event::default()
                        .event("daily_execution.error")
                        .data(json!({"error": "stream_unavailable"}).to_string()));
                    break;
                }
            }
        }
    };

    Sse::new(Box::pin(event_stream) as DailyEventStream)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response()
}

#[derive(Debug, Serialize)]
pub struct DailyDetail {
    pub daily: ticketing_system::Daily,
    pub runs: Vec<ticketing_system::DailyRun>,
}

#[derive(Debug, Serialize)]
pub struct PackageUpdateReviewDetail {
    pub review: ticketing_system::PackageUpdateReview,
    pub report: PackageUpdateScanReport,
}

#[derive(Debug, Deserialize)]
pub struct PauseDailyRequest {
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ResumeDailyRequest {
    pub run_until: Option<i64>,
    pub next_run_at: Option<i64>,
}

pub async fn list_dailies(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Response {
    if let Err(e) = dailies_scheduler::ensure_package_update_daily(&pool, &user.user_id).await {
        return server_error("Failed to ensure package update Daily", e);
    }

    let memberships =
        match ticketing_system::memberships::list_user_organizations(&pool, &user.user_id).await {
            Ok(memberships) => memberships
                .into_iter()
                .map(|membership| membership.organization)
                .collect::<HashSet<_>>(),
            Err(error) => return server_error("Failed to authorize dailies", error),
        };
    match ticketing_system::dailies::list_dailies(&pool, Some(&user.user_id), None).await {
        Ok(dailies) => {
            let authorized = dailies
                .into_iter()
                .filter(|daily| memberships.contains(&daily.organization))
                .collect::<Vec<_>>();
            (StatusCode::OK, Json(json!(authorized))).into_response()
        }
        Err(e) => server_error("Failed to list dailies", e),
    }
}

pub async fn create_daily(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(mut req): Json<ticketing_system::CreateDailyRequest>,
) -> Response {
    req.user_id = user.user_id;
    match ticketing_system::memberships::check_membership(&pool, &req.user_id, &req.organization)
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            return daily_forbidden(
                "source_organization_forbidden",
                "The source organization is not authorized for this account.",
            )
        }
        Err(error) => return server_error("Failed to authorize source organization", error),
    }
    match ticketing_system::dailies::create_daily(&pool, req).await {
        Ok(daily) => (StatusCode::CREATED, Json(json!(daily))).into_response(),
        Err(e) => bad_request("Failed to create daily", e),
    }
}

pub async fn get_daily(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(daily_id): Path<String>,
    Query(query): Query<RunsQuery>,
) -> Response {
    let daily = match authorized_daily(&pool, &user.user_id, &daily_id).await {
        Ok(daily) => daily,
        Err(response) => return response,
    };

    match ticketing_system::dailies::list_runs(&pool, &daily_id, query.limit.unwrap_or(25)).await {
        Ok(runs) => (StatusCode::OK, Json(json!(DailyDetail { daily, runs }))).into_response(),
        Err(e) => server_error("Failed to list daily runs", e),
    }
}

pub async fn update_daily(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(daily_id): Path<String>,
    Json(req): Json<ticketing_system::UpdateDailyRequest>,
) -> Response {
    if let Err(response) = authorized_daily(&pool, &user.user_id, &daily_id).await {
        return response;
    }
    match ticketing_system::dailies::update_daily_for_user(&pool, &user.user_id, &daily_id, req)
        .await
    {
        Ok(Some(daily)) => (StatusCode::OK, Json(json!(daily))).into_response(),
        Ok(None) => not_found("Daily not found"),
        Err(e) => bad_request("Failed to update daily", e),
    }
}

pub async fn pause_daily(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(daily_id): Path<String>,
    Json(req): Json<PauseDailyRequest>,
) -> Response {
    if let Err(response) = authorized_daily(&pool, &user.user_id, &daily_id).await {
        return response;
    }
    let reason = req.reason.unwrap_or_else(|| "Paused by user".to_string());
    match ticketing_system::dailies::pause_daily_for_user(&pool, &user.user_id, &daily_id, &reason)
        .await
    {
        Ok(Some(daily)) => (StatusCode::OK, Json(json!(daily))).into_response(),
        Ok(None) => not_found("Daily not found"),
        Err(e) => server_error("Failed to pause daily", e),
    }
}

pub async fn resume_daily(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(daily_id): Path<String>,
    Json(req): Json<ResumeDailyRequest>,
) -> Response {
    if let Err(response) = authorized_daily(&pool, &user.user_id, &daily_id).await {
        return response;
    }
    match ticketing_system::dailies::resume_daily_for_user(
        &pool,
        &user.user_id,
        &daily_id,
        req.run_until,
        req.next_run_at,
    )
    .await
    {
        Ok(Some(daily)) => (StatusCode::OK, Json(json!(daily))).into_response(),
        Ok(None) => not_found("Daily not found"),
        Err(e) => bad_request("Failed to resume daily", e),
    }
}

pub async fn run_daily_now(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Path(daily_id): Path<String>,
) -> Response {
    let idempotency_key = match required_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let daily = match authorized_daily(&pool, &user.user_id, &daily_id).await {
        Ok(daily) => daily,
        Err(response) => return response,
    };
    let item = match dailies_scheduler::materialize_run_now_occurrence(&pool, &daily).await {
        Ok(Some(item)) => item,
        Ok(None) => {
            return daily_not_found(
                "daily_occurrence_not_found",
                "No scheduled occurrence is available for this Daily.",
            )
        }
        Err(error) => {
            return internal_daily_error(
                &pool,
                &user.user_id,
                "run_now",
                "Failed to materialize the next Daily occurrence",
                error,
            )
            .await
        }
    };

    launch_scoped_daily(
        &pool,
        &user,
        daily,
        item.occurrence.occurrence_id,
        item.action.action_key,
        idempotency_key,
        None,
        "run_now",
        "dailies_run_now",
    )
    .await
}

pub async fn mark_daily_run_read(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path((daily_id, run_id)): Path<(String, String)>,
) -> Response {
    if let Err(response) = authorized_daily(&pool, &user.user_id, &daily_id).await {
        return response;
    }
    match ticketing_system::dailies::get_run(&pool, &run_id).await {
        Ok(Some(run)) if run.daily_id == daily_id => {}
        Ok(_) => return not_found("Daily run not found"),
        Err(error) => return server_error("Failed to fetch daily run", error),
    }
    match ticketing_system::dailies::mark_run_read_for_user(&pool, &user.user_id, &run_id).await {
        Ok(Some(run)) => (StatusCode::OK, Json(json!(run))).into_response(),
        Ok(None) => not_found("Daily run not found"),
        Err(e) => server_error("Failed to mark daily run read", e),
    }
}

pub async fn get_package_update_review(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path((daily_id, run_id)): Path<(String, String)>,
) -> Response {
    if let Err(response) = authorized_daily(&pool, &user.user_id, &daily_id).await {
        return response;
    }
    match user_package_update_review(&pool, &user.user_id, &daily_id, &run_id).await {
        Ok(Some(detail)) => (StatusCode::OK, Json(json!(detail))).into_response(),
        Ok(None) => not_found("Package update review not found"),
        Err(e) => server_error("Failed to fetch package update review", e),
    }
}

pub async fn deny_package_update_review(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path((daily_id, run_id)): Path<(String, String)>,
) -> Response {
    if let Err(response) = authorized_daily(&pool, &user.user_id, &daily_id).await {
        return response;
    }
    let Some(_) = (match user_package_update_review(&pool, &user.user_id, &daily_id, &run_id).await
    {
        Ok(value) => value,
        Err(e) => return server_error("Failed to fetch package update review", e),
    }) else {
        return not_found("Package update review not found");
    };

    match ticketing_system::package_update_reviews::deny_for_user_run(&pool, &user.user_id, &run_id)
        .await
    {
        Ok(Some(review)) => (StatusCode::OK, Json(json!(review))).into_response(),
        Ok(None) => bad_request_text("Package update review is not pending"),
        Err(e) => server_error("Failed to deny package update review", e),
    }
}

pub async fn approve_package_update_review(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path((daily_id, run_id)): Path<(String, String)>,
) -> Response {
    if let Err(response) = authorized_daily(&pool, &user.user_id, &daily_id).await {
        return response;
    }
    let detail = match user_package_update_review(&pool, &user.user_id, &daily_id, &run_id).await {
        Ok(Some(detail)) => detail,
        Ok(None) => return not_found("Package update review not found"),
        Err(e) => return server_error("Failed to fetch package update review", e),
    };

    if detail.review.status != "pending" && detail.review.status != "failed" {
        return bad_request_text("Package update review is not pending");
    }
    match ticketing_system::system_logs::is_admin(&pool, &user.user_id).await {
        Ok(true) => {}
        Ok(false) => {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "Admin access required"})),
            )
                .into_response()
        }
        Err(error) => return server_error("Failed to authorize package update approval", error),
    }

    tracing::warn!(
        component = "dailies_api",
        operation = "daily_action.legacy_package_apply_rejected",
        daily_id,
        run_id,
        user_id = %user.user_id,
        "legacy package apply rejected because it has no durable queue ownership"
    );
    system_log_helper::log_event(
        &pool,
        "warn",
        "dailies",
        "Legacy package apply rejected; durable apply action required",
        Some(&format!("daily_id={daily_id};run_id={run_id}")),
        Some(&user.user_id),
        None,
    )
    .await;
    (
        StatusCode::CONFLICT,
        Json(json!({
            "ok": false,
            "error": {
                "code": "durable_package_apply_required",
                "message": "Package approval must be submitted through a durable Daily apply action.",
                "details": {"review_id": detail.review.review_id}
            }
        })),
    )
        .into_response()
}

async fn authorized_daily(
    pool: &SqlitePool,
    user_id: &str,
    daily_id: &str,
) -> Result<ticketing_system::Daily, Response> {
    match ticketing_system::dailies::get_daily_for_user(pool, user_id, daily_id).await {
        Ok(Some(daily)) => match ticketing_system::memberships::check_membership(
            pool,
            user_id,
            &daily.organization,
        )
        .await
        {
            Ok(true) => Ok(daily),
            Ok(false) => Err(daily_not_found(
                "daily_occurrence_not_found",
                "The Daily was not found.",
            )),
            Err(error) => Err(server_error("Failed to authorize Daily", error)),
        },
        Ok(None) => Err(daily_not_found(
            "daily_occurrence_not_found",
            "The Daily was not found.",
        )),
        Err(error) => Err(server_error("Failed to fetch Daily", error)),
    }
}

async fn launch_scoped_daily(
    pool: &Arc<SqlitePool>,
    user: &AuthenticatedUser,
    daily: ticketing_system::Daily,
    occurrence_id: String,
    action_key: String,
    idempotency_key: String,
    retry_of_execution_id: Option<String>,
    operation: &'static str,
    admission_context: &'static str,
) -> Response {
    let started = Instant::now();
    tracing::info!(
        component = "dailies_api",
        operation = "daily_action.request_received",
        route_operation = operation,
        organization = %daily.organization,
        user_id = %user.user_id,
        daily_id = %daily.daily_id,
        occurrence_id = %occurrence_id,
        action_key = %action_key,
        retry = retry_of_execution_id.is_some(),
        "received authenticated Daily action request"
    );

    let result = daily_actions::launch_daily_action(
        pool,
        &daily,
        DailyLaunchRequest {
            occurrence_id,
            action_key,
            idempotency_key,
            retry_of_execution_id,
            admission_context,
        },
    )
    .await;

    match result {
        Ok(result) => {
            let outcome = if result.created {
                if result.execution.attempt == 1 {
                    "created"
                } else {
                    "retry_created"
                }
            } else if result.replayed {
                "replayed"
            } else {
                "deduplicated"
            };
            record_daily_metric(operation, outcome, started.elapsed());
            tracing::info!(
                component = "dailies_api",
                operation = "daily_action.execution_admitted",
                route_operation = operation,
                execution_id = %result.execution.execution_id,
                conversation_id = %result.execution.conversation.id,
                job_id = %result.execution.job.id,
                created = result.created,
                replayed = result.replayed,
                deduplicated = result.deduplicated,
                "Daily action admission completed"
            );
            let status = launch_http_status(result.created);
            (
                status,
                Json(LaunchDailyExecutionResponse {
                    execution: DailyExecutionDto::from(result.execution),
                    deduplicated: result.deduplicated,
                    replayed: result.replayed,
                }),
            )
                .into_response()
        }
        Err(DailyLaunchError::QueueRejected(admission)) => {
            record_daily_metric(operation, "queue_rejected", started.elapsed());
            runner_capacity::queue_admission_rejection_response(admission)
        }
        Err(DailyLaunchError::PermissionDenied) => {
            record_daily_metric(operation, "permission_denied", started.elapsed());
            tracing::warn!(
                component = "dailies_api",
                operation = "daily_action.permission_denied",
                route_operation = operation,
                organization = %daily.organization,
                user_id = %user.user_id,
                daily_id = %daily.daily_id,
                "privileged Daily action permission denied"
            );
            system_log_helper::log_event(
                pool,
                "warn",
                "dailies",
                "Privileged Daily action permission denied",
                Some(&format!(
                    "operation={operation};daily_id={}",
                    daily.daily_id
                )),
                Some(&user.user_id),
                None,
            )
            .await;
            (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "ok": false,
                    "error": {
                        "code": "permission_denied",
                        "message": "Admin access is required for this Daily action.",
                        "details": {},
                    }
                })),
            )
                .into_response()
        }
        Err(DailyLaunchError::UnsupportedAgent(agent)) => {
            record_daily_metric(operation, "invalid_configuration", started.elapsed());
            tracing::error!(
                component = "dailies_api",
                operation = "daily_action.unsupported_agent",
                daily_id = %daily.daily_id,
                agent,
                "Daily uses an unsupported stored agent"
            );
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({
                    "ok": false,
                    "error": {
                        "code": "unsupported_daily_agent",
                        "message": "The Daily has no supported server-side agent policy.",
                        "details": {},
                    }
                })),
            )
                .into_response()
        }
        Err(DailyLaunchError::Core(error)) => {
            record_daily_core_error(operation, &error, started.elapsed());
            log_daily_core_error(pool, &user.user_id, operation, &error).await;
            daily_core_error_response(error)
        }
        Err(DailyLaunchError::Internal(error)) => {
            record_daily_metric(operation, "internal_error", started.elapsed());
            internal_daily_error(
                pool,
                &user.user_id,
                operation,
                "Failed to prepare durable Daily action",
                error,
            )
            .await
        }
    }
}

fn launch_http_status(created: bool) -> StatusCode {
    if created {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    }
}

fn required_idempotency_key(headers: &HeaderMap) -> Result<String, Response> {
    let value = headers
        .get(IDEMPOTENCY_KEY_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if value.is_empty() {
        return Err(daily_bad_request(
            "invalid_request",
            "Idempotency-Key is required.",
        ));
    }
    Ok(value.to_string())
}

fn last_event_id(headers: &HeaderMap) -> Result<Option<i64>, Response> {
    let Some(value) = headers.get("Last-Event-ID") else {
        return Ok(None);
    };
    let value = value.to_str().map_err(|_| {
        daily_bad_request(
            "invalid_cursor",
            "Last-Event-ID must be a non-negative integer.",
        )
    })?;
    let cursor = value.parse::<i64>().map_err(|_| {
        daily_bad_request(
            "invalid_cursor",
            "Last-Event-ID must be a non-negative integer.",
        )
    })?;
    Ok(Some(cursor))
}

fn encode_daily_sse_event(event: DailyActionExecutionEvent) -> Event {
    let event_id = event.event_id;
    let event_type = event.event_type.clone();
    let data = serde_json::to_string(&DailyExecutionEventDto::from(event))
        .expect("Daily execution event DTO serialization must be infallible");
    Event::default()
        .id(event_id.to_string())
        .event(event_type)
        .data(data)
}

fn daily_core_error_response(error: DailyActionError) -> Response {
    let status =
        StatusCode::from_u16(error.code.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(json!({"ok": false, "error": error}))).into_response()
}

fn daily_not_found(code: &'static str, message: &'static str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "ok": false,
            "error": {"code": code, "message": message, "details": {}}
        })),
    )
        .into_response()
}

fn daily_bad_request(code: &'static str, message: &'static str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "ok": false,
            "error": {"code": code, "message": message, "details": {}}
        })),
    )
        .into_response()
}

fn daily_forbidden(code: &'static str, message: &'static str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "ok": false,
            "error": {"code": code, "message": message, "details": {}}
        })),
    )
        .into_response()
}

fn record_daily_metric(operation: &'static str, outcome: &'static str, elapsed: Duration) {
    metrics::counter!(
        "api_daily_action_requests_total",
        "operation" => operation,
        "outcome" => outcome,
    )
    .increment(1);
    metrics::histogram!(
        "api_daily_action_request_duration_seconds",
        "operation" => operation,
        "outcome" => outcome,
    )
    .record(elapsed.as_secs_f64());
}

fn record_daily_core_error(operation: &'static str, error: &DailyActionError, elapsed: Duration) {
    let outcome = if error.code == DailyActionErrorCode::Internal {
        "internal_error"
    } else {
        "rejected"
    };
    record_daily_metric(operation, outcome, elapsed);
}

async fn log_daily_core_error(
    pool: &Arc<SqlitePool>,
    user_id: &str,
    operation: &'static str,
    error: &DailyActionError,
) {
    if error.code != DailyActionErrorCode::Internal {
        return;
    }
    tracing::error!(
        component = "dailies_api",
        operation = "daily_action.internal_error",
        route_operation = operation,
        error_code = error.code.as_str(),
        "Daily action core request failed internally"
    );
    system_log_helper::log_event(
        pool,
        "error",
        "dailies",
        "Daily action API request failed internally",
        Some(&format!(
            "operation={operation};error_code={}",
            error.code.as_str()
        )),
        Some(user_id),
        None,
    )
    .await;
}

async fn internal_daily_error(
    pool: &Arc<SqlitePool>,
    user_id: &str,
    operation: &'static str,
    message: &'static str,
    error: impl std::fmt::Display,
) -> Response {
    let detail = error.to_string();
    tracing::error!(
        component = "dailies_api",
        operation = "daily_action.internal_error",
        route_operation = operation,
        error = %detail,
        "{message}"
    );
    system_log_helper::log_event(
        pool,
        "error",
        "dailies",
        message,
        Some(&format!("operation={operation};error={detail}")),
        Some(user_id),
        None,
    )
    .await;
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({
            "ok": false,
            "error": {
                "code": "daily_action_internal",
                "message": "The Daily action could not be processed.",
                "details": {},
            }
        })),
    )
        .into_response()
}

impl From<DailyActionExecutionSnapshot> for DailyExecutionDto {
    fn from(value: DailyActionExecutionSnapshot) -> Self {
        Self {
            execution_id: value.execution_id,
            daily_id: value.daily_id,
            occurrence_id: value.occurrence_id,
            action_key: value.action_key,
            attempt: value.attempt,
            retry_of_execution_id: value.retry_of_execution_id,
            completion_policy: value.completion_policy,
            status: value.status,
            phase: value.phase,
            conversation: DailyConversationDto {
                id: value.conversation.id,
                parent_conversation_id: value.conversation.parent_conversation_id,
                title: value.conversation.title,
                deep_link: value.conversation.deep_link,
            },
            job: DailyJobDto {
                id: value.job.id,
                status: value.job.status,
            },
            failure: value.failure.map(|failure| DailyFailureDto {
                code: failure.code,
                message: failure.message,
            }),
            created_at: value.created_at,
            started_at: value.started_at,
            completed_at: value.completed_at,
            updated_at: value.updated_at,
            version: value.version,
            sync_cursor: value.sync_cursor,
        }
    }
}

impl DailyWindowDto {
    fn from_window(value: DailyOccurrenceWindow, progression: DailyProgressionDto) -> Self {
        Self {
            start_date: value.start_date,
            days: value.days,
            items: value
                .items
                .into_iter()
                .map(|item| {
                    let source_organization = item.occurrence.organization;
                    DailyWindowItemDto {
                        occurrence: DailyOccurrenceDto {
                            occurrence_id: item.occurrence.occurrence_id,
                            daily_id: item.occurrence.daily_id,
                            occurrence_date: item.occurrence.occurrence_date,
                            action_key: item.occurrence.action_key,
                        },
                        source_organization,
                        title: item.title,
                        description: item.description,
                        kind: item.kind,
                        tags: item.tags,
                        daily_status: item.daily_status,
                        quest_points: item.quest_points,
                        action: item.action,
                        latest_execution: item.latest_execution.map(DailyExecutionDto::from),
                    }
                })
                .collect(),
            materialized_count: value.materialized_count,
            sync_cursor: value.sync_cursor,
            progression,
        }
    }
}

impl TryFrom<ticketing_system::quest_economy::QuestUnlockStatus> for QuestUnlockStatusDto {
    type Error = anyhow::Error;

    fn try_from(
        value: ticketing_system::quest_economy::QuestUnlockStatus,
    ) -> Result<Self, Self::Error> {
        let item = value.item;
        Ok(Self {
            item: QuestUnlockCatalogItemDto {
                unlock_id: item.unlock_id,
                unlock_type: item.unlock_type,
                name: item.name,
                description: item.description,
                required_points: item.required_points,
                metadata: serde_json::from_str(&item.metadata_json)
                    .context("decode quest unlock metadata")?,
            },
            unlocked: value.unlocked,
            unlocked_at: value.unlocked_at,
            equipped: value.equipped,
        })
    }
}

impl From<ticketing_system::quest_economy::QuestMountState> for QuestMountStateDto {
    fn from(value: ticketing_system::quest_economy::QuestMountState) -> Self {
        Self {
            equipped_unlock_id: value.equipped_unlock_id,
            equipped_at: value.equipped_at,
            updated_at: value.updated_at,
        }
    }
}

async fn load_daily_progression(
    pool: &SqlitePool,
    user_id: &str,
) -> anyhow::Result<DailyProgressionDto> {
    let (balance, unlocks, mount) = tokio::try_join!(
        ticketing_system::quest_economy::get_quest_point_balance(pool, user_id),
        ticketing_system::quest_economy::list_quest_unlock_status(pool, user_id),
        ticketing_system::quest_economy::get_quest_mount_state(pool, user_id),
    )?;
    Ok(DailyProgressionDto {
        quest_points: balance.balance,
        grant_count: balance.grant_count,
        unlocks: unlocks
            .into_iter()
            .map(QuestUnlockStatusDto::try_from)
            .collect::<anyhow::Result<Vec<_>>>()?,
        mount: mount.map(QuestMountStateDto::from),
    })
}

impl From<DailyActionExecutionEvent> for DailyExecutionEventDto {
    fn from(value: DailyActionExecutionEvent) -> Self {
        Self {
            event_id: value.event_id,
            event_type: value.event_type,
            execution: DailyExecutionDto::from(value.execution),
            created_at: value.created_at,
        }
    }
}

async fn user_package_update_review(
    pool: &SqlitePool,
    user_id: &str,
    daily_id: &str,
    run_id: &str,
) -> anyhow::Result<Option<PackageUpdateReviewDetail>> {
    let Some(review) =
        ticketing_system::package_update_reviews::get_review_for_user_run(pool, user_id, run_id)
            .await?
    else {
        return Ok(None);
    };
    if review.daily_id != daily_id {
        return Ok(None);
    }

    let report = package_updates::parse_report(&review.scanner_report_json)?;
    Ok(Some(PackageUpdateReviewDetail { review, report }))
}

fn not_found(message: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": message.to_string() })),
    )
        .into_response()
}

fn bad_request(context: &str, error: anyhow::Error) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": format!("{context}: {error}") })),
    )
        .into_response()
}

fn bad_request_text(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": message.to_string() })),
    )
        .into_response()
}

fn server_error(context: &str, error: anyhow::Error) -> Response {
    tracing::error!("{context}: {error:?}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": context.to_string() })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::{HeaderValue, Request};
    use axum::routing::get;
    use axum::Router;
    use futures::StreamExt;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::path::PathBuf;
    use std::str::FromStr;
    use tower::ServiceExt;

    #[test]
    fn launch_transport_uses_202_only_for_new_execution() {
        assert_eq!(launch_http_status(true), StatusCode::ACCEPTED);
        assert_eq!(launch_http_status(false), StatusCode::OK);
    }

    #[test]
    fn launch_body_rejects_client_controlled_execution_policy() {
        let parsed = serde_json::from_value::<LaunchDailyExecutionBody>(json!({
            "prompt_name": "full-access",
            "working_dir": "/tmp",
            "agent": "full-access"
        }));
        assert!(parsed.is_err());
        assert!(serde_json::from_value::<LaunchDailyExecutionBody>(json!({})).is_ok());
        assert!(serde_json::from_value::<LaunchDailyExecutionBody>(json!({
            "retry_of_execution_id": "DX-FAILED"
        }))
        .is_ok());
    }

    #[test]
    fn idempotency_and_event_cursor_headers_are_strict() {
        let empty = HeaderMap::new();
        assert_eq!(
            required_idempotency_key(&empty).unwrap_err().status(),
            StatusCode::BAD_REQUEST
        );

        let mut headers = HeaderMap::new();
        headers.insert(
            IDEMPOTENCY_KEY_HEADER,
            HeaderValue::from_static("request-1"),
        );
        headers.insert("Last-Event-ID", HeaderValue::from_static("42"));
        assert_eq!(required_idempotency_key(&headers).unwrap(), "request-1");
        assert_eq!(last_event_id(&headers).unwrap(), Some(42));

        headers.insert("Last-Event-ID", HeaderValue::from_static("not-a-number"));
        assert_eq!(
            last_event_id(&headers).unwrap_err().status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn native_execution_dto_omits_internal_identity_and_payload_fields() {
        let dto = DailyExecutionDto {
            execution_id: "DX-1".to_string(),
            daily_id: "DLY-1".to_string(),
            occurrence_id: "DO-1".to_string(),
            action_key: "run".to_string(),
            attempt: 1,
            retry_of_execution_id: None,
            completion_policy: DailyActionCompletionPolicy::JobTerminal,
            status: DailyActionExecutionStatus::Queued,
            phase: Some("waiting_for_runner".to_string()),
            conversation: DailyConversationDto {
                id: "conversation-1".to_string(),
                parent_conversation_id: "conversation-root".to_string(),
                title: "Daily".to_string(),
                deep_link: "agenticflowstate://conversation/conversation-1".to_string(),
            },
            job: DailyJobDto {
                id: "job-1".to_string(),
                status: "pending".to_string(),
            },
            failure: None,
            created_at: 1,
            started_at: None,
            completed_at: None,
            updated_at: 1,
            version: 1,
            sync_cursor: 1,
        };
        let value = serde_json::to_value(dto).expect("serialize native Daily DTO");
        assert_eq!(value["status"], "queued");
        assert_eq!(value["conversation"]["id"], "conversation-1");
        assert!(value.get("user_id").is_none());
        assert!(value.get("organization").is_none());
        assert!(value.get("idempotency_key").is_none());
        assert!(value.get("request_fingerprint").is_none());
        assert!(value.get("checkpoint").is_none());
    }

    #[tokio::test]
    async fn daily_identity_authorizes_its_stored_source_organization() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect in-memory database");
        sqlx::query(
            r#"
            CREATE TABLE dailies (
                daily_id TEXT PRIMARY KEY,
                user_id TEXT NOT NULL,
                organization TEXT NOT NULL,
                title TEXT NOT NULL,
                description TEXT NOT NULL,
                kind TEXT NOT NULL,
                tags TEXT NOT NULL,
                status TEXT NOT NULL,
                cadence_unit TEXT NOT NULL,
                cadence_interval INTEGER NOT NULL,
                run_until INTEGER,
                next_run_at INTEGER,
                last_run_at INTEGER,
                unread_pause_threshold INTEGER NOT NULL,
                consecutive_unread_runs INTEGER NOT NULL,
                agent_type TEXT NOT NULL,
                prompt TEXT NOT NULL,
                search_query TEXT NOT NULL,
                max_age_hours INTEGER,
                pause_reason TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            )
            "#,
        )
        .execute(&pool)
        .await
        .expect("create dailies table");
        sqlx::query(
            r#"
            CREATE TABLE organization_memberships (
                user_id TEXT NOT NULL,
                organization TEXT NOT NULL,
                role TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                PRIMARY KEY (user_id, organization)
            )
            "#,
        )
        .execute(&pool)
        .await
        .expect("create membership table");
        sqlx::query(
            r#"
            INSERT INTO dailies (
                daily_id, user_id, organization, title, description, kind, tags,
                status, cadence_unit, cadence_interval, run_until, next_run_at,
                last_run_at, unread_pause_threshold, consecutive_unread_runs,
                agent_type, prompt, search_query, max_age_hours, pause_reason,
                created_at, updated_at
            ) VALUES (
                'DLY-1', 'alex', 'org-a', 'Daily', '', 'research', '[]',
                'active', 'day', 1, NULL, 1, NULL, 0, 0,
                'daily-research', 'stored', 'query', NULL, NULL, 1, 1
            )
            "#,
        )
        .execute(&pool)
        .await
        .expect("seed Daily");
        sqlx::query("INSERT INTO organization_memberships VALUES ('alex', 'org-a', 'owner', 1, 1)")
            .execute(&pool)
            .await
            .expect("seed membership");

        assert!(authorized_daily(&pool, "alex", "DLY-1").await.is_ok());
        sqlx::query("DELETE FROM organization_memberships WHERE user_id = 'alex'")
            .execute(&pool)
            .await
            .expect("revoke membership");
        let hidden = authorized_daily(&pool, "alex", "DLY-1").await.unwrap_err();
        assert_eq!(hidden.status(), StatusCode::NOT_FOUND);
        let other_user = authorized_daily(&pool, "other", "DLY-1").await.unwrap_err();
        assert_eq!(other_user.status(), StatusCode::NOT_FOUND);
    }

    async fn durable_daily_test_pool() -> SqlitePool {
        let url = format!(
            "file:api-daily-actions-test-{}?mode=memory&cache=shared",
            uuid::Uuid::new_v4()
        );
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(
                SqliteConnectOptions::from_str(&url)
                    .expect("parse in-memory SQLite URL")
                    .create_if_missing(true)
                    .foreign_keys(true)
                    .busy_timeout(Duration::from_secs(5)),
            )
            .await
            .expect("connect shared in-memory database");

        let schema_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../agentic-flowstate-ticketing-system/src/schema.sql");
        let schema = std::fs::read_to_string(&schema_path)
            .unwrap_or_else(|error| panic!("read {}: {error}", schema_path.display()))
            .lines()
            .filter(|line| !line.trim().starts_with("--"))
            .collect::<Vec<_>>()
            .join("\n");
        for statement in schema.split(';') {
            let statement = statement.trim();
            if statement.is_empty() {
                continue;
            }
            sqlx::query(statement)
                .execute(&pool)
                .await
                .unwrap_or_else(|error| panic!("execute schema statement: {error}: {statement}"));
            if statement.starts_with("CREATE TABLE IF NOT EXISTS conversations (") {
                sqlx::query(
                    "ALTER TABLE conversations ADD COLUMN status TEXT NOT NULL DEFAULT 'open'",
                )
                .execute(&pool)
                .await
                .expect("add historical conversation status fixup");
            }
        }
        daily_action_executions::ensure_schema(&pool)
            .await
            .expect("install durable Daily schema");
        ticketing_system::runner_capacity::ensure_schema_and_seed_policy(&pool)
            .await
            .expect("install runner capacity schema");

        let anchor = "2026-07-13T09:00:00-05:00"
            .parse::<chrono::DateTime<chrono::FixedOffset>>()
            .expect("parse Daily anchor")
            .timestamp();
        sqlx::query(
            r#"
            INSERT INTO organizations (name, description, timezone, created_at, updated_at)
            VALUES ('org-a', NULL, NULL, 1, 1)
            "#,
        )
        .execute(&pool)
        .await
        .expect("seed organization");
        sqlx::query(
            r#"
            INSERT INTO users (user_id, name, created_at, updated_at)
            VALUES ('alex', 'Alex', '1', '1')
            "#,
        )
        .execute(&pool)
        .await
        .expect("seed user");
        sqlx::query(
            r#"
            INSERT INTO organization_memberships (
                user_id, organization, role, created_at, updated_at
            ) VALUES ('alex', 'org-a', 'owner', 1, 1)
            "#,
        )
        .execute(&pool)
        .await
        .expect("seed source organization membership");
        sqlx::query(
            r#"
            INSERT INTO dailies (
                daily_id, user_id, organization, title, description, kind, tags,
                status, cadence_unit, cadence_interval, next_run_at,
                unread_pause_threshold, consecutive_unread_runs, agent_type,
                prompt, search_query, max_age_hours, created_at, updated_at
            ) VALUES (
                'DLY-TEST', 'alex', 'org-a', 'Daily Test', 'Test durable Daily',
                'research', '["core"]', 'active', 'day', 1, ?, 0, 0,
                'daily-research', 'Run the stored research action.', 'stored query',
                24, ?, ?
            )
            "#,
        )
        .bind(anchor)
        .bind(anchor)
        .bind(anchor)
        .execute(&pool)
        .await
        .expect("seed Daily");
        sqlx::query(
            r#"
            INSERT INTO agent_runner_generations (
                generation_id, runner_kind, version_hash, pid, status,
                active_turn_count, started_at, last_heartbeat_at
            ) VALUES ('gen-api-test', 'agent-runner', 'test', 1, 'accepting', 0, ?, ?)
            "#,
        )
        .bind(chrono::Utc::now().timestamp())
        .bind(chrono::Utc::now().timestamp())
        .execute(&pool)
        .await
        .expect("seed accepting runner generation");
        pool
    }

    fn daily_headers(idempotency_key: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            IDEMPOTENCY_KEY_HEADER,
            HeaderValue::from_str(idempotency_key).expect("valid idempotency header"),
        );
        headers
    }

    async fn response_json(response: Response) -> serde_json::Value {
        let body = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("read response body");
        serde_json::from_slice(&body).expect("decode JSON response")
    }

    async fn execute_for_occurrence(pool: &SqlitePool, occurrence_id: &str, key: &str) -> Response {
        create_daily_execution(
            State(Arc::new(pool.clone())),
            Extension(AuthenticatedUser {
                user_id: "alex".to_string(),
            }),
            daily_headers(key),
            Path((
                "DLY-TEST".to_string(),
                occurrence_id.to_string(),
                "run".to_string(),
            )),
            Json(LaunchDailyExecutionBody {
                retry_of_execution_id: None,
            }),
        )
        .await
    }

    #[tokio::test]
    async fn window_route_needs_timezone_but_never_an_organization_header() {
        let pool = durable_daily_test_pool().await;
        let app = Router::new()
            .route("/api/dailies/window", get(get_daily_window))
            .with_state(Arc::new(pool.clone()))
            .layer(Extension(AuthenticatedUser {
                user_id: "alex".to_string(),
            }));

        let missing_timezone = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/dailies/window?start=2026-07-13&days=7")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing_timezone.status(), StatusCode::BAD_REQUEST);

        let invalid_timezone = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/dailies/window?start=2026-07-13&days=7&timezone=UTC-5")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(invalid_timezone.status(), StatusCode::BAD_REQUEST);
        let invalid_body = response_json(invalid_timezone).await;
        assert_eq!(invalid_body["error"]["code"], "occurrence_timezone_invalid");

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/dailies/window?start=2026-07-13&days=7&timezone=America%2FBogota")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(body["items"][0]["source_organization"], "org-a");
        assert_eq!(body["items"][0]["quest_points"], 100);
        assert_eq!(body["progression"]["quest_points"], 0);
        assert_eq!(body["progression"]["grant_count"], 0);
        assert_eq!(body["progression"]["unlocks"].as_array().unwrap().len(), 7);
        assert!(body["progression"]["mount"].is_null());
        let persisted: Option<String> =
            sqlx::query_scalar("SELECT occurrence_timezone FROM users WHERE user_id = 'alex'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(persisted.as_deref(), Some("America/Bogota"));
    }

    #[tokio::test]
    async fn durable_route_contract_covers_replay_concurrency_rollback_and_job_transitions() {
        let pool = durable_daily_test_pool().await;
        let window = daily_action_executions::materialize_daily_occurrence_window(
            &pool,
            "alex",
            "America/Bogota",
            "2026-07-13",
            7,
        )
        .await
        .expect("materialize deterministic week");
        assert_eq!(window.items.len(), 7);
        let occurrence_ids = window
            .items
            .iter()
            .map(|item| item.occurrence.occurrence_id.clone())
            .collect::<Vec<_>>();

        let created = execute_for_occurrence(&pool, &occurrence_ids[0], "request-created").await;
        assert_eq!(created.status(), StatusCode::ACCEPTED);
        let created_json = response_json(created).await;
        assert_eq!(created_json["deduplicated"], false);
        assert_eq!(created_json["replayed"], false);
        assert_eq!(created_json["status"], "queued");
        let execution_id = created_json["execution_id"]
            .as_str()
            .expect("execution id")
            .to_string();
        let job_id = created_json["job"]["id"]
            .as_str()
            .expect("job id")
            .to_string();
        let conversation_id = created_json["conversation"]["id"]
            .as_str()
            .expect("conversation id")
            .to_string();

        let replay = execute_for_occurrence(&pool, &occurrence_ids[0], "request-created").await;
        assert_eq!(replay.status(), StatusCode::OK);
        let replay_json = response_json(replay).await;
        assert_eq!(replay_json["execution_id"], execution_id);
        assert_eq!(replay_json["job"]["id"], job_id);
        assert_eq!(replay_json["conversation"]["id"], conversation_id);
        assert_eq!(replay_json["replayed"], true);

        let reused_for_other_request =
            execute_for_occurrence(&pool, &occurrence_ids[1], "request-created").await;
        assert_eq!(reused_for_other_request.status(), StatusCode::CONFLICT);
        let reused_json = response_json(reused_for_other_request).await;
        assert_eq!(reused_json["error"]["code"], "idempotency_key_reused");

        let claimed =
            ticketing_system::conversation_turn_jobs::claim_next_job(&pool, "gen-api-test")
                .await
                .expect("claim queued Daily job")
                .expect("Daily job available");
        assert_eq!(claimed.id, job_id);
        let running =
            daily_action_executions::get_daily_action_execution(&pool, "alex", &execution_id)
                .await
                .expect("read running execution")
                .expect("running execution exists");
        assert_eq!(running.status, DailyActionExecutionStatus::Running);
        assert_eq!(running.phase.as_deref(), Some("agent_running"));

        ticketing_system::conversation_turn_jobs::mark_job_terminal(
            &pool,
            &job_id,
            "completed",
            None,
        )
        .await
        .expect("complete Daily conversation job");
        let completed = get_daily_execution(
            State(Arc::new(pool.clone())),
            Extension(AuthenticatedUser {
                user_id: "alex".to_string(),
            }),
            Path(execution_id.clone()),
        )
        .await;
        assert_eq!(completed.status(), StatusCode::OK);
        let completed_json = response_json(completed).await;
        assert_eq!(completed_json["status"], "completed");
        assert_eq!(completed_json["version"], 3);
        let progression = load_daily_progression(&pool, "alex").await.unwrap();
        assert_eq!(progression.quest_points, 100);
        assert_eq!(progression.grant_count, 1);
        assert!(
            progression
                .unlocks
                .iter()
                .find(|unlock| unlock.item.unlock_id == "sunsteel")
                .unwrap()
                .unlocked
        );
        assert!(
            !progression
                .unlocks
                .iter()
                .find(|unlock| unlock.item.unlock_id == "stormglass")
                .unwrap()
                .unlocked
        );
        let equipped = equip_daily_mount(
            State(Arc::new(pool.clone())),
            Extension(AuthenticatedUser {
                user_id: "alex".to_string(),
            }),
            Path("sunsteel".to_string()),
        )
        .await;
        assert_eq!(equipped.status(), StatusCode::OK);
        let equipped_json = response_json(equipped).await;
        assert_eq!(equipped_json["equipped_unlock_id"], "sunsteel");
        let locked = equip_daily_mount(
            State(Arc::new(pool.clone())),
            Extension(AuthenticatedUser {
                user_id: "alex".to_string(),
            }),
            Path("stormglass".to_string()),
        )
        .await;
        assert_eq!(locked.status(), StatusCode::CONFLICT);
        let by_key = get_daily_execution_by_idempotency_key(
            State(Arc::new(pool.clone())),
            Extension(AuthenticatedUser {
                user_id: "alex".to_string(),
            }),
            Path("request-created".to_string()),
        )
        .await;
        assert_eq!(by_key.status(), StatusCode::OK);
        let by_key_json = response_json(by_key).await;
        assert_eq!(by_key_json["execution_id"], execution_id);
        let events =
            daily_action_executions::list_daily_action_execution_events(&pool, "alex", 0, 100)
                .await
                .expect("read durable Daily events");
        assert_eq!(events.events.len(), 3);
        let event_stream = stream_daily_execution_events(
            State(Arc::new(pool.clone())),
            Extension(AuthenticatedUser {
                user_id: "alex".to_string(),
            }),
            daily_headers("unused-stream-key"),
            Query(DailyEventsQuery {
                starting_after: Some(0),
            }),
        )
        .await;
        assert_eq!(event_stream.status(), StatusCode::OK);
        assert!(event_stream
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/event-stream")));
        let mut frames = event_stream.into_body().into_data_stream();
        let first_frame = tokio::time::timeout(Duration::from_secs(1), frames.next())
            .await
            .expect("initial SSE frame timeout")
            .expect("initial SSE frame")
            .expect("initial SSE frame bytes");
        let first_frame = String::from_utf8(first_frame.to_vec()).expect("UTF-8 SSE frame");
        assert!(first_frame.contains("daily_execution.updated"));
        assert!(first_frame.contains(&execution_id));

        let first = execute_for_occurrence(&pool, &occurrence_ids[1], "concurrent-a");
        let second = execute_for_occurrence(&pool, &occurrence_ids[1], "concurrent-b");
        let (first, second) = tokio::join!(first, second);
        let mut statuses = [first.status(), second.status()];
        statuses.sort();
        assert_eq!(statuses, [StatusCode::OK, StatusCode::ACCEPTED]);
        let execution_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM daily_action_executions WHERE occurrence_id = ?",
        )
        .bind(&occurrence_ids[1])
        .fetch_one(&pool)
        .await
        .expect("count concurrent executions");
        assert_eq!(execution_count, 1);
        let child_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM conversations WHERE conversation_type = 'daily_action'",
        )
        .fetch_one(&pool)
        .await
        .expect("count Daily child conversations");
        assert_eq!(child_count, 2);

        sqlx::query(
            "UPDATE runner_capacity_policy SET max_pending_jobs = 1 WHERE runner_kind = 'agent-runner'",
        )
        .execute(&pool)
        .await
        .expect("saturate queue policy");
        let rejected = execute_for_occurrence(&pool, &occurrence_ids[2], "queue-rejected").await;
        assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS);
        let rejected_execution_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM daily_action_executions WHERE occurrence_id = ?",
        )
        .bind(&occurrence_ids[2])
        .fetch_one(&pool)
        .await
        .expect("count rejected executions");
        assert_eq!(rejected_execution_count, 0);
        let child_count_after_rejection: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM conversations WHERE conversation_type = 'daily_action'",
        )
        .fetch_one(&pool)
        .await
        .expect("count children after queue rejection");
        assert_eq!(child_count_after_rejection, child_count);
    }
}
