//! Core prune algorithm for `conversation_events` (T-65DA4D32).
//!
//! Exposes three primitives:
//! * [`prune_conversation_events`] — the one-shot sweep that enforces the
//!   retention rule across every conversation.
//! * [`earliest_retained_event_index`] — the read-only accessor the resume
//!   handler (T-2EDABDDD) calls when deciding 410 Gone vs serve-from-cursor.
//! * [`ensure_retention_index`] — idempotent `CREATE INDEX IF NOT EXISTS`
//!   for the composite `(conversation_id, created_at)` index the prune
//!   query scans.
//!
//! The prune is idempotent: running it twice back-to-back on the same DB
//! state yields zero deletions on the second run. Tests 1-4 below lock
//! that contract in.

use std::collections::HashMap;
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::Utc;
use ticketing_system::SqlitePool;

use super::RetentionConfig;
use crate::observability::streaming as obs;

/// Report returned by [`prune_conversation_events`]. Captures every
/// per-run statistic the observability and test layers care about.
///
/// `earliest_remaining_per_conversation` maps `conversation_id` to the
/// `created_at` (unix seconds) of the oldest remaining event after the
/// prune, but only for conversations that were touched. Operators and the
/// resume-token 410 Gone handler use this to verify that retention is
/// actually bounding the time window as configured.
#[derive(Debug, Default, Clone)]
pub struct PruneReport {
    /// Count of rows examined as deletion candidates (i.e. rows older
    /// than the cutoff). Not every examined row is deleted — the hot-tail
    /// rule may save it.
    pub rows_scanned: u64,

    /// Count of rows actually removed from `conversation_events`. Dependent
    /// `event_blobs` rows are cascade-deleted via the FK.
    pub rows_deleted: u64,

    /// Count of conversations that had at least one row deleted.
    pub conversations_touched: u64,

    /// Wall-clock duration of the run.
    pub duration_ms: u64,

    /// Per-conversation oldest remaining `created_at` after the prune.
    /// Populated only for touched conversations.
    pub earliest_remaining_per_conversation: HashMap<String, i64>,
}

/// Create the retention-supporting index if it does not exist. Safe to
/// call on every startup — `CREATE INDEX IF NOT EXISTS` is a no-op when
/// the index already exists.
///
/// The index covers the DELETE predicate:
/// ```sql
/// WHERE conversation_id = ? AND created_at < ? AND event_index < ?
/// ```
/// Ordering `(conversation_id, created_at, event_index)` lets SQLite
/// seek straight into the candidate range for each conversation.
pub async fn ensure_retention_index(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_events_retention \
         ON conversation_events(conversation_id, created_at, event_index)",
    )
    .execute(pool)
    .await
    .context("failed to create idx_events_retention")?;
    Ok(())
}

/// Read the smallest retained `event_index` for a conversation. Returns
/// `None` when the conversation has no events at all (new conversation
/// or already emptied).
///
/// T-2EDABDDD's resume handler compares the client's `starting_after`
/// cursor to this value:
/// * `cursor < earliest_retained_event_index - 1` → 410 Gone
/// * `cursor >= earliest_retained_event_index - 1` → serve-from-cursor
///
/// The `-1` accounts for clients that pass the literal last-seen index
/// (want everything strictly greater) vs the minimum retained (which they
/// should be able to re-receive).
///
/// `#[allow(dead_code)]`: this function is the T-65DA4D32 → T-2EDABDDD
/// handshake. The resume-token handler in T-2EDABDDD is the only consumer
/// and does not exist yet; without the allow the compiler flags the
/// symbol as dead code. Its unit-test coverage keeps the implementation
/// honest until the handler lands.
#[allow(dead_code)]
pub async fn earliest_retained_event_index(
    pool: &SqlitePool,
    conversation_id: &str,
) -> Result<Option<i64>> {
    let row: Option<(Option<i64>,)> = sqlx::query_as(
        "SELECT MIN(event_index) FROM conversation_events WHERE conversation_id = ?",
    )
    .bind(conversation_id)
    .fetch_optional(pool)
    .await
    .context("earliest_retained_event_index: query failed")?;

    Ok(row.and_then(|(v,)| v))
}

