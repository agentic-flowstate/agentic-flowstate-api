//! Per-(user_id, conversation_id, kind) SSE stream rate limiter (T-C410DD96).
//!
//! Two independent checks per admission:
//!
//! 1. **Reconnect-rate limit** — sliding window of the last N reconnect
//!    attempts, where "attempt" means "admitted into the stream handler".
//!    Defaults to 10 reconnects per 60 seconds per bucket. When exceeded
//!    the client gets a 429 with `Retry-After: <oldest_entry + window - now>`.
//! 2. **Concurrent-stream cap** — hard limit on the number of SSE streams
//!    currently open for a given bucket. Defaults to 1. When exceeded the
//!    client gets a 429 with a short `Retry-After: 1` so a just-closed
//!    stream has time to release the slot.
//!
//! The reconnect-rate check runs FIRST so that a client stuck in a
//! reconnect loop is told loudly ("too many reconnects, come back in N
//! seconds") instead of getting a series of "slot full" 429s that hide
//! the underlying bug.
//!
//! ## Bucket key dimensionality (T-1BEAA41E)
//!
//! The bucket key includes a [`StreamKind`] axis so that the POST /chat
//! "generate" path and the GET /events "resume" path do not share a
//! permit. The two paths CAN overlap in time on a legitimate client: the
//! app holds an SSE reader open to observe events (resume) while also
//! issuing a POST to send a new message (generate). Before this split,
//! those two streams contended on the same `(user, conv)` bucket with
//! `max_concurrent_streams: 1`, and whichever landed second got a 429
//! because permit release via RAII drop races TCP close delivery.
//! Splitting by kind lets each path have its own slot and makes the
//! 429 surface only when the *same* kind of stream is already open
//! (which is the actual race condition the limiter is there to police).
//!
//! ## Concurrency model
//!
//! The per-bucket state sits in a [`dashmap::DashMap`]. DashMap shards
//! internally, so a check on `(user_a, conv_a)` does not block a check
//! on `(user_b, conv_b)`. Only a mutation on the SAME bucket contends,
//! and even then only briefly — the critical section is O(1): prune the
//! oldest entries off the VecDeque, compare counts, push if admitted.
//!
//! The [`StreamPermit`] returned on `Allow` is an RAII guard. On drop it
//! decrements `active_streams` for its bucket atomically, which is
//! critical: the stream handler holds the permit for the entire stream
//! lifetime (possibly minutes), so we MUST NOT hold the bucket's lock
//! over that interval. The permit carries a weak-ish reference (an
//! [`Arc`] to the shared limiter state) so that even if the limiter
//! itself is dropped from [`AppState`] the drop handler still works.
//!
//! ## Cleanup
//!
//! [`StreamRateLimiter::cleanup_idle_entries`] prunes map entries that
//! are both (a) holding no active streams AND (b) either have an empty
//! reconnect log OR the oldest log entry is older than `2 *
//! window_duration`. The reason for the 2x window is that a client that
//! just hit the reconnect-rate limit needs the entries to stay in the
//! log until the window actually passes — pruning too early would reset
//! the counter and allow the exact DOSing behavior the limiter is meant
//! to prevent.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;

use crate::observability::streaming::record_stream_rate_limited;

/// Which SSE path is asking for admission.
///
/// The two paths operate on completely different resources server-side —
/// POST /chat spawns a new agent turn; GET /events just re-emits
/// already-persisted events from `events_buffer`. It is safe, and
/// expected, for a client to have both open simultaneously. Admission
/// accounting MUST NOT treat them as competing for the same slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamKind {
    /// POST /chat path (starts a new generation turn).
    Generate,
    /// GET /api/v1/conversations/:id/events (replay/tail durable events).
    Resume,
}

impl StreamKind {
    /// Stable wire label used in logs and metrics.
    pub fn as_str(&self) -> &'static str {
        match self {
            StreamKind::Generate => "generate",
            StreamKind::Resume => "resume",
        }
    }
}

