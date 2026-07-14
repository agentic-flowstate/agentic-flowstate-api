//! SSE production hardening — keepalive pings, idle timeout, graceful
//! disconnect (T-CFFAF032).
//!
//! # Why not `axum::response::sse::KeepAlive`?
//!
//! axum's built-in `KeepAlive` helper only emits `: ping\n\n` comment
//! lines. That works for keeping TCP middleboxes alive, but:
//!
//! 1. the protocol's 8-event streaming vocabulary (A-18DB4221) defines
//!    `ping` as a proper named event with `event: ping\ndata: {}`, not a
//!    comment. Clients that parse by event-type (including our own iOS
//!    reader) ignore comment pings and will still trip their own idle
//!    timers.
//! 2. There's no hook to close the stream cleanly on server-side idle,
//!    so the only exit path is the client's read timeout — which wastes
//!    a reconnect round-trip and spams `stream_closed_total{reason="
//!    client_disconnect"}`.
//! 3. There's no integration with our process-wide `CancellationToken`
//!    for SIGTERM draining.
//!
//! This module wraps an arbitrary `Stream<Item = Event>` and multiplexes
//! three concerns:
//!
//! * **Keepalive cadence** — fires every `interval` (default 15s) and
//!   emits `event: ping\ndata: {}` (no `.id()` so EventSource's
//!   `lastEventId` cursor does NOT advance past a ping).
//! * **Idle timeout** — if no REAL event (not a ping) has been emitted
//!   for `idle_timeout` (default 10min), emits one terminal
//!   `event: message\ndata: {"type":"end","reason":"server_idle_timeout"}`
//!   frame and closes.
//! * **Graceful shutdown** — watches a shared `CancellationToken`. When
//!   the token fires (SIGTERM/Ctrl+C), emits
//!   `event: message\ndata: {"type":"end","reason":"server_shutdown"}`
//!   and closes.
//!
//! # Reset semantics for the keepalive interval
//!
//! A real event RESETS the ping timer. Rationale: the ping exists to
//! prove liveness to middleboxes. If the server just shipped a real
//! event, middleboxes already saw bytes — a ping in the next 15s window
//! is redundant bandwidth. This matches the protocol's own pattern (their
//! streaming endpoint sends `ping` only during idle gaps) and Ably's
//! heartbeat docs.
//!
//! Idle-timeout resets on REAL events only (not pings) — otherwise the
//! stream would be immortal.
//!
//! # Frame identity rules
//!
//! * **Ping frames** — `.event("ping").data("{}")`. NO `.id(N)`. If we
//!   stamped an id, EventSource would store it as `lastEventId` and
//!   advance the cursor, causing a resume to skip real events.
//! * **End frames** — `.event("message").data(r#"{"type":"end",...}"#)`.
//!   Also no `.id(N)` — the end marker is ephemeral, clients should not
//!   try to "resume past" it.
//!
//! # Configuration
//!
//! | Env var                         | Default | Purpose                              |
//! |---------------------------------|---------|--------------------------------------|
//! | `SSE_KEEPALIVE_INTERVAL_SECS`   | 15      | Ping cadence.                        |
//! | `SSE_IDLE_TIMEOUT_SECS`         | 600     | No-real-event timeout (10 min).      |
//!
//! Malformed values fall back to defaults and emit a `tracing::warn!`.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::OnceLock;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::response::sse::Event;
use futures::stream::Stream;
use tokio::time::{interval, sleep, Interval, Sleep};
use tokio_util::sync::CancellationToken;

use crate::observability::streaming::{record_keepalive_sent, DisconnectReason};

/// Default keepalive ping cadence (15 seconds).
pub const DEFAULT_KEEPALIVE_INTERVAL_SECS: u64 = 15;

/// Default server-side idle timeout (10 minutes).
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 600;

/// Environment variable name for the keepalive ping cadence.
pub const ENV_KEEPALIVE_INTERVAL: &str = "SSE_KEEPALIVE_INTERVAL_SECS";

/// Environment variable name for the idle-no-real-event timeout.
pub const ENV_IDLE_TIMEOUT: &str = "SSE_IDLE_TIMEOUT_SECS";

/// Wire-format payload for ping frames. The object is intentionally
/// empty so that the conversation protocol stays byte-identical to
/// `{"type":"ping"}` when clients serialize the event back through
/// their own parsers — the `event: ping` line plus `data: {}` matches
/// the shape the protocol's SDK emits.
pub const PING_DATA: &str = "{}";

