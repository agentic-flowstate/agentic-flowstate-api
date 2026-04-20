//! Canonical SSE reconnect endpoint with Ably-style resume tokens
//! (T-2EDABDDD).
//!
//! Implements the durable-streaming protocol:
//!
//! * `GET /v1/conversations/:id/events` — canonical path.
//! * Accepts either `Last-Event-ID: N` HTTP header OR `?starting_after=N`
//!   query parameter as the resume cursor. Header wins when both are
//!   present (WHATWG EventSource spec + Ably precedence rule).
//! * Validates the cursor against the server-side retention floor
//!   reported by [`crate::retention::prune::earliest_retained_event_index`].
//! * Returns HTTP 410 Gone with a JSON body when the cursor sits below
//!   the floor:
//!   ```json
//!   {
//!     "reason": "cursor_expired",
//!     "earliest_event_index": <N>,
//!     "cursor": <cursor_value>,
//!     "retention_policy": {"age_days": 90, "min_keep": 10000}
//!   }
//!   ```
//! * Otherwise emits an SSE stream. Every frame carries
//!   `id: <event_index>` so that the standard EventSource reconnect
//!   path automatically sets `Last-Event-ID` on the next connection.
//! * Blob-offloaded payloads (T-E184E642) are materialized to real JSON
//!   via [`ticketing_system::conversations::load_event_payload_str`]
//!   before being shipped. Raw `{"$blob":...}` sentinels NEVER leak
//!   to clients.
//!
//! # iOS consumer notes (T-E51843B2)
//!
//! The iOS SSE reader should:
//!
//! 1. Hit `GET /api/v1/conversations/{id}/events` on initial connect
//!    (no cursor headers/params).
//! 2. On reconnect, set `Last-Event-ID: <last-seen-index>` — native
//!    `URLSession` does NOT do this automatically, the app must add
//!    the header itself. Alternatively append `?starting_after=N`.
//! 3. On HTTP 410 response, parse the JSON body, show a "history too
//!    old to resume" banner, and refetch full history via
//!    `GET /api/conversations/:id/messages`. Then open a fresh stream
//!    with no cursor.
//! 4. Any other non-200 status is a hard error (auth/forbidden) — do
//!    not retry without the user acting.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_stream::stream;
use axum::{
    extract::{Extension, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    Json,
};
use futures::stream::Stream;
use serde::Serialize;
use serde_json::json;
use ticketing_system::{checkpoints, conversations, SqlitePool};

use crate::auth_middleware::AuthenticatedUser;
use crate::observability::streaming::{
    record_cursor_expired, record_stream_closed, record_stream_event_emitted, record_stream_opened,
    DisconnectReason,
};
use crate::rate_limiting::{DenyReason, RateLimitDecision, StreamPermit, StreamRateLimiter};
use crate::retention::{self, prune as retention_prune, RetentionConfig};

use super::chat_stream::get_broadcast_sender;
use super::resume_cursor::{extract_cursor, CursorError, ResumeCursor, ResumeQuery};
use super::sse_keepalive::{wrap_stream_with_keepalive, KeepaliveConfig};

/// Canonical SSE reconnect endpoint:
/// `GET /v1/conversations/:id/events`.
///
/// # Resume precedence
///
/// The caller may resume from a specific event index using either:
///
/// | Source                      | How the client sets it            |
/// |-----------------------------|-----------------------------------|
/// | `Last-Event-ID: <N>` header | WHATWG EventSource default        |
/// | `?starting_after=N` query   | Fallback for native HTTP clients  |
///
/// The header wins when both are present. A client that supplies
/// neither gets a fresh stream from event 0.
///
/// # Status codes
///
/// * `200 OK` — SSE body. Each frame carries `id: <event_index>` so the
///   standard EventSource reconnect path automatically threads the
///   cursor on the next `Last-Event-ID` header.
/// * `400 Bad Request` — malformed cursor (non-numeric, negative).
/// * `404 Not Found` — conversation does not exist, or the authenticated
///   user does not own it.
/// * `410 Gone` — valid cursor but below the server-side retention
///   floor. Client must refetch full history and reconnect fresh.
///
/// # Frame `.id()` mapping
///
/// Every SSE frame emitted from the replay phase AND the live-tailing
/// phase is stamped with `.id(event_index.to_string())`. The browser
/// EventSource reads this into its `lastEventId` state and automatically
/// emits it as the `Last-Event-ID` header on the next reconnect — zero
/// client code required for basic resume. Native clients should mirror
/// this behavior.
pub async fn resume_conversation_stream(
    State(pool): State<Arc<SqlitePool>>,
    State(rate_limiter): State<Arc<StreamRateLimiter>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
    Query(query): Query<ResumeQuery>,
    headers: HeaderMap,
) -> Response {
    // ---- 1. Parse cursor from header or query. ----
    let cursor = match extract_cursor(&headers, &query) {
        Ok(c) => c,
        Err(CursorError::Malformed(detail)) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "malformed_cursor",
                    "detail": detail,
                })),
            )
                .into_response();
        }
        Err(CursorError::Retention { .. }) => {
            // extract_cursor does not produce Retention errors; this
            // branch exists only to satisfy the exhaustive match. If a
            // future refactor starts emitting Retention here, fall
            // through to the retention-validation step below.
            unreachable!("extract_cursor never produces CursorError::Retention");
        }
    };

    // ---- 2. Auth + conversation ownership. ----
    let conv = match conversations::get_conversation(&pool, &id, false).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "conversation_not_found"})),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(
                conversation_id = %id,
                error = %e,
                "resume_conversation_stream: conversation lookup failed"
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal_error"})),
            )
                .into_response();
        }
    };
    if conv.user_id != user.user_id {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "conversation_not_found"})),
        )
            .into_response();
    }

    // ---- 2b. Per-(user, conversation) SSE rate limit (T-C410DD96). ----
    //
    // Runs BEFORE retention validation + checkpoint lookup so a client
    // stuck in a reconnect loop can't amplify cheap 429 rejection into
    // expensive DB reads. The permit is RAII — storing it in the
    // `StreamCloseGuard` below means it drops exactly when the outer
    // `Sse` response is dropped (natural EOS, idle timeout, client
    // disconnect, or panic-unwind).
    let permit = match rate_limiter.check(&user.user_id, &id) {
        RateLimitDecision::Allow(permit) => permit,
        RateLimitDecision::Deny {
            retry_after,
            reason,
        } => {
            return rate_limited_response(retry_after, reason);
        }
    };

    // ---- 3. Retention validation: compare cursor to earliest retained. ----
    //
    // `earliest_retained_event_index` returns None when the conversation
    // has no events yet. On a fresh stream (`source == FreshStream`) we
    // skip the check entirely — there is no cursor to reject. For real
    // resumes we treat None as "no floor" (the conversation is empty,
    // the client is free to tail from wherever; nothing to replay
    // anyway).
    if cursor.is_resume() {
        match retention_prune::earliest_retained_event_index(&pool, &id).await {
            Ok(Some(earliest)) => {
                if cursor.event_index < earliest.saturating_sub(1) {
                    // The `-1` adjustment mirrors the contract in
                    // `earliest_retained_event_index`'s doc comment: a
                    // client that saw event N and asks for "everything
                    // strictly greater than N" should be allowed through
                    // even when N is exactly one below the floor — their
                    // next index (N+1) is still retained.
                    let cfg = RetentionConfig::from_env();
                    return cursor_expired_response(&id, cursor.event_index, earliest, &cfg);
                }
            }
            Ok(None) => {
                // Empty conversation — nothing to replay or reject. The
                // stream will open, send zero replay frames, and either
                // tail live events or close cleanly.
            }
            Err(e) => {
                tracing::error!(
                    conversation_id = %id,
                    error = %e,
                    "resume_conversation_stream: retention floor query failed"
                );
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "retention_lookup_failed"})),
                )
                    .into_response();
            }
        }
    }

    // ---- 4. Record stream.opened{resume=bool}. ----
    record_stream_opened(&id, &user.user_id, cursor.is_resume());

    // ---- 5. Snapshot checkpoint status so the replay stream knows
    //         whether to transition into live-tailing mode after catching
    //         up. Failure to read the checkpoint is not fatal — we default
    //         to "none" which skips the live phase. ----
    let checkpoint_status = match checkpoints::get_checkpoint(&pool, &id).await {
        Ok(Some(cp)) => cp.status,
        _ => "none".to_string(),
    };

    // ---- 6. Build the SSE body stream. ----
    //
    // Layer the stream from inside out:
    //   (a) `build_resume_stream`: replay + live-tail events
    //   (b) `StreamCloseGuard`: RAII metric emission on drop
    //   (c) `wrap_stream_with_keepalive`: Anthropic-vocab `ping` frames
    //       every 15s, 10-minute server-side idle timeout, graceful
    //       shutdown on SIGTERM (T-CFFAF032).
    //
    // The `.keep_alive(...)` call on `Sse` is set to 1h so axum's
    // built-in comment-ping generator NEVER fires — we own the ping
    // cadence end-to-end via the keepalive wrapper so frames are
    // named-event `ping` (which our iOS reader and idle-timer both
    // understand), not `: ping\n` comment lines.
    let inner = build_resume_stream(pool.clone(), id.clone(), cursor, checkpoint_status);
    let inner_boxed: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> =
        Box::pin(inner);
    let guarded = StreamCloseGuard::new(id.clone(), inner_boxed, Some(permit));
    let guarded_boxed: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> =
        Box::pin(guarded);
    let hardened = wrap_stream_with_keepalive(guarded_boxed, KeepaliveConfig::from_env(), id);

    Sse::new(Box::pin(hardened) as Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(3600)))
        .into_response()
}

