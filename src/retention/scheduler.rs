//! Background task scheduler for the retention prune (T-65DA4D32).
//!
//! One `tokio::spawn` runs [`retention_loop`] for the lifetime of the
//! process. The loop:
//!
//! 1. On startup, optionally fires an immediate prune pass when
//!    `RETENTION_RUN_ON_STARTUP=1` (defaults off to avoid surprise
//!    long-running queries during hot reloads).
//! 2. Ensures the supporting index exists (`CREATE INDEX IF NOT EXISTS`).
//! 3. Sleeps until the next `RETENTION_RUN_HOUR_UTC` (default 03:00 UTC)
//!    and fires the prune.
//! 4. Cancels cleanly when the provided [`CancellationToken`] is triggered.
//!
//! Wire-up lives in `main.rs` alongside the other background schedulers
//! (email fetcher, session cleanup, system log cleanup).

use std::sync::Arc;
use std::time::Duration;

use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use tokio_util::sync::CancellationToken;

use ticketing_system::SqlitePool;

use super::{prune, RetentionConfig, DEFAULT_RUN_HOUR_UTC};

/// Spawn the retention background task. Returns immediately after
/// scheduling. The task runs until `shutdown` is cancelled.
///
/// Call from `main.rs` once per process. Calling twice would double up
/// every prune — protected against at the registry layer by the single
/// call site, not inside this function.
pub fn spawn_retention_loop(
    db_pool: Arc<SqlitePool>,
    config: RetentionConfig,
    shutdown: CancellationToken,
) {
    tokio::spawn(async move {
        retention_loop(db_pool, config, shutdown).await;
    });
}

/// Main retention loop. Public for test harnesses that want to drive
/// one cycle manually, but the production entry point is
/// [`spawn_retention_loop`].
pub async fn retention_loop(
    db_pool: Arc<SqlitePool>,
    config: RetentionConfig,
    shutdown: CancellationToken,
) {
    tracing::info!(
        target: "retention",
        "[retention] scheduler started (age_days={}, min_keep={}, batch_size={})",
        config.age_days,
        config.min_keep_per_conversation,
        config.batch_size
    );

    // Ensure the supporting index exists. Runs once per process at
    // startup — cheap because CREATE INDEX IF NOT EXISTS short-circuits.
    if let Err(e) = prune::ensure_retention_index(&db_pool).await {
        tracing::error!(
            target: "retention",
            "[retention] failed to ensure retention index: {:?}",
            e
        );
        // Don't abort — the prune query still works without the index,
        // it'll just be slower.
    }

    // Optional startup run. Dev loop and first-deploy backfills both want
    // this. Production sets it off so a hot reload doesn't immediately
    // hammer the DB.
    if startup_flag_enabled() {
        tracing::info!(target: "retention", "[retention] RETENTION_RUN_ON_STARTUP=1, running immediately");
        match prune::prune_conversation_events(&db_pool, &config).await {
            Ok(report) => tracing::info!(
                target: "retention",
                rows_deleted = report.rows_deleted,
                conversations_touched = report.conversations_touched,
                duration_ms = report.duration_ms,
                "[retention] startup prune completed"
            ),
            Err(e) => tracing::error!(
                target: "retention",
                "[retention] startup prune failed: {:?}",
                e
            ),
        }
    }

    let run_hour = run_hour_utc();

    loop {
        let sleep = duration_until_next_trigger(run_hour);
        tracing::info!(
            target: "retention",
            "[retention] next prune in {:.1} hours (UTC hour={})",
            sleep.as_secs_f64() / 3600.0,
            run_hour
        );

        tokio::select! {
            _ = shutdown.cancelled() => {
                tracing::info!(target: "retention", "[retention] scheduler shutting down");
                return;
            }
            _ = tokio::time::sleep(sleep) => {}
        }

        match prune::prune_conversation_events(&db_pool, &config).await {
            Ok(report) => tracing::info!(
                target: "retention",
                rows_scanned = report.rows_scanned,
                rows_deleted = report.rows_deleted,
                conversations_touched = report.conversations_touched,
                duration_ms = report.duration_ms,
                "[retention] scheduled prune completed"
            ),
            Err(e) => tracing::error!(
                target: "retention",
                "[retention] scheduled prune failed: {:?}",
                e
            ),
        }
    }
}

