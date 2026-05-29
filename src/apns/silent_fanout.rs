//! Silent-push fan-out for the durable chat streaming pipeline (T-90C7FAC4).
//!
//! When a `message_stop` event is written to `conversation_events`, the
//! `ConversationWorker` spawns a background task that looks up every
//! active APNs device token registered to the conversation's owner and
//! sends a silent (background) push per device. iOS receives the push,
//! runs its 25-second `SyncEngine.deltaSync()`, and catches up to the
//! server state even when the app was backgrounded during the turn.
//!
//! This module exists as its own unit so the fan-out logic can be unit
//! tested against a fake `SilentPushSender` without spinning up the full
//! worker. See the `tests` module.
//!
//! ## Contract (from A-C710FBCA + A-F797AFC7)
//!
//! * The sender returns `Err(ApnsSilentError::DeviceUnregistered)` on APNs
//!   410 Gone / `Unregistered`. That variant is terminal for the given token
//!   — the fan-out MUST soft-delete it via
//!   `ticketing_system::device_tokens::soft_delete_device_token` and MUST NOT
//!   retry.
//! * All other errors (`Rejected`, `Transport`, `NotInitialized`) are
//!   logged at the appropriate level and do not trigger soft-delete.
//! * The fan-out counts every token it attempts — skipped, delivered,
//!   unregistered, timeout, and other-failure buckets are emitted via
//!   structured `tracing::info!` so T-56987678 (observability) can scrape
//!   them later. When a metrics crate is wired in we'll fold these into
//!   the `push_*_total` counter family documented in the task brief.
//!
//! ## Concurrency
//!
//! The fan-out is invoked via `tokio::spawn` from the worker's
//! `emit_event` path. It must NOT block the SSE stream — which means it
//! must not hold the worker's mutex or its sqlx connection. The fan-out
//! takes `Arc<SqlitePool>` and an `Arc<dyn SilentPushSender>` directly,
//! both cheap to clone, and runs on its own tokio task.

use std::sync::Arc;

use sqlx::SqlitePool;

use super::silent::{ApnsClient, ApnsSilentError};
use crate::observability::streaming::{record_push_sent, PushResult};

/// Trait abstraction over the APNs silent-push sender so the fan-out
/// logic can be unit-tested against a fake. Implemented by
/// [`super::silent::ApnsClient`] and by the test fake in `#[cfg(test)]`.
#[async_trait::async_trait]
pub trait SilentPushSender: Send + Sync + 'static {
    async fn send_silent_push(
        &self,
        device_token: &str,
        conversation_id: &str,
        last_message_id: &str,
    ) -> Result<(), ApnsSilentError>;
}

#[async_trait::async_trait]
impl SilentPushSender for ApnsClient {
    async fn send_silent_push(
        &self,
        device_token: &str,
        conversation_id: &str,
        last_message_id: &str,
    ) -> Result<(), ApnsSilentError> {
        ApnsClient::send_silent_push(self, device_token, conversation_id, last_message_id).await
    }
}

/// Process-wide registration of the active silent-push sender + its
/// enabled flag. Populated once at server startup from `main.rs`
/// (alongside `APNS_SILENT_ENABLED` parsing); consumed by the
/// `ConversationWorker` via [`global_config`] without plumbing the
/// sender through `AppState`, `WORKER_MANAGER`, and the worker's
/// constructor.
#[derive(Clone)]
pub struct SilentPushConfig {
    pub enabled: bool,
    pub sender: Arc<dyn SilentPushSender>,
}

static GLOBAL: once_cell::sync::OnceCell<SilentPushConfig> = once_cell::sync::OnceCell::new();

/// Install the process-wide silent-push config. Call at most once from
/// `main.rs` after parsing `APNS_SILENT_ENABLED` and initializing the
/// [`ApnsClient`]. Returns `Ok(())` on first call, `Err(_)` on
/// subsequent calls (the cell is one-shot — tests may call
/// [`set_global_for_test`] which side-steps `OnceCell`).
pub fn init_global(config: SilentPushConfig) -> Result<(), &'static str> {
    GLOBAL
        .set(config)
        .map_err(|_| "silent_fanout global already initialized")
}