impl fmt::Display for StreamKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Composite bucket key: the rate limiter tracks one bucket per
/// `(user_id, conversation_id, stream_kind)` triple. See module-level
/// docs for why the `kind` axis is part of the key.
pub type BucketKey = (String, String, StreamKind);

/// Per-bucket mutable state.
#[derive(Debug)]
pub struct RateLimitState {
    /// Number of SSE streams currently open for this bucket. Bumped on
    /// `Allow` and decremented on [`StreamPermit::drop`].
    pub active_streams: u32,
    /// Timestamps of the most recent reconnect *admissions* (NOT denials).
    /// Denied attempts do not enter the log — otherwise a client in a
    /// reconnect loop would keep its own denial count high forever.
    pub reconnect_log: VecDeque<Instant>,
}

impl RateLimitState {
    fn new() -> Self {
        Self {
            active_streams: 0,
            reconnect_log: VecDeque::with_capacity(16),
        }
    }
}

/// Configuration for the rate limiter. All fields are env-overridable;
/// see [`StreamRateLimitConfig::from_env`].
#[derive(Debug, Clone, Copy)]
pub struct StreamRateLimitConfig {
    /// Hard cap on active SSE streams per (user, conversation) bucket.
    /// Default: 1.
    pub max_concurrent_streams: u32,
    /// Maximum number of reconnect admissions inside the sliding window.
    /// Default: 10.
    pub max_reconnects_per_window: u32,
    /// Sliding-window duration for the reconnect-rate check. Default: 60s.
    pub window_duration: Duration,
}

impl Default for StreamRateLimitConfig {
    fn default() -> Self {
        Self {
            max_concurrent_streams: 1,
            max_reconnects_per_window: 10,
            window_duration: Duration::from_secs(60),
        }
    }
}

impl StreamRateLimitConfig {
    /// Read config from environment variables, panicking on any invalid
    /// value. Per project-wide fail-loud policy (see CLAUDE.md): invalid
    /// env vars MUST panic at startup rather than silently degrade.
    ///
    /// Env vars:
    /// * `RATE_LIMIT_MAX_CONCURRENT_STREAMS` (default `1`)
    /// * `RATE_LIMIT_MAX_RECONNECTS_PER_MIN` (default `10`)
    /// * `RATE_LIMIT_WINDOW_SECS` (default `60`)
    pub fn from_env() -> Self {
        fn parse_u32(name: &str, default: u32) -> u32 {
            match std::env::var(name) {
                Ok(v) => v.parse::<u32>().unwrap_or_else(|e| {
                    panic!("[RATE_LIMIT] invalid {}={:?} (must be u32): {}", name, v, e)
                }),
                Err(_) => default,
            }
        }

        let max_concurrent = parse_u32("RATE_LIMIT_MAX_CONCURRENT_STREAMS", 1);
        let max_reconnects = parse_u32("RATE_LIMIT_MAX_RECONNECTS_PER_MIN", 10);
        let window_secs = parse_u32("RATE_LIMIT_WINDOW_SECS", 60);

        if max_concurrent == 0 {
            panic!("[RATE_LIMIT] RATE_LIMIT_MAX_CONCURRENT_STREAMS must be >= 1");
        }
        if max_reconnects == 0 {
            panic!("[RATE_LIMIT] RATE_LIMIT_MAX_RECONNECTS_PER_MIN must be >= 1");
        }
        if window_secs == 0 {
            panic!("[RATE_LIMIT] RATE_LIMIT_WINDOW_SECS must be >= 1");
        }

        Self {
            max_concurrent_streams: max_concurrent,
            max_reconnects_per_window: max_reconnects,
            window_duration: Duration::from_secs(window_secs as u64),
        }
    }
}

