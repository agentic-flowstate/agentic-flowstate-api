use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{Row, SqlitePool};
use std::sync::Arc;

const MAX_UNITS_PER_SYNC: usize = 20_000;
const MAX_TRACKS_PER_SYNC: usize = 80_000;
const MAX_EVENTS_PER_SYNC: usize = 20_000;

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct LeitnerSyncPayload {
    pub sync_id: String,
    pub device_id: String,
    pub user_id: Option<String>,
    pub app_version: String,
    pub build_number: String,
    pub platform: String,
    pub schema_version: i64,
    pub foundation_checkpoint: String,
    pub trigger: String,
    pub captured_at_unix: i64,
    pub selected_course_id: String,
    pub day_key: String,
    pub settings: LeitnerSettingsPayload,
    pub stats: LeitnerStatsPayload,
    pub units: Vec<LeitnerUnitPayload>,
    pub tracks: Vec<LeitnerTrackPayload>,
    pub study_events: Vec<LeitnerStudyEventPayload>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct LeitnerSettingsPayload {
    pub daily_new_unit_target: i64,
    pub review_limit: i64,
    pub lemma_goal: i64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct LeitnerStatsPayload {
    pub due_prompt_count: i64,
    pub new_unit_slots: i64,
    pub introduced_unit_count: i64,
    pub practiced_today_count: i64,
    pub mastered_lemma_count: i64,
    pub productive_unit_count: i64,
    pub total_lemma_count: i64,
    pub total_unit_count: i64,
    pub average_stability: f64,
    pub average_retrievability: f64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct LeitnerUnitPayload {
    pub unit_id: String,
    pub course_id: String,
    pub category_id: String,
    pub category_title: String,
    pub english: String,
    pub target: String,
    pub pronunciation: Option<String>,
    pub note: Option<String>,
    pub unit_kind: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct LeitnerTrackPayload {
    pub unit_id: String,
    pub course_id: String,
    pub mode: String,
    pub introduced_day: String,
    pub due_day: String,
    pub last_reviewed_day: Option<String>,
    pub stage: i64,
    pub stability: f64,
    pub difficulty: f64,
    pub retrievability: f64,
    pub interval_days: i64,
    pub review_count: i64,
    pub lapse_count: i64,
    pub correct_streak: i64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct LeitnerStudyEventPayload {
    pub event_id: String,
    pub created_at_unix: i64,
    pub day_key: String,
    pub session_id: String,
    pub course_id: String,
    pub unit_id: String,
    pub mode: String,
    pub grade: i64,
    pub was_correct: bool,
    pub stability_after: f64,
    pub difficulty_after: f64,
    pub due_day_after: String,
}

#[derive(Debug, Deserialize)]
pub struct LeitnerProgressQuery {
    pub device_id: Option<String>,
    pub limit: Option<i64>,
}

pub async fn ingest_sync(
    State(pool): State<Arc<SqlitePool>>,
    Json(payload): Json<LeitnerSyncPayload>,
) -> Response {
    if payload.units.len() > MAX_UNITS_PER_SYNC {
        return bad_request("Too many vocabulary units in sync payload");
    }
    if payload.tracks.len() > MAX_TRACKS_PER_SYNC {
        return bad_request("Too many memory tracks in sync payload");
    }
    if payload.study_events.len() > MAX_EVENTS_PER_SYNC {
        return bad_request("Too many study events in sync payload");
    }
    if payload.sync_id.trim().is_empty() || payload.device_id.trim().is_empty() {
        return bad_request("sync_id and device_id are required");
    }

    match persist_sync(&pool, &payload).await {
        Ok(result) => (StatusCode::OK, Json(json!(result))).into_response(),
        Err(e) => {
            tracing::error!("Failed to ingest Leitner sync payload: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Failed to ingest Leitner sync payload"})),
            )
                .into_response()
        }
    }
}

pub async fn latest_progress(
    State(pool): State<Arc<SqlitePool>>,
    Query(query): Query<LeitnerProgressQuery>,
) -> Response {
    if let Err(e) = ensure_schema(&pool).await {
        tracing::error!("Failed to ensure Leitner sync schema: {:?}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "Failed to prepare Leitner progress tables"})),
        )
            .into_response();
    }

    let limit = query.limit.unwrap_or(20).clamp(1, 200);
    let rows = if let Some(device_id) = query.device_id.as_deref() {
        sqlx::query(
            r#"
            SELECT sync_id, device_id, user_id, app_version, build_number, platform,
                   schema_version, foundation_checkpoint, trigger, captured_at_unix,
                   selected_course_id, day_key, due_prompt_count, introduced_unit_count,
                   practiced_today_count, mastered_lemma_count, productive_unit_count,
                   total_lemma_count, total_unit_count, average_stability,
                   average_retrievability, track_count, event_count, unit_count,
                   received_at
            FROM leitner_sync_batches
            WHERE device_id = ?
            ORDER BY received_at DESC
            LIMIT ?
            "#,
        )
        .bind(device_id)
        .bind(limit)
        .fetch_all(pool.as_ref())
        .await
    } else {
        sqlx::query(
            r#"
            SELECT sync_id, device_id, user_id, app_version, build_number, platform,
                   schema_version, foundation_checkpoint, trigger, captured_at_unix,
                   selected_course_id, day_key, due_prompt_count, introduced_unit_count,
                   practiced_today_count, mastered_lemma_count, productive_unit_count,
                   total_lemma_count, total_unit_count, average_stability,
                   average_retrievability, track_count, event_count, unit_count,
                   received_at
            FROM leitner_sync_batches
            ORDER BY received_at DESC
            LIMIT ?
            "#,
        )
        .bind(limit)
        .fetch_all(pool.as_ref())
        .await
    };

    match rows {
        Ok(rows) => {
            let summaries: Vec<_> = rows
                .into_iter()
                .map(|row| {
                    let captured_at_unix: i64 = row.get("captured_at_unix");
                    let received_at: i64 = row.get("received_at");
                    json!({
                        "sync_id": row.get::<String, _>("sync_id"),
                        "device_id": row.get::<String, _>("device_id"),
                        "user_id": row.get::<Option<String>, _>("user_id"),
                        "app_version": row.get::<String, _>("app_version"),
                        "build_number": row.get::<String, _>("build_number"),
                        "platform": row.get::<String, _>("platform"),
                        "schema_version": row.get::<i64, _>("schema_version"),
                        "foundation_checkpoint": row.get::<String, _>("foundation_checkpoint"),
                        "trigger": row.get::<String, _>("trigger"),
                        "captured_at_unix": captured_at_unix,
                        "captured_at_iso": unix_iso(captured_at_unix),
                        "selected_course_id": row.get::<String, _>("selected_course_id"),
                        "day_key": row.get::<String, _>("day_key"),
                        "due_prompt_count": row.get::<i64, _>("due_prompt_count"),
                        "introduced_unit_count": row.get::<i64, _>("introduced_unit_count"),
                        "practiced_today_count": row.get::<i64, _>("practiced_today_count"),
                        "mastered_lemma_count": row.get::<i64, _>("mastered_lemma_count"),
                        "productive_unit_count": row.get::<i64, _>("productive_unit_count"),
                        "total_lemma_count": row.get::<i64, _>("total_lemma_count"),
                        "total_unit_count": row.get::<i64, _>("total_unit_count"),
                        "average_stability": row.get::<f64, _>("average_stability"),
                        "average_retrievability": row.get::<f64, _>("average_retrievability"),
                        "track_count": row.get::<i64, _>("track_count"),
                        "event_count": row.get::<i64, _>("event_count"),
                        "unit_count": row.get::<i64, _>("unit_count"),
                        "received_at": received_at,
                        "received_at_iso": unix_iso(received_at),
                    })
                })
                .collect();
            (StatusCode::OK, Json(json!({ "syncs": summaries }))).into_response()
        }
        Err(e) => {
            tracing::error!("Failed to list Leitner progress: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Failed to list Leitner progress"})),
            )
                .into_response()
        }
    }
}

