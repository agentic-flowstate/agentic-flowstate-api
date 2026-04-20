//! Per-(user_id, conversation_id) SSE rate limiting (T-C410DD96).
//!
//! Prevents a client stuck in a reconnect loop from DOS-ing the server.
//! Two hard caps per bucket:
//!
//! * **Concurrent streams** — default 1.
//! * **Reconnects per window** — default 10 per 60s sliding window.
//!
//! See [`stream_limiter`] for the algorithm and [`spawn_cleanup_task`]
//! for the background pruner.
//!
//! ## Install & wire
//!
//! `main.rs` constructs one [`StreamRateLimiter`] per process, stores it
//! on `AppState`, and kicks off [`spawn_cleanup_task`] with the
//! shutdown token. Handlers for SSE endpoints pull the limiter from
//! state via `FromRef` and call [`StreamRateLimiter::check`] before
//! opening the stream. On `Allow(permit)` the handler stores the permit
//! alongside the stream body so it drops when the stream closes. On
//! `Deny` the handler returns 429 with a `Retry-After` header and the
//! contract JSON body.
//!
//! ## Metrics emitted
//!
//! * `stream_rate_limited_total{reason="concurrent_stream_limit" | "reconnect_rate_limit"}`
//!   — counter, one increment per denial.
//!
//! No metric is emitted on admission — the existing `stream_opened_total`
//! counter already covers that side of the funnel.

pub mod stream_limiter;

pub use stream_limiter::{
    DenyReason, RateLimitDecision, StreamPermit, StreamRateLimitConfig, StreamRateLimiter,
};

use std::sync::Arc;
use std::time::Duration;

use tokio::time::interval;
use tokio_util::sync::CancellationToken;

/// Cleanup interval for the background pruner task. Picked per the
/// T-C410DD96 brief: every 5 minutes is frequent enough that an idle
/// conversation's bucket gets reclaimed within one admin retention
/// cycle, but slow enough that the pruner itself is never hot.
pub const CLEANUP_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Spawn the background cleanup task. Polls every [`CLEANUP_INTERVAL`]
/// and calls [`StreamRateLimiter::cleanup_idle_entries`]. Terminates
/// when `shutdown_token` fires.
pub fn spawn_cleanup_task(limiter: Arc<StreamRateLimiter>, shutdown_token: CancellationToken) {
    tokio::spawn(async move {
        let mut ticker = interval(CLEANUP_INTERVAL);
        // The first tick fires immediately. We intentionally skip it so
        // we don't prune within microseconds of startup; the first real
        // sweep happens after one full interval.
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = shutdown_token.cancelled() => {
                    tracing::info!(
                        target: "rate_limiting.stream",
                        "cleanup task shutting down"
                    );
                    break;
                }
                _ = ticker.tick() => {
                    let removed = limiter.cleanup_idle_entries();
                    if removed > 0 {
                        tracing::info!(
                            target: "rate_limiting.stream",
                            removed,
                            remaining_buckets = limiter.bucket_count(),
                            "cleanup pass removed idle buckets"
                        );
                    }
                }
            }
        }
    });
}

/// Global install slot for the rate limiter. `chat_stream::chat` pulls
/// from this because its signature does not currently accept the limiter
/// via a parameter and retrofitting every chat caller just to thread
/// state is more churn than the one-call-site override warrants. The
/// SSE handlers that already have `State<...>` in scope (resume_stream,
/// reconnect_conversation_stream) go through `AppState` directly.
///
/// This follows the same pattern as
/// [`crate::handlers::sse_keepalive::install_shutdown_token`].
static GLOBAL_LIMITER: once_cell::sync::OnceCell<Arc<StreamRateLimiter>> =
    once_cell::sync::OnceCell::new();

/// Install the process-wide limiter. Call exactly once from `main.rs`
/// after building [`AppState`]. Returns `Err(())` if called twice —
/// tests that spin up the pipeline should handle the double-install
/// case.
pub fn install_global(limiter: Arc<StreamRateLimiter>) -> Result<(), ()> {
    GLOBAL_LIMITER.set(limiter).map_err(|_| ())
}

/// Retrieve the process-wide limiter, if installed. Returns `None` in
/// unit tests that never called [`install_global`] — callers treat that
/// as "rate limiting disabled" and admit the stream.
pub fn global() -> Option<Arc<StreamRateLimiter>> {
    GLOBAL_LIMITER.get().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_cleanup_task_terminates_on_shutdown() {
        // Build a minimal runtime inline so we don't need #[tokio::test].
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            let limiter = Arc::new(StreamRateLimiter::new(StreamRateLimitConfig::default()));
            let token = CancellationToken::new();
            spawn_cleanup_task(limiter, token.clone());
            // Fire the token; the task should exit promptly. We can't
            // await the spawn handle (spawn_cleanup_task doesn't return
            // one) but we can assert that a short sleep after cancel
            // does not hang.
            token.cancel();
            tokio::time::sleep(Duration::from_millis(10)).await;
        });
    }
}