/// The reason a [`RateLimitDecision::Deny`] was returned. Used as the
/// `reason` field in the 429 body and the `reason` label in the
/// `stream_rate_limited_total` metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DenyReason {
    /// The bucket already has `max_concurrent_streams` open.
    ConcurrentStreamLimit,
    /// The bucket has exceeded `max_reconnects_per_window` admissions.
    ReconnectRateLimit,
}

impl DenyReason {
    /// The exact string used in the JSON body and Prometheus label.
    /// Locked down by a unit test so dashboards don't silently break
    /// on a refactor.
    pub fn as_wire_str(&self) -> &'static str {
        match self {
            DenyReason::ConcurrentStreamLimit => "concurrent_stream_limit",
            DenyReason::ReconnectRateLimit => "reconnect_rate_limit",
        }
    }
}

impl fmt::Display for DenyReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_wire_str())
    }
}

/// Decision returned by [`StreamRateLimiter::check`]. Handlers must
/// either pattern-match on `Allow(permit)` and hold the permit for the
/// stream lifetime, or pattern-match on `Deny` and emit the 429
/// response.
#[derive(Debug)]
pub enum RateLimitDecision {
    /// Admission granted. The permit MUST be stored in the handler's
    /// async state so that it is dropped when the stream closes (either
    /// naturally, on client disconnect, or on error).
    Allow(StreamPermit),
    /// Admission denied. The client MAY retry after `retry_after`.
    Deny {
        retry_after: Duration,
        reason: DenyReason,
    },
}

/// RAII guard that holds a rate-limit slot for the lifetime of one SSE
/// stream. On drop it decrements `active_streams` for the bucket.
///
/// The guard holds an `Arc` to the shared [`StreamRateLimiter`] state so
/// that the drop handler works even if the [`AppState`] copy is dropped
/// first (e.g. during a shutdown race).
#[must_use = "rate-limit permit must be held for the stream lifetime; \
              dropping it immediately releases the slot"]
pub struct StreamPermit {
    key: Option<BucketKey>,
    inner: Arc<StreamRateLimiterInner>,
}

impl StreamPermit {
    /// Test-only: the bucket key this permit guards.
    #[cfg(test)]
    pub fn key(&self) -> Option<&BucketKey> {
        self.key.as_ref()
    }
}

impl fmt::Debug for StreamPermit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamPermit")
            .field("key", &self.key)
            .finish()
    }
}

impl Drop for StreamPermit {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            self.inner.release(&key);
        }
    }
}

/// Private inner object referenced by both the public
/// [`StreamRateLimiter`] façade and every outstanding [`StreamPermit`].
/// Split out so permits can hold an `Arc<StreamRateLimiterInner>` without
/// forcing every caller to `Arc<StreamRateLimiter>` indirection.
struct StreamRateLimiterInner {
    state: DashMap<BucketKey, RateLimitState>,
    config: StreamRateLimitConfig,
    clock: Clock,
}

impl StreamRateLimiterInner {
    fn release(&self, key: &BucketKey) {
        // `get_mut` acquires the shard lock briefly, decrements, drops.
        // We do not remove empty entries here — the cleanup task handles
        // that so a burst of connect/disconnect cycles stays cheap.
        if let Some(mut entry) = self.state.get_mut(key) {
            entry.active_streams = entry.active_streams.saturating_sub(1);
            tracing::trace!(
                target: "rate_limiting.stream",
                user_id = %key.0,
                conversation_id = %key.1,
                kind = %key.2,
                active_streams = entry.active_streams,
                "permit released"
            );
        }
    }
}

/// Minimal time abstraction so tests can drive the sliding-window check
/// with a mock clock without introducing a test-time dependency.
#[derive(Clone)]
enum Clock {
    Real,
    #[cfg(test)]
    Mock(Arc<std::sync::Mutex<Instant>>),
}

impl Clock {
    fn now(&self) -> Instant {
        match self {
            Clock::Real => Instant::now(),
            #[cfg(test)]
            Clock::Mock(m) => *m.lock().unwrap(),
        }
    }
}