async fn persist_sync(
    pool: &SqlitePool,
    payload: &LeitnerSyncPayload,
) -> anyhow::Result<serde_json::Value> {
    ensure_schema(pool).await?;

    let now = chrono::Utc::now().timestamp();
    let raw_payload = serde_json::to_string(payload)?;
    let mut tx = pool.begin().await?;

    sqlx::query(
        r#"
        INSERT INTO leitner_sync_devices (
            device_id, user_id, app_version, build_number, platform,
            first_seen_at, last_seen_at, last_sync_id
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(device_id) DO UPDATE SET
            user_id = excluded.user_id,
            app_version = excluded.app_version,
            build_number = excluded.build_number,
            platform = excluded.platform,
            last_seen_at = excluded.last_seen_at,
            last_sync_id = excluded.last_sync_id
        "#,
    )
    .bind(&payload.device_id)
    .bind(&payload.user_id)
    .bind(&payload.app_version)
    .bind(&payload.build_number)
    .bind(&payload.platform)
    .bind(now)
    .bind(now)
    .bind(&payload.sync_id)
    .execute(&mut *tx)
    .await?;

    let batch_result = sqlx::query(
        r#"
        INSERT OR IGNORE INTO leitner_sync_batches (
            sync_id, device_id, user_id, app_version, build_number, platform,
            schema_version, foundation_checkpoint, trigger, captured_at_unix,
            selected_course_id, day_key, daily_new_unit_target, review_limit,
            lemma_goal, due_prompt_count, new_unit_slots, introduced_unit_count,
            practiced_today_count, mastered_lemma_count, productive_unit_count,
            total_lemma_count, total_unit_count, average_stability,
            average_retrievability, track_count, event_count, unit_count,
            payload_json, received_at
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(&payload.sync_id)
    .bind(&payload.device_id)
    .bind(&payload.user_id)
    .bind(&payload.app_version)
    .bind(&payload.build_number)
    .bind(&payload.platform)
    .bind(payload.schema_version)
    .bind(&payload.foundation_checkpoint)
    .bind(&payload.trigger)
    .bind(payload.captured_at_unix)
    .bind(&payload.selected_course_id)
    .bind(&payload.day_key)
    .bind(payload.settings.daily_new_unit_target)
    .bind(payload.settings.review_limit)
    .bind(payload.settings.lemma_goal)
    .bind(payload.stats.due_prompt_count)
    .bind(payload.stats.new_unit_slots)
    .bind(payload.stats.introduced_unit_count)
    .bind(payload.stats.practiced_today_count)
    .bind(payload.stats.mastered_lemma_count)
    .bind(payload.stats.productive_unit_count)
    .bind(payload.stats.total_lemma_count)
    .bind(payload.stats.total_unit_count)
    .bind(payload.stats.average_stability)
    .bind(payload.stats.average_retrievability)
    .bind(payload.tracks.len() as i64)
    .bind(payload.study_events.len() as i64)
    .bind(payload.units.len() as i64)
    .bind(&raw_payload)
    .bind(now)
    .execute(&mut *tx)
    .await?;

    for unit in &payload.units {
        sqlx::query(
            r#"
            INSERT INTO leitner_units (
                course_id, unit_id, category_id, category_title, english, target,
                pronunciation, note, unit_kind, updated_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(course_id, unit_id) DO UPDATE SET
                category_id = excluded.category_id,
                category_title = excluded.category_title,
                english = excluded.english,
                target = excluded.target,
                pronunciation = excluded.pronunciation,
                note = excluded.note,
                unit_kind = excluded.unit_kind,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(&unit.course_id)
        .bind(&unit.unit_id)
        .bind(&unit.category_id)
        .bind(&unit.category_title)
        .bind(&unit.english)
        .bind(&unit.target)
        .bind(&unit.pronunciation)
        .bind(&unit.note)
        .bind(&unit.unit_kind)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    }

    for track in &payload.tracks {
        sqlx::query(
            r#"
            INSERT INTO leitner_memory_tracks (
                device_id, course_id, unit_id, mode, introduced_day, due_day,
                last_reviewed_day, stage, stability, difficulty, retrievability,
                interval_days, review_count, lapse_count, correct_streak, synced_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(device_id, unit_id, mode) DO UPDATE SET
                course_id = excluded.course_id,
                introduced_day = excluded.introduced_day,
                due_day = excluded.due_day,
                last_reviewed_day = excluded.last_reviewed_day,
                stage = excluded.stage,
                stability = excluded.stability,
                difficulty = excluded.difficulty,
                retrievability = excluded.retrievability,
                interval_days = excluded.interval_days,
                review_count = excluded.review_count,
                lapse_count = excluded.lapse_count,
                correct_streak = excluded.correct_streak,
                synced_at = excluded.synced_at
            "#,
        )
        .bind(&payload.device_id)
        .bind(&track.course_id)
        .bind(&track.unit_id)
        .bind(&track.mode)
        .bind(&track.introduced_day)
        .bind(&track.due_day)
        .bind(&track.last_reviewed_day)
        .bind(track.stage)
        .bind(track.stability)
        .bind(track.difficulty)
        .bind(track.retrievability)
        .bind(track.interval_days)
        .bind(track.review_count)
        .bind(track.lapse_count)
        .bind(track.correct_streak)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    }

    let mut accepted_events = 0u64;
    for event in &payload.study_events {
        let result = sqlx::query(
            r#"
            INSERT OR IGNORE INTO leitner_study_events (
                event_id, device_id, sync_id, created_at_unix, day_key, session_id,
                course_id, unit_id, mode, grade, was_correct, stability_after,
                difficulty_after, due_day_after, received_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&event.event_id)
        .bind(&payload.device_id)
        .bind(&payload.sync_id)
        .bind(event.created_at_unix)
        .bind(&event.day_key)
        .bind(&event.session_id)
        .bind(&event.course_id)
        .bind(&event.unit_id)
        .bind(&event.mode)
        .bind(event.grade)
        .bind(if event.was_correct { 1 } else { 0 })
        .bind(event.stability_after)
        .bind(event.difficulty_after)
        .bind(&event.due_day_after)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        accepted_events += result.rows_affected();
    }

    tx.commit().await?;

    Ok(json!({
        "accepted": true,
        "inserted_batch": batch_result.rows_affected() == 1,
        "accepted_events": accepted_events,
        "received_tracks": payload.tracks.len(),
        "received_units": payload.units.len(),
        "sync_id": payload.sync_id,
        "received_at": now,
        "received_at_iso": unix_iso(now),
    }))
}

async fn ensure_schema(pool: &SqlitePool) -> anyhow::Result<()> {
    let statements = [
        r#"
        CREATE TABLE IF NOT EXISTS leitner_sync_devices (
            device_id TEXT PRIMARY KEY,
            user_id TEXT,
            app_version TEXT NOT NULL,
            build_number TEXT NOT NULL,
            platform TEXT NOT NULL,
            first_seen_at INTEGER NOT NULL,
            last_seen_at INTEGER NOT NULL,
            last_sync_id TEXT NOT NULL
        )
        "#,
        r#"
        CREATE TABLE IF NOT EXISTS leitner_sync_batches (
            sync_id TEXT PRIMARY KEY,
            device_id TEXT NOT NULL,
            user_id TEXT,
            app_version TEXT NOT NULL,
            build_number TEXT NOT NULL,
            platform TEXT NOT NULL,
            schema_version INTEGER NOT NULL,
            foundation_checkpoint TEXT NOT NULL,
            trigger TEXT NOT NULL,
            captured_at_unix INTEGER NOT NULL,
            selected_course_id TEXT NOT NULL,
            day_key TEXT NOT NULL,
            daily_new_unit_target INTEGER NOT NULL,
            review_limit INTEGER NOT NULL,
            lemma_goal INTEGER NOT NULL,
            due_prompt_count INTEGER NOT NULL,
            new_unit_slots INTEGER NOT NULL,
            introduced_unit_count INTEGER NOT NULL,
            practiced_today_count INTEGER NOT NULL,
            mastered_lemma_count INTEGER NOT NULL,
            productive_unit_count INTEGER NOT NULL,
            total_lemma_count INTEGER NOT NULL,
            total_unit_count INTEGER NOT NULL,
            average_stability REAL NOT NULL,
            average_retrievability REAL NOT NULL,
            track_count INTEGER NOT NULL,
            event_count INTEGER NOT NULL,
            unit_count INTEGER NOT NULL,
            payload_json TEXT NOT NULL,
            received_at INTEGER NOT NULL
        )
        "#,
        "CREATE INDEX IF NOT EXISTS idx_leitner_sync_batches_device_received ON leitner_sync_batches(device_id, received_at DESC)",
        "CREATE INDEX IF NOT EXISTS idx_leitner_sync_batches_course_received ON leitner_sync_batches(selected_course_id, received_at DESC)",
        r#"
        CREATE TABLE IF NOT EXISTS leitner_units (
            course_id TEXT NOT NULL,
            unit_id TEXT NOT NULL,
            category_id TEXT NOT NULL,
            category_title TEXT NOT NULL,
            english TEXT NOT NULL,
            target TEXT NOT NULL,
            pronunciation TEXT,
            note TEXT,
            unit_kind TEXT NOT NULL,
            updated_at INTEGER NOT NULL,
            PRIMARY KEY(course_id, unit_id)
        )
        "#,
        "CREATE INDEX IF NOT EXISTS idx_leitner_units_kind ON leitner_units(course_id, unit_kind)",
        r#"
        CREATE TABLE IF NOT EXISTS leitner_memory_tracks (
            device_id TEXT NOT NULL,
            course_id TEXT NOT NULL,
            unit_id TEXT NOT NULL,
            mode TEXT NOT NULL,
            introduced_day TEXT NOT NULL,
            due_day TEXT NOT NULL,
            last_reviewed_day TEXT,
            stage INTEGER NOT NULL,
            stability REAL NOT NULL,
            difficulty REAL NOT NULL,
            retrievability REAL NOT NULL,
            interval_days INTEGER NOT NULL,
            review_count INTEGER NOT NULL,
            lapse_count INTEGER NOT NULL,
            correct_streak INTEGER NOT NULL,
            synced_at INTEGER NOT NULL,
            PRIMARY KEY(device_id, unit_id, mode)
        )
        "#,
        "CREATE INDEX IF NOT EXISTS idx_leitner_memory_tracks_due ON leitner_memory_tracks(device_id, course_id, due_day)",
        "CREATE INDEX IF NOT EXISTS idx_leitner_memory_tracks_stage ON leitner_memory_tracks(device_id, course_id, stage)",
        r#"
        CREATE TABLE IF NOT EXISTS leitner_study_events (
            event_id TEXT PRIMARY KEY,
            device_id TEXT NOT NULL,
            sync_id TEXT NOT NULL,
            created_at_unix INTEGER NOT NULL,
            day_key TEXT NOT NULL,
            session_id TEXT NOT NULL,
            course_id TEXT NOT NULL,
            unit_id TEXT NOT NULL,
            mode TEXT NOT NULL,
            grade INTEGER NOT NULL,
            was_correct INTEGER NOT NULL,
            stability_after REAL NOT NULL,
            difficulty_after REAL NOT NULL,
            due_day_after TEXT NOT NULL,
            received_at INTEGER NOT NULL
        )
        "#,
        "CREATE INDEX IF NOT EXISTS idx_leitner_study_events_device_created ON leitner_study_events(device_id, created_at_unix DESC)",
        "CREATE INDEX IF NOT EXISTS idx_leitner_study_events_course_day ON leitner_study_events(course_id, day_key)",
    ];

    for statement in statements {
        sqlx::query(statement).execute(pool).await?;
    }

    Ok(())
}

fn bad_request(message: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response()
}

fn unix_iso(timestamp: i64) -> String {
    chrono::DateTime::from_timestamp(timestamp, 0)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_default()
}