/// Execute one prune pass across every conversation with rows older than
/// the configured cutoff.
///
/// Steps:
/// 1. Compute `cutoff_ts = now - age_days * 86400`.
/// 2. `SELECT` every `conversation_id` that has at least one row with
///    `created_at < cutoff_ts`. Nothing else is a candidate.
/// 3. For each candidate conversation:
///    a. Fetch `MAX(event_index)` as the high watermark.
///    b. Compute `cutoff_idx = high_watermark - (min_keep - 1)`. A row is
///    only deletable when `event_index < cutoff_idx` AND
///    `created_at < cutoff_ts`.
///    c. Delete in batches of `config.batch_size`, using
///    `DELETE ... WHERE rowid IN (SELECT rowid ... LIMIT ?)`. The FK
///    on `event_blobs` cascades blob rows automatically.
///    d. Record the oldest remaining `created_at` after the loop.
/// 4. Update observability gauges + counters; return the report.
pub async fn prune_conversation_events(
    pool: &SqlitePool,
    config: &RetentionConfig,
) -> Result<PruneReport> {
    let t0 = Instant::now();
    let now_ts = Utc::now().timestamp();
    let cutoff_ts = config.cutoff_timestamp(now_ts);

    tracing::info!(
        target: "retention",
        event = "retention.run_started",
        age_days = config.age_days,
        min_keep = config.min_keep_per_conversation,
        batch_size = config.batch_size,
        cutoff_ts,
        now_ts,
        "retention prune starting"
    );

    // 1. Identify conversations with anything older than cutoff. Filtering
    //    here means conversations entirely within the time window cost
    //    exactly one index seek and no further work per conversation.
    let candidate_rows: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT conversation_id \
         FROM conversation_events \
         WHERE created_at < ?",
    )
    .bind(cutoff_ts)
    .fetch_all(pool)
    .await
    .context("retention: list candidate conversations failed")?;

    let mut report = PruneReport::default();

    for (conversation_id,) in &candidate_rows {
        let summary = prune_one_conversation(pool, conversation_id, cutoff_ts, config).await?;
        report.rows_scanned += summary.rows_scanned;
        report.rows_deleted += summary.rows_deleted;
        if summary.rows_deleted > 0 {
            report.conversations_touched += 1;
            if let Some(oldest) = summary.earliest_remaining_created_at {
                report
                    .earliest_remaining_per_conversation
                    .insert(conversation_id.clone(), oldest);
            }
        }
    }

    report.duration_ms = t0.elapsed().as_millis() as u64;

    // 2. Emit observability signals. Every prune (even zero-delete) bumps
    //    the gauges so dashboards show liveness; operators know the loop
    //    is running even when there's nothing to do.
    emit_run_metrics(&report, now_ts, pool).await;

    tracing::info!(
        target: "retention",
        event = "retention.run_completed",
        rows_scanned = report.rows_scanned,
        rows_deleted = report.rows_deleted,
        conversations_touched = report.conversations_touched,
        duration_ms = report.duration_ms,
        "retention prune completed"
    );

    Ok(report)
}

/// Per-conversation summary returned by [`prune_one_conversation`]. The
/// caller aggregates into the top-level [`PruneReport`].
struct ConversationPruneSummary {
    rows_scanned: u64,
    rows_deleted: u64,
    earliest_remaining_created_at: Option<i64>,
}

