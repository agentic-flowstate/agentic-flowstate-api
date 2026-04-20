//! Admin endpoints + background refresher for runtime-flippable feature
//! flags (T-257C060D).
//!
//! Exposed routes (both guarded by `require_admin`):
//!   - `GET  /api/admin/feature-flags/event_vocab_mode`
//!   - `PUT  /api/admin/feature-flags/event_vocab_mode`
//!
//! GET returns both the in-memory snapshot and the DB row. When they
//! disagree we log, return 500, and expose both values so the operator
//! can see the split-brain condition directly.
//!
//! PUT validates the `{ "mode": "<canonical>" }` body, writes to the DB
//! first (durability), then atomically swaps the in-memory cell.
//!
//! The 30-second refresher task is spawned from `main.rs` via
//! [`spawn_refresher`]. Every tick it reads `feature_flags.event_vocab_mode`
//! and republishes via `event_vocab::store` when the DB value differs
//! from the current in-memory value. This keeps multi-instance
//! deployments convergent — a PUT to instance A is picked up by
//! instance B within ~30s.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::SqlitePool;
use tokio_util::sync::CancellationToken;

use crate::auth_middleware::AuthenticatedUser;
use ticketing_system::feature_flags;

use super::event_vocab::{self, EventVocabMode};

/// Refresher cadence. 30 seconds is a balance between cross-instance
/// convergence (low = fast fan-out) and wasted queries on a quiet flag
/// (high = fewer useless reads). The read is a single indexed lookup on
/// a tiny table so the tradeoff is forgiving.
const REFRESH_INTERVAL_SECS: u64 = 30;

// ---- GET handler ---------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct FlagStatusResponse {
    /// Canonical flag key (always `event_vocab_mode` here).
    pub key: String,
    /// In-memory snapshot held by the ArcSwap. Always present — the
    /// cell auto-initializes to the default if no one has written.
    pub in_memory: String,
    /// DB row value. `None` when the `feature_flags` row is missing
    /// (should not happen after the startup seed).
    pub in_db: Option<String>,
    /// Unix seconds of the last DB write. `None` when the row is missing.
    pub updated_at: Option<i64>,
    /// The writer identifier from the last DB write. `None` when the
    /// row is missing or was bootstrapped anonymously.
    pub updated_by: Option<String>,
}

/// `GET /api/admin/feature-flags/event_vocab_mode`
///
/// Returns the in-memory snapshot and the DB row. If they disagree we
/// return 500 and log — the refresher should be reconciling them, so a
/// disagreement here is an operator signal.
pub async fn get_event_vocab_mode(
    State(pool): State<Arc<SqlitePool>>,
) -> Response {
    let in_memory = event_vocab::current_mode();

    let db_row = match feature_flags::get_flag(&pool, EventVocabMode::FLAG_KEY).await {
        Ok(row) => row,
        Err(e) => {
            tracing::error!("[FEATURE_FLAGS] DB read failed: {:?}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "feature_flags read failed" })),
            )
                .into_response();
        }
    };

    let in_db_raw = db_row.as_ref().map(|r| r.value.clone());
    let updated_at = db_row.as_ref().map(|r| r.updated_at);
    let updated_by = db_row.as_ref().and_then(|r| r.updated_by.clone());

    // Consistency check: if the DB row exists and parses, compare to
    // the in-memory snapshot. Disagreement is logged and surfaced.
    if let Some(raw) = in_db_raw.as_ref() {
        match EventVocabMode::parse(raw) {
            Ok(parsed) if parsed != in_memory => {
                tracing::error!(
                    "[FEATURE_FLAGS] split-brain on {}: in_memory={}, in_db={}",
                    EventVocabMode::FLAG_KEY,
                    in_memory,
                    raw
                );
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "error": "in-memory and DB values disagree",
                        "key": EventVocabMode::FLAG_KEY,
                        "in_memory": in_memory.as_str(),
                        "in_db": raw,
                    })),
                )
                    .into_response();
            }
            Ok(_) => {}
            Err(e) => {
                // DB row contains garbage. Log and return 500 — there's
                // no safe auto-heal here; the operator needs to fix it.
                tracing::error!(
                    "[FEATURE_FLAGS] DB row has invalid value {:?}: {}",
                    raw,
                    e
                );
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "error": "DB row is not a canonical EventVocabMode value",
                        "key": EventVocabMode::FLAG_KEY,
                        "in_db": raw,
                    })),
                )
                    .into_response();
            }
        }
    }

    let body = FlagStatusResponse {
        key: EventVocabMode::FLAG_KEY.to_string(),
        in_memory: in_memory.as_str().to_string(),
        in_db: in_db_raw,
        updated_at,
        updated_by,
    };
    (StatusCode::OK, Json(body)).into_response()
}