/// Wire-format payload for the terminal `message:end reason:...` frame.
/// Kept as a function so call sites don't build strings themselves and
/// the reason label stays in the DisconnectReason taxonomy.
fn end_frame_json(reason: DisconnectReason) -> String {
    format!(r#"{{"type":"end","reason":"{}"}}"#, reason)
}

/// Runtime configuration for [`KeepaliveStream`].
#[derive(Debug, Clone, Copy)]
pub struct KeepaliveConfig {
    /// Cadence for `ping` frames. Real events reset this timer (see
    /// module docs).
    pub interval: Duration,
    /// Window of no-real-events after which the stream is terminated
    /// with `reason:server_idle_timeout`. Pings do NOT reset this
    /// timer.
    pub idle_timeout: Duration,
}

impl Default for KeepaliveConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(DEFAULT_KEEPALIVE_INTERVAL_SECS),
            idle_timeout: Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS),
        }
    }
}

impl KeepaliveConfig {
    /// Resolve configuration from the environment, falling back to
    /// sane defaults. A parse failure logs a warn and uses the default
    /// for that slot — one bad env var never crashes the SSE stack.
    pub fn from_env() -> Self {
        Self {
            interval: parse_env_duration(ENV_KEEPALIVE_INTERVAL, DEFAULT_KEEPALIVE_INTERVAL_SECS),
            idle_timeout: parse_env_duration(ENV_IDLE_TIMEOUT, DEFAULT_IDLE_TIMEOUT_SECS),
        }
    }
}

fn parse_env_duration(name: &str, default_secs: u64) -> Duration {
    match std::env::var(name) {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(secs) if secs > 0 => Duration::from_secs(secs),
            Ok(_) => {
                tracing::warn!(
                    env = %name,
                    value = %raw,
                    default_secs,
                    "SSE keepalive env var must be > 0 — using default"
                );
                Duration::from_secs(default_secs)
            }
            Err(e) => {
                tracing::warn!(
                    env = %name,
                    value = %raw,
                    error = %e,
                    default_secs,
                    "SSE keepalive env var is not a positive integer — using default"
                );
                Duration::from_secs(default_secs)
            }
        },
        Err(_) => Duration::from_secs(default_secs),
    }
}

/// Process-wide cancellation token used by the keepalive wrapper to
/// detect SIGTERM and close active streams with a `server_shutdown`
/// end frame. `main.rs` installs the token at startup via
/// [`install_shutdown_token`]; handlers read it via
/// [`shutdown_token`]. When the token is absent (unit tests that
/// don't install one) the wrapper simply never observes a shutdown —
/// a [`CancellationToken::new()`] that never fires is substituted.
static SHUTDOWN_TOKEN: OnceLock<CancellationToken> = OnceLock::new();

/// Install the process-wide shutdown token. Idempotent — the second
/// call returns `Err(())` so a misconfigured startup surfaces instead
/// of silently ignoring the second handle.
pub fn install_shutdown_token(token: CancellationToken) -> Result<(), ()> {
    SHUTDOWN_TOKEN.set(token).map_err(|_| ())
}

/// Fetch the shutdown token, returning a never-cancelled handle when
/// no token has been installed. The returned handle is always safe to
/// `cancelled().await` on — it simply parks forever in the no-install
/// case.
pub fn shutdown_token() -> CancellationToken {
    SHUTDOWN_TOKEN.get().cloned().unwrap_or_default()
}

/// Wrap an SSE event stream with keepalive pings, idle timeout, and
/// graceful shutdown. Returns a stream that emits the same
/// `Result<Event, Infallible>` items as the inner stream, interleaved
/// with ping frames, and terminates with a single `message:end` frame
/// on idle timeout or shutdown.
///
/// The `conversation_id` is used for structured logs and the
/// `StreamCloseGuard`; the wrapper does not embed it in any wire-format
/// field.
///
/// The returned stream captures its own `CancellationToken` handle by
/// cloning from [`shutdown_token()`]. Tests that need to exercise the
/// shutdown path use [`wrap_stream_with_keepalive_and_token`] to pass
/// an explicit handle.
pub fn wrap_stream_with_keepalive<S>(
    inner: S,
    config: KeepaliveConfig,
    conversation_id: String,
) -> KeepaliveStream<S>
where
    S: Stream<Item = Result<Event, Infallible>> + Unpin + Send + 'static,
{
    wrap_stream_with_keepalive_and_token(inner, config, conversation_id, shutdown_token())
}

