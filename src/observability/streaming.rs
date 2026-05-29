//! Durable-streaming observability (T-56987678).
//!
//! Typed emitters for every metric named in the task brief. Call sites
//! DO NOT write metric names as literals — they call the helper
//! functions in this module so names, labels, and types are centralized
//! and renameable in one place.
//!
//! ## Metric catalog
//!
//! | Name                                   | Type       | Purpose                                                         |
//! |----------------------------------------|------------|-----------------------------------------------------------------|
//! | `stream_opened_total`                  | counter    | Every SSE stream open, labeled by `resume` bool.                |
//! | `stream_concurrent`                    | gauge      | Current number of active SSE streams.                           |
//! | `stream_closed_total`                  | counter    | Every close, labeled by `reason`.                               |
//! | `stream_duration_ms`                   | histogram  | Wall-clock duration of each stream, labeled by close `reason`.  |
//! | `stream_events_emitted_total`          | counter    | Events pushed downstream on live or replay stream.              |
//! | `stream_bytes_emitted_total`           | counter    | Bytes of JSON payload shipped downstream.                       |
//! | `stream_resume_total`                  | counter    | SSE opens with `starting_after` / replay — the "resume" arm.    |
//! | `stream_cold_start_total`              | counter    | SSE opens WITHOUT a cursor — the "cold start" arm.              |
//! | `stream_resume_ratio`                  | gauge      | `resume_total / (resume_total + cold_start_total)`.             |
//! | `stream_cursor_expired_410_total`      | counter    | Rejects for `starting_after` below oldest_retained — 410 Gone.  |
//! | `push_sent_total`                      | counter    | APNs silent-push attempts, labeled by `result` + `apns_status`. |
//! | `events_gap_detected_total`            | counter    | event_index allocator saw a skip (missing index), by size.      |
//! | `clients_session_start_total`          | counter    | Client session_start events, labeled by platform+version.       |
//! | `ticket_preflight_total`               | counter    | Deterministic ticket preflight decisions by status/action.      |
//! | `ticket_preflight_duration_ms`         | histogram  | Wall-clock duration of deterministic ticket preflight.          |

use std::borrow::Cow;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use metrics::{counter, gauge, histogram};

// ---------------------------------------------------------------------------
// Metric name constants — centralized for grepability and rename safety.
// ---------------------------------------------------------------------------

pub const METRIC_STREAM_OPENED: &str = "stream_opened_total";
pub const METRIC_STREAM_CONCURRENT: &str = "stream_concurrent";
pub const METRIC_STREAM_CLOSED: &str = "stream_closed_total";
pub const METRIC_STREAM_DURATION_MS: &str = "stream_duration_ms";
pub const METRIC_STREAM_EVENTS_EMITTED: &str = "stream_events_emitted_total";
pub const METRIC_STREAM_BYTES_EMITTED: &str = "stream_bytes_emitted_total";
pub const METRIC_STREAM_RESUME: &str = "stream_resume_total";
pub const METRIC_STREAM_COLD_START: &str = "stream_cold_start_total";
pub const METRIC_STREAM_RESUME_RATIO: &str = "stream_resume_ratio";
pub const METRIC_STREAM_CURSOR_EXPIRED: &str = "stream_cursor_expired_410_total";
/// Counter: SSE stream admissions rejected by the per-(user,conv,kind)
/// rate limiter (T-C410DD96 / T-1BEAA41E). Labeled by:
///   * `reason` ∈ {`concurrent_stream_limit`, `reconnect_rate_limit`}
///   * `kind` ∈ {`generate`, `resume`} — which SSE path was denied
/// One increment per 429 Too Many Requests response. No per-user /
/// per-conversation label: the bucket cardinality would explode the
/// Prometheus registry.
pub const METRIC_STREAM_RATE_LIMITED: &str = "stream_rate_limited_total";
/// Counter: `ping` keepalive frames shipped downstream. Incremented by
/// the SSE keepalive wrapper (T-CFFAF032). One increment per frame.
pub const METRIC_STREAM_KEEPALIVE_SENT: &str = "stream_keepalive_sent_total";
pub const METRIC_PUSH_SENT: &str = "push_sent_total";
pub const METRIC_EVENTS_GAP_DETECTED: &str = "events_gap_detected_total";
pub const METRIC_CLIENTS_SESSION_START: &str = "clients_session_start_total";
pub const METRIC_TICKET_PREFLIGHT_TOTAL: &str = "ticket_preflight_total";
pub const METRIC_TICKET_PREFLIGHT_DURATION_MS: &str = "ticket_preflight_duration_ms";