// ---- PUT handler ---------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct PutEventVocabModeRequest {
    pub mode: String,
}

/// `PUT /api/admin/feature-flags/event_vocab_mode`
///
/// Body: `{ "mode": "legacy_only" | "dual" | "modern_only" }`.
/// Any other value → 400. On success writes to the DB first, then
/// atomically swaps the in-memory cell, then returns the new value.
pub async fn put_event_vocab_mode(
    State(pool): State<Arc<SqlitePool>>,
    axum::Extension(user): axum::Extension<AuthenticatedUser>,
    Json(body): Json<PutEventVocabModeRequest>,
) -> Response {
    let parsed = match EventVocabMode::parse(&body.mode) {
        Ok(m) => m,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!("{}", e),
                    "accepted": ["legacy_only", "dual", "modern_only"],
                })),
            )
                .into_response();
        }
    };

    if let Err(e) = feature_flags::set_flag(
        &pool,
        EventVocabMode::FLAG_KEY,
        parsed.as_str(),
        Some(&user.user_id),
    )
    .await
    {
        tracing::error!("[FEATURE_FLAGS] DB write failed: {:?}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "feature_flags write failed" })),
        )
            .into_response();
    }

    // Publish to every reader in this process. The refresher will pick
    // the DB change up on other instances within one tick.
    event_vocab::store(parsed);

    tracing::info!(
        "[FEATURE_FLAGS] {} set to {} by {}",
        EventVocabMode::FLAG_KEY,
        parsed,
        user.user_id
    );

    let body = FlagStatusResponse {
        key: EventVocabMode::FLAG_KEY.to_string(),
        in_memory: parsed.as_str().to_string(),
        in_db: Some(parsed.as_str().to_string()),
        updated_at: Some(chrono::Utc::now().timestamp()),
        updated_by: Some(user.user_id.clone()),
    };
    (StatusCode::OK, Json(body)).into_response()
}

// ---- Startup bootstrap --------------------------------------------------

/// Resolve the startup value for `event_vocab_mode`, writing to the DB
/// when the env var wins and returning the final value for
/// [`event_vocab::init_global`] to publish.
///
/// Precedence:
///   1. `EVENT_VOCAB_MODE` env var (if set and parseable) — env wins
///      and overwrites the DB row. This is the documented "operator
///      override in emergencies" lever.
///   2. `feature_flags.event_vocab_mode` row in the DB — the durable
///      operator-managed value.
///   3. [`EventVocabMode::DEFAULT`] (dual) — bootstrapped into the DB
///      via `insert_if_missing`.
///
/// A parse error on the env var is a fail-loud condition; the caller
/// propagates it up and startup aborts.
pub async fn bootstrap_event_vocab_mode(
    pool: &SqlitePool,
) -> anyhow::Result<EventVocabMode> {
    // 1. Env var override (may be None when unset).
    let env_override = EventVocabMode::from_env_optional()
        .map_err(|e| anyhow::anyhow!(e))?;

    // 2. DB row (may be None before first run).
    let db_row = feature_flags::get_flag(pool, EventVocabMode::FLAG_KEY).await?;
    let db_parsed = match db_row.as_ref() {
        Some(row) => match EventVocabMode::parse(&row.value) {
            Ok(m) => Some(m),
            Err(e) => {
                // A corrupt DB row is fatal. The operator needs to see
                // this and fix it explicitly.
                return Err(anyhow::anyhow!(
                    "feature_flags.{} contains invalid value {:?}: {}",
                    EventVocabMode::FLAG_KEY,
                    row.value,
                    e
                ));
            }
        },
        None => None,
    };

    let (resolved, source) = match (env_override, db_parsed) {
        (Some(env), Some(db)) if env != db => {
            // Env wins AND overwrites DB. Documented semantic.
            feature_flags::set_flag(
                pool,
                EventVocabMode::FLAG_KEY,
                env.as_str(),
                Some("startup:env_override"),
            )
            .await?;
            tracing::warn!(
                "[EVENT_VOCAB] env override {:?} overwrote DB value {:?}",
                env.as_str(),
                db.as_str()
            );
            (env, "env_overwrote_db")
        }
        (Some(env), Some(_db)) => (env, "env_matches_db"),
        (Some(env), None) => {
            // Env set, DB empty — bootstrap DB from env.
            feature_flags::set_flag(
                pool,
                EventVocabMode::FLAG_KEY,
                env.as_str(),
                Some("startup:env"),
            )
            .await?;
            (env, "env_seeded_db")
        }
        (None, Some(db)) => (db, "db"),
        (None, None) => {
            // Neither source set. Seed the DB with the default so
            // subsequent reads / refreshers have a row to compare
            // against.
            let default = EventVocabMode::DEFAULT;
            feature_flags::insert_if_missing(
                pool,
                EventVocabMode::FLAG_KEY,
                default.as_str(),
                Some("startup:default"),
            )
            .await?;
            (default, "default")
        }
    };

    tracing::info!(
        "[EVENT_VOCAB] resolved startup mode = {} (source={})",
        resolved,
        source
    );
    Ok(resolved)
}