impl fmt::Debug for Clock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Clock::Real => f.write_str("Clock::Real"),
            #[cfg(test)]
            Clock::Mock(_) => f.write_str("Clock::Mock"),
        }
    }
}

/// Public rate-limiter façade. Held in `AppState` as `Arc<StreamRateLimiter>`.
pub struct StreamRateLimiter {
    inner: Arc<StreamRateLimiterInner>,
}

impl fmt::Debug for StreamRateLimiter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamRateLimiter")
            .field("config", &self.inner.config)
            .field("buckets", &self.inner.state.len())
            .finish()
    }
}

impl StreamRateLimiter {
    /// Build a new limiter with the given config.
    pub fn new(config: StreamRateLimitConfig) -> Self {
        Self {
            inner: Arc::new(StreamRateLimiterInner {
                state: DashMap::new(),
                config,
                clock: Clock::Real,
            }),
        }
    }

    /// Test-only: construct with a mock clock.
    #[cfg(test)]
    fn with_mock_clock(
        config: StreamRateLimitConfig,
        clock_handle: Arc<std::sync::Mutex<Instant>>,
    ) -> Self {
        Self {
            inner: Arc::new(StreamRateLimiterInner {
                state: DashMap::new(),
                config,
                clock: Clock::Mock(clock_handle),
            }),
        }
    }

    /// The active configuration.
    pub fn config(&self) -> StreamRateLimitConfig {
        self.inner.config
    }

    /// Number of buckets currently in the map. Intended for cleanup-task
    /// tests and ops dashboards.
    pub fn bucket_count(&self) -> usize {
        self.inner.state.len()
    }

    /// Admission check. Evaluates the reconnect-rate sliding window, then
    /// the concurrent-stream cap. On success, returns a
    /// [`StreamPermit`] that the caller MUST hold for the stream
    /// lifetime. On failure, emits the matching rate-limit metric and
    /// returns [`RateLimitDecision::Deny`] with a precomputed
    /// `retry_after`.
    ///
    /// The `kind` axis isolates POST /chat (generate) from GET /events
    /// (resume) — they CAN overlap in time on a legitimate client and
    /// must not contend for the same slot. See module-level docs.
    pub fn check(
        &self,
        user_id: &str,
        conversation_id: &str,
        kind: StreamKind,
    ) -> RateLimitDecision {
        let key: BucketKey = (user_id.to_string(), conversation_id.to_string(), kind);
        let now = self.inner.clock.now();
        let config = self.inner.config;

        let mut entry = self
            .inner
            .state
            .entry(key.clone())
            .or_insert_with(RateLimitState::new);

        // --- 1. Prune expired reconnect log entries. ---
        while let Some(&front) = entry.reconnect_log.front() {
            if now.duration_since(front) >= config.window_duration {
                entry.reconnect_log.pop_front();
            } else {
                break;
            }
        }

        // --- 2. Reconnect-rate check. ---
        //
        // The list is kept to the most recent `max_reconnects_per_window`
        // admissions; once it fills up, a new admission must wait until
        // the oldest entry ages out of the window.
        if entry.reconnect_log.len() >= config.max_reconnects_per_window as usize {
            // Safe unwrap: non-empty by the `>= N` check above (N >= 1
            // enforced at startup).
            let oldest = *entry.reconnect_log.front().unwrap();
            let window_end = oldest + config.window_duration;
            let retry_after = window_end.saturating_duration_since(now);
            let retry_after = retry_after.max(Duration::from_secs(1));

            drop(entry); // release shard lock before metric emission

            record_stream_rate_limited(
                user_id,
                conversation_id,
                kind,
                DenyReason::ReconnectRateLimit,
            );
            tracing::warn!(
                target: "rate_limiting.stream",
                user_id = %user_id,
                conversation_id = %conversation_id,
                kind = %kind,
                reason = %DenyReason::ReconnectRateLimit,
                retry_after_secs = retry_after.as_secs(),
                "stream rate-limited: reconnect window full"
            );

            return RateLimitDecision::Deny {
                retry_after,
                reason: DenyReason::ReconnectRateLimit,
            };
        }

        // --- 3. Concurrent-stream check. ---
        if entry.active_streams >= config.max_concurrent_streams {
            drop(entry);

            record_stream_rate_limited(
                user_id,
                conversation_id,
                kind,
                DenyReason::ConcurrentStreamLimit,
            );
            tracing::warn!(
                target: "rate_limiting.stream",
                user_id = %user_id,
                conversation_id = %conversation_id,
                kind = %kind,
                reason = %DenyReason::ConcurrentStreamLimit,
                "stream rate-limited: concurrent cap reached"
            );

            return RateLimitDecision::Deny {
                retry_after: Duration::from_secs(1),
                reason: DenyReason::ConcurrentStreamLimit,
            };
        }

        // --- 4. Admit: log + bump + return permit. ---
        entry.reconnect_log.push_back(now);
        entry.active_streams += 1;

        // Explicitly drop the guard so the shard lock is released before
        // we return the permit — even though rust would drop at end-of-
        // function, spelling it out keeps the critical section visible.
        drop(entry);

        tracing::debug!(
            target: "rate_limiting.stream",
            user_id = %user_id,
            conversation_id = %conversation_id,
            kind = %kind,
            "stream admitted"
        );

        RateLimitDecision::Allow(StreamPermit {
            key: Some(key),
            inner: Arc::clone(&self.inner),
        })
    }