/// JSON body the server sends when a client cursor sits below retention.
#[derive(Debug, Serialize)]
struct CursorExpiredBody {
    reason: &'static str,
    earliest_event_index: i64,
    cursor: i64,
    retention_policy: RetentionPolicySnapshot,
}

/// Public view of the retention configuration, shipped inside the 410
/// body so clients can diagnose "why did my cursor expire" without a
/// round-trip to the config endpoint.
#[derive(Debug, Serialize)]
struct RetentionPolicySnapshot {
    age_days: u32,
    min_keep: u64,
}

impl RetentionPolicySnapshot {
    fn from_config(cfg: &RetentionConfig) -> Self {
        Self {
            age_days: cfg.age_days,
            min_keep: cfg.min_keep_per_conversation,
        }
    }
}

/// Build the 410 Gone response body. Centralizes both the wire-format
/// body and the observability emit so that any future change to one
/// automatically updates the other.
fn cursor_expired_response(
    conversation_id: &str,
    cursor: i64,
    earliest: i64,
    cfg: &RetentionConfig,
) -> Response {
    // Observability: counter + structured warn log. Clamp to i32 for the
    // streaming metric API — the table schema uses i32 event_index and
    // saturation at i32::MAX is acceptable (clients can't exceed ~2B
    // events per conversation).
    let cursor_i32 = cursor.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
    let earliest_i32 = earliest.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
    record_cursor_expired(conversation_id, cursor_i32, earliest_i32);
    tracing::warn!(
        conversation_id = %conversation_id,
        cursor,
        earliest_retained = earliest,
        retention_age_days = cfg.age_days,
        retention_min_keep = cfg.min_keep_per_conversation,
        "resume cursor expired — emitting 410 Gone"
    );

    let body = CursorExpiredBody {
        reason: "cursor_expired",
        earliest_event_index: earliest,
        cursor,
        retention_policy: RetentionPolicySnapshot::from_config(cfg),
    };
    (StatusCode::GONE, Json(body)).into_response()
}

