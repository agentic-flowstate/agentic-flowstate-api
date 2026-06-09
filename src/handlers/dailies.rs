use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Extension, Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::SqlitePool;
use std::sync::Arc;

use crate::auth_middleware::AuthenticatedUser;
use crate::dailies_scheduler;
use crate::package_updates::{self, PackageUpdateScanReport};

const DAILIES_STORAGE_ORGANIZATION: &str = "agentic-flowstate";

#[derive(Debug, Deserialize)]
pub struct RunsQuery {
    pub limit: Option<i64>,
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

    match ticketing_system::dailies::list_dailies(&pool, Some(&user.user_id), None).await {
        Ok(dailies) => (StatusCode::OK, Json(json!(dailies))).into_response(),
        Err(e) => server_error("Failed to list dailies", e),
    }
}

pub async fn create_daily(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(mut req): Json<ticketing_system::CreateDailyRequest>,
) -> Response {
    req.user_id = user.user_id;
    req.organization = DAILIES_STORAGE_ORGANIZATION.to_string();
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
    let daily = match ticketing_system::dailies::get_daily_for_user(&pool, &user.user_id, &daily_id)
        .await
    {
        Ok(Some(daily)) => daily,
        Ok(None) => return not_found("Daily not found"),
        Err(e) => return server_error("Failed to fetch daily", e),
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
    Path(daily_id): Path<String>,
) -> Response {
    let daily = match ticketing_system::dailies::get_daily_for_user(&pool, &user.user_id, &daily_id)
        .await
    {
        Ok(Some(daily)) => daily,
        Ok(None) => return not_found("Daily not found"),
        Err(e) => return server_error("Failed to fetch daily", e),
    };

    match dailies_scheduler::spawn_daily_run(pool.clone(), daily, chrono::Utc::now().timestamp())
        .await
    {
        Ok(run) => (StatusCode::ACCEPTED, Json(json!(run))).into_response(),
        Err(e) => server_error("Failed to start daily run", e),
    }
}

pub async fn mark_daily_run_read(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path((_daily_id, run_id)): Path<(String, String)>,
) -> Response {
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
    let detail = match user_package_update_review(&pool, &user.user_id, &daily_id, &run_id).await {
        Ok(Some(detail)) => detail,
        Ok(None) => return not_found("Package update review not found"),
        Err(e) => return server_error("Failed to fetch package update review", e),
    };

    if detail.review.status != "pending" && detail.review.status != "failed" {
        return bad_request_text("Package update review is not pending");
    }

    let review = match ticketing_system::package_update_reviews::mark_applying_for_user_run(
        &pool,
        &user.user_id,
        &run_id,
    )
    .await
    {
        Ok(Some(review)) => review,
        Ok(None) => return bad_request_text("Package update review is not pending"),
        Err(e) => return server_error("Failed to approve package update review", e),
    };

    let pool_clone = pool.clone();
    let review_id = review.review_id.clone();
    let report = detail.report;
    tokio::spawn(async move {
        match package_updates::apply_updates(&report).await {
            Ok(output) => {
                if let Err(e) = ticketing_system::package_update_reviews::complete_apply(
                    &pool_clone,
                    &review_id,
                    &output,
                )
                .await
                {
                    tracing::error!(
                        "[PACKAGE_UPDATES] failed to mark review {} applied: {}",
                        review_id,
                        e
                    );
                }
            }
            Err(e) => {
                if let Err(update_error) = ticketing_system::package_update_reviews::fail_apply(
                    &pool_clone,
                    &review_id,
                    &e.to_string(),
                )
                .await
                {
                    tracing::error!(
                        "[PACKAGE_UPDATES] failed to mark review {} failed: {}",
                        review_id,
                        update_error
                    );
                }
            }
        }
    });

    (StatusCode::ACCEPTED, Json(json!(review))).into_response()
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