/// Test-friendly variant that accepts an explicit shutdown token.
pub fn wrap_stream_with_keepalive_and_token<S>(
    inner: S,
    config: KeepaliveConfig,
    conversation_id: String,
    shutdown: CancellationToken,
) -> KeepaliveStream<S>
where
    S: Stream<Item = Result<Event, Infallible>> + Unpin + Send + 'static,
{
    let mut ping_tick = interval(config.interval);
    // `Interval` fires immediately on the first `.tick()` by default; we
    // want the FIRST ping at t=interval, not t=0, so skip the first tick.
    ping_tick.reset();
    let idle_sleep = Box::pin(sleep(config.idle_timeout));

    KeepaliveStream {
        inner,
        ping_tick,
        idle_sleep,
        idle_timeout: config.idle_timeout,
        shutdown,
        conversation_id,
        closed: false,
        pending_terminal: None,
    }
}

/// Stream combinator that multiplexes (inner events | ping tick | idle
/// watchdog | shutdown signal). See module docs for the ruleset.
///
/// `S::Item` is `Result<Event, Infallible>` because that matches the
/// concrete `Sse::new` input shape the axum handlers already use.
pub struct KeepaliveStream<S> {
    inner: S,
    ping_tick: Interval,
    idle_sleep: Pin<Box<Sleep>>,
    idle_timeout: Duration,
    shutdown: CancellationToken,
    conversation_id: String,
    /// Once we've emitted a terminal frame (idle or shutdown) the next
    /// poll must return Ready(None) to signal end-of-stream to axum.
    closed: bool,
    /// If the inner stream is ready simultaneously with a terminal
    /// condition, we buffer the terminal frame here so the NEXT poll
    /// emits it before returning end-of-stream.
    pending_terminal: Option<Event>,
}

impl<S> Stream for KeepaliveStream<S>
where
    S: Stream<Item = Result<Event, Infallible>> + Unpin,
{
    type Item = Result<Event, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.closed {
            return Poll::Ready(None);
        }

        // If we buffered a terminal frame on the previous poll, ship it
        // now and mark the stream closed so the NEXT poll returns None.
        if let Some(ev) = self.pending_terminal.take() {
            self.closed = true;
            return Poll::Ready(Some(Ok(ev)));
        }

        // 1. Shutdown signal takes priority. If SIGTERM has fired we want
        //    to emit the shutdown end-frame and close even if the inner
        //    stream is still producing events.
        if self.shutdown.is_cancelled() {
            let reason = DisconnectReason::ServerShutdown;
            tracing::info!(
                conversation_id = %self.conversation_id,
                reason = %reason,
                "sse keepalive: server shutdown — emitting end frame"
            );
            self.closed = true;
            return Poll::Ready(Some(Ok(end_frame(reason))));
        }

        // 2. Inner stream. A real event resets BOTH the ping cadence
        //    (bandwidth efficiency) and the idle watchdog.
        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(ev))) => {
                // Reset ping cadence — real events keep middleboxes
                // alive just as well as pings do.
                self.ping_tick.reset();
                // Reset idle watchdog.
                let new_deadline = tokio::time::Instant::now() + self.idle_timeout;
                self.idle_sleep.as_mut().reset(new_deadline);
                return Poll::Ready(Some(Ok(ev)));
            }
            Poll::Ready(Some(Err(_inf))) => {
                // `Infallible` — this match arm is statically unreachable,
                // but rustc still requires it to satisfy the exhaustive
                // pattern match. Treat as end-of-stream defensively.
                self.closed = true;
                return Poll::Ready(None);
            }
            Poll::Ready(None) => {
                // Inner stream ended cleanly — propagate. No terminal
                // frame needed; the wrapping handler's StreamCloseGuard
                // records DisconnectReason::Normal.
                self.closed = true;
                return Poll::Ready(None);
            }
            Poll::Pending => {}
        }

        // 3. Idle watchdog. Must poll the sleep future before the ping
        //    interval so a simultaneously-ready idle-timeout wins over
        //    a ping (pings emitted during an idle window would otherwise
        //    mask the timeout intent).
        if self.idle_sleep.as_mut().poll(cx).is_ready() {
            let reason = DisconnectReason::ServerIdleTimeout;
            tracing::warn!(
                conversation_id = %self.conversation_id,
                idle_timeout_secs = self.idle_timeout.as_secs(),
                reason = %reason,
                "sse keepalive: idle timeout — emitting end frame"
            );
            self.closed = true;
            return Poll::Ready(Some(Ok(end_frame(reason))));
        }

        // 4. Ping cadence. `Interval::poll_tick` returns Ready every
        //    `interval` — we only emit a ping on those ticks. Pings do
        //    NOT reset the idle watchdog.
        if self.ping_tick.poll_tick(cx).is_ready() {
            record_keepalive_sent(&self.conversation_id);
            return Poll::Ready(Some(Ok(ping_frame())));
        }

        // 5. Watch the shutdown token. We register a waker against its
        //    cancellation future so we get polled immediately on SIGTERM
        //    rather than sitting on the ping interval. Clone the token
        //    first so the future doesn't borrow `self.shutdown` and
        //    collide with the `self.closed = true` write below.
        let shutdown_clone = self.shutdown.clone();
        let mut cancelled = Box::pin(shutdown_clone.cancelled_owned());
        if cancelled.as_mut().poll(cx).is_ready() {
            let reason = DisconnectReason::ServerShutdown;
            tracing::info!(
                conversation_id = %self.conversation_id,
                reason = %reason,
                "sse keepalive: server shutdown signaled — emitting end frame"
            );
            self.closed = true;
            return Poll::Ready(Some(Ok(end_frame(reason))));
        }

        Poll::Pending
    }
}