/// Build the SSE body generator: replay events strictly after the
/// cursor, then optionally tail live events until the agent finishes or
/// the stream idles out.
///
/// Tagging every frame with `.id(event_index)` is the load-bearing
/// contract: the client's next reconnect will automatically supply the
/// latest index as `Last-Event-ID`, giving us a continuous resumable
/// stream for free from the EventSource side.
fn build_resume_stream(
    db: Arc<SqlitePool>,
    conversation_id: String,
    cursor: ResumeCursor,
    checkpoint_status: String,
) -> impl Stream<Item = Result<Event, Infallible>> {
    stream! {
        // ---- Phase 1: replay stored events strictly after the cursor. ----
        //
        // `cursor.event_index == -1` means FreshStream, so the query
        // `event_index > -1` returns every retained event. `event_index > N`
        // returns everything strictly greater than N, which is exactly the
        // contract the client expects.
        let after = cursor.event_index.clamp(i32::MIN as i64, i32::MAX as i64) as i32;

        let replay = match conversations::get_events_after(&db, &conversation_id, after).await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(
                    conversation_id = %conversation_id,
                    cursor = cursor.event_index,
                    error = %e,
                    "resume stream: events_after query failed — emitting empty replay"
                );
                Vec::new()
            }
        };

        let mut last_event_index: i32 = after;
        for ev in &replay {
            last_event_index = ev.event_index;
            let payload = match conversations::load_event_payload_str(&db, ev).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(
                        conversation_id = %ev.conversation_id,
                        event_index = ev.event_index,
                        error = %e,
                        "resume stream: blob materialization failed — skipping frame"
                    );
                    continue;
                }
            };
            record_stream_event_emitted(&conversation_id, payload.len());
            yield Ok(Event::default()
                .id(ev.event_index.to_string())
                .event(ev.event_type.clone())
                .data(payload));
        }

        // ---- Phase 2: tail live events via the broadcast channel. ----
        //
        // Only run the live phase when the worker is actively producing
        // events. A stream that was fresh-reconnected to a completed
        // conversation closes cleanly after the replay.
        let worker_active =
            checkpoint_status == "running" || checkpoint_status == "pending";
        if !worker_active {
            return;
        }

        let broadcast_tx = get_broadcast_sender(&conversation_id).await;
        let mut broadcast_rx = broadcast_tx.subscribe();
        drop(broadcast_tx);

        let idle_timeout = Duration::from_secs(600);
        let sleep = tokio::time::sleep(idle_timeout);
        tokio::pin!(sleep);

        loop {
            tokio::select! {
                result = broadcast_rx.recv() => {
                    match result {
                        Ok((event_index, event_data)) => {
                            if event_index > last_event_index {
                                last_event_index = event_index;
                                sleep.as_mut().reset(tokio::time::Instant::now() + idle_timeout);
                                record_stream_event_emitted(&conversation_id, event_data.len());
                                yield Ok(Event::default()
                                    .id(event_index.to_string())
                                    .data(event_data));
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(
                                conversation_id = %conversation_id,
                                lag = n,
                                "resume stream: broadcast lagged, catching up from DB"
                            );
                            if let Ok(missed) = conversations::get_events_after(
                                &db,
                                &conversation_id,
                                last_event_index,
                            )
                            .await
                            {
                                for ev in &missed {
                                    last_event_index = ev.event_index;
                                    let payload = match conversations::load_event_payload_str(
                                        &db, ev,
                                    )
                                    .await
                                    {
                                        Ok(s) => s,
                                        Err(e) => {
                                            tracing::error!(
                                                conversation_id = %ev.conversation_id,
                                                event_index = ev.event_index,
                                                error = %e,
                                                "resume stream: lag-catchup materialization failed"
                                            );
                                            continue;
                                        }
                                    };
                                    record_stream_event_emitted(&conversation_id, payload.len());
                                    yield Ok(Event::default()
                                        .id(ev.event_index.to_string())
                                        .event(ev.event_type.clone())
                                        .data(payload));
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            // Worker finished. Drain anything persisted
                            // after the last broadcast event so the final
                            // turn's events do not get orphaned.
                            if let Ok(tail) = conversations::get_events_after(
                                &db,
                                &conversation_id,
                                last_event_index,
                            )
                            .await
                            {
                                for ev in &tail {
                                    let payload = match conversations::load_event_payload_str(
                                        &db, ev,
                                    )
                                    .await
                                    {
                                        Ok(s) => s,
                                        Err(e) => {
                                            tracing::error!(
                                                conversation_id = %ev.conversation_id,
                                                event_index = ev.event_index,
                                                error = %e,
                                                "resume stream: tail-drain materialization failed"
                                            );
                                            continue;
                                        }
                                    };
                                    record_stream_event_emitted(&conversation_id, payload.len());
                                    yield Ok(Event::default()
                                        .id(ev.event_index.to_string())
                                        .event(ev.event_type.clone())
                                        .data(payload));
                                }
                            }
                            break;
                        }
                    }
                }
                _ = &mut sleep => {
                    tracing::warn!(
                        conversation_id = %conversation_id,
                        "resume stream: idle timeout (10 min), closing"
                    );
                    break;
                }
            }
        }
    }
}

/// RAII drop guard that records `stream_closed_total` + duration when
/// the outer `Sse` response terminates (natural completion or client
/// disconnect). Also holds the rate-limit [`StreamPermit`] — dropping
/// the guard drops the permit, which decrements the concurrent-streams
/// gauge in the limiter bucket. The `_permit` field is named with a
/// leading underscore to signal it is drop-only (never read).
struct StreamCloseGuard {
    conversation_id: String,
    opened_at: std::time::Instant,
    reason: DisconnectReason,
    inner: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>,
    closed: bool,
    /// Rate-limit permit. `None` when rate limiting is disabled (the
    /// limiter was never installed, e.g. in unit tests). `Some` during
    /// normal operation. Drop order (struct-field top-to-bottom in
    /// Rust) runs `inner` before `_permit`, which is fine — we want
    /// the generator shut down before releasing the concurrent-streams
    /// slot.
    _permit: Option<StreamPermit>,
}

impl StreamCloseGuard {
    fn new(
        conversation_id: String,
        inner: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>,
        permit: Option<StreamPermit>,
    ) -> Self {
        Self {
            conversation_id,
            opened_at: std::time::Instant::now(),
            reason: DisconnectReason::ClientDisconnect,
            inner,
            closed: false,
            _permit: permit,
        }
    }
}

impl Stream for StreamCloseGuard {
    type Item = Result<Event, Infallible>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let poll = self.inner.as_mut().poll_next(cx);
        if matches!(poll, std::task::Poll::Ready(None)) {
            self.reason = DisconnectReason::Normal;
        }
        poll
    }
}