/// Read-only accessor for the installed config. Returns `None` before
/// `init_global` runs (e.g. in early startup or in tests that don't
/// care about silent push).
pub fn global_config() -> Option<SilentPushConfig> {
    GLOBAL.get().cloned()
}

/// Outcome of a single-device fan-out attempt. Used only by the tests
/// and by structured tracing — the production fan-out does not return
/// a Vec, it logs and spawns fire-and-forget.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub enum PerDeviceOutcome {
    Delivered,
    Unregistered,
    Timeout,
    Other(String),
    SkippedDisabled,
    SkippedNotInitialized,
}

/// Fan-out the silent push to every active device token for `user_id`.
///
/// Records structured tracing events for each attempt:
///
/// * `event="push_attempt" result="initiated|skipped_disabled"`
/// * `event="push_delivered" result="ok"`
/// * `event="push_failed" reason="unregistered|timeout|other"`
///
/// All DB + APNs work happens in the background task. The caller
/// spawns this via `tokio::spawn` and does not await its result.
pub async fn fan_out_silent_push(
    pool: Arc<SqlitePool>,
    sender: Arc<dyn SilentPushSender>,
    enabled: bool,
    user_id: String,
    conversation_id: String,
    last_message_id: String,
) -> Vec<PerDeviceOutcome> {
    // 1. Look up the user's active tokens. A DB failure here is the
    //    only reason the whole fan-out aborts — there are no devices to
    //    push to anyway.
    let tokens = match ticketing_system::device_tokens::list_active_for_user(
        pool.as_ref(),
        &user_id,
    )
    .await
    {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(
                target: "silent_push",
                user_id = %user_id,
                conversation_id = %conversation_id,
                error = %e,
                "[SILENT_PUSH_FANOUT] Failed to list active device tokens"
            );
            return Vec::new();
        }
    };

    if tokens.is_empty() {
        tracing::debug!(
            target: "silent_push",
            user_id = %user_id,
            conversation_id = %conversation_id,
            "[SILENT_PUSH_FANOUT] No active device tokens — nothing to push"
        );
        return Vec::new();
    }

    let mut outcomes = Vec::with_capacity(tokens.len());

    for token in tokens {
        // 2. Honor the APNS_SILENT_ENABLED gate. We still count the
        //    attempt so the dashboard shows "we had N devices we could
        //    have woken up but silent push is off".
        if !enabled {
            tracing::info!(
                target: "silent_push",
                event = "push_attempt",
                result = "skipped_disabled",
                user_id = %user_id,
                conversation_id = %conversation_id,
                device_token = %short_token(&token.device_token),
                bundle_id = %token.bundle_id,
                "[SILENT_PUSH_FANOUT] skipped (APNS_SILENT_ENABLED=false)"
            );
            outcomes.push(PerDeviceOutcome::SkippedDisabled);
            continue;
        }

        tracing::info!(
            target: "silent_push",
            event = "push_attempt",
            result = "initiated",
            user_id = %user_id,
            conversation_id = %conversation_id,
            device_token = %short_token(&token.device_token),
            bundle_id = %token.bundle_id,
            last_message_id = %last_message_id,
            "[SILENT_PUSH_FANOUT] initiated"
        );

        match sender
            .send_silent_push(&token.device_token, &conversation_id, &last_message_id)
            .await
        {
            Ok(()) => {
                tracing::info!(
                    target: "silent_push",
                    event = "push_delivered",
                    result = "ok",
                    user_id = %user_id,
                    conversation_id = %conversation_id,
                    device_token = %short_token(&token.device_token),
                    "[SILENT_PUSH_FANOUT] delivered"
                );
                // T-56987678: count every accepted silent push.
                record_push_sent(PushResult::Success, 200);
                outcomes.push(PerDeviceOutcome::Delivered);
            }
            Err(ApnsSilentError::DeviceUnregistered(reason)) => {
                tracing::info!(
                    target: "silent_push",
                    event = "push_failed",
                    reason = "unregistered",
                    user_id = %user_id,
                    conversation_id = %conversation_id,
                    device_token = %short_token(&token.device_token),
                    detail = %reason,
                    "[SILENT_PUSH_FANOUT] token unregistered — soft-deleting"
                );
                match ticketing_system::device_tokens::soft_delete_device_token(
                    pool.as_ref(),
                    &user_id,
                    &token.device_token,
                )
                .await
                {
                    Ok(flipped) => {
                        tracing::debug!(
                            target: "silent_push",
                            user_id = %user_id,
                            device_token = %short_token(&token.device_token),
                            flipped,
                            "[SILENT_PUSH_FANOUT] soft_delete_device_token completed"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "silent_push",
                            user_id = %user_id,
                            device_token = %short_token(&token.device_token),
                            error = %e,
                            "[SILENT_PUSH_FANOUT] soft_delete_device_token failed"
                        );
                    }
                }
                // T-56987678: APNs 410 Gone maps to PushResult::Unregistered.
                record_push_sent(PushResult::Unregistered, 410);
                outcomes.push(PerDeviceOutcome::Unregistered);
            }
            Err(ApnsSilentError::NotInitialized) => {
                tracing::warn!(
                    target: "silent_push",
                    event = "push_attempt",
                    result = "skipped_not_initialized",
                    user_id = %user_id,
                    conversation_id = %conversation_id,
                    device_token = %short_token(&token.device_token),
                    "[SILENT_PUSH_FANOUT] sender not initialized — APNS_SILENT_ENABLED=true but init_from_env was never called"
                );
                // T-56987678: not-initialized counts as an error bucket
                // with apns_status=0 (never reached APNs).
                record_push_sent(PushResult::Error, 0);
                outcomes.push(PerDeviceOutcome::SkippedNotInitialized);
            }
            Err(ApnsSilentError::Transport(e)) => {
                tracing::warn!(
                    target: "silent_push",
                    event = "push_failed",
                    reason = "timeout",
                    user_id = %user_id,
                    conversation_id = %conversation_id,
                    device_token = %short_token(&token.device_token),
                    error = %e,
                    "[SILENT_PUSH_FANOUT] transport error"
                );
                // T-56987678: transport error — never reached APNs, status=0.
                record_push_sent(PushResult::Error, 0);
                outcomes.push(PerDeviceOutcome::Timeout);
            }
            Err(other) => {
                tracing::warn!(
                    target: "silent_push",
                    event = "push_failed",
                    reason = "other",
                    user_id = %user_id,
                    conversation_id = %conversation_id,
                    device_token = %short_token(&token.device_token),
                    error = %other,
                    "[SILENT_PUSH_FANOUT] rejected"
                );
                // T-56987678: other rejection — map the typed error
                // variant to PushResult and emit with the real APNs
                // status code where available.
                let (result, status) = match &other {
                    ApnsSilentError::Rejected { code, .. } => {
                        let r = match *code {
                            429 => PushResult::Throttled,
                            400 | 403 | 405 | 413 => PushResult::BadToken,
                            _ => PushResult::Error,
                        };
                        (r, *code)
                    }
                    _ => (PushResult::Error, 0),
                };
                record_push_sent(result, status);
                outcomes.push(PerDeviceOutcome::Other(other.to_string()));
            }
        }
    }

    outcomes
}