// Retention prune metrics (T-65DA4D32). Emitted by `src/retention/prune.rs`
// after every scheduled sweep so operators can confirm (a) the prune is
// running, (b) it's keeping up with ingest volume, and (c) the fleet's
// oldest surviving event is inside the retention window — the precondition
// for the resume-token 410 Gone contract to be meaningful.
pub const METRIC_RETENTION_ROWS_DELETED: &str = "retention_rows_deleted_total";
pub const METRIC_RETENTION_PRUNE_DURATION_MS: &str = "retention_prune_duration_ms";
pub const METRIC_RETENTION_CONVERSATIONS_TOUCHED: &str = "retention_conversations_touched";
pub const METRIC_RETENTION_EARLIEST_AGE_SECS: &str =
    "retention_earliest_remaining_event_age_seconds";

// ---------------------------------------------------------------------------
// Enums for metric labels. Display impls produce the exact label string
// that Prometheus will see — no runtime string building at call sites.
// ---------------------------------------------------------------------------

/// Reason the SSE stream was closed. Single source of truth for the
/// `reason` label on `stream_closed_total` and `stream_duration_ms`.
///
/// Adding a variant requires updating the dashboard legend at
/// `observability/dashboards/durable-streaming.json`.
///
/// `#[allow(dead_code)]` is intentional: the enum is part of the
/// observability contract. Some variants are only constructed on rare
/// paths (keepalive-timeout on TCP half-close, error on DB poisoning)
/// and Cargo's dead-code analyzer would otherwise flag them. Keeping
/// the full taxonomy in one place is more valuable than chasing the
/// lint per-variant.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DisconnectReason {
    /// Client disconnected the TCP/HTTP connection before the server
    /// saw a natural end-of-stream (normal "user backgrounded the app"
    /// path on iOS).
    ClientDisconnect,
    /// Server-side idle timeout: no new events for the idle window.
    ServerIdleTimeout,
    /// Keep-alive ping failed to send — the transport is dead.
    KeepaliveTimeout,
    /// Client sent a `starting_after` cursor older than the oldest
    /// retained event and was served 410 Gone before the stream opened.
    CursorExpired,
    /// Server received SIGTERM / Ctrl+C and is draining active streams.
    /// The keepalive wrapper watches a process-wide `CancellationToken`
    /// and terminates the stream cleanly with a `message:end
    /// reason:server_shutdown` frame so the client can reconnect to the
    /// next replica (T-CFFAF032).
    ServerShutdown,
    /// Stream terminated due to an unhandled error (DB failure, broadcast
    /// panic, worker crash).
    Error,
    /// Stream completed normally — worker signaled terminal event and
    /// drained.
    Normal,
}

impl fmt::Display for DisconnectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DisconnectReason::ClientDisconnect => "client_disconnect",
            DisconnectReason::ServerIdleTimeout => "server_idle_timeout",
            DisconnectReason::KeepaliveTimeout => "keepalive_timeout",
            DisconnectReason::CursorExpired => "cursor_expired",
            DisconnectReason::ServerShutdown => "server_shutdown",
            DisconnectReason::Error => "error",
            DisconnectReason::Normal => "normal",
        })
    }
}

/// Outcome label for `push_sent_total`. Mirrors the failure taxonomy
/// used in `apns::silent_fanout::PerDeviceOutcome` but sits in the
/// observability layer so the APNs error types don't have to grow
/// Prometheus-specific traits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PushResult {
    /// HTTP 200 from APNs — the push was accepted.
    Success,
    /// HTTP 410 from APNs — the token is unregistered and was
    /// soft-deleted server-side. The same row will never be attempted
    /// again until the client re-registers.
    Unregistered,
    /// The device token was rejected as malformed, wrong-environment, etc.
    /// This is not enough evidence to soft-delete the token.
    BadToken,
    /// APNs told us we sent too many pushes — backpressure. Caller MUST
    /// NOT retry immediately; the apns-collapse-id already ensures at
    /// least the latest wake-up is delivered.
    Throttled,
    /// Any other failure bucket — transport, rejection, not-initialized.
    Error,
}