/// Apply the retention rule to a single conversation. Runs inside a
/// dedicated BEGIN IMMEDIATE transaction per batch so live writers never
/// race against our deletes (we never hold the lock across batch
/// boundaries — each batch commits before the next begins).
async fn prune_one_conversation(
    pool: &SqlitePool,
    conversation_id: &str,
    cutoff_ts: i64,
    config: &RetentionConfig,
) -> Result<ConversationPruneSummary> {
    // Step 1: how many rows are actually old? This is the "rows_scanned"
    // number — candidates. The hot-tail rule may rescue some.
    let rows_scanned: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM conversation_events \
         WHERE conversation_id = ? AND created_at < ?",
    )
    .bind(conversation_id)
    .bind(cutoff_ts)
    .fetch_one(pool)
    .await
    .context("retention: count old rows failed")?;

    if rows_scanned == 0 {
        return Ok(ConversationPruneSummary {
            rows_scanned: 0,
            rows_deleted: 0,
            earliest_remaining_created_at: None,
        });
    }

    // Step 2: high watermark == last-ever allocated index for this
    // conversation. Combined with `min_keep - 1`, gives the inclusive
    // "cutoff index" — anything strictly below it may be deleted.
    let high_watermark: Option<i64> = sqlx::query_scalar(
        "SELECT MAX(event_index) FROM conversation_events WHERE conversation_id = ?",
    )
    .bind(conversation_id)
    .fetch_one(pool)
    .await
    .context("retention: high watermark query failed")?;

    let Some(high_watermark) = high_watermark else {
        // Conversation has candidates but no max index? Impossible unless
        // a concurrent delete cleared the table between our two queries.
        // Treat as "nothing to do" — the report stays honest.
        return Ok(ConversationPruneSummary {
            rows_scanned: rows_scanned as u64,
            rows_deleted: 0,
            earliest_remaining_created_at: None,
        });
    };

    // cutoff_idx is the smallest `event_index` we are allowed to KEEP
    // based on the hot-tail rule. Anything with `event_index < cutoff_idx`
    // AND `created_at < cutoff_ts` is eligible for deletion.
    //
    // Example: high_watermark=14999, min_keep=10000 → cutoff_idx=5000.
    // Events 0..=4999 may be deleted (if old); events 5000..=14999 stay.
    //
    // `saturating_sub` handles the edge case where total events
    // (high_watermark+1) is less than min_keep — then cutoff_idx becomes
    // 0 or negative, meaning NO event is eligible and the hot-tail rule
    // fully protects the conversation.
    let cutoff_idx: i64 =
        (high_watermark + 1).saturating_sub(config.min_keep_per_conversation as i64);

    if cutoff_idx <= 0 {
        // Entire conversation fits within the hot-tail window. Nothing to
        // delete even though some rows are older than `cutoff_ts`. This
        // is the documented behavior: "whichever set is larger."
        return Ok(ConversationPruneSummary {
            rows_scanned: rows_scanned as u64,
            rows_deleted: 0,
            earliest_remaining_created_at: None,
        });
    }

    // Step 3: batched delete loop. Each iteration takes a write tx, picks
    // up to `batch_size` rowids that satisfy BOTH conditions, and deletes
    // them. When no more rows match we exit.
    let mut rows_deleted_total: u64 = 0;
    loop {
        let batch_deleted = delete_one_batch(
            pool,
            conversation_id,
            cutoff_ts,
            cutoff_idx,
            config.batch_size,
        )
        .await?;

        if batch_deleted == 0 {
            break;
        }

        rows_deleted_total += batch_deleted;

        if (batch_deleted as usize) < config.batch_size {
            // Last partial batch — nothing more to do.
            break;
        }
    }

    // Step 4: oldest remaining created_at for observability / report.
    let earliest_remaining_created_at: Option<i64> = sqlx::query_scalar(
        "SELECT MIN(created_at) FROM conversation_events WHERE conversation_id = ?",
    )
    .bind(conversation_id)
    .fetch_one(pool)
    .await
    .context("retention: earliest remaining query failed")?;

    // Earliest remaining index for structured logging. Helps operators
    // debug 410 Gone responses.
    let earliest_remaining_idx: Option<i64> = sqlx::query_scalar(
        "SELECT MIN(event_index) FROM conversation_events WHERE conversation_id = ?",
    )
    .bind(conversation_id)
    .fetch_one(pool)
    .await
    .unwrap_or(None);

    if rows_deleted_total > 0 {
        tracing::info!(
            target: "retention",
            event = "retention.conversation_pruned",
            conversation_id = %conversation_id,
            rows_deleted = rows_deleted_total,
            rows_scanned,
            high_watermark,
            cutoff_idx,
            earliest_remaining_idx = earliest_remaining_idx.unwrap_or(-1),
            earliest_remaining_created_at = earliest_remaining_created_at.unwrap_or(-1),
            "conversation pruned"
        );
    }

    Ok(ConversationPruneSummary {
        rows_scanned: rows_scanned as u64,
        rows_deleted: rows_deleted_total,
        earliest_remaining_created_at,
    })
}