/// Build a `ping` SSE frame per the Conversation 8-event vocabulary. No
/// `.id()` — ping frames are ephemeral and must NOT advance the
/// client's `Last-Event-ID` cursor (otherwise a resume would skip past
/// real events).
fn ping_frame() -> Event {
    Event::default().event("ping").data(PING_DATA)
}

/// Build the terminal `message:end` frame with the given reason. No
/// `.id()` — end frames are ephemeral; clients should reconnect rather
/// than resume past them.
fn end_frame(reason: DisconnectReason) -> Event {
    Event::default()
        .event("message")
        .data(end_frame_json(reason))
}

// Import `std::future::Future` locally only where needed — avoids
// polluting the module's public surface.
use std::future::Future;

#[cfg(test)]
mod tests {
    use super::*;
    use async_stream::stream;
    use axum::response::sse::{KeepAlive, Sse};
    use axum::response::IntoResponse;
    use futures::StreamExt;
    use std::pin::Pin;

    /// Helper: run a KeepaliveStream through axum's Sse response path so
    /// the wire bytes are what clients actually see. Returns parsed
    /// (event_type, data) tuples in emission order.
    async fn drain_to_frames<S>(s: KeepaliveStream<S>) -> Vec<(String, String)>
    where
        S: Stream<Item = Result<Event, Infallible>> + Unpin + Send + 'static,
    {
        let boxed: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> = Box::pin(s);
        // Keepalive interval set to an hour so axum's OWN ping generator
        // never interferes — the comment-style ping it would emit would
        // otherwise confuse our parser.
        let sse = Sse::new(boxed).keep_alive(KeepAlive::new().interval(Duration::from_secs(3600)));
        let resp = sse.into_response();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body drained");
        let text = String::from_utf8(bytes.to_vec()).expect("utf-8 wire format");
        parse_wire(&text)
    }