impl fmt::Display for PushResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            PushResult::Success => "success",
            PushResult::Unregistered => "unregistered",
            PushResult::BadToken => "bad_token",
            PushResult::Throttled => "throttled",
            PushResult::Error => "error",
        })
    }
}

/// Platform label for `clients_session_start_total`. Mirrors the
/// `device_tokens.platform` column — right now ios-only, but modeled as
/// an enum so adding web/android is a typed change, not a label
/// freetext.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClientPlatform {
    Ios,
    /// Web or desktop client (future-proofing — not yet emitted).
    Web,
    /// Unknown / unspecified. Emitted when the client skipped the
    /// `platform` field; dashboards use this bucket as a data-quality
    /// signal.
    Unknown,
}

impl fmt::Display for ClientPlatform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ClientPlatform::Ios => "ios",
            ClientPlatform::Web => "web",
            ClientPlatform::Unknown => "unknown",
        })
    }
}

impl ClientPlatform {
    /// Parse a case-insensitive client string into the enum. Unknown or
    /// empty values collapse to [`ClientPlatform::Unknown`] so the
    /// label space stays closed.
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "ios" => ClientPlatform::Ios,
            "web" | "macos" | "desktop" => ClientPlatform::Web,
            _ => ClientPlatform::Unknown,
        }
    }
}

// ---------------------------------------------------------------------------
// Resume-vs-cold-start ratio: two counters plus a gauge kept in sync.
// The gauge is derived; we update it atomically on every open.
// ---------------------------------------------------------------------------

static RESUME_OPENS: AtomicU64 = AtomicU64::new(0);
static COLD_OPENS: AtomicU64 = AtomicU64::new(0);

/// Observed a stream open. Updates three metrics in one call:
///   * `stream_opened_total{resume="..."}` counter +1
///   * `stream_concurrent` gauge +1
///   * `stream_resume_total` / `stream_cold_start_total` counter +1
///   * `stream_resume_ratio` gauge = resume / (resume + cold)
///
/// Emits a structured `tracing::info!` alongside the metrics so
/// operators can correlate logs with the dashboard.
pub fn record_stream_opened(conversation_id: &str, user_id: &str, resume: bool) {
    let resume_label: Cow<'static, str> = if resume {
        "true".into()
    } else {
        "false".into()
    };

    counter!(METRIC_STREAM_OPENED, "resume" => resume_label).increment(1);
    gauge!(METRIC_STREAM_CONCURRENT).increment(1.0);

    // Ratio side: pick the correct bucket.
    let (resumes, colds) = if resume {
        let r = RESUME_OPENS.fetch_add(1, Ordering::Relaxed) + 1;
        counter!(METRIC_STREAM_RESUME).increment(1);
        (r, COLD_OPENS.load(Ordering::Relaxed))
    } else {
        let c = COLD_OPENS.fetch_add(1, Ordering::Relaxed) + 1;
        counter!(METRIC_STREAM_COLD_START).increment(1);
        (RESUME_OPENS.load(Ordering::Relaxed), c)
    };
    let denom = (resumes + colds) as f64;
    let ratio = if denom > 0.0 {
        resumes as f64 / denom
    } else {
        0.0
    };
    gauge!(METRIC_STREAM_RESUME_RATIO).set(ratio);

    tracing::info!(
        target: "observability.streaming",
        event = "stream.opened",
        conversation_id = %conversation_id,
        user_id = %user_id,
        resume,
        resume_ratio = ratio,
        "stream opened"
    );
}