// ---- Refresher task ------------------------------------------------------

/// Spawn the periodic refresher. Reads the DB row every 30s and
/// republishes when it differs from the in-memory snapshot. The loop
/// exits when the cancellation token fires (graceful shutdown).
pub fn spawn_refresher(
    pool: Arc<SqlitePool>,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(Duration::from_secs(REFRESH_INTERVAL_SECS));
        // First tick fires immediately — skip it so we don't race with
        // the startup bootstrap that just ran.
        interval.tick().await;

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    tracing::info!("[EVENT_VOCAB] refresher shutting down");
                    break;
                }
                _ = interval.tick() => {}
            }

            match feature_flags::get_flag(&pool, EventVocabMode::FLAG_KEY).await {
                Ok(Some(row)) => match EventVocabMode::parse(&row.value) {
                    Ok(parsed) => {
                        let current = event_vocab::current_mode();
                        if parsed != current {
                            event_vocab::store(parsed);
                            tracing::info!(
                                "[EVENT_VOCAB] refresher picked up {} -> {} (db updated_by={:?})",
                                current,
                                parsed,
                                row.updated_by
                            );
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            "[EVENT_VOCAB] refresher: DB row has invalid value {:?}: {}",
                            row.value,
                            e
                        );
                    }
                },
                Ok(None) => {
                    // The row disappeared — log but don't panic. The
                    // in-memory value stays as-is.
                    tracing::warn!(
                        "[EVENT_VOCAB] refresher: DB row for {} missing",
                        EventVocabMode::FLAG_KEY
                    );
                }
                Err(e) => {
                    tracing::error!(
                        "[EVENT_VOCAB] refresher: DB read failed: {:?}",
                        e
                    );
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    /// Serialize tests that touch the process-wide `event_vocab` ArcSwap
    /// and/or `EVENT_VOCAB_MODE` env var — cargo runs tests in parallel
    /// by default, and the shared mutable state would race otherwise.
    /// Shared with `event_vocab::tests` via `TEST_STATE_LOCK`.
    use super::event_vocab::TEST_STATE_LOCK as STATE_LOCK;

    async fn make_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(
            r#"CREATE TABLE feature_flags (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                updated_at INTEGER NOT NULL,
                updated_by TEXT
            )"#,
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    #[tokio::test]
    async fn bootstrap_empty_db_no_env_writes_default() {
        let _guard = STATE_LOCK.lock().unwrap();
        // Make sure the env var is clear for this test.
        // SAFETY: env var mutation — tests in this module are serialized via STATE_LOCK.
        unsafe {
            std::env::remove_var(EventVocabMode::ENV_VAR);
        }

        let pool = make_pool().await;
        let mode = bootstrap_event_vocab_mode(&pool).await.unwrap();
        assert_eq!(mode, EventVocabMode::Dual);

        // Row was seeded.
        let row = feature_flags::get_flag(&pool, EventVocabMode::FLAG_KEY)
            .await
            .unwrap()
            .expect("row seeded");
        assert_eq!(row.value, "dual");
        assert_eq!(row.updated_by.as_deref(), Some("startup:default"));
    }

    #[tokio::test]
    async fn bootstrap_env_overrides_and_overwrites_db() {
        let _guard = STATE_LOCK.lock().unwrap();
        let pool = make_pool().await;
        // Pre-seed the DB with a different value.
        feature_flags::set_flag(
            &pool,
            EventVocabMode::FLAG_KEY,
            "legacy_only",
            Some("test"),
        )
        .await
        .unwrap();

        // SAFETY: env var mutation — tests in this module run sequentially.
        unsafe {
            std::env::set_var(EventVocabMode::ENV_VAR, "modern_only");
        }
        let mode = bootstrap_event_vocab_mode(&pool).await.unwrap();
        // SAFETY: env var mutation — tests in this module run sequentially.
        unsafe {
            std::env::remove_var(EventVocabMode::ENV_VAR);
        }
        assert_eq!(mode, EventVocabMode::ModernOnly);

        // DB was overwritten.
        let row = feature_flags::get_flag(&pool, EventVocabMode::FLAG_KEY)
            .await
            .unwrap()
            .expect("row");
        assert_eq!(row.value, "modern_only");
        assert_eq!(row.updated_by.as_deref(), Some("startup:env_override"));
    }

    #[tokio::test]
    async fn bootstrap_uses_db_when_no_env() {
        let _guard = STATE_LOCK.lock().unwrap();
        // SAFETY: env var mutation — tests in this module are serialized via STATE_LOCK.
        unsafe {
            std::env::remove_var(EventVocabMode::ENV_VAR);
        }
        let pool = make_pool().await;
        feature_flags::set_flag(
            &pool,
            EventVocabMode::FLAG_KEY,
            "modern_only",
            Some("operator"),
        )
        .await
        .unwrap();

        let mode = bootstrap_event_vocab_mode(&pool).await.unwrap();
        assert_eq!(mode, EventVocabMode::ModernOnly);
    }

    /// Simulates what the PUT handler does (write DB + swap) and
    /// verifies readers see the new value. Round-trips all three
    /// canonical values.
    #[tokio::test]
    async fn put_then_read_roundtrip() {
        let _guard = STATE_LOCK.lock().unwrap();
        let pool = make_pool().await;
        for mode in [
            EventVocabMode::LegacyOnly,
            EventVocabMode::Dual,
            EventVocabMode::ModernOnly,
        ] {
            feature_flags::set_flag(
                &pool,
                EventVocabMode::FLAG_KEY,
                mode.as_str(),
                Some("test-admin"),
            )
            .await
            .unwrap();
            event_vocab::store(mode);

            let row = feature_flags::get_flag(&pool, EventVocabMode::FLAG_KEY)
                .await
                .unwrap()
                .expect("row");
            assert_eq!(row.value, mode.as_str());
            assert_eq!(event_vocab::current_mode(), mode);
        }
        // Reset
        event_vocab::store(EventVocabMode::Dual);
    }

    /// Validates the invalid-body path: `EventVocabMode::parse` is the
    /// gate on the PUT handler, and it rejects non-canonical values.
    /// The full HTTP-level test is integration-scope; this verifies
    /// the same decision function the handler uses.
    #[test]
    fn invalid_put_body_rejected() {
        assert!(EventVocabMode::parse("banana").is_err());
        assert!(EventVocabMode::parse("").is_err());
        assert!(EventVocabMode::parse("legacy").is_err());
        assert!(EventVocabMode::parse("modern").is_err());
    }

    /// The refresher reads the DB and republishes the in-memory value
    /// when they differ. Simulate one iteration of its core logic by
    /// writing the DB, calling the same parse+store the task does.
    #[tokio::test]
    async fn refresher_picks_up_db_change() {
        let _guard = STATE_LOCK.lock().unwrap();
        let pool = make_pool().await;
        event_vocab::store(EventVocabMode::Dual);
        feature_flags::set_flag(
            &pool,
            EventVocabMode::FLAG_KEY,
            "legacy_only",
            Some("operator"),
        )
        .await
        .unwrap();

        // This mirrors the refresher's per-tick logic.
        let row = feature_flags::get_flag(&pool, EventVocabMode::FLAG_KEY)
            .await
            .unwrap()
            .unwrap();
        let parsed = EventVocabMode::parse(&row.value).unwrap();
        if parsed != event_vocab::current_mode() {
            event_vocab::store(parsed);
        }
        assert_eq!(event_vocab::current_mode(), EventVocabMode::LegacyOnly);
        // Reset.
        event_vocab::store(EventVocabMode::Dual);
    }

    #[tokio::test]
    async fn bootstrap_rejects_corrupt_db_row() {
        let _guard = STATE_LOCK.lock().unwrap();
        // SAFETY: env var mutation — tests in this module are serialized via STATE_LOCK.
        unsafe {
            std::env::remove_var(EventVocabMode::ENV_VAR);
        }
        let pool = make_pool().await;
        feature_flags::set_flag(
            &pool,
            EventVocabMode::FLAG_KEY,
            "banana",
            Some("corrupt"),
        )
        .await
        .unwrap();
        let err = bootstrap_event_vocab_mode(&pool).await.unwrap_err();
        assert!(
            err.to_string().contains("invalid value"),
            "unexpected error: {}",
            err
        );
    }
}