    fn parse_wire(text: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut event_type = String::new();
        let mut data_parts: Vec<String> = Vec::new();
        for raw_line in text.split('\n') {
            let line = raw_line.trim_end_matches('\r');
            if line.is_empty() {
                if !event_type.is_empty() || !data_parts.is_empty() {
                    out.push((std::mem::take(&mut event_type), data_parts.join("\n")));
                    data_parts.clear();
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("event:") {
                event_type = rest.trim_start().to_string();
            } else if let Some(rest) = line.strip_prefix("data:") {
                data_parts.push(rest.trim_start().to_string());
            }
            // Ignore id, retry, and comment lines for this parser.
        }
        if !event_type.is_empty() || !data_parts.is_empty() {
            out.push((event_type, data_parts.join("\n")));
        }
        out
    }

    /// Inner stream that never produces any real events — stays Pending
    /// until an explicit close. Used to drive the idle/ping paths.
    fn pending_forever() -> Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> {
        Box::pin(stream! {
            // Park on a never-completing future so the outer select in
            // KeepaliveStream sees Pending from the inner every poll.
            // We close off the stream when the test's shutdown or idle
            // path fires, which terminates the whole KeepaliveStream.
            let () = std::future::pending().await;
            // Unreachable but forces the generator to type-check as a
            // stream of Result<Event, Infallible>.
            #[allow(unreachable_code)]
            {
                yield Ok(Event::default());
            }
        })
    }

    fn config(interval_secs: u64, idle_secs: u64) -> KeepaliveConfig {
        KeepaliveConfig {
            interval: Duration::from_secs(interval_secs),
            idle_timeout: Duration::from_secs(idle_secs),
        }
    }

    // -----------------------------------------------------------------
    // Test 1: Zero real events over 45s → 3 ping frames (15, 30, 45).
    // Uses tokio::time::pause + advance for deterministic mock-clock
    // timing, and caps the idle timeout ABOVE the 45s window so the
    // test doesn't accidentally exercise the idle path.
    // -----------------------------------------------------------------
    #[tokio::test(start_paused = true)]
    async fn three_pings_in_45s_with_no_real_events() {
        let inner = pending_forever();
        let cfg = config(15, 600);
        let shutdown = CancellationToken::new();
        let stream = wrap_stream_with_keepalive_and_token(
            inner,
            cfg,
            "c-ping-3".to_string(),
            shutdown.clone(),
        );
        let mut stream = Box::pin(stream);

        // Advance the clock 45s and collect any frames that became ready.
        // tokio::time::advance drives all timers forward; poll the stream
        // between advances so ping frames get produced.
        let mut frames: Vec<(String, String)> = Vec::new();
        for _ in 0..3 {
            tokio::time::advance(Duration::from_secs(15)).await;
            // Drain all frames currently ready.
            loop {
                match futures::poll!(stream.as_mut().next()) {
                    Poll::Ready(Some(Ok(ev))) => {
                        // Serialize the Event through axum's wire format by
                        // emitting a single-item stream and parsing bytes.
                        let one: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> =
                            Box::pin(async_stream::stream! { yield Ok(ev); });
                        let sse = Sse::new(one);
                        let resp = sse.into_response();
                        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                            .await
                            .unwrap();
                        let text = String::from_utf8(bytes.to_vec()).unwrap();
                        let parsed = parse_wire(&text);
                        assert_eq!(parsed.len(), 1);
                        frames.push(parsed.into_iter().next().unwrap());
                    }
                    Poll::Ready(None) => break,
                    Poll::Ready(Some(Err(_))) => unreachable!("infallible"),
                    Poll::Pending => break,
                }
            }
        }
        // Drop shutdown to release any lingering references.
        drop(shutdown);

        // Exactly 3 pings expected.
        assert_eq!(frames.len(), 3, "expected 3 pings in 45s, got {:?}", frames);
        for (etype, data) in &frames {
            assert_eq!(etype, "ping", "wrong event_type in keepalive frame");
            assert_eq!(data, PING_DATA, "ping data must be empty object");
        }
    }

    // -----------------------------------------------------------------
    // Test 2: A real event RESETS the ping timer. With the keepalive
    // interval at 15s and a real event delivered partway through the
    // first window, the NEXT ping must be 15s later on the post-reset
    // clock — not on the naive fixed-cadence clock.
    //
    // Under `tokio::time::pause() + advance()`, a `sleep(N)` inside the
    // inner stream does not begin counting until the stream is first
    // polled. So a `sleep(10s)` scheduled from the inner generator
    // actually lands in the test bucket AFTER the first advance past
    // the sleep deadline — in practice the real event shows up around
    // the t=15s advance (the first sample point at or past the sleep
    // deadline). The important invariant for THIS test is the
    // RELATIVE ordering: `first_ping_t - real_event_t >= 15s`.
    // -----------------------------------------------------------------
    #[tokio::test(start_paused = true)]
    async fn real_event_resets_ping_cadence() {
        // Inner stream that emits one real event after 10s, then stays
        // Pending forever.
        let inner = Box::pin(stream! {
            tokio::time::sleep(Duration::from_secs(10)).await;
            yield Ok(Event::default().event("text_delta").data("{\"i\":1}"));
            let () = std::future::pending().await;
            #[allow(unreachable_code)] { yield Ok(Event::default()); }
        });

        let cfg = config(15, 600);
        let shutdown = CancellationToken::new();
        let stream = wrap_stream_with_keepalive_and_token(
            inner,
            cfg,
            "c-reset".to_string(),
            shutdown.clone(),
        );
        let mut stream = Box::pin(stream);

        // Collected (t_secs, event_type) pairs.
        let mut events: Vec<(u64, String)> = Vec::new();
        // Drive clock in 5s chunks up to t=45s so we capture both the
        // real event's bucket and the expected post-reset ping bucket.
        let mut now_secs = 0u64;
        while now_secs < 45 {
            tokio::time::advance(Duration::from_secs(5)).await;
            now_secs += 5;
            loop {
                match futures::poll!(stream.as_mut().next()) {
                    Poll::Ready(Some(Ok(ev))) => {
                        let one: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> =
                            Box::pin(async_stream::stream! { yield Ok(ev); });
                        let sse = Sse::new(one);
                        let resp = sse.into_response();
                        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                            .await
                            .unwrap();
                        let text = String::from_utf8(bytes.to_vec()).unwrap();
                        let parsed = parse_wire(&text);
                        for (etype, _data) in parsed {
                            events.push((now_secs, etype));
                        }
                    }
                    Poll::Ready(None) => break,
                    Poll::Ready(Some(Err(_))) => unreachable!(),
                    Poll::Pending => break,
                }
            }
        }
        drop(shutdown);

        // Real event must fire — bucket is whichever advance first
        // observed the inner sleep crossing its deadline.
        let real_event_t = events
            .iter()
            .find(|(_, e)| e == "text_delta")
            .map(|(t, _)| *t)
            .unwrap_or_else(|| panic!("expected text_delta in stream, got events={:?}", events));

        // No ping should have fired before the real event — both the
        // ping cadence and the real event start ticking on first poll,
        // so the 15s ping window has not elapsed by the time the 10s
        // sleep fires.
        assert!(
            !events.iter().any(|(t, e)| *t < real_event_t && e == "ping"),
            "no ping should fire before the real event, events={:?}",
            events
        );

        // A ping must fire. The reset-on-real-event semantic says the
        // ping fires at or after (real_event_t + 15s). We allow a small
        // +5s slop because sampling happens in 5s increments — the
        // ping may land in the same bucket as its deadline or the
        // next one.
        let first_ping_t = events
            .iter()
            .find(|(_, e)| e == "ping")
            .map(|(t, _)| *t)
            .unwrap_or_else(|| {
                panic!(
                    "expected at least one ping after the real event, events={:?}",
                    events
                )
            });
        assert!(
            first_ping_t >= real_event_t + 15,
            "ping must be >= real_event_t + 15s (reset-on-real-event); got real_event_t={}, first_ping_t={}, events={:?}",
            real_event_t,
            first_ping_t,
            events
        );
    }

    // -----------------------------------------------------------------
    // Test 3: Idle timeout with no real events → `message:end
    // reason:server_idle_timeout` and the stream closes.
    // -----------------------------------------------------------------
    #[tokio::test(start_paused = true)]
    async fn idle_timeout_emits_end_frame_and_closes() {
        let inner = pending_forever();
        // 60s idle — short enough to advance past without wall-clock wait.
        // 15s ping so we see several pings before the idle fires.
        let cfg = config(15, 60);
        let shutdown = CancellationToken::new();
        let stream = wrap_stream_with_keepalive_and_token(
            inner,
            cfg,
            "c-idle".to_string(),
            shutdown.clone(),
        );
        let mut stream = Box::pin(stream);

        let mut events: Vec<(String, String)> = Vec::new();
        let mut total = Duration::from_secs(0);
        let tick = Duration::from_secs(5);
        let mut closed = false;
        while total < Duration::from_secs(75) && !closed {
            tokio::time::advance(tick).await;
            total += tick;
            loop {
                match futures::poll!(stream.as_mut().next()) {
                    Poll::Ready(Some(Ok(ev))) => {
                        let one: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> =
                            Box::pin(async_stream::stream! { yield Ok(ev); });
                        let sse = Sse::new(one);
                        let resp = sse.into_response();
                        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                            .await
                            .unwrap();
                        let text = String::from_utf8(bytes.to_vec()).unwrap();
                        events.extend(parse_wire(&text));
                    }
                    Poll::Ready(None) => {
                        closed = true;
                        break;
                    }
                    Poll::Ready(Some(Err(_))) => unreachable!(),
                    Poll::Pending => break,
                }
            }
        }
        drop(shutdown);

        // 4 pings at 15/30/45/60 would overlap idle at 60 — poll order
        // guarantees idle wins when both fire on the same tick (see
        // poll_next docs). Either way the end frame must be present and
        // the stream must have closed.
        assert!(
            closed,
            "stream did not close after idle timeout, events={:?}",
            events
        );
        let end = events
            .iter()
            .find(|(etype, _)| etype == "message")
            .expect("expected a terminal message-event, events={:?}");
        assert!(
            end.1.contains("\"reason\":\"server_idle_timeout\""),
            "end frame reason mismatch: {}",
            end.1
        );
        // And at least one ping must have fired before the idle close.
        assert!(
            events.iter().any(|(etype, _)| etype == "ping"),
            "expected at least one ping before idle, events={:?}",
            events
        );
    }

    // -----------------------------------------------------------------
    // Test 4: A real event at t=9min resets the idle timer. Idle does
    // NOT fire at t=10min; it fires at t=19min.
    // -----------------------------------------------------------------
    #[tokio::test(start_paused = true)]
    async fn real_event_resets_idle_watchdog() {
        let inner = Box::pin(stream! {
            tokio::time::sleep(Duration::from_secs(9 * 60)).await;
            yield Ok(Event::default().event("text_delta").data("{\"i\":1}"));
            let () = std::future::pending().await;
            #[allow(unreachable_code)] { yield Ok(Event::default()); }
        });
        let cfg = config(15, 600); // 10min idle
        let shutdown = CancellationToken::new();
        let stream = wrap_stream_with_keepalive_and_token(
            inner,
            cfg,
            "c-reset-idle".to_string(),
            shutdown.clone(),
        );
        let mut stream = Box::pin(stream);

        let mut events: Vec<(u64, String, String)> = Vec::new();
        let tick = Duration::from_secs(30);
        let mut total = Duration::from_secs(0);
        let mut closed_at: Option<u64> = None;
        // Drive up to 21 minutes of virtual time.
        while total < Duration::from_secs(21 * 60) && closed_at.is_none() {
            tokio::time::advance(tick).await;
            total += tick;
            let t_secs = total.as_secs();
            loop {
                match futures::poll!(stream.as_mut().next()) {
                    Poll::Ready(Some(Ok(ev))) => {
                        let one: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> =
                            Box::pin(async_stream::stream! { yield Ok(ev); });
                        let sse = Sse::new(one);
                        let resp = sse.into_response();
                        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                            .await
                            .unwrap();
                        let text = String::from_utf8(bytes.to_vec()).unwrap();
                        for (etype, data) in parse_wire(&text) {
                            events.push((t_secs, etype, data));
                        }
                    }
                    Poll::Ready(None) => {
                        closed_at = Some(t_secs);
                        break;
                    }
                    Poll::Ready(Some(Err(_))) => unreachable!(),
                    Poll::Pending => break,
                }
            }
        }
        drop(shutdown);

        // Real event must be present around t=9min (540s).
        let real_event_t = events
            .iter()
            .find(|(_t, etype, _)| etype == "text_delta")
            .map(|(t, _, _)| *t);
        assert!(
            matches!(real_event_t, Some(t) if (540..=570).contains(&t)),
            "real event missing or at wrong time, events={:?}",
            events
                .iter()
                .filter(|(_, e, _)| e != "ping")
                .collect::<Vec<_>>()
        );

        // NO end-frame should have been emitted at t ≈ 10min (600s).
        let end_at_10min = events
            .iter()
            .any(|(t, etype, _data)| etype == "message" && (590..=620).contains(t));
        assert!(
            !end_at_10min,
            "idle fired at 10min despite reset, events={:?}",
            events
        );

        // Idle must have fired around t=19min (1140s) — reset from 9min
        // + 10min = 19min.
        let end_frame = events
            .iter()
            .find(|(_, etype, _)| etype == "message")
            .expect("end frame must eventually fire");
        assert!(
            (1110..=1170).contains(&end_frame.0),
            "idle end-frame fired at wrong time (expected ~19min), got t={}, events={:?}",
            end_frame.0,
            events
                .iter()
                .filter(|(_, e, _)| e != "ping")
                .collect::<Vec<_>>()
        );
        assert!(
            end_frame.2.contains("server_idle_timeout"),
            "wrong reason: {}",
            end_frame.2
        );
    }

    // -----------------------------------------------------------------
    // Test 5: Client disconnect (inner stream ends with Ready(None))
    // closes the keepalive stream cleanly with NO end frame — the
    // StreamCloseGuard in the wrapping handler attributes the close
    // reason. This test locks down that our wrapper does not emit a
    // spurious end frame on natural stream termination.
    // -----------------------------------------------------------------
    #[tokio::test(start_paused = true)]
    async fn client_disconnect_ends_without_end_frame() {
        // Inner stream: emit one real event then end cleanly.
        let inner = Box::pin(stream! {
            yield Ok(Event::default().event("text_delta").data("{\"i\":1}"));
            // Stream ends (Ready(None)) — this is the same path axum sees
            // when the client's TCP connection drops and the inner
            // generator short-circuits out.
        });
        let cfg = config(15, 600);
        let shutdown = CancellationToken::new();
        let stream = wrap_stream_with_keepalive_and_token(
            inner,
            cfg,
            "c-disconnect".to_string(),
            shutdown.clone(),
        );
        let frames = drain_to_frames(stream).await;
        drop(shutdown);

        // Exactly one text_delta, no end frame, no pings.
        assert_eq!(frames.len(), 1, "expected 1 frame, got {:?}", frames);
        assert_eq!(frames[0].0, "text_delta");
        assert!(
            !frames.iter().any(|(etype, _)| etype == "message"),
            "no end frame expected on clean close, got {:?}",
            frames
        );
    }

    // -----------------------------------------------------------------
    // Test 6: CancellationToken cancel() emits `message:end
    // reason:server_shutdown` and closes the stream.
    // -----------------------------------------------------------------
    #[tokio::test(start_paused = true)]
    async fn server_shutdown_emits_end_frame_and_closes() {
        let inner = pending_forever();
        let cfg = config(15, 600);
        let shutdown = CancellationToken::new();
        let stream = wrap_stream_with_keepalive_and_token(
            inner,
            cfg,
            "c-shutdown".to_string(),
            shutdown.clone(),
        );
        let mut stream = Box::pin(stream);

        // Advance 5s so the ping tick has started but not yet fired.
        tokio::time::advance(Duration::from_secs(5)).await;
        // Fire the shutdown signal.
        shutdown.cancel();
        // Next poll should emit the end frame.
        let first = futures::poll!(stream.as_mut().next());
        let ev = match first {
            Poll::Ready(Some(Ok(ev))) => ev,
            other => panic!("expected Ready(Some(Ok)), got {:?}", other),
        };
        // Serialize to wire and inspect.
        let one: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> =
            Box::pin(async_stream::stream! { yield Ok(ev); });
        let sse = Sse::new(one);
        let resp = sse.into_response();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        let parsed = parse_wire(&text);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].0, "message", "wrong event_type: {:?}", parsed);
        assert!(
            parsed[0].1.contains("\"reason\":\"server_shutdown\""),
            "wrong end frame payload: {}",
            parsed[0].1
        );

        // Next poll MUST be Ready(None) — the stream has closed.
        let second = futures::poll!(stream.as_mut().next());
        assert!(
            matches!(second, Poll::Ready(None)),
            "stream not closed after shutdown"
        );
    }