/// Observed a stream close. Updates:
///   * `stream_closed_total{reason="..."}` counter +1
///   * `stream_duration_ms{reason="..."}` histogram += duration_ms
///   * `stream_concurrent` gauge -1
pub fn record_stream_closed(conversation_id: &str, duration_ms: u64, reason: DisconnectReason) {
    let reason_label: Cow<'static, str> = reason.to_string().into();

    counter!(METRIC_STREAM_CLOSED, "reason" => reason_label.clone()).increment(1);
    histogram!(METRIC_STREAM_DURATION_MS, "reason" => reason_label).record(duration_ms as f64);
    gauge!(METRIC_STREAM_CONCURRENT).decrement(1.0);

    tracing::info!(
        target: "observability.streaming",
        event = "stream.closed",
        conversation_id = %conversation_id,
        duration_ms,
        reason = %reason,
        "stream closed"
    );
}

/// Observed an event shipped to an SSE subscriber. Tracks both count and
/// byte size so we can chart bandwidth per conversation independently of
/// event rate.
pub fn record_stream_event_emitted(conversation_id: &str, bytes: usize) {
    counter!(METRIC_STREAM_EVENTS_EMITTED).increment(1);
    counter!(METRIC_STREAM_BYTES_EMITTED).increment(bytes as u64);

    tracing::trace!(
        target: "observability.streaming",
        event = "stream.event_emitted",
        conversation_id = %conversation_id,
        bytes,
        "event emitted"
    );
}

/// Observed a `ping` keepalive frame being pushed downstream. Used by
/// the SSE keepalive wrapper (T-CFFAF032) so operators can chart ping
/// cadence per endpoint and catch stuck-interval regressions.
///
/// The counter is unlabeled — pings are uniform across all streams, and
/// adding a per-conversation label would explode cardinality. If you
/// need to localize a regression, correlate against the structured
/// `tracing::trace!` log which carries the conversation_id.
pub fn record_keepalive_sent(conversation_id: &str) {
    counter!(METRIC_STREAM_KEEPALIVE_SENT).increment(1);
    tracing::trace!(
        target: "observability.streaming",
        event = "stream.keepalive_sent",
        conversation_id = %conversation_id,
        "keepalive ping emitted"
    );
}

/// Observed a `starting_after` cursor that sits below the oldest
/// retained event. Emitted alongside the 410 Gone response.
pub fn record_cursor_expired(conversation_id: &str, requested_cursor: i32, oldest_retained: i32) {
    counter!(METRIC_STREAM_CURSOR_EXPIRED).increment(1);

    tracing::warn!(
        target: "observability.streaming",
        event = "stream.cursor_expired",
        conversation_id = %conversation_id,
        requested_cursor,
        oldest_retained,
        "cursor expired — emitting 410 Gone"
    );
}

/// Observed a rate-limit denial on an SSE stream open. Emitted by the
/// per-(user, conversation, kind) [`crate::rate_limiting::StreamRateLimiter`]
/// when it returns a `Deny` decision. The `reason` label is sourced
/// from the rate-limiter's `DenyReason::as_wire_str`, which is locked
/// down by a unit test so dashboards do not silently break on a
/// refactor. The `kind` label distinguishes POST /chat (generate) from
/// GET /events (resume) denials — see T-1BEAA41E.
///
/// `user_id` and `conversation_id` are intentionally logged (structured
/// tracing) but NOT attached as metric labels — high-cardinality labels
/// would blow up the Prometheus registry. Ops runs `grep` over the
/// tracing log when they need to diagnose a specific loop.
pub fn record_stream_rate_limited(
    user_id: &str,
    conversation_id: &str,
    kind: crate::rate_limiting::StreamKind,
    reason: crate::rate_limiting::DenyReason,
) {
    let reason_label: Cow<'static, str> = reason.as_wire_str().into();
    let kind_label: Cow<'static, str> = kind.as_str().into();
    counter!(
        METRIC_STREAM_RATE_LIMITED,
        "reason" => reason_label,
        "kind" => kind_label,
    )
    .increment(1);

    tracing::warn!(
        target: "observability.streaming",
        event = "stream.rate_limited",
        user_id = %user_id,
        conversation_id = %conversation_id,
        kind = %kind,
        reason = %reason,
        "stream rate-limited"
    );
}