impl Drop for StreamCloseGuard {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        let duration_ms = self.opened_at.elapsed().as_millis().min(u64::MAX as u128) as u64;
        record_stream_closed(&self.conversation_id, duration_ms, self.reason);
    }
}

// Ensure the `retention` reference is exercised so unused-import lints
// stay quiet if the module is ever compiled without the retention
// feature gate.
#[doc(hidden)]
#[allow(dead_code)]
pub(crate) fn _retention_link() -> &'static str {
    let _cfg = retention::RetentionConfig::default();
    "linked"
}

/// Build the 429 Too Many Requests response used by both SSE
/// endpoints when the per-(user, conversation) limiter denies an open.
///
/// Wire contract:
///   * Status: `429 Too Many Requests`
///   * Header: `Retry-After: <seconds>` (RFC 9110 §10.2.3 integer form)
///   * Body:   `{"error":"rate_limited","reason":"...","retry_after_seconds":N}`
///
/// Exposed as `pub(crate)` so `chat_stream::chat` can share the exact
/// same contract without duplicating wire logic.
pub(crate) fn rate_limited_response(retry_after: Duration, reason: DenyReason) -> Response {
    // Round up so "less than 1s left" still reports as 1. Clients
    // comparing `retry-after` to a wall clock must never see 0 — that
    // would encourage an instant retry.
    let seconds = retry_after.as_secs().max(1);
    let reason_str = match reason {
        DenyReason::ConcurrentStreamLimit => "concurrent_stream_limit",
        DenyReason::ReconnectRateLimit => "reconnect_rate_limit",
    };
    let body = json!({
        "error": "rate_limited",
        "reason": reason_str,
        "retry_after_seconds": seconds,
    });

    let mut response = (StatusCode::TOO_MANY_REQUESTS, Json(body)).into_response();
    // Retry-After MUST be a non-negative integer number of seconds per
    // RFC 9110 §10.2.3. Parsing into HeaderValue via a formatted string
    // cannot fail for ASCII digits, so `.unwrap()` is sound here.
    response.headers_mut().insert(
        axum::http::header::RETRY_AFTER,
        axum::http::HeaderValue::from_str(&seconds.to_string()).unwrap(),
    );
    response
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    use axum::http::{HeaderMap, HeaderValue};
    use chrono::Utc;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    use super::super::resume_cursor::CursorSource;
    use crate::retention::prune as retention_prune;

    /// Build a test DB with the minimal schema the resume handler touches:
    /// conversations, conversation_events, event_blobs. No FK cascade
    /// needed — tests insert directly.
    async fn fresh_pool() -> Arc<SqlitePool> {
        let dsn = format!(
            "file:resume-test-{}?mode=memory&cache=shared",
            uuid::Uuid::new_v4()
        );
        let options = SqliteConnectOptions::from_str(&dsn)
            .unwrap()
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .unwrap();

        sqlx::query(
            r#"
            CREATE TABLE conversations (
                id TEXT PRIMARY KEY,
                user_id TEXT,
                session_id TEXT,
                organization TEXT,
                agent TEXT,
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
        .unwrap();

        sqlx::query(
            r#"
            CREATE TABLE conversation_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                event_index INTEGER NOT NULL,
                event_type TEXT NOT NULL,
                event_data TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                event_schema_version INTEGER NOT NULL DEFAULT 1,
                anthropic_event_type TEXT,
                backfill_source_event_id INTEGER,
                UNIQUE(conversation_id, event_index)
            )
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

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
        .unwrap();

        retention_prune::ensure_retention_index(&pool)
            .await
            .unwrap();

        Arc::new(pool)
    }

    async fn insert_conversation(pool: &SqlitePool, id: &str) {
        sqlx::query("INSERT INTO conversations (id, status) VALUES (?, 'open')")
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
    }

    async fn insert_event(
        pool: &SqlitePool,
        conversation_id: &str,
        event_index: i32,
        event_type: &str,
        event_data: &str,
    ) {
        let now = Utc::now().timestamp();
        sqlx::query(
            "INSERT INTO conversation_events \
             (conversation_id, event_index, event_type, event_data, created_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(conversation_id)
        .bind(event_index)
        .bind(event_type)
        .bind(event_data)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn seed_range(pool: &SqlitePool, conversation_id: &str, lo: i32, hi: i32) {
        for i in lo..hi {
            insert_event(
                pool,
                conversation_id,
                i,
                "text_delta",
                &format!("{{\"i\":{}}}", i),
            )
            .await;
        }
    }

    /// Drain an SSE stream into a vec of (id, event_type, data) triples.
    ///
    /// Implementation note: axum's [`Event`] type exposes no public
    /// getters for its fields. To inspect emitted events we wrap the
    /// inner stream in [`Sse::new`], call `into_response()`, and drain
    /// the HTTP body. The resulting byte stream is canonical SSE wire
    /// format — `id: N\n`, `event: TYPE\n`, `data: PAYLOAD\n\n` — which
    /// we parse with a small line scanner. Going through the real
    /// response path catches framing bugs the Debug-based helper cannot
    /// (e.g. embedded newlines inside `data:` being split into multiple
    /// `data:` lines per the SSE spec).
    /// Concrete alias for the SSE-inner stream shape. Pulling this out
    /// of the function signature silences `clippy::type_complexity` and
    /// keeps the test helper signatures readable.
    type SseInnerStream = Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>;

    async fn collect_stream(stream: SseInnerStream) -> Vec<(String, String, String)> {
        // Wrap in Sse and pull the HTTP body. Keep-alive interval is
        // set to an hour so the test never races with a `: ping\n`
        // frame that would confuse the wire-format parser.
        let sse: Sse<SseInnerStream> =
            Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(3600)));
        let resp = sse.into_response();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body drained");
        let text = String::from_utf8(bytes.to_vec()).expect("utf-8 wire format");
        parse_sse_text(&text)
    }

    /// Parse raw SSE wire text into a vec of (id, event_type, data)
    /// triples. One triple per SSE "event" (blank-line-separated block).
    /// Multiple `data:` lines inside a single event are joined with `\n`
    /// per the SSE spec.
    fn parse_sse_text(text: &str) -> Vec<(String, String, String)> {
        let mut out = Vec::new();
        let mut id = String::new();
        let mut event_type = String::new();
        let mut data_parts: Vec<String> = Vec::new();
        for line in text.split('\n') {
            // Trim the trailing `\r` from CRLF wire format.
            let line = line.trim_end_matches('\r');
            if line.is_empty() {
                if !id.is_empty() || !event_type.is_empty() || !data_parts.is_empty() {
                    out.push((
                        std::mem::take(&mut id),
                        std::mem::take(&mut event_type),
                        data_parts.join("\n"),
                    ));
                    data_parts.clear();
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("id:") {
                id = rest.trim_start().to_string();
            } else if let Some(rest) = line.strip_prefix("event:") {
                event_type = rest.trim_start().to_string();
            } else if let Some(rest) = line.strip_prefix("data:") {
                data_parts.push(rest.trim_start().to_string());
            }
            // `:` comment lines and anything else are ignored.
        }
        // Flush trailing event if the body did not end with a blank line.
        if !id.is_empty() || !event_type.is_empty() || !data_parts.is_empty() {
            out.push((id, event_type, data_parts.join("\n")));
        }
        out
    }

    // -------- Unit tests for the extract_cursor layer live in
    // resume_cursor.rs. The tests below exercise the DB-bound pieces:
    // retention validation, emission ordering, blob materialization,
    // and metrics. --------

    fn headers_with(map: &[(&str, &str)]) -> HeaderMap {
        use axum::http::HeaderName;
        let mut h = HeaderMap::new();
        for (k, v) in map {
            let name = HeaderName::from_bytes(k.as_bytes()).unwrap();
            h.insert(name, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    /// Retention 410: cursor below the earliest retained event index
    /// returns a 410 Gone response with the contract body shape.
    #[tokio::test]
    async fn retention_410_when_cursor_below_floor() {
        crate::observability::install_for_test();
        let pool = fresh_pool().await;
        insert_conversation(&pool, "c-ret").await;
        // Seed 10 events starting at index 100. Earliest retained = 100.
        for i in 100..110 {
            insert_event(&pool, "c-ret", i, "text_delta", "{}").await;
        }

        let earliest = retention_prune::earliest_retained_event_index(&pool, "c-ret")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(earliest, 100);

        let cfg = RetentionConfig::default();
        let resp = cursor_expired_response("c-ret", 50, earliest, &cfg);
        assert_eq!(resp.status(), StatusCode::GONE);

        let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["reason"], "cursor_expired");
        assert_eq!(body["earliest_event_index"], 100);
        assert_eq!(body["cursor"], 50);
        assert_eq!(body["retention_policy"]["age_days"], 90);
        assert_eq!(body["retention_policy"]["min_keep"], 10000);
    }

    /// Emission ordering: with cursor=50 and events 0..200, the stream
    /// emits 51..200 in order, each frame's id matches the event_index.
    #[tokio::test]
    async fn emission_order_matches_event_index() {
        let pool = fresh_pool().await;
        insert_conversation(&pool, "c-ord").await;
        seed_range(&pool, "c-ord", 0, 200).await;

        let cursor = ResumeCursor {
            event_index: 50,
            source: CursorSource::LastEventIdHeader,
        };
        let s = build_resume_stream(
            pool.clone(),
            "c-ord".to_string(),
            cursor,
            "none".to_string(),
        );
        let boxed: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> = Box::pin(s);
        let frames = collect_stream(boxed).await;

        // 200 - 51 = 149 frames expected (indices 51..=199).
        assert_eq!(frames.len(), 149, "frame count mismatch");

        for (i, (id, _etype, data)) in frames.iter().enumerate() {
            let expected = (51 + i) as i64;
            assert_eq!(id.as_str(), expected.to_string(), "frame {} id drift", i);
            assert!(
                data.contains(&format!("\"i\":{}", expected)),
                "frame {} data = {}",
                i,
                data
            );
        }
    }

    /// Fresh stream (cursor = -1) emits every retained event starting
    /// at index 0.
    #[tokio::test]
    async fn fresh_stream_emits_from_zero() {
        let pool = fresh_pool().await;
        insert_conversation(&pool, "c-fresh").await;
        seed_range(&pool, "c-fresh", 0, 5).await;

        let cursor = ResumeCursor {
            event_index: -1,
            source: CursorSource::FreshStream,
        };
        let s = build_resume_stream(
            pool.clone(),
            "c-fresh".to_string(),
            cursor,
            "none".to_string(),
        );
        let boxed: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> = Box::pin(s);
        let frames = collect_stream(boxed).await;

        assert_eq!(frames.len(), 5);
        assert_eq!(frames[0].0, "0");
        assert_eq!(frames[4].0, "4");
    }

    /// Blob materialization: when an event's event_data is a
    /// `{"$blob":N}` sentinel, the stream emits the materialized bytes
    /// from the blob table, not the sentinel.
    #[tokio::test]
    async fn blob_materialization_replaces_sentinel_on_wire() {
        let pool = fresh_pool().await;
        insert_conversation(&pool, "c-blob").await;

        // Insert one inline event and one blob-offloaded event. The
        // sentinel format must match ticketing_system's spec.
        insert_event(&pool, "c-blob", 0, "text_delta", "{\"i\":0}").await;
        // Blob row first (get its id for the sentinel).
        let now = Utc::now().timestamp();
        // Insert event row so we know its id for the blob FK.
        sqlx::query(
            "INSERT INTO conversation_events \
             (conversation_id, event_index, event_type, event_data, created_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind("c-blob")
        .bind(1i32)
        .bind("text_delta")
        .bind("__PLACEHOLDER__")
        .bind(now)
        .execute(&*pool)
        .await
        .unwrap();
        let event_id: i64 =
            sqlx::query_scalar("SELECT id FROM conversation_events WHERE event_index = 1")
                .fetch_one(&*pool)
                .await
                .unwrap();
        let payload_bytes = b"{\"materialized\":true}".to_vec();
        sqlx::query(
            "INSERT INTO event_blobs (event_id, mime, bytes, created_at) \
             VALUES (?, 'application/json', ?, ?)",
        )
        .bind(event_id)
        .bind(&payload_bytes)
        .bind(now)
        .execute(&*pool)
        .await
        .unwrap();
        let blob_id: i64 = sqlx::query_scalar("SELECT id FROM event_blobs WHERE event_id = ?")
            .bind(event_id)
            .fetch_one(&*pool)
            .await
            .unwrap();
        // Now rewrite the event_data column to the sentinel. The
        // canonical sentinel shape (see ticketing_system::conversations)
        // requires mime + size fields, not just blob id.
        let sentinel = format!(
            "{{\"$blob\":{},\"mime\":\"application/json\",\"size\":{}}}",
            blob_id,
            payload_bytes.len()
        );
        sqlx::query("UPDATE conversation_events SET event_data = ? WHERE id = ?")
            .bind(&sentinel)
            .bind(event_id)
            .execute(&*pool)
            .await
            .unwrap();

        let cursor = ResumeCursor {
            event_index: -1,
            source: CursorSource::FreshStream,
        };
        let s = build_resume_stream(
            pool.clone(),
            "c-blob".to_string(),
            cursor,
            "none".to_string(),
        );
        let boxed: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> = Box::pin(s);
        let frames = collect_stream(boxed).await;

        assert_eq!(frames.len(), 2);
        // Frame 1 must contain the materialized payload, not the sentinel.
        assert!(
            frames[1].2.contains("materialized"),
            "blob-offloaded frame did not materialize: got {:?}",
            frames[1].2
        );
        assert!(
            !frames[1].2.contains("$blob"),
            "sentinel leaked onto the wire: got {:?}",
            frames[1].2
        );
    }

    /// Metric emission: after one fresh stream open, one resume open,
    /// and one 410 rejection, the Prometheus counters should reflect
    /// each event.
    #[tokio::test]
    async fn metrics_reflect_one_fresh_one_resume_one_410() {
        use crate::observability::streaming::{METRIC_STREAM_CURSOR_EXPIRED, METRIC_STREAM_OPENED};
        let _guard = crate::observability::streaming::TEST_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::observability::install_for_test();

        let before = crate::observability::render_prometheus().unwrap_or_default();
        let expired_before = count_line(&before, METRIC_STREAM_CURSOR_EXPIRED).unwrap_or(0.0);
        let opened_resume_before =
            labeled_value(&before, METRIC_STREAM_OPENED, &[("resume", "true")]).unwrap_or(0.0);
        let opened_cold_before =
            labeled_value(&before, METRIC_STREAM_OPENED, &[("resume", "false")]).unwrap_or(0.0);

        // Two opens via the helper (mirrors the live handler path).
        crate::observability::streaming::record_stream_opened("c-x", "u", false);
        crate::observability::streaming::record_stream_opened("c-y", "u", true);
        // One 410 via cursor_expired_response.
        let cfg = RetentionConfig::default();
        let _ = cursor_expired_response("c-z", 5, 100, &cfg);

        let after = crate::observability::render_prometheus().unwrap();
        let expired_after = count_line(&after, METRIC_STREAM_CURSOR_EXPIRED).unwrap_or(0.0);
        let opened_resume_after =
            labeled_value(&after, METRIC_STREAM_OPENED, &[("resume", "true")]).unwrap_or(0.0);
        let opened_cold_after =
            labeled_value(&after, METRIC_STREAM_OPENED, &[("resume", "false")]).unwrap_or(0.0);

        assert_eq!((expired_after - expired_before) as u64, 1);
        assert_eq!((opened_resume_after - opened_resume_before) as u64, 1);
        assert_eq!((opened_cold_after - opened_cold_before) as u64, 1);
    }

    /// The `_retention_link` helper exists only to ensure the retention
    /// module path is referenced. A smoke test confirms it compiles
    /// and runs.
    #[test]
    fn retention_module_linked() {
        assert_eq!(_retention_link(), "linked");
    }

    // --- tiny Prometheus scrapers (duplicated from streaming::tests to
    // avoid making them pub) ---

    fn count_line(text: &str, name: &str) -> Option<f64> {
        for line in text.lines() {
            if line.starts_with('#') {
                continue;
            }
            // `continue` (not `?`) on non-matching lines — the first
            // non-matching line must not abort the whole scan.
            let Some(stripped) = line.strip_prefix(name) else {
                continue;
            };
            let first = stripped.chars().next();
            let without = match first {
                Some(' ') | Some('\t') => stripped.trim_start(),
                Some('{') => {
                    let inner = stripped.strip_prefix('{').unwrap_or(stripped);
                    match inner.strip_prefix('}') {
                        Some(tail) => tail.trim_start(),
                        None => continue,
                    }
                }
                _ => continue,
            };
            if let Some(v) = without
                .split_whitespace()
                .next()
                .and_then(|s| s.parse::<f64>().ok())
            {
                return Some(v);
            }
        }
        None
    }

    fn labeled_value(text: &str, name: &str, labels: &[(&str, &str)]) -> Option<f64> {
        for line in text.lines() {
            if line.starts_with('#') {
                continue;
            }
            let Some(rest) = line.strip_prefix(name) else {
                continue;
            };
            let Some(after) = rest.strip_prefix('{') else {
                continue;
            };
            let Some((label_sect, tail)) = after.split_once('}') else {
                continue;
            };
            let ok = labels
                .iter()
                .all(|(k, v)| label_sect.contains(&format!("{}=\"{}\"", k, v)));
            if !ok {
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

    /// Build a HeaderMap shortcut for the test below so the cursor
    /// extraction path is exercised end-to-end from headers_with through
    /// build_resume_stream.
    #[tokio::test]
    async fn header_cursor_triggers_replay_after_index() {
        let pool = fresh_pool().await;
        insert_conversation(&pool, "c-hdr").await;
        seed_range(&pool, "c-hdr", 0, 10).await;

        let headers = headers_with(&[("last-event-id", "4")]);
        let q = ResumeQuery::default();
        let cursor = extract_cursor(&headers, &q).unwrap();
        assert_eq!(cursor.event_index, 4);

        let s = build_resume_stream(
            pool.clone(),
            "c-hdr".to_string(),
            cursor,
            "none".to_string(),
        );
        let boxed: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> = Box::pin(s);
        let frames = collect_stream(boxed).await;
        assert_eq!(frames.len(), 5); // 5..=9
        assert_eq!(frames[0].0, "5");
        assert_eq!(frames[4].0, "9");
    }
}
