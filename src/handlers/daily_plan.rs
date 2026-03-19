//! Daily plan REST API handlers

use axum::{
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    Json,
    response::sse::{Event, KeepAlive, Sse},
};
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use crate::auth_middleware::AuthenticatedUser;
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
    Extension(user): Extension<AuthenticatedUser>,
    Query(query): Query<DateQuery>,
) -> Result<Json<DailyPlanView>, (StatusCode, String)> {
    let date = query.date.unwrap_or_else(|| chrono::Utc::now().format("%Y-%m-%d").to_string());

    let plan = ticketing_system::daily_plan::get_plan_for_date(&db, &user.user_id, &date)
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
    Extension(user): Extension<AuthenticatedUser>,
    Json(req): Json<ToggleRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let toggle_req = ToggleDailyPlanItemRequest {
        user_id: user.user_id,
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
    Extension(user): Extension<AuthenticatedUser>,
    Json(mut req): Json<CreateDailyPlanItemRequest>,
) -> Result<Json<DailyPlanItem>, (StatusCode, String)> {
    req.user_id = user.user_id;
    let item = ticketing_system::daily_plan::create_item(&db, req)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(item))
}

/// POST /api/daily-plan/date-items
pub async fn create_daily_plan_date_item(
    State(db): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(mut req): Json<CreateDailyPlanDateItemRequest>,
) -> Result<Json<DailyPlanDateItem>, (StatusCode, String)> {
    req.user_id = user.user_id;
    let item = ticketing_system::daily_plan::create_date_item(&db, req)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(item))
}

/// PATCH /api/daily-plan/items/:item_id
pub async fn update_daily_plan_item(
    Path(item_id): Path<String>,
    State(db): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(req): Json<UpdateDailyPlanItemRequest>,
) -> Result<Json<DailyPlanItem>, (StatusCode, String)> {
    let item = ticketing_system::daily_plan::update_item(&db, &user.user_id, &item_id, req)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Item not found".to_string()))?;

    Ok(Json(item))
}

/// DELETE /api/daily-plan/items/:item_id
pub async fn delete_daily_plan_item(
    Path(item_id): Path<String>,
    State(db): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<StatusCode, (StatusCode, String)> {
    // Try recurring first
    let deleted = ticketing_system::daily_plan::delete_item(&db, &user.user_id, &item_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if deleted {
        return Ok(StatusCode::NO_CONTENT);
    }

    // Try date-specific
    let deleted = ticketing_system::daily_plan::delete_date_item(&db, &user.user_id, &item_id)
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
    Extension(user): Extension<AuthenticatedUser>,
    Query(query): Query<ListItemsQuery>,
) -> Result<Json<Vec<DailyPlanItem>>, (StatusCode, String)> {
    let include_inactive = query.include_inactive.unwrap_or(false);

    let items = ticketing_system::daily_plan::list_items(&db, &user.user_id, include_inactive)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(items))
}

#[derive(Deserialize)]
pub struct ListItemsQuery {
    pub include_inactive: Option<bool>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum DailyPlanStreamEvent {
    Sync { plan: DailyPlanView },
}

/// GET /api/daily-plan/subscribe?date=2026-02-14
/// SSE endpoint for real-time daily plan updates
pub async fn subscribe_daily_plan(
    State(db): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Query(query): Query<DateQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let user_id = user.user_id.clone();
    let stream = async_stream::stream! {
        let mut last_hash: u64 = 0;
        let date = query.date.unwrap_or_else(|| chrono::Utc::now().format("%Y-%m-%d").to_string());

        loop {
            match ticketing_system::daily_plan::get_plan_for_date(&db, &user_id, &date).await {
                Ok(plan) => {
                    use std::hash::{Hash, Hasher};
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    plan.items.len().hash(&mut hasher);
                    for item in &plan.items {
                        item.item_id.hash(&mut hasher);
                        item.title.hash(&mut hasher);
                        item.scheduled_time.hash(&mut hasher);
                        item.checked.hash(&mut hasher);
                        item.sort_order.hash(&mut hasher);
                    }
                    let current_hash = hasher.finish();

                    if current_hash != last_hash {
                        last_hash = current_hash;
                        let event = DailyPlanStreamEvent::Sync { plan };
                        if let Ok(json) = serde_json::to_string(&event) {
                            yield Ok(Event::default().data(json));
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to fetch daily plan for SSE: {}", e);
                }
            }

            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    };

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping")
    )
}