    // -----------------------------------------------------------------
    // Config parsing smoke tests.
    // -----------------------------------------------------------------
    #[test]
    fn default_config_matches_documented_values() {
        let cfg = KeepaliveConfig::default();
        assert_eq!(cfg.interval, Duration::from_secs(15));
        assert_eq!(cfg.idle_timeout, Duration::from_secs(600));
    }

    #[test]
    fn parse_env_duration_rejects_zero_and_garbage() {
        // Zero falls back to default.
        std::env::set_var("TEST_KA_ZERO", "0");
        assert_eq!(
            parse_env_duration("TEST_KA_ZERO", 15),
            Duration::from_secs(15)
        );
        std::env::remove_var("TEST_KA_ZERO");

        // Garbage falls back to default.
        std::env::set_var("TEST_KA_BAD", "not-a-number");
        assert_eq!(
            parse_env_duration("TEST_KA_BAD", 42),
            Duration::from_secs(42)
        );
        std::env::remove_var("TEST_KA_BAD");

        // Valid parses through.
        std::env::set_var("TEST_KA_OK", "7");
        assert_eq!(parse_env_duration("TEST_KA_OK", 15), Duration::from_secs(7));
        std::env::remove_var("TEST_KA_OK");
    }

    #[test]
    fn end_frame_json_uses_disconnect_reason_label() {
        assert_eq!(
            end_frame_json(DisconnectReason::ServerIdleTimeout),
            r#"{"type":"end","reason":"server_idle_timeout"}"#
        );
        assert_eq!(
            end_frame_json(DisconnectReason::ServerShutdown),
            r#"{"type":"end","reason":"server_shutdown"}"#
        );
    }
}
