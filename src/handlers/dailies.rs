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

fn server_error(context: &str, error: anyhow::Error) -> Response {
    tracing::error!("{context}: {error:?}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": context.to_string() })),
    )
        .into_response()
}
