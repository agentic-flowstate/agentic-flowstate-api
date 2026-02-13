//! Daily plan REST API handlers

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use sqlx::SqlitePool;
use std::sync::Arc;

use ticketing_system::{
    CreateDailyPlanDateItemRequest, CreateDailyPlanItemRequest, DailyPlanItem,
    DailyPlanDateItem, DailyPlanView, ToggleDailyPlanItemRequest,
    UpdateDailyPlanItemRequest,
};

#[derive(Deserialize)]
pub struct DateQuery {
    pub date: Option<String>,
}

/// GET /api/daily-plan?date=2026-02-12
pub async fn get_daily_plan(
    State(db): State<Arc<SqlitePool>>,
    Query(query): Query<DateQuery>,
) -> Result<Json<DailyPlanView>, (StatusCode, String)> {
    let date = query.date.unwrap_or_else(|| chrono::Utc::now().format("%Y-%m-%d").to_string());

    let plan = ticketing_system::daily_plan::get_plan_for_date(&db, &date)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(plan))
}

#[derive(Deserialize)]
pub struct ToggleRequest {
    pub item_id: String,
    pub date: String,
    pub note: Option<String>,
}

/// POST /api/daily-plan/toggle
pub async fn toggle_daily_plan_item(
    State(db): State<Arc<SqlitePool>>,
    Json(req): Json<ToggleRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let toggle_req = ToggleDailyPlanItemRequest {
        item_id: req.item_id.clone(),
        date: req.date.clone(),
        note: req.note,
    };

    let checked = ticketing_system::daily_plan::toggle_item(&db, toggle_req)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(serde_json::json!({
        "item_id": req.item_id,
        "date": req.date,
        "checked": checked,
    })))
}

/// POST /api/daily-plan/items
pub async fn create_daily_plan_item(
    State(db): State<Arc<SqlitePool>>,
    Json(req): Json<CreateDailyPlanItemRequest>,
) -> Result<Json<DailyPlanItem>, (StatusCode, String)> {
    let item = ticketing_system::daily_plan::create_item(&db, req)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(item))
}

/// POST /api/daily-plan/date-items
pub async fn create_daily_plan_date_item(
    State(db): State<Arc<SqlitePool>>,
    Json(req): Json<CreateDailyPlanDateItemRequest>,
) -> Result<Json<DailyPlanDateItem>, (StatusCode, String)> {
    let item = ticketing_system::daily_plan::create_date_item(&db, req)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(item))
}

/// PATCH /api/daily-plan/items/:item_id
pub async fn update_daily_plan_item(
    Path(item_id): Path<String>,
    State(db): State<Arc<SqlitePool>>,
    Json(req): Json<UpdateDailyPlanItemRequest>,
) -> Result<Json<DailyPlanItem>, (StatusCode, String)> {
    let item = ticketing_system::daily_plan::update_item(&db, &item_id, req)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Item not found".to_string()))?;

    Ok(Json(item))
}

/// DELETE /api/daily-plan/items/:item_id
pub async fn delete_daily_plan_item(
    Path(item_id): Path<String>,
    State(db): State<Arc<SqlitePool>>,
) -> Result<StatusCode, (StatusCode, String)> {
    // Try recurring first
    let deleted = ticketing_system::daily_plan::delete_item(&db, &item_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if deleted {
        return Ok(StatusCode::NO_CONTENT);
    }

    // Try date-specific
    let deleted = ticketing_system::daily_plan::delete_date_item(&db, &item_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err((StatusCode::NOT_FOUND, "Item not found".to_string()))
    }
}

/// GET /api/daily-plan/items
pub async fn list_daily_plan_items(
    State(db): State<Arc<SqlitePool>>,
    Query(query): Query<ListItemsQuery>,
) -> Result<Json<Vec<DailyPlanItem>>, (StatusCode, String)> {
    let include_inactive = query.include_inactive.unwrap_or(false);

    let items = ticketing_system::daily_plan::list_items(&db, include_inactive)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(items))
}

#[derive(Deserialize)]
pub struct ListItemsQuery {
    pub include_inactive: Option<bool>,
}