/// Observed an APNs silent-push attempt. `apns_status` is the raw APNs
/// HTTP status as a number (200, 410, 429, ...) or 0 for client-side
/// failures that never reached the server.
pub fn record_push_sent(result: PushResult, apns_status: u16) {
    let result_label: Cow<'static, str> = result.to_string().into();
    let status_label: Cow<'static, str> = apns_status.to_string().into();

    counter!(
        METRIC_PUSH_SENT,
        "result" => result_label.clone(),
        "apns_status" => status_label.clone(),
    )
    .increment(1);

    tracing::info!(
        target: "observability.streaming",
        event = "push.sent",
        result = %result,
        apns_status,
        "push delivery observed"
    );
}

/// Observed a gap in the event_index allocator. `expected` is the index
/// the worker believed was next; `actual` is what the allocator
/// returned. `gap_size = actual - expected` is the number of skipped
/// indices (1 in the common case — never negative).
pub fn record_gap_detected(conversation_id: &str, expected: i32, actual: i32) {
    let gap_size = (actual - expected).max(0) as u64;
    if gap_size == 0 {
        return; // no gap — don't emit.
    }
    counter!(METRIC_EVENTS_GAP_DETECTED).increment(gap_size);

    tracing::warn!(
        target: "observability.streaming",
        event = "events.gap_detected",
        conversation_id = %conversation_id,
        expected,
        actual,
        gap_size,
        "event_index gap detected"
    );
}

// ---------------------------------------------------------------------------
// Deterministic ticket preflight metrics.
// ---------------------------------------------------------------------------

pub fn record_ticket_preflight(status: &str, action: &str, duration_ms: u64) {
    let status_label: Cow<'static, str> = status.to_string().into();
    let action_label: Cow<'static, str> = action.to_string().into();

    counter!(
        METRIC_TICKET_PREFLIGHT_TOTAL,
        "status" => status_label.clone(),
        "action" => action_label.clone(),
    )
    .increment(1);
    histogram!(
        METRIC_TICKET_PREFLIGHT_DURATION_MS,
        "status" => status_label.clone(),
        "action" => action_label.clone(),
    )
    .record(duration_ms as f64);

    tracing::info!(
        target: "observability.streaming",
        event = "ticket_preflight.completed",
        status,
        action,
        duration_ms,
        "deterministic ticket preflight completed"
    );
}

pub fn record_ticket_preflight_error(error_kind: &str, duration_ms: u64) {
    record_ticket_preflight("error", error_kind, duration_ms);
}

// ---------------------------------------------------------------------------
// Client session-start counter.
// ---------------------------------------------------------------------------

/// Observed a client session_start. Updates:
///   * `clients_session_start_total{platform, client_version}` counter +1
pub fn record_session_start(user_id: &str, platform: ClientPlatform, client_version: &str) {
    let platform_label: Cow<'static, str> = platform.to_string().into();
    let version_label: Cow<'static, str> = client_version.to_string().into();

    counter!(
        METRIC_CLIENTS_SESSION_START,
        "platform" => platform_label,
        "client_version" => version_label,
    )
    .increment(1);

    tracing::info!(
        target: "observability.streaming",
        event = "clients.session_start",
        user_id = %user_id,
        platform = %platform,
        client_version = %client_version,
        "client session start"
    );
}

// ---------------------------------------------------------------------------
// Test-only reset — both AtomicU64 ratio state vars live at module level,
// so a single test session will see values from every earlier test. We
// reset the counters before each test that reads them.
// ---------------------------------------------------------------------------

/// Reset the in-memory atomic resume/cold ratio counters. Test-only.
/// Does NOT reset Prometheus counters — those are additive by design
/// and are snapshot by the per-test render asserts.
#[cfg(test)]
pub(crate) fn reset_ratios_for_test() {
    RESUME_OPENS.store(0, Ordering::SeqCst);
    COLD_OPENS.store(0, Ordering::SeqCst);
}