    /// Prune map entries that are idle. An entry is idle when
    /// `active_streams == 0` AND either the reconnect_log is empty OR
    /// its oldest entry is older than `2 * window_duration`.
    ///
    /// Returns the number of entries removed.
    pub fn cleanup_idle_entries(&self) -> usize {
        let now = self.inner.clock.now();
        let window = self.inner.config.window_duration;
        let retain_window = window.saturating_mul(2);

        // We cannot mutate the map while iterating, so collect keys to
        // remove first, then drop them. The two-pass pattern is cheap
        // because removal lists are small under normal load.
        let mut to_remove: Vec<BucketKey> = Vec::new();
        for entry in self.inner.state.iter() {
            let (key, state) = (entry.key(), entry.value());
            if state.active_streams != 0 {
                continue;
            }
            let log_empty_or_stale = match state.reconnect_log.front() {
                None => true,
                Some(&oldest) => now.duration_since(oldest) > retain_window,
            };
            if log_empty_or_stale {
                to_remove.push(key.clone());
            }
        }

        let mut removed = 0usize;
        for key in to_remove {
            // Re-check under the shard lock to avoid racing with a
            // concurrent `check()` that admitted a new stream between
            // the scan and the removal.
            let still_idle = self
                .inner
                .state
                .get(&key)
                .map(|state| {
                    if state.active_streams != 0 {
                        return false;
                    }
                    match state.reconnect_log.front() {
                        None => true,
                        Some(&oldest) => now.duration_since(oldest) > retain_window,
                    }
                })
                .unwrap_or(false);
            if still_idle && self.inner.state.remove(&key).is_some() {
                removed += 1;
            }
        }

        if removed > 0 {
            tracing::debug!(
                target: "rate_limiting.stream",
                removed,
                remaining_buckets = self.inner.state.len(),
                "cleanup pruned idle rate-limit buckets"
            );
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    fn test_config() -> StreamRateLimitConfig {
        StreamRateLimitConfig {
            max_concurrent_streams: 1,
            max_reconnects_per_window: 10,
            window_duration: Duration::from_secs(60),
        }
    }

    fn mock_clock(now: Instant) -> Arc<std::sync::Mutex<Instant>> {
        Arc::new(std::sync::Mutex::new(now))
    }

    fn advance(clock: &Arc<std::sync::Mutex<Instant>>, by: Duration) {
        let mut g = clock.lock().unwrap();
        *g += by;
    }

    /// 1. First stream per (user, conv) is admitted; active_streams=1.
    #[test]
    fn first_stream_allowed() {
        let limiter = StreamRateLimiter::new(test_config());
        let decision = limiter.check("alex", "conv-1", StreamKind::Generate);
        match decision {
            RateLimitDecision::Allow(permit) => {
                assert_eq!(
                    permit.key().unwrap(),
                    &(
                        "alex".to_string(),
                        "conv-1".to_string(),
                        StreamKind::Generate
                    )
                );
                let entry = limiter
                    .inner
                    .state
                    .get(&(
                        "alex".to_string(),
                        "conv-1".to_string(),
                        StreamKind::Generate,
                    ))
                    .unwrap();
                assert_eq!(entry.active_streams, 1);
                assert_eq!(entry.reconnect_log.len(), 1);
            }
            other => panic!("expected Allow, got {:?}", other),
        }
    }

    /// 2. A second concurrent stream on the same bucket is denied with
    ///    ConcurrentStreamLimit and a retry_after <= 1s.
    #[test]
    fn second_concurrent_stream_denied_with_short_retry() {
        let limiter = StreamRateLimiter::new(test_config());
        let _permit = match limiter.check("alex", "conv-1", StreamKind::Generate) {
            RateLimitDecision::Allow(p) => p,
            other => panic!("expected Allow, got {:?}", other),
        };
        match limiter.check("alex", "conv-1", StreamKind::Generate) {
            RateLimitDecision::Deny {
                retry_after,
                reason,
            } => {
                assert_eq!(reason, DenyReason::ConcurrentStreamLimit);
                assert!(
                    retry_after <= Duration::from_secs(1),
                    "retry_after should be short for concurrent-limit denials; got {:?}",
                    retry_after
                );
            }
            other => panic!("expected Deny, got {:?}", other),
        }
    }

    /// 3. After the first permit drops, the next stream is allowed again.
    #[test]
    fn permit_drop_frees_slot() {
        let limiter = StreamRateLimiter::new(test_config());
        {
            let _permit = match limiter.check("alex", "conv-1", StreamKind::Generate) {
                RateLimitDecision::Allow(p) => p,
                other => panic!("expected Allow, got {:?}", other),
            };
            // permit dropped at end of scope
        }
        match limiter.check("alex", "conv-1", StreamKind::Generate) {
            RateLimitDecision::Allow(_) => {}
            other => panic!("expected Allow after drop, got {:?}", other),
        }
    }

    /// 4. 10 reconnects inside the window are all admitted; the 11th is
    ///    denied with ReconnectRateLimit. Uses a mock clock so we do not
    ///    race a real wall clock.
    #[test]
    fn eleventh_reconnect_denied() {
        let clock = mock_clock(Instant::now());
        let limiter = StreamRateLimiter::with_mock_clock(test_config(), clock.clone());

        // Burn 10 admissions by admitting + dropping each permit so the
        // concurrent-stream check doesn't fire first.
        for _ in 0..10 {
            match limiter.check("alex", "conv-1", StreamKind::Generate) {
                RateLimitDecision::Allow(p) => drop(p),
                other => panic!("expected Allow, got {:?}", other),
            }
            advance(&clock, Duration::from_millis(100));
        }

        // 11th inside the same window → denied with ReconnectRateLimit.
        match limiter.check("alex", "conv-1", StreamKind::Generate) {
            RateLimitDecision::Deny {
                reason,
                retry_after,
            } => {
                assert_eq!(reason, DenyReason::ReconnectRateLimit);
                // Retry_after must be > 0 and <= window.
                assert!(retry_after > Duration::ZERO);
                assert!(retry_after <= Duration::from_secs(60));
            }
            other => panic!("expected Deny, got {:?}", other),
        }
    }

    /// 5. Once the window passes, the log is pruned and admissions
    ///    resume.
    #[test]
    fn window_expiration_resets_reconnect_log() {
        let clock = mock_clock(Instant::now());
        let limiter = StreamRateLimiter::with_mock_clock(test_config(), clock.clone());

        for _ in 0..10 {
            match limiter.check("alex", "conv-1", StreamKind::Generate) {
                RateLimitDecision::Allow(p) => drop(p),
                other => panic!("expected Allow, got {:?}", other),
            }
        }

        // 11th denied
        matches!(
            limiter.check("alex", "conv-1", StreamKind::Generate),
            RateLimitDecision::Deny {
                reason: DenyReason::ReconnectRateLimit,
                ..
            }
        );

        // Advance past the window. Pruning happens lazily on next check.
        advance(&clock, Duration::from_secs(61));
        match limiter.check("alex", "conv-1", StreamKind::Generate) {
            RateLimitDecision::Allow(_) => {}
            other => panic!("expected Allow after window, got {:?}", other),
        }
    }

    /// 6. Different users on the same conversation have independent
    ///    buckets.
    #[test]
    fn different_users_have_separate_buckets() {
        let limiter = StreamRateLimiter::new(test_config());
        let _p_alex = match limiter.check("alex", "conv-shared", StreamKind::Generate) {
            RateLimitDecision::Allow(p) => p,
            other => panic!("expected Allow for alex, got {:?}", other),
        };
        // Bob's check must NOT be throttled by alex's active stream.
        match limiter.check("bob", "conv-shared", StreamKind::Generate) {
            RateLimitDecision::Allow(_) => {}
            other => panic!("expected Allow for bob, got {:?}", other),
        }
    }

    /// 6b. (T-1BEAA41E) Generate and Resume on the SAME (user, conv)
    ///     must have independent slots. This is the regression test for
    ///     the 429 storm where the app held an SSE resume reader open
    ///     while issuing a POST /chat — the two contended on one permit
    ///     and the POST got a `concurrent_stream_limit` 429.
    #[test]
    fn generate_and_resume_have_separate_slots_per_conversation() {
        let limiter = StreamRateLimiter::new(test_config());

        // Hold a Resume permit open (simulating the durable SSE reader).
        let _resume = match limiter.check("alex", "conv-1", StreamKind::Resume) {
            RateLimitDecision::Allow(p) => p,
            other => panic!("expected Allow for resume, got {:?}", other),
        };

        // A Generate request on the SAME conversation MUST still be
        // admitted — different `kind` means different bucket.
        match limiter.check("alex", "conv-1", StreamKind::Generate) {
            RateLimitDecision::Allow(_) => {}
            other => panic!(
                "generate must not be blocked by an open resume stream, got {:?}",
                other
            ),
        }

        // But a second Resume on the same conversation WHILE one is open
        // must still be denied (within-kind contention is legitimate).
        match limiter.check("alex", "conv-1", StreamKind::Resume) {
            RateLimitDecision::Deny {
                reason: DenyReason::ConcurrentStreamLimit,
                ..
            } => {}
            other => panic!(
                "second resume on same conv should still be denied, got {:?}",
                other
            ),
        }
    }

    /// 7. Cleanup task removes buckets whose logs are stale and whose
    ///    active_streams is zero.
    #[test]
    fn cleanup_removes_idle_entries() {
        let clock = mock_clock(Instant::now());
        let limiter = StreamRateLimiter::with_mock_clock(test_config(), clock.clone());

        // Create two buckets; let both go idle (drop their permits).
        {
            let _p1 = match limiter.check("alex", "conv-a", StreamKind::Generate) {
                RateLimitDecision::Allow(p) => p,
                other => panic!("{:?}", other),
            };
            let _p2 = match limiter.check("bob", "conv-b", StreamKind::Generate) {
                RateLimitDecision::Allow(p) => p,
                other => panic!("{:?}", other),
            };
        }
        assert_eq!(limiter.bucket_count(), 2);

        // Cleanup BEFORE 2x window → should not remove.
        assert_eq!(limiter.cleanup_idle_entries(), 0);
        assert_eq!(limiter.bucket_count(), 2);

        // Advance past 2x window.
        advance(&clock, Duration::from_secs(121));
        let removed = limiter.cleanup_idle_entries();
        assert_eq!(removed, 2);
        assert_eq!(limiter.bucket_count(), 0);
    }

    /// 7b. Cleanup preserves buckets with an active stream even past
    ///     the stale-log threshold.
    #[test]
    fn cleanup_preserves_active_buckets() {
        let clock = mock_clock(Instant::now());
        let limiter = StreamRateLimiter::with_mock_clock(test_config(), clock.clone());

        let _permit = match limiter.check("alex", "conv-a", StreamKind::Generate) {
            RateLimitDecision::Allow(p) => p,
            other => panic!("{:?}", other),
        };

        advance(&clock, Duration::from_secs(300));
        let removed = limiter.cleanup_idle_entries();
        assert_eq!(removed, 0, "active bucket must not be cleaned up");
        assert_eq!(limiter.bucket_count(), 1);
    }

    /// 8. Even if a permit drop happens inside a catch_unwind scope
    ///    (simulating a panic during stream processing), the slot is
    ///    correctly released. This exercises the RAII contract.
    #[test]
    fn permit_drop_on_panic_releases_slot() {
        let limiter = StreamRateLimiter::new(test_config());

        let result = catch_unwind(AssertUnwindSafe(|| {
            let _permit = match limiter.check("alex", "conv-panic", StreamKind::Generate) {
                RateLimitDecision::Allow(p) => p,
                other => panic!("expected Allow, got {:?}", other),
            };
            // Simulate a stream handler panic mid-stream. The permit is
            // in scope so rustc inserts the drop even on unwind.
            panic!("simulated stream failure");
        }));
        assert!(result.is_err(), "expected the inner closure to panic");

        // Slot MUST be released — next check succeeds.
        match limiter.check("alex", "conv-panic", StreamKind::Generate) {
            RateLimitDecision::Allow(_) => {}
            other => panic!(
                "permit did not release on panic — follow-up check got {:?}",
                other
            ),
        }
    }

    /// StreamKind wire labels must match the metric label contract.
    #[test]
    fn stream_kind_wire_strings_are_stable() {
        assert_eq!(StreamKind::Generate.as_str(), "generate");
        assert_eq!(StreamKind::Resume.as_str(), "resume");
    }

    /// Deny wire strings must match the contract exposed via the 429
    /// JSON body and the Prometheus label. Dashboards alert on these
    /// strings, so we lock them down.
    #[test]
    fn deny_reason_wire_strings_are_stable() {
        assert_eq!(
            DenyReason::ConcurrentStreamLimit.as_wire_str(),
            "concurrent_stream_limit"
        );
        assert_eq!(
            DenyReason::ReconnectRateLimit.as_wire_str(),
            "reconnect_rate_limit"
        );
    }

    /// Config parse: defaults when env is empty.
    #[test]
    fn config_defaults_when_env_unset() {
        // Ensure no leaked values.
        std::env::remove_var("RATE_LIMIT_MAX_CONCURRENT_STREAMS");
        std::env::remove_var("RATE_LIMIT_MAX_RECONNECTS_PER_MIN");
        std::env::remove_var("RATE_LIMIT_WINDOW_SECS");

        let cfg = StreamRateLimitConfig::from_env();
        assert_eq!(cfg.max_concurrent_streams, 1);
        assert_eq!(cfg.max_reconnects_per_window, 10);
        assert_eq!(cfg.window_duration, Duration::from_secs(60));
    }
}