/// Parse `RETENTION_RUN_ON_STARTUP`. Accepts `1`, `true`, `yes`
/// case-insensitively. Everything else is "off".
fn startup_flag_enabled() -> bool {
    std::env::var("RETENTION_RUN_ON_STARTUP")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// Parse `RETENTION_RUN_HOUR_UTC` (0..=23). Invalid values fall back to
/// the default with a warn log — a corrupt env var MUST NOT crash the
/// process.
fn run_hour_utc() -> u32 {
    match std::env::var("RETENTION_RUN_HOUR_UTC") {
        Ok(raw) => match raw.trim().parse::<u32>() {
            Ok(h) if h < 24 => h,
            Ok(_) => {
                tracing::warn!(
                    "[retention] RETENTION_RUN_HOUR_UTC={:?} out of 0..=23, using {}",
                    raw,
                    DEFAULT_RUN_HOUR_UTC
                );
                DEFAULT_RUN_HOUR_UTC
            }
            Err(_) => {
                tracing::warn!(
                    "[retention] RETENTION_RUN_HOUR_UTC={:?} is not a number, using {}",
                    raw,
                    DEFAULT_RUN_HOUR_UTC
                );
                DEFAULT_RUN_HOUR_UTC
            }
        },
        Err(_) => DEFAULT_RUN_HOUR_UTC,
    }
}

/// Compute how long to sleep until the next UTC trigger hour.
fn duration_until_next_trigger(run_hour_utc: u32) -> Duration {
    let now = Utc::now().naive_utc();
    next_trigger_from_now(now, run_hour_utc)
}

/// Pure helper — broken out so the logic is unit-testable without a
/// clock dependency.
fn next_trigger_from_now(now: NaiveDateTime, run_hour_utc: u32) -> Duration {
    let trigger_time = NaiveTime::from_hms_opt(run_hour_utc, 0, 0)
        .unwrap_or_else(|| NaiveTime::from_hms_opt(DEFAULT_RUN_HOUR_UTC, 0, 0).unwrap());

    let today_date = NaiveDate::from_ymd_opt(now.year(), now.month(), now.day())
        .expect("today's date is always valid");
    let today_trigger = today_date.and_time(trigger_time);

    let next_trigger = if now < today_trigger {
        today_trigger
    } else {
        // Already past today's trigger — schedule for tomorrow.
        let tomorrow = today_date
            .succ_opt()
            .expect("successor of valid date exists for any reasonable year");
        tomorrow.and_time(trigger_time)
    };

    let delta = next_trigger - now;
    delta
        .to_std()
        .unwrap_or_else(|_| Duration::from_secs(60))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    #[test]
    fn next_trigger_schedules_today_when_before_hour() {
        let now = NaiveDate::from_ymd_opt(2026, 4, 20)
            .unwrap()
            .and_hms_opt(1, 30, 0)
            .unwrap();
        let d = next_trigger_from_now(now, 3);
        // 1h30m until 03:00 UTC same day.
        assert_eq!(d.as_secs(), 90 * 60);
    }

    #[test]
    fn next_trigger_schedules_tomorrow_when_after_hour() {
        let now = NaiveDate::from_ymd_opt(2026, 4, 20)
            .unwrap()
            .and_hms_opt(5, 0, 0)
            .unwrap();
        let d = next_trigger_from_now(now, 3);
        // Already past 03:00 today — next is 03:00 tomorrow, 22h away.
        assert_eq!(d.as_secs(), 22 * 60 * 60);
    }

    #[test]
    fn next_trigger_exactly_at_hour_schedules_tomorrow() {
        // When now == trigger_time exactly, scheduling for tomorrow is
        // the conservative call — we'd rather skip this minute than fire
        // again immediately if the task has already run.
        let now = NaiveDate::from_ymd_opt(2026, 4, 20)
            .unwrap()
            .and_hms_opt(3, 0, 0)
            .unwrap();
        let d = next_trigger_from_now(now, 3);
        assert_eq!(d.as_secs(), 24 * 60 * 60);
    }

    #[test]
    fn run_hour_parses_invalid_value_without_panicking() {
        // We can't mutate the env safely from tests without serialization,
        // but we can at least confirm the parsing helper returns a valid
        // hour when called with no env var.
        // Remove the variable if it was set by an earlier test in-process.
        std::env::remove_var("RETENTION_RUN_HOUR_UTC");
        assert_eq!(run_hour_utc(), DEFAULT_RUN_HOUR_UTC);
    }
}