/// Process-wide mutex used by tests that read Prometheus registry
/// deltas and/or mutate the shared atomic ratio counters. Cargo runs
/// unit tests in parallel by default and every test in this module
/// touches the SAME global `Lazy<Registry>` + AtomicU64 ratio vars,
/// so without serialization their snapshot-before / mutate / snapshot-after
/// pattern races and assertions flap.
///
/// Also locked by the `observability::tests` integration test since it
/// touches the same globals via `record_*` calls.
#[cfg(test)]
pub(crate) static TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    /// `DisconnectReason::Display` must produce the exact strings the
    /// dashboard legend and alerting queries are keyed off of. Locking
    /// the strings down with a unit test prevents an enum rename from
    /// silently breaking dashboards.
    #[test]
    fn disconnect_reason_display_strings_are_stable() {
        let cases = [
            (DisconnectReason::ClientDisconnect, "client_disconnect"),
            (DisconnectReason::ServerIdleTimeout, "server_idle_timeout"),
            (DisconnectReason::KeepaliveTimeout, "keepalive_timeout"),
            (DisconnectReason::CursorExpired, "cursor_expired"),
            (DisconnectReason::ServerShutdown, "server_shutdown"),
            (DisconnectReason::Error, "error"),
            (DisconnectReason::Normal, "normal"),
        ];
        for (variant, expected) in cases {
            assert_eq!(variant.to_string(), expected);
        }
    }

    #[test]
    fn push_result_display_strings_are_stable() {
        assert_eq!(PushResult::Success.to_string(), "success");
        assert_eq!(PushResult::Unregistered.to_string(), "unregistered");
        assert_eq!(PushResult::BadToken.to_string(), "bad_token");
        assert_eq!(PushResult::Throttled.to_string(), "throttled");
        assert_eq!(PushResult::Error.to_string(), "error");
    }

    #[test]
    fn client_platform_parse_closes_the_label_space() {
        assert_eq!(ClientPlatform::parse("iOS"), ClientPlatform::Ios);
        assert_eq!(ClientPlatform::parse("web"), ClientPlatform::Web);
        assert_eq!(ClientPlatform::parse("MacOS"), ClientPlatform::Web);
        assert_eq!(ClientPlatform::parse(""), ClientPlatform::Unknown);
        assert_eq!(ClientPlatform::parse("potato"), ClientPlatform::Unknown);
    }

    /// The gap recorder must be a no-op when the allocator returned
    /// the expected index. It's the hot path on every event; we never
    /// want it to emit a zero-gap counter inc.
    #[test]
    fn gap_detector_is_noop_when_indices_match() {
        let _guard = TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        super::super::install_for_test();
        // Snapshot before.
        let before = super::super::render_prometheus().unwrap_or_default();
        let before_gap = line_value(&before, METRIC_EVENTS_GAP_DETECTED).unwrap_or(0.0);

        record_gap_detected("conv-no-gap", 41, 41);

        let after = super::super::render_prometheus().unwrap_or_default();
        let after_gap = line_value(&after, METRIC_EVENTS_GAP_DETECTED).unwrap_or(0.0);
        assert_eq!(
            before_gap, after_gap,
            "gap counter moved from {} to {} on a zero-sized gap",
            before_gap, after_gap
        );
    }

    /// A real gap must increment by exactly `actual - expected`.
    #[test]
    fn gap_detector_increments_by_gap_size() {
        let _guard = TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        super::super::install_for_test();
        let before = super::super::render_prometheus().unwrap_or_default();
        let before_gap = line_value(&before, METRIC_EVENTS_GAP_DETECTED).unwrap_or(0.0);

        record_gap_detected("conv-gap-3", 10, 13); // gap of 3

        let after = super::super::render_prometheus().unwrap_or_default();
        let after_gap = line_value(&after, METRIC_EVENTS_GAP_DETECTED).unwrap_or(0.0);
        assert_eq!((after_gap - before_gap) as u64, 3);
    }

    /// session_start must increment the labeled counter (platform, client_version).
    #[tokio::test]
    async fn session_start_increments_counter() {
        let _guard = TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        super::super::install_for_test();

        let before = super::super::render_prometheus().unwrap_or_default();
        let before_v = labeled_counter(
            &before,
            METRIC_CLIENTS_SESSION_START,
            &[("platform", "ios"), ("client_version", "17")],
        )
        .unwrap_or(0.0);

        record_session_start("u1", ClientPlatform::Ios, "17");
        record_session_start("u1", ClientPlatform::Ios, "17");

        let after = super::super::render_prometheus().expect("exporter installed");
        let after_v = labeled_counter(
            &after,
            METRIC_CLIENTS_SESSION_START,
            &[("platform", "ios"), ("client_version", "17")],
        )
        .expect("counter present");
        assert_eq!((after_v - before_v) as u64, 2);
    }

    /// resume vs cold-start ratio gauge: 2 resume + 2 cold = 0.5
    #[tokio::test]
    async fn resume_cold_ratio_gauge_matches_counter_ratio() {
        let _guard = TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        super::super::install_for_test();
        reset_ratios_for_test();

        record_stream_opened("c1", "u1", true);
        record_stream_opened("c1", "u1", false);
        record_stream_opened("c2", "u1", true);
        record_stream_opened("c2", "u1", false);

        let text = super::super::render_prometheus().expect("exporter installed");
        let ratio = line_value(&text, METRIC_STREAM_RESUME_RATIO).expect("gauge emitted");
        assert!(
            (ratio - 0.5).abs() < 1e-9,
            "expected 0.5, got {}\n{}",
            ratio,
            text
        );
    }

    /// stream_closed records the duration in the histogram bucket
    /// tagged with the reason label.
    #[tokio::test]
    async fn stream_closed_increments_reason_counter() {
        let _guard = TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        super::super::install_for_test();

        let before = super::super::render_prometheus().unwrap_or_default();
        let before_normal =
            labeled_counter(&before, METRIC_STREAM_CLOSED, &[("reason", "normal")]).unwrap_or(0.0);

        record_stream_closed("c-close", 1234, DisconnectReason::Normal);

        let after = super::super::render_prometheus().expect("exporter installed");
        let after_normal = labeled_counter(&after, METRIC_STREAM_CLOSED, &[("reason", "normal")])
            .expect("counter present");
        assert_eq!((after_normal - before_normal) as u64, 1);

        // Histogram is exported as `_sum{reason=...}` + `_count{reason=...}`
        // + per-bucket lines. Sum for reason="normal" must have gone up by
        // 1234. Labeled so we route through `labeled_counter` (which handles
        // any metric family with a label set, not just counters).
        let sum_metric = format!("{}_sum", METRIC_STREAM_DURATION_MS);
        let sum_before =
            labeled_counter(&before, &sum_metric, &[("reason", "normal")]).unwrap_or(0.0);
        let sum_after =
            labeled_counter(&after, &sum_metric, &[("reason", "normal")]).unwrap_or(0.0);
        assert!(
            (sum_after - sum_before - 1234.0).abs() < 1e-6,
            "histogram sum delta expected ~1234, got {}",
            sum_after - sum_before,
        );
    }

    #[tokio::test]
    async fn push_sent_counter_increments_per_result() {
        let _guard = TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        super::super::install_for_test();

        let before = super::super::render_prometheus().unwrap_or_default();
        let before_succ = labeled_counter(
            &before,
            METRIC_PUSH_SENT,
            &[("result", "success"), ("apns_status", "200")],
        )
        .unwrap_or(0.0);

        record_push_sent(PushResult::Success, 200);
        record_push_sent(PushResult::Success, 200);
        record_push_sent(PushResult::Unregistered, 410);

        let after = super::super::render_prometheus().expect("exporter installed");
        let after_succ = labeled_counter(
            &after,
            METRIC_PUSH_SENT,
            &[("result", "success"), ("apns_status", "200")],
        )
        .expect("counter present");
        assert_eq!((after_succ - before_succ) as u64, 2);

        let unregistered = labeled_counter(
            &after,
            METRIC_PUSH_SENT,
            &[("result", "unregistered"), ("apns_status", "410")],
        )
        .expect("unregistered counter present");
        assert!(unregistered >= 1.0);
    }

    #[tokio::test]
    async fn cursor_expired_counter_increments() {
        let _guard = TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        super::super::install_for_test();

        let before = super::super::render_prometheus().unwrap_or_default();
        let before_v = line_value(&before, METRIC_STREAM_CURSOR_EXPIRED).unwrap_or(0.0);

        record_cursor_expired("c-old", 5, 100);
        record_cursor_expired("c-old", 7, 100);

        let after = super::super::render_prometheus().expect("exporter installed");
        let after_v = line_value(&after, METRIC_STREAM_CURSOR_EXPIRED).unwrap_or(0.0);
        assert_eq!((after_v - before_v) as u64, 2);
    }

    #[tokio::test]
    async fn event_emitted_tracks_count_and_bytes() {
        let _guard = TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        super::super::install_for_test();

        let before = super::super::render_prometheus().unwrap_or_default();
        let before_count = line_value(&before, METRIC_STREAM_EVENTS_EMITTED).unwrap_or(0.0);
        let before_bytes = line_value(&before, METRIC_STREAM_BYTES_EMITTED).unwrap_or(0.0);

        record_stream_event_emitted("c-payload", 128);
        record_stream_event_emitted("c-payload", 256);

        let after = super::super::render_prometheus().expect("exporter installed");
        let after_count = line_value(&after, METRIC_STREAM_EVENTS_EMITTED).unwrap_or(0.0);
        let after_bytes = line_value(&after, METRIC_STREAM_BYTES_EMITTED).unwrap_or(0.0);

        assert_eq!((after_count - before_count) as u64, 2);
        assert_eq!((after_bytes - before_bytes) as u64, 384);
    }

    // --- tiny Prometheus text scraper for assertions ---------------------

    /// Parse a Prometheus text-format value for a simple (unlabeled)
    /// metric. Returns None when the metric is absent.
    fn line_value(text: &str, name: &str) -> Option<f64> {
        for line in text.lines() {
            if line.starts_with('#') {
                continue;
            }
            // Accept either "name VALUE" or "name{} VALUE". Must NOT
            // match `name_sum`, `name_count`, `name_bucket`, or any other
            // metric that happens to share a prefix — nor should a
            // non-matching line abort the whole scan.
            let stripped = match line.strip_prefix(name) {
                Some(s) => s,
                None => continue,
            };
            // Reject prefix-only matches: next char must be ASCII space
            // (unlabeled) or '{' (labeled).
            let first = stripped.chars().next();
            let without_labels = match first {
                Some(' ') | Some('\t') => stripped.trim_start(),
                Some('{') => {
                    // Only accept the exact empty-label form `name{} VALUE`
                    // here — labeled series route through `labeled_counter`.
                    let inner = stripped.strip_prefix('{').unwrap_or(stripped);
                    match inner.strip_prefix('}') {
                        Some(tail) => tail.trim_start(),
                        None => continue,
                    }
                }
                _ => continue,
            };
            if let Some(val) = without_labels
                .split_whitespace()
                .next()
                .and_then(|s| s.parse::<f64>().ok())
            {
                return Some(val);
            }
        }
        None
    }

    /// Parse the value of a labeled counter by substring-matching the
    /// label pairs. Intentionally forgiving of label ordering — the
    /// Prometheus exporter does not guarantee deterministic order.
    fn labeled_counter(text: &str, name: &str, labels: &[(&str, &str)]) -> Option<f64> {
        for line in text.lines() {
            if line.starts_with('#') {
                continue;
            }
            // The text format is `name{k1="v1",k2="v2"} VALUE`. We need
            // the immediate next char after `name` to be `{` so that
            // `stream_opened_total_bucket` doesn't match `stream_opened_total`.
            let rest = match line.strip_prefix(name) {
                Some(r) => r,
                None => continue,
            };
            let after_brace = match rest.strip_prefix('{') {
                Some(r) => r,
                None => continue,
            };
            let (label_sect, tail) = match after_brace.split_once('}') {
                Some(parts) => parts,
                None => continue,
            };
            let all_present = labels.iter().all(|(k, v)| {
                let needle = format!("{}=\"{}\"", k, v);
                label_sect.contains(&needle)
            });
            if !all_present {
                continue;
            }
            if let Some(val) = tail
                .split_whitespace()
                .next()
                .and_then(|s| s.parse::<f64>().ok())
            {
                return Some(val);
            }
        }
        None
    }
}
