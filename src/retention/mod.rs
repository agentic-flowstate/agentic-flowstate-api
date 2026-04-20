//! Per-conversation retention for `conversation_events` (T-65DA4D32).
//!
//! Purpose: bound the `conversation_events` table growth while preserving
//! enough history to make the SSE resume-token contract honest. A resume
//! cursor is guaranteed to return either (a) the events after it, or
//! (b) a clean 410 Gone — never a partial result or silent truncation.
//!
//! # Retention rule
//!
//! Per conversation, keep events where **either** of the following holds:
//!
//! 1. `created_at >= now - age_days * 86400` (within the time window), **or**
//! 2. the event is in the last `min_keep_per_conversation` events for that
//!    conversation (within the "hot tail" — ordered by `event_index`).
//!
//! A row is only eligible for deletion when **both** rules say "purge":
//!
//! ```text
//! DELETE WHERE created_at < cutoff_ts
//!          AND event_index < high_watermark - (min_keep_per_conversation - 1)
//! ```
//!
//! # Interaction with resume tokens (T-2EDABDDD)
//!
//! The resume handler calls [`prune::earliest_retained_event_index`] to
//! decide whether a client cursor sits below the retention floor. When it
//! does, the handler emits the cursor-expired observability signal and
//! returns 410 Gone before any stream is opened.
//!
//! # Scheduler
//!
//! [`scheduler::spawn_retention_loop`] spins up a background task that
//! runs [`prune::prune_conversation_events`] once per day at the UTC hour
//! specified by `RETENTION_RUN_HOUR_UTC` (default `3`). Setting
//! `RETENTION_RUN_ON_STARTUP=1` fires an extra run at boot — useful for
//! local development and first-deploy backfills.
//!
//! # Observability
//!
//! Every prune emits:
//! * `retention_rows_deleted_total` counter — unlabeled total, cheap to sum.
//! * `retention_prune_duration_ms` histogram — wall-clock per run.
//! * `retention_conversations_touched` gauge — last-run count.
//! * `retention_earliest_remaining_event_age_seconds` gauge — seconds since
//!   the oldest retained event across all conversations. The resume-token
//!   410 Gone contract is "an event older than this does not exist on the
//!   server"; operators watch the gauge to verify the invariant.
//! * `tracing::info!` with `rows_scanned`, `rows_deleted`, `conversations_touched`,
//!   `duration_ms` fields at the end of each run.

pub mod prune;
pub mod scheduler;

use std::time::Duration;

/// Default: keep events for 90 days (the time-window arm of the OR rule).
pub const DEFAULT_AGE_DAYS: u32 = 90;

/// Default: keep at least the last 10 000 events per conversation (the
/// hot-tail arm of the OR rule). A conversation with <10k total events
/// NEVER loses any data to retention, regardless of age.
pub const DEFAULT_MIN_KEEP_PER_CONVERSATION: u64 = 10_000;

/// Default: delete in batches of 5 000 rows. Scoped per-conversation so a
/// large prune doesn't hold a single long write lock that could stall
/// live stream inserts.
pub const DEFAULT_BATCH_SIZE: usize = 5_000;

/// Default: run once every 24 hours.
pub const DEFAULT_RUN_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Default hour-of-day (UTC) at which the scheduler fires the nightly
/// prune. Chosen to sit outside North America business hours and after
/// the typical European evening peak. Override with `RETENTION_RUN_HOUR_UTC`.
pub const DEFAULT_RUN_HOUR_UTC: u32 = 3;

/// Runtime tunables for [`prune::prune_conversation_events`].
///
/// All defaults live in module constants so test fixtures can point at a
/// single source of truth. Construct explicitly with aggressive values in
/// tests (tiny `min_keep_per_conversation`, 0-day `age_days`) to exercise
/// the deletion path under controlled seed data.
///
/// `#[allow(dead_code)]` on `run_interval`: the production scheduler
/// ([`scheduler::spawn_retention_loop`]) currently hour-anchors the next
/// run via `RETENTION_RUN_HOUR_UTC` rather than interval-polling. The
/// field is part of the published struct shape (tests, custom drivers,
/// future interval-based scheduling) and is documented by the T-65DA4D32
/// ticket. Removing it would break those callers; leaving it without the
/// attribute produces a spurious dead-code warning.
#[derive(Debug, Clone)]
pub struct RetentionConfig {
    /// Time-window arm of the OR rule. An event younger than this is
    /// always retained.
    pub age_days: u32,

    /// Hot-tail arm of the OR rule. Regardless of age, the most-recent
    /// N events per conversation are always retained.
    pub min_keep_per_conversation: u64,

    /// Maximum rows deleted per DELETE statement. Large batches hold the
    /// SQLite write lock longer; small batches incur more round-trips.
    /// Per-conversation pruning is wrapped in a transaction per batch so
    /// readers see a consistent view mid-run.
    pub batch_size: usize,

    /// How often [`scheduler::spawn_retention_loop`] fires the prune
    /// after startup. The default is 24h, so the task wakes once per day.
    #[allow(dead_code)]
    pub run_interval: Duration,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            age_days: DEFAULT_AGE_DAYS,
            min_keep_per_conversation: DEFAULT_MIN_KEEP_PER_CONVERSATION,
            batch_size: DEFAULT_BATCH_SIZE,
            run_interval: DEFAULT_RUN_INTERVAL,
        }
    }
}

impl RetentionConfig {
    /// Load tunables from environment variables, falling back to module
    /// defaults. Bad integer values fall back too (with a warn log) so a
    /// stray `RETENTION_AGE_DAYS=foo` can never crash startup.
    ///
    /// Recognized variables:
    /// * `RETENTION_AGE_DAYS` (default 90)
    /// * `RETENTION_MIN_KEEP` (default 10000)
    /// * `RETENTION_BATCH_SIZE` (default 5000)
    pub fn from_env() -> Self {
        fn parse<T: std::str::FromStr>(key: &str, default: T) -> T {
            match std::env::var(key) {
                Ok(raw) => raw.trim().parse::<T>().unwrap_or_else(|_| {
                    tracing::warn!(
                        "[retention] invalid {}={:?}, using default",
                        key,
                        raw
                    );
                    default
                }),
                Err(_) => default,
            }
        }
        Self {
            age_days: parse("RETENTION_AGE_DAYS", DEFAULT_AGE_DAYS),
            min_keep_per_conversation: parse(
                "RETENTION_MIN_KEEP",
                DEFAULT_MIN_KEEP_PER_CONVERSATION,
            ),
            batch_size: parse("RETENTION_BATCH_SIZE", DEFAULT_BATCH_SIZE),
            run_interval: DEFAULT_RUN_INTERVAL,
        }
    }

    /// Unix-seconds cutoff below which events are candidates for deletion
    /// (still gated by the `min_keep_per_conversation` hot-tail rule).
    pub fn cutoff_timestamp(&self, now: i64) -> i64 {
        now - (self.age_days as i64) * 86_400
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_retention_matches_spec() {
        let cfg = RetentionConfig::default();
        assert_eq!(cfg.age_days, 90);
        assert_eq!(cfg.min_keep_per_conversation, 10_000);
        assert_eq!(cfg.batch_size, 5_000);
        assert_eq!(cfg.run_interval, Duration::from_secs(86_400));
    }

    #[test]
    fn cutoff_timestamp_is_now_minus_age() {
        let cfg = RetentionConfig {
            age_days: 90,
            ..RetentionConfig::default()
        };
        assert_eq!(cfg.cutoff_timestamp(1_000_000_000), 1_000_000_000 - 90 * 86_400);
    }
}