/// Redact a device token for logs — keep first 8 chars only.
fn short_token(tok: &str) -> String {
    let end = tok
        .char_indices()
        .nth(8)
        .map(|(i, _)| i)
        .unwrap_or(tok.len());
    format!("{}…", &tok[..end])
}

#[cfg(test)]
pub(crate) fn set_global_for_test(config: SilentPushConfig) {
    // OnceCell::set returns Err if already set; tests that need to
    // rotate the global should use fan_out_silent_push directly with
    // the sender arg instead.
    let _ = GLOBAL.set(config);
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };

    /// In-memory fake sender that records every call and emits a
    /// programmable sequence of responses.
    struct FakeSender {
        calls: Mutex<Vec<(String, String, String)>>,
        responses: Mutex<Vec<Result<(), ApnsSilentError>>>,
    }

    impl FakeSender {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                responses: Mutex::new(Vec::new()),
            })
        }

        fn with_responses(responses: Vec<Result<(), ApnsSilentError>>) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                responses: Mutex::new(responses),
            })
        }

        fn calls(&self) -> Vec<(String, String, String)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl SilentPushSender for FakeSender {
        async fn send_silent_push(
            &self,
            device_token: &str,
            conversation_id: &str,
            last_message_id: &str,
        ) -> Result<(), ApnsSilentError> {
            self.calls.lock().unwrap().push((
                device_token.to_string(),
                conversation_id.to_string(),
                last_message_id.to_string(),
            ));
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                return Ok(());
            }
            responses.remove(0)
        }
    }

    /// Build a fresh in-memory sqlite pool with the device_tokens v2
    /// schema, matching the ticketing-system migration v45.
    async fn fresh_pool() -> Arc<SqlitePool> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(
            r#"CREATE TABLE device_tokens (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                user_id TEXT NOT NULL,
                device_token TEXT NOT NULL,
                bundle_id TEXT NOT NULL,
                platform TEXT NOT NULL CHECK(platform IN ('ios')),
                device_name TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                last_seen_at INTEGER NOT NULL,
                deleted_at INTEGER
            )"#,
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE UNIQUE INDEX idx_device_tokens_unique_active
                ON device_tokens(user_id, device_token) WHERE deleted_at IS NULL",
        )
        .execute(&pool)
        .await
        .unwrap();
        Arc::new(pool)
    }

    async fn seed_tokens(pool: &SqlitePool, user: &str, tokens: &[&str]) {
        for tok in tokens {
            ticketing_system::device_tokens::upsert_device_token(
                pool,
                user,
                tok,
                "com.agenticflowstate.app",
                "ios",
                None,
            )
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn fans_out_to_every_active_token() {
        let pool = fresh_pool().await;
        seed_tokens(&pool, "alex", &["tok-a", "tok-b", "tok-c"]).await;

        let sender = FakeSender::new();
        let outcomes = fan_out_silent_push(
            pool,
            sender.clone() as Arc<dyn SilentPushSender>,
            true,
            "alex".to_string(),
            "conv-xyz".to_string(),
            "msg-99".to_string(),
        )
        .await;

        assert_eq!(outcomes.len(), 3);
        assert!(outcomes.iter().all(|o| *o == PerDeviceOutcome::Delivered));

        let calls = sender.calls();
        assert_eq!(calls.len(), 3);
        let tokens: Vec<_> = calls.iter().map(|(t, _, _)| t.clone()).collect();
        for expected in &["tok-a", "tok-b", "tok-c"] {
            assert!(tokens.iter().any(|t| t == expected), "missing {}", expected);
        }
        // Every call carries the same (conversation_id, last_message_id).
        for (_, conv, last) in &calls {
            assert_eq!(conv, "conv-xyz");
            assert_eq!(last, "msg-99");
        }
    }

    #[tokio::test]
    async fn unregistered_triggers_soft_delete_exactly_once() {
        let pool = fresh_pool().await;
        seed_tokens(&pool, "alex", &["tok-a", "tok-b", "tok-c"]).await;

        // 3 responses: ok, Unregistered for tok-b, ok. Order matches the
        // newest-first ordering from list_active_for_user — last upsert
        // is newest-first, so the order is: tok-c, tok-b, tok-a.
        let sender = FakeSender::with_responses(vec![
            Ok(()),
            Err(ApnsSilentError::DeviceUnregistered("Unregistered".into())),
            Ok(()),
        ]);

        let outcomes = fan_out_silent_push(
            pool.clone(),
            sender.clone() as Arc<dyn SilentPushSender>,
            true,
            "alex".to_string(),
            "conv-xyz".to_string(),
            "msg-99".to_string(),
        )
        .await;

        assert_eq!(outcomes.len(), 3);
        assert_eq!(
            outcomes
                .iter()
                .filter(|o| **o == PerDeviceOutcome::Delivered)
                .count(),
            2,
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|o| **o == PerDeviceOutcome::Unregistered)
                .count(),
            1,
        );

        // Verify exactly one row was soft-deleted, and it was tok-b.
        let active = ticketing_system::device_tokens::list_active_for_user(pool.as_ref(), "alex")
            .await
            .unwrap();
        let remaining: Vec<_> = active.iter().map(|d| d.device_token.clone()).collect();
        assert!(
            remaining.contains(&"tok-a".to_string()),
            "tok-a should still be active"
        );
        assert!(
            remaining.contains(&"tok-c".to_string()),
            "tok-c should still be active"
        );
        assert!(
            !remaining.contains(&"tok-b".to_string()),
            "tok-b should have been soft-deleted"
        );
        assert_eq!(active.len(), 2);
    }

    #[tokio::test]
    async fn disabled_skips_without_calling_sender() {
        let pool = fresh_pool().await;
        seed_tokens(&pool, "alex", &["tok-a", "tok-b"]).await;

        let sender = FakeSender::new();
        let outcomes = fan_out_silent_push(
            pool,
            sender.clone() as Arc<dyn SilentPushSender>,
            false, // disabled
            "alex".to_string(),
            "conv-xyz".to_string(),
            "msg-99".to_string(),
        )
        .await;

        assert_eq!(outcomes.len(), 2);
        assert!(outcomes
            .iter()
            .all(|o| *o == PerDeviceOutcome::SkippedDisabled));
        assert_eq!(
            sender.calls().len(),
            0,
            "sender must not be invoked when disabled"
        );
    }

    #[tokio::test]
    async fn no_tokens_is_a_no_op() {
        let pool = fresh_pool().await;
        let sender = FakeSender::new();
        let outcomes = fan_out_silent_push(
            pool,
            sender.clone() as Arc<dyn SilentPushSender>,
            true,
            "alex".to_string(),
            "conv-xyz".to_string(),
            "msg-99".to_string(),
        )
        .await;
        assert!(outcomes.is_empty());
        assert_eq!(sender.calls().len(), 0);
    }

    /// Fire-and-forget contract: the caller's future must resolve
    /// BEFORE the spawned fan-out completes its HTTP work. We model
    /// "HTTP work" with a channel the fake holds open until we release
    /// it from the test thread.
    #[tokio::test]
    async fn caller_future_returns_before_fanout_completes() {
        let pool = fresh_pool().await;
        seed_tokens(&pool, "alex", &["tok-a"]).await;

        // The fake blocks on a barrier so fan_out_silent_push cannot
        // finish until we release it.
        let gate = Arc::new(tokio::sync::Notify::new());
        let released = Arc::new(AtomicUsize::new(0));

        struct GatedSender {
            gate: Arc<tokio::sync::Notify>,
            released: Arc<AtomicUsize>,
        }
        #[async_trait::async_trait]
        impl SilentPushSender for GatedSender {
            async fn send_silent_push(
                &self,
                _device_token: &str,
                _conversation_id: &str,
                _last_message_id: &str,
            ) -> Result<(), ApnsSilentError> {
                self.gate.notified().await;
                self.released.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }

        let sender: Arc<dyn SilentPushSender> = Arc::new(GatedSender {
            gate: gate.clone(),
            released: released.clone(),
        });

        // Simulate the emit_event call path: spawn the fan-out and
        // return immediately, as the worker does.
        let spawn_start = std::time::Instant::now();
        let handle = tokio::spawn(fan_out_silent_push(
            pool,
            sender,
            true,
            "alex".to_string(),
            "conv-xyz".to_string(),
            "msg-99".to_string(),
        ));

        // The caller's "future" is just `spawn()` — returns immediately.
        // Assert we can race past the gate without the spawned task
        // having completed.
        assert!(
            spawn_start.elapsed() < std::time::Duration::from_millis(200),
            "spawn() returned too slowly — the caller was blocking"
        );
        assert_eq!(
            released.load(Ordering::SeqCst),
            0,
            "spawned task ran to completion before the caller returned — NOT fire-and-forget"
        );

        // Now release the gate and await the spawned task to confirm
        // it was actually running (not, e.g., cancelled).
        gate.notify_one();
        let outcomes = handle.await.unwrap();
        assert_eq!(outcomes, vec![PerDeviceOutcome::Delivered]);
        assert_eq!(released.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn short_token_redacts_long_tokens() {
        assert_eq!(short_token("deadbeefcafebabe1234"), "deadbeef…");
        // Short tokens are still truncated at whatever the full length is.
        assert_eq!(short_token("abc"), "abc…");
    }
}