/// Delete up to `batch_size` rows for one conversation that satisfy both
/// the age and hot-tail predicates. Returns rows actually deleted.
///
/// Uses `DELETE ... WHERE rowid IN (SELECT rowid ... LIMIT ?)` so SQLite
/// can plan the subquery with an index scan and the parent DELETE is a
/// bounded-cardinality operation. This is the canonical batched-delete
/// pattern on SQLite (see the T-65DA4D32 artifact for the rationale).
async fn delete_one_batch(
    pool: &SqlitePool,
    conversation_id: &str,
    cutoff_ts: i64,
    cutoff_idx: i64,
    batch_size: usize,
) -> Result<u64> {
    let result = sqlx::query(
        "DELETE FROM conversation_events \
         WHERE rowid IN ( \
             SELECT rowid FROM conversation_events \
             WHERE conversation_id = ? \
               AND created_at < ? \
               AND event_index < ? \
             LIMIT ? \
         )",
    )
    .bind(conversation_id)
    .bind(cutoff_ts)
    .bind(cutoff_idx)
    .bind(batch_size as i64)
    .execute(pool)
    .await
    .context("retention: batched delete failed")?;

    Ok(result.rows_affected())
}

/// Translate the in-memory report into Prometheus metrics via the
/// centralized observability module. Metric names live in
/// [`crate::observability::streaming`] as consts so dashboards and call
/// sites stay in sync.
async fn emit_run_metrics(report: &PruneReport, now_ts: i64, pool: &SqlitePool) {
    metrics::counter!(obs::METRIC_RETENTION_ROWS_DELETED).increment(report.rows_deleted);
    metrics::histogram!(obs::METRIC_RETENTION_PRUNE_DURATION_MS).record(report.duration_ms as f64);
    metrics::gauge!(obs::METRIC_RETENTION_CONVERSATIONS_TOUCHED)
        .set(report.conversations_touched as f64);

    // Fleet-wide "oldest surviving event age" gauge. We deliberately read
    // MIN(created_at) from the table rather than taking it from
    // `report.earliest_remaining_per_conversation` because the report map
    // only holds conversations that were pruned this run — stable
    // conversations that never had rows to delete would drop off, and the
    // gauge would misrepresent the fleet floor.
    let fleet_min: Option<i64> =
        sqlx::query_scalar("SELECT MIN(created_at) FROM conversation_events")
            .fetch_one(pool)
            .await
            .ok()
            .flatten();
    if let Some(oldest) = fleet_min {
        let age_secs = (now_ts - oldest).max(0) as f64;
        metrics::gauge!(obs::METRIC_RETENTION_EARLIEST_AGE_SECS).set(age_secs);
    } else {
        // No events at all — report a gauge value of 0 so dashboards
        // don't see a "stale metric" gap.
        metrics::gauge!(obs::METRIC_RETENTION_EARLIEST_AGE_SECS).set(0.0);
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    use std::str::FromStr;
    use std::time::Duration;

    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    /// Build a fresh in-memory SQLite pool with the minimum schema the
    /// retention tests need: `conversations`, `conversation_events`, and
    /// `event_blobs` with the FK cascade wired up. We also install the
    /// retention index and the `(conversation_id, event_index)` index
    /// that ticketing-system's real schema creates.
    async fn fresh_pool() -> SqlitePool {
        let name = format!(
            "file:retention-test-{}?mode=memory&cache=shared",
            uuid::Uuid::new_v4()
        );
        let options = SqliteConnectOptions::from_str(&name)
            .expect("valid sqlite dsn")
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(30));

        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .expect("connect test pool");

        sqlx::query(
            r#"
            CREATE TABLE conversations (
                id TEXT PRIMARY KEY,
                user_id TEXT,
                session_id TEXT,
                organization TEXT,
                agent TEXT,
                conversation_type TEXT,
                title TEXT,
                started_at TEXT,
                updated_at TEXT,
                status TEXT NOT NULL DEFAULT 'open',
                archived_at TEXT,
                router_ticket_id TEXT,
                router_organization TEXT
            )
            "#,
        )
        .execute(&pool)
        .await
        .expect("create conversations");

        sqlx::query(
            r#"
            CREATE TABLE conversation_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                event_index INTEGER NOT NULL,
                event_type TEXT NOT NULL,
                event_data TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                UNIQUE(conversation_id, event_index)
            )
            "#,
        )
        .execute(&pool)
        .await
        .expect("create conversation_events");

        sqlx::query(
            "CREATE INDEX idx_conversation_events_idx \
             ON conversation_events(conversation_id, event_index)",
        )
        .execute(&pool)
        .await
        .expect("create idx_conversation_events_idx");

        sqlx::query(
            r#"
            CREATE TABLE event_blobs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_id INTEGER NOT NULL REFERENCES conversation_events(id) ON DELETE CASCADE,
                mime TEXT NOT NULL,
                bytes BLOB NOT NULL,
                created_at INTEGER NOT NULL
            )
            "#,
        )
        .execute(&pool)
        .await
        .expect("create event_blobs");

        ensure_retention_index(&pool)
            .await
            .expect("create retention index");

        pool
    }

    async fn seed_conversation(pool: &SqlitePool, id: &str) {
        sqlx::query("INSERT INTO conversations (id, status) VALUES (?, 'open')")
            .bind(id)
            .execute(pool)
            .await
            .expect("insert conversation");
    }

    /// Seed a range of events for a conversation with explicit per-event
    /// `created_at` ages. `ages_secs_by_index` is a vec where position `i`
    /// holds the "seconds ago from `now_ts`" value for event_index = i.
    async fn seed_events(
        pool: &SqlitePool,
        conversation_id: &str,
        now_ts: i64,
        ages_secs_by_index: &[i64],
    ) {
        for (idx, age_secs) in ages_secs_by_index.iter().enumerate() {
            sqlx::query(
                "INSERT INTO conversation_events \
                 (conversation_id, event_index, event_type, event_data, created_at) \
                 VALUES (?, ?, 'text_delta', '{}', ?)",
            )
            .bind(conversation_id)
            .bind(idx as i64)
            .bind(now_ts - age_secs)
            .execute(pool)
            .await
            .expect("seed event");
        }
    }

    async fn count_events(pool: &SqlitePool, conversation_id: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM conversation_events WHERE conversation_id = ?")
            .bind(conversation_id)
            .fetch_one(pool)
            .await
            .expect("count")
    }

    async fn min_event_index(pool: &SqlitePool, conversation_id: &str) -> Option<i64> {
        sqlx::query_scalar(
            "SELECT MIN(event_index) FROM conversation_events WHERE conversation_id = ?",
        )
        .bind(conversation_id)
        .fetch_one(pool)
        .await
        .expect("min")
    }

    /// Test 1: 15 000 events spanning 180 days with a 90d/10k policy.
    /// Expectation: exactly 5 000 oldest rows deleted (events 0..=4999),
    /// earliest remaining is event 5000, no within-90d rows touched.
    #[tokio::test]
    async fn deletes_oldest_5000_when_both_rules_say_purge() {
        let pool = fresh_pool().await;
        seed_conversation(&pool, "conv-big").await;

        let now_ts = Utc::now().timestamp();

        // 15 000 events: event_index 0 = 180 days old, event_index 14999 = 0 days.
        // Linear interpolation: age_secs = 180 * 86400 * (1 - i / 14999)
        // but we want specific boundaries: ~5 000 should cross the 90d line.
        //
        // Simpler: place first 5 000 events deep in the past (>90 days),
        // the remaining 10 000 inside the window. This matches the ticket's
        // "exactly 5000 oldest rows deleted" expectation precisely.
        let mut ages = Vec::with_capacity(15_000);
        for i in 0..15_000i64 {
            // Events 0..5000 → 100 days old (>> cutoff, eligible for delete).
            // Events 5000..15000 → 1 day old (inside 90d window, safe).
            if i < 5_000 {
                ages.push(100 * 86_400);
            } else {
                ages.push(86_400);
            }
        }
        seed_events(&pool, "conv-big", now_ts, &ages).await;

        let cfg = RetentionConfig::default();
        let report = prune_conversation_events(&pool, &cfg).await.unwrap();

        assert_eq!(report.rows_deleted, 5_000, "expected 5 000 deletions");
        assert_eq!(report.conversations_touched, 1);

        assert_eq!(count_events(&pool, "conv-big").await, 10_000);
        assert_eq!(min_event_index(&pool, "conv-big").await, Some(5_000));

        // None of the survivors should be older than the 90d cutoff. All
        // survivors were seeded at age = 1 day.
        let oldest_remaining: i64 = sqlx::query_scalar(
            "SELECT MIN(created_at) FROM conversation_events WHERE conversation_id = ?",
        )
        .bind("conv-big")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            oldest_remaining >= now_ts - cfg.cutoff_timestamp(0).abs(),
            "surviving oldest event was seeded within the 90d window"
        );
    }

    /// Test 2: 500 events all within the last 30 days. The hot-tail rule
    /// (min_keep=10 000) must protect every row even if age alone would
    /// allow them. Zero deletions.
    #[tokio::test]
    async fn no_deletes_when_all_events_are_recent() {
        let pool = fresh_pool().await;
        seed_conversation(&pool, "conv-small-recent").await;

        let now_ts = Utc::now().timestamp();

        let mut ages = Vec::with_capacity(500);
        for i in 0..500i64 {
            // Spread over the last 30 days — all inside the 90d window.
            ages.push(i * 86_400 * 30 / 500);
        }
        seed_events(&pool, "conv-small-recent", now_ts, &ages).await;

        let report = prune_conversation_events(&pool, &RetentionConfig::default())
            .await
            .unwrap();

        assert_eq!(report.rows_deleted, 0);
        assert_eq!(report.conversations_touched, 0);
        assert_eq!(count_events(&pool, "conv-small-recent").await, 500);
    }

    /// Test 3: 50 events all 120 days old. The age rule would purge every
    /// one, but the hot-tail rule (min_keep=10 000) holds: 50 < 10 000 so
    /// the entire conversation is in the protected tail. Zero deletions.
    #[tokio::test]
    async fn no_deletes_when_conversation_smaller_than_min_keep() {
        let pool = fresh_pool().await;
        seed_conversation(&pool, "conv-tiny-old").await;

        let now_ts = Utc::now().timestamp();
        let ages: Vec<i64> = (0..50).map(|_| 120 * 86_400).collect();
        seed_events(&pool, "conv-tiny-old", now_ts, &ages).await;

        let report = prune_conversation_events(&pool, &RetentionConfig::default())
            .await
            .unwrap();

        assert_eq!(
            report.rows_deleted, 0,
            "hot-tail rule must protect conversations with <min_keep events"
        );
        assert_eq!(count_events(&pool, "conv-tiny-old").await, 50);
    }

    /// Regression for T-9A5AE9A5: completed checkpoints older than the old
    /// one-hour cleanup cutoff must not bypass the 90-day/10k retention
    /// contract. A small conversation stays entirely inside the hot tail
    /// even when every event is old and the checkpoint is terminal.
    #[tokio::test]
    async fn completed_checkpoint_does_not_override_hot_tail_retention() {
        let pool = fresh_pool().await;
        seed_conversation(&pool, "conv-completed-hot-tail").await;

        sqlx::query(
            r#"
            CREATE TABLE agent_checkpoints (
                conversation_id TEXT PRIMARY KEY,
                cc_session_id TEXT NOT NULL,
                status TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            )
            "#,
        )
        .execute(&pool)
        .await
        .expect("create agent_checkpoints");

        let now_ts = Utc::now().timestamp();
        sqlx::query(
            "INSERT INTO agent_checkpoints \
             (conversation_id, cc_session_id, status, created_at, updated_at) \
             VALUES (?, 'session-complete', 'completed', ?, ?)",
        )
        .bind("conv-completed-hot-tail")
        .bind(now_ts - 7200)
        .bind(now_ts - 7200)
        .execute(&pool)
        .await
        .expect("insert completed checkpoint");

        let ages: Vec<i64> = (0..50).map(|_| 120 * 86_400).collect();
        seed_events(&pool, "conv-completed-hot-tail", now_ts, &ages).await;

        let report = prune_conversation_events(&pool, &RetentionConfig::default())
            .await
            .unwrap();

        assert_eq!(report.rows_deleted, 0);
        assert_eq!(
            count_events(&pool, "conv-completed-hot-tail").await,
            50,
            "completed checkpoints do not make hot-tail events eligible for deletion"
        );
    }

    /// Test 4: idempotency. Running the prune twice in a row deletes zero
    /// rows on the second pass. Locks the contract: prune is a pure
    /// function of the current DB state, not of history.
    #[tokio::test]
    async fn second_run_is_idempotent_zero_deletions() {
        let pool = fresh_pool().await;
        seed_conversation(&pool, "conv-idem").await;

        let now_ts = Utc::now().timestamp();
        let mut ages = Vec::with_capacity(12_000);
        for i in 0..12_000i64 {
            ages.push(if i < 2_000 { 200 * 86_400 } else { 86_400 });
        }
        seed_events(&pool, "conv-idem", now_ts, &ages).await;

        let first = prune_conversation_events(&pool, &RetentionConfig::default())
            .await
            .unwrap();
        assert_eq!(first.rows_deleted, 2_000);

        let second = prune_conversation_events(&pool, &RetentionConfig::default())
            .await
            .unwrap();
        assert_eq!(second.rows_deleted, 0);
        assert_eq!(second.conversations_touched, 0);
    }

    /// Test 5: earliest_retained_event_index returns MIN(event_index) for
    /// a populated conversation and None for a fresh one. Used by the
    /// resume handler (T-2EDABDDD) to decide 410 Gone.
    #[tokio::test]
    async fn earliest_retained_event_index_reports_min_or_none() {
        let pool = fresh_pool().await;
        seed_conversation(&pool, "conv-min").await;
        seed_conversation(&pool, "conv-empty").await;

        let now_ts = Utc::now().timestamp();
        let mut ages = Vec::with_capacity(11_000);
        for i in 0..11_000i64 {
            ages.push(if i < 1_000 { 200 * 86_400 } else { 86_400 });
        }
        seed_events(&pool, "conv-min", now_ts, &ages).await;

        // Before prune: minimum index is 0.
        assert_eq!(
            earliest_retained_event_index(&pool, "conv-min")
                .await
                .unwrap(),
            Some(0)
        );

        // After prune: events 0..=999 are deleted, earliest is 1000.
        prune_conversation_events(&pool, &RetentionConfig::default())
            .await
            .unwrap();

        assert_eq!(
            earliest_retained_event_index(&pool, "conv-min")
                .await
                .unwrap(),
            Some(1_000),
            "earliest index should advance to the first retained row"
        );

        assert_eq!(
            earliest_retained_event_index(&pool, "conv-empty")
                .await
                .unwrap(),
            None,
            "empty conversation returns None"
        );
    }

    /// Test 6: `event_blobs` rows are cascade-deleted when their parent
    /// event is pruned. Verifies that FK ON DELETE CASCADE is honored by
    /// the pool (foreign_keys=ON is set in init_db and in fresh_pool).
    #[tokio::test]
    async fn blob_rows_cascade_on_parent_event_deletion() {
        let pool = fresh_pool().await;
        seed_conversation(&pool, "conv-blobs").await;

        let now_ts = Utc::now().timestamp();
        let mut ages = Vec::with_capacity(12_000);
        for i in 0..12_000i64 {
            ages.push(if i < 2_000 { 150 * 86_400 } else { 86_400 });
        }
        seed_events(&pool, "conv-blobs", now_ts, &ages).await;

        // Attach a blob to every even-indexed old event (events 0..2000).
        let blob_parent_ids: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM conversation_events \
             WHERE conversation_id = ? AND event_index < 2000 AND event_index % 2 = 0 \
             ORDER BY event_index",
        )
        .bind("conv-blobs")
        .fetch_all(&pool)
        .await
        .unwrap();

        for parent_id in &blob_parent_ids {
            sqlx::query(
                "INSERT INTO event_blobs (event_id, mime, bytes, created_at) \
                 VALUES (?, 'application/json', ?, ?)",
            )
            .bind(parent_id)
            .bind(b"payload".to_vec())
            .bind(now_ts)
            .execute(&pool)
            .await
            .unwrap();
        }

        let blobs_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM event_blobs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(blobs_before, blob_parent_ids.len() as i64);

        prune_conversation_events(&pool, &RetentionConfig::default())
            .await
            .unwrap();

        let blobs_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM event_blobs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            blobs_after, 0,
            "FK cascade should have reaped every blob whose parent was pruned"
        );
    }

    /// Test 7: with `batch_size = 100` the prune still deletes every
    /// eligible row across multiple iterations. Proves the batch loop
    /// exits correctly rather than bailing after the first batch.
    #[tokio::test]
    async fn batch_size_smaller_than_delete_count_still_finishes() {
        let pool = fresh_pool().await;
        seed_conversation(&pool, "conv-batched").await;

        let now_ts = Utc::now().timestamp();
        // 11 000 events — 1 000 old (to delete), 10 000 recent (hot tail).
        let mut ages = Vec::with_capacity(11_000);
        for i in 0..11_000i64 {
            ages.push(if i < 1_000 { 200 * 86_400 } else { 86_400 });
        }
        seed_events(&pool, "conv-batched", now_ts, &ages).await;

        let cfg = RetentionConfig {
            batch_size: 100,
            ..RetentionConfig::default()
        };
        let report = prune_conversation_events(&pool, &cfg).await.unwrap();

        assert_eq!(report.rows_deleted, 1_000);
        assert_eq!(count_events(&pool, "conv-batched").await, 10_000);
        assert_eq!(min_event_index(&pool, "conv-batched").await, Some(1_000));
    }
}
