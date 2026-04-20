//! Mid-block resumption snapshot reconstruction (T-F8F986A9).
//!
//! ## Problem
//!
//! A client that reconnects with `Last-Event-ID: N` tells the server "I
//! saw everything through event index N; ship me N+1 onward." When `N`
//! happens to land INSIDE a content_block — between two `text_delta`
//! frames, or between an `input_json_delta` and its matching
//! `content_block_stop` — the naive replay leaves the client with:
//!
//! * A persistent `content_block_start` frame they saw before the gap.
//! * A partial prefix of the block's deltas.
//! * An empty "in progress" UI that will never be filled in, because the
//!   server won't re-emit the deltas the client already saw.
//!
//! On iOS specifically this shows up as half-rendered assistant messages,
//! tool-call cards stuck in "pending" state, and thinking blocks frozen
//! mid-thought.
//!
//! ## Fix
//!
//! Before the forward-emission phase in [`crate::handlers::resume_stream`]
//! ships events strictly greater than the cursor, walk every event in the
//! conversation through event index `<= cursor` and reconstruct the
//! authoritative state of each currently-open content block. For every
//! block that is still unstopped at the cursor position we synthesize ONE
//! `content_block_snapshot` event carrying the full accumulated state, and
//! ship it before the forward deltas. The client treats the snapshot as
//! a REPLACEMENT for its local copy of that block — not a cumulative
//! delta — which heals the half-rendered state in a single frame.
//!
//! ## Cursor-id stamping
//!
//! Each synthesized snapshot is stamped with `id: <cursor>` on the SSE
//! frame (NOT a new event_index). Rationale: the snapshot is an
//! authoritative rollup of state the client already observed up to
//! `cursor`. If the client drops and reconnects again before finishing
//! the stream, the server will emit an equivalent snapshot derived fresh
//! from the DB — there is no sequence number that should advance past
//! `cursor`. Stamping the snapshot with `cursor` also guarantees that
//! clients which DO advance their `lastEventId` only when they see a
//! strictly-greater id will NEVER advance past the live forward cursor
//! on a snapshot frame, so the next reconnect will still resume from the
//! same point.
//!
//! This is the same idempotency pattern Ably uses for their resumption
//! snapshots — see research artifact A-18DB4221 (§ "Resume Token
//! Security, Gap Detection") and Anthropic's Messages streaming docs
//! on `content_block_snapshot`.
//!
//! ## What gets reconstructed
//!
//! | Block kind  | Accumulated fields                                                    |
//! |-------------|-----------------------------------------------------------------------|
//! | `text`      | accumulated_text (all `text_delta` payloads joined in emission order) |
//! | `tool_use`  | id, name, accumulated_input_json (raw partial_json concat, unparsed)  |
//! | `thinking`  | accumulated_text (all `thinking_delta`s) + optional signature         |
//! | `tool_result`| content + is_error (tool_result blocks are atomic — start+stop, no    |
//! |             | deltas — so a mid-block tool_result is just a not-yet-stopped block   |
//! |             | with its original content)                                            |
//!
//! `tool_use.input` in the snapshot is shipped as a STRING, not a parsed
//! object. Reason: streaming `input_json_delta`s are explicitly permitted
//! to land mid-token — e.g., half a UTF-8 escape or an unclosed brace.
//! A mid-block parse failure would corrupt the snapshot; clients parse
//! after they see the eventual `content_block_stop` anyway, and are
//! already accumulating the prefix themselves for mid-stream display.
//! We publish the identical raw prefix the client has been building so
//! both sides agree on bytes.
//!
//! ## Not covered
//!
//! * Fresh streams (`cursor = -1`). The handler skips the walker
//!   entirely — there is no "half-rendered state" to heal when the
//!   client has not seen anything yet. The public
//!   [`reconstruct_state_up_to_cursor`] still returns `Vec::new()` for
//!   that input so a misuse fails open rather than emitting phantom
//!   frames.

use anyhow::Result;
use metrics::counter;
use serde::Deserialize;

use ticketing_system::{conversations, SqlitePool};

use crate::agents::anthropic_events::{AnthropicEvent, ContentBlockStub};

/// Counter: `content_block_snapshot` frames emitted by the resume
/// handler. Labeled by `block_kind` so operators can chart whether
/// clients are predominantly resuming mid-text (common — streaming
/// assistant reply) vs mid-tool_use (rarer — tool_use arguments are
/// atomic in cc-sdk, but the Anthropic vocabulary still streams
/// `input_json_delta`s).
pub const METRIC_STREAM_SNAPSHOT_EMITTED: &str = "stream_snapshot_emitted_total";

/// Classification of a content block being reconstructed. Mirrors the
/// `type` tag on [`ContentBlockStub`] but is cheaper to compare and
/// carries the label we emit on the
/// [`METRIC_STREAM_SNAPSHOT_EMITTED`] counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentBlockKind {
    Text,
    ToolUse,
    Thinking,
    ToolResult,
}

impl ContentBlockKind {
    /// Label value for the `block_kind` Prometheus label. Must match
    /// Anthropic's canonical block type string so dashboards can join
    /// against the `event_type` column analytics.
    pub fn as_label(self) -> &'static str {
        match self {
            ContentBlockKind::Text => "text",
            ContentBlockKind::ToolUse => "tool_use",
            ContentBlockKind::Thinking => "thinking",
            ContentBlockKind::ToolResult => "tool_result",
        }
    }
}

/// Reconstructed in-progress state of a single content block. Built by
/// replaying every persisted `content_block_start`,
/// `content_block_delta`, and `content_block_stop` frame in order, up
/// to and including the resume cursor.
///
/// `stopped = true` means the log shows a matching `content_block_stop`
/// for this block — the client has already seen the terminal frame and
/// the block does NOT need a snapshot. Only unstopped blocks at the end
/// of the walk are emitted by [`synthesize_snapshot_events`].
#[derive(Debug, Clone)]
pub struct ContentBlockState {
    pub index: usize,
    pub kind: ContentBlockKind,
    /// For `text` blocks: concat of every `text_delta.text`.
    /// For `thinking` blocks: concat of every `thinking_delta.thinking`.
    /// For `tool_use` blocks: the `name` from the start frame (we need
    /// somewhere to stash this alongside `accumulated_input_json`).
    /// For `tool_result` blocks: the inline `content` from the start frame.
    pub accumulated_text: String,
    /// For `tool_use` blocks only: raw concat of every delta's
    /// `partial_json`. Stored as a string, not parsed — see module docs.
    pub accumulated_input_json: String,
    /// For `thinking` blocks only: the most recent `signature_delta`
    /// payload, or `None` if no signature has arrived yet.
    pub signature: Option<String>,
    /// For `tool_use` blocks only: the tool-use id from the start frame,
    /// required to emit a well-formed snapshot.
    pub tool_use_id: Option<String>,
    /// For `tool_result` blocks only: the `is_error` flag from the start
    /// frame. Persisted so a mid-stream tool_result snapshot round-trips
    /// the error marker.
    pub tool_result_is_error: bool,
    /// Has the client seen a matching `content_block_stop` for this
    /// block at or before the cursor? Stopped blocks do NOT need a
    /// snapshot.
    pub stopped: bool,
}

/// Shape of a persisted `content_block_start` event payload. Only the
/// fields we need for reconstruction are deserialized; unknown fields
/// are tolerated so schema drift never breaks resume.
#[derive(Debug, Deserialize)]
struct StoredBlockStart {
    index: u32,
    content_block: StoredBlockStub,
}

/// Shape of the `content_block` field inside a persisted start frame.
/// The `type` discriminator matches Anthropic's wire format
/// (`text` / `tool_use` / `thinking` / `tool_result`).
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StoredBlockStub {
    Text {
        #[serde(default)]
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: serde_json::Value,
    },
    Thinking {
        #[serde(default)]
        thinking: String,
        #[serde(default)]
        signature: String,
    },
    ToolResult {
        tool_use_id: String,
        #[serde(default)]
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

/// Shape of a persisted `content_block_delta` event payload.
#[derive(Debug, Deserialize)]
struct StoredBlockDelta {
    index: u32,
    delta: StoredDeltaBody,
}

/// The four delta sub-variants Anthropic defines. Matches
/// [`crate::agents::anthropic_events::ContentBlockDelta`] but uses owned
/// strings without the `PartialEq` hop so deserialization is zero-copy
/// friendly on large text deltas. Variant names intentionally carry the
/// `Delta` suffix so they round-trip with Anthropic's wire-format
/// discriminator (`text_delta`, `input_json_delta`, etc.).
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::enum_variant_names)]
enum StoredDeltaBody {
    TextDelta { text: String },
    InputJsonDelta { partial_json: String },
    ThinkingDelta { thinking: String },
    SignatureDelta { signature: String },
}

/// Shape of a persisted `content_block_stop` event payload.
#[derive(Debug, Deserialize)]
struct StoredBlockStop {
    index: u32,
}

/// Walk every persisted event for `conversation_id` with
/// `event_index <= cursor`, build a [`ContentBlockState`] per observed
/// block, and return the blocks that are still unstopped at the end of
/// the walk.
///
/// # Cursor semantics
///
/// The predicate is `event_index <= cursor` (inclusive). When the cursor
/// lands EXACTLY on a `content_block_stop` frame, that block IS
/// considered stopped — the client saw the stop and does not need a
/// snapshot. When the cursor lands on any other frame, the block is
/// unstopped and a snapshot will be emitted.
///
/// # Fresh stream
///
/// When `cursor < 0` (the `FreshStream` sentinel from
/// [`crate::handlers::resume_cursor::CursorSource::FreshStream`]) the
/// function short-circuits and returns `Vec::new()`. A client that
/// hasn't seen anything yet has no in-progress state to heal.
///
/// # Vocabulary filtering
///
/// Only events whose `event_type` is one of `content_block_start`,
/// `content_block_delta`, or `content_block_stop` contribute block state.
/// `message_start` resets per-message state, `message_stop` defensively
/// closes any still-open blocks, and `ping` / `message_delta` / `error`
/// are ignored. A `message_stop` ALSO marks any still-open blocks as
/// stopped (defensive: the worker emits one `content_block_stop` per
/// block before `message_stop`, but if that invariant is ever violated
/// the walker must not emit phantom snapshots for a message that is
/// conceptually complete).
pub async fn reconstruct_state_up_to_cursor(
    pool: &SqlitePool,
    conversation_id: &str,
    cursor: i64,
) -> Result<Vec<ContentBlockState>> {
    if cursor < 0 {
        return Ok(Vec::new());
    }

    // Load every persisted event in order. `get_events` returns rows
    // sorted ASC by event_index, which matches emission order.
    let events = conversations::get_events(pool, conversation_id).await?;

    // Per-block state keyed by block index. Using a Vec keeps emission
    // order stable (blocks are opened in increasing index order on the
    // wire) without the hash overhead — content blocks per message are
    // typically single digits, rarely more than 10.
    let mut states: Vec<ContentBlockState> = Vec::new();

    for event in events {
        if event.event_index as i64 > cursor {
            break;
        }

        match event.event_type.as_str() {
            "content_block_start" => {
                let payload = conversations::load_event_payload_str(pool, &event).await?;
                let parsed: StoredBlockStart = match serde_json::from_str(&payload) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(
                            conversation_id = %event.conversation_id,
                            event_index = event.event_index,
                            error = %e,
                            "resume snapshot: malformed content_block_start payload — skipping"
                        );
                        continue;
                    }
                };
                let state = new_state_from_start(parsed);
                upsert_state(&mut states, state);
            }
            "content_block_delta" => {
                let payload = conversations::load_event_payload_str(pool, &event).await?;
                let parsed: StoredBlockDelta = match serde_json::from_str(&payload) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(
                            conversation_id = %event.conversation_id,
                            event_index = event.event_index,
                            error = %e,
                            "resume snapshot: malformed content_block_delta payload — skipping"
                        );
                        continue;
                    }
                };
                apply_delta(&mut states, parsed);
            }
            "content_block_stop" => {
                let payload = conversations::load_event_payload_str(pool, &event).await?;
                let parsed: StoredBlockStop = match serde_json::from_str(&payload) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(
                            conversation_id = %event.conversation_id,
                            event_index = event.event_index,
                            error = %e,
                            "resume snapshot: malformed content_block_stop payload — skipping"
                        );
                        continue;
                    }
                };
                mark_stopped(&mut states, parsed.index as usize);
            }
            "message_start" => {
                // A new assistant message resets the per-message block
                // state. Drop any prior in-progress blocks — they belong
                // to an earlier turn the client already rendered, and a
                // mid-turn resume can't produce snapshots for completed
                // turns.
                states.clear();
            }
            "message_stop" => {
                // Defensive: if the worker ever ships a message_stop
                // without closing every open block, treat all outstanding
                // blocks as stopped. A complete message has no
                // half-rendered content by definition.
                for s in &mut states {
                    s.stopped = true;
                }
            }
            _ => {
                // Everything else (ping, message_delta, error, future
                // schema additions) contributes no block state.
            }
        }
    }

    // Return only the unstopped blocks — these are the half-rendered
    // ones that need a snapshot.
    Ok(states.into_iter().filter(|s| !s.stopped).collect())
}

/// Turn every [`ContentBlockState`] produced by
/// [`reconstruct_state_up_to_cursor`] into an
/// [`AnthropicEvent::ContentBlockSnapshot`]. One snapshot per unstopped
/// block, preserving the block's index on the wire so clients can route
/// each snapshot to the correct half-rendered UI element.
///
/// Also emits the [`METRIC_STREAM_SNAPSHOT_EMITTED`] counter, labeled by
/// `block_kind`, so operators can chart resume-snapshot volume per block
/// type. The counter is incremented once per snapshot (not once per
/// call).
pub fn synthesize_snapshot_events(states: &[ContentBlockState]) -> Vec<AnthropicEvent> {
    let mut out = Vec::with_capacity(states.len());
    for state in states {
        let block = match state.kind {
            ContentBlockKind::Text => ContentBlockStub::Text {
                text: state.accumulated_text.clone(),
            },
            ContentBlockKind::ToolUse => ContentBlockStub::ToolUse {
                // The snapshot's `id` MUST match the id the client saw
                // on the original `content_block_start` — that is the
                // correlation key the client uses to route future
                // tool_result frames to the right in-progress card.
                id: state.tool_use_id.clone().unwrap_or_default(),
                name: state.accumulated_text.clone(),
                // Ship the raw accumulated JSON as a string field. See
                // module docs on why we deliberately do NOT parse.
                input: serde_json::Value::String(state.accumulated_input_json.clone()),
            },
            ContentBlockKind::Thinking => ContentBlockStub::Thinking {
                thinking: state.accumulated_text.clone(),
                signature: state.signature.clone().unwrap_or_default(),
            },
            ContentBlockKind::ToolResult => ContentBlockStub::ToolResult {
                tool_use_id: state.tool_use_id.clone().unwrap_or_default(),
                content: state.accumulated_text.clone(),
                is_error: state.tool_result_is_error,
            },
        };

        counter!(
            METRIC_STREAM_SNAPSHOT_EMITTED,
            "block_kind" => state.kind.as_label(),
        )
        .increment(1);

        out.push(AnthropicEvent::ContentBlockSnapshot {
            index: state.index as u32,
            block,
        });
    }
    out
}

/// Build a fresh `ContentBlockState` from a parsed `content_block_start`
/// payload. `stopped` starts `false` — only a matching
/// `content_block_stop` (or a terminal `message_stop`) flips it true.
fn new_state_from_start(parsed: StoredBlockStart) -> ContentBlockState {
    match parsed.content_block {
        StoredBlockStub::Text { text } => ContentBlockState {
            index: parsed.index as usize,
            kind: ContentBlockKind::Text,
            accumulated_text: text,
            accumulated_input_json: String::new(),
            signature: None,
            tool_use_id: None,
            tool_result_is_error: false,
            stopped: false,
        },
        StoredBlockStub::ToolUse { id, name, input } => {
            // cc-sdk fires `content_block_start` with `input: {}` and
            // streams the payload via `input_json_delta`. We still seed
            // `accumulated_input_json` from any non-empty `input` we see
            // on start — defensive against backfilled rows where the
            // full input was materialized eagerly.
            let seed = if matches!(input, serde_json::Value::Object(ref m) if m.is_empty()) {
                String::new()
            } else {
                serde_json::to_string(&input).unwrap_or_default()
            };
            ContentBlockState {
                index: parsed.index as usize,
                kind: ContentBlockKind::ToolUse,
                // Stash the tool `name` in `accumulated_text` — the
                // tool_use branch of `synthesize_snapshot_events`
                // reads it back into `ContentBlockStub::ToolUse.name`.
                accumulated_text: name,
                accumulated_input_json: seed,
                signature: None,
                tool_use_id: Some(id),
                tool_result_is_error: false,
                stopped: false,
            }
        }
        StoredBlockStub::Thinking {
            thinking,
            signature,
        } => ContentBlockState {
            index: parsed.index as usize,
            kind: ContentBlockKind::Thinking,
            accumulated_text: thinking,
            accumulated_input_json: String::new(),
            signature: if signature.is_empty() {
                None
            } else {
                Some(signature)
            },
            tool_use_id: None,
            tool_result_is_error: false,
            stopped: false,
        },
        StoredBlockStub::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => ContentBlockState {
            index: parsed.index as usize,
            kind: ContentBlockKind::ToolResult,
            accumulated_text: content,
            accumulated_input_json: String::new(),
            signature: None,
            tool_use_id: Some(tool_use_id),
            tool_result_is_error: is_error,
            stopped: false,
        },
    }
}

/// Append a freshly-parsed state to the per-message state list. If a
/// block with the same index is already present (shouldn't happen under
/// the worker's invariants, but defensive against replay of a
/// re-materialized log), overwrite it.
fn upsert_state(states: &mut Vec<ContentBlockState>, state: ContentBlockState) {
    if let Some(existing) = states.iter_mut().find(|s| s.index == state.index) {
        *existing = state;
    } else {
        states.push(state);
    }
}

/// Apply a `content_block_delta` to the block at `delta.index`. Deltas
/// that target a block we never saw a `content_block_start` for are
/// silently dropped (log-and-move-on) rather than panicking — a
/// corrupted log should never take down a resume handler.
fn apply_delta(states: &mut [ContentBlockState], delta: StoredBlockDelta) {
    let Some(state) = states.iter_mut().find(|s| s.index == delta.index as usize) else {
        tracing::warn!(
            block_index = delta.index,
            "resume snapshot: delta targets a block with no prior start — dropping"
        );
        return;
    };
    match delta.delta {
        StoredDeltaBody::TextDelta { text } => {
            state.accumulated_text.push_str(&text);
        }
        StoredDeltaBody::InputJsonDelta { partial_json } => {
            state.accumulated_input_json.push_str(&partial_json);
        }
        StoredDeltaBody::ThinkingDelta { thinking } => {
            state.accumulated_text.push_str(&thinking);
        }
        StoredDeltaBody::SignatureDelta { signature } => {
            state.signature = Some(signature);
        }
    }
}

/// Mark the block at `index` as stopped. A stop for an index we never
/// saw a start for is ignored — same defensive posture as
/// [`apply_delta`].
fn mark_stopped(states: &mut [ContentBlockState], index: usize) {
    if let Some(state) = states.iter_mut().find(|s| s.index == index) {
        state.stopped = true;
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::sync::Arc;

    use chrono::Utc;
    use serde_json::json;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    use super::*;

    async fn fresh_pool() -> Arc<SqlitePool> {
        let dsn = format!(
            "file:resume-snap-test-{}?mode=memory&cache=shared",
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

        sqlx::query("INSERT INTO conversations (id, status) VALUES (?, 'open')")
            .bind("c-snap")
            .execute(&pool)
            .await
            .unwrap();

        Arc::new(pool)
    }

    async fn insert(pool: &SqlitePool, index: i32, event_type: &str, payload: serde_json::Value) {
        let now = Utc::now().timestamp();
        sqlx::query(
            "INSERT INTO conversation_events \
             (conversation_id, event_index, event_type, event_data, created_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind("c-snap")
        .bind(index)
        .bind(event_type)
        .bind(payload.to_string())
        .bind(now)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Helper: seed a completed text turn (message_start → start(0) →
    /// 3 text_deltas → stop(0) → message_delta → message_stop). Returns
    /// the final event_index (== index of message_stop).
    async fn seed_completed_text_turn(pool: &SqlitePool) -> i32 {
        insert(
            pool,
            0,
            "message_start",
            json!({"type": "message_start", "message": {
                "id": "msg_1", "type": "message", "role": "assistant",
                "content": []
            }}),
        )
        .await;
        insert(
            pool,
            1,
            "content_block_start",
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        )
        .await;
        insert(
            pool,
            2,
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "alpha "}}),
        )
        .await;
        insert(
            pool,
            3,
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "beta "}}),
        )
        .await;
        insert(
            pool,
            4,
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "gamma"}}),
        )
        .await;
        insert(
            pool,
            5,
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        )
        .await;
        insert(
            pool,
            6,
            "message_delta",
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
        )
        .await;
        insert(pool, 7, "message_stop", json!({"type": "message_stop"})).await;
        7
    }

    // ---------------------------------------------------------------
    // 1. Cursor at end of fully-completed message → 0 snapshots.
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn completed_message_produces_no_snapshot() {
        let pool = fresh_pool().await;
        let end = seed_completed_text_turn(&pool).await;

        let states = reconstruct_state_up_to_cursor(&pool, "c-snap", end as i64)
            .await
            .unwrap();
        assert!(
            states.is_empty(),
            "expected 0 unstopped blocks, got {:?}",
            states
        );

        let events = synthesize_snapshot_events(&states);
        assert_eq!(events.len(), 0);
    }

    // ---------------------------------------------------------------
    // 2. Cursor mid text_delta stream (3 deltas, cursor after 2nd) →
    //    1 snapshot with accumulated_text = first 2 deltas joined.
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn mid_text_stream_produces_one_snapshot_with_prefix() {
        let pool = fresh_pool().await;
        // Seed through event_index 3 (the 2nd text_delta). Cursor = 3
        // means the client saw: message_start, start(0), delta("alpha "),
        // delta("beta "). The block is NOT stopped.
        insert(
            &pool,
            0,
            "message_start",
            json!({"type": "message_start", "message": {
                "id": "msg_1", "type": "message", "role": "assistant", "content": []
            }}),
        )
        .await;
        insert(
            &pool,
            1,
            "content_block_start",
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        )
        .await;
        insert(
            &pool,
            2,
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "alpha "}}),
        )
        .await;
        insert(
            &pool,
            3,
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "beta "}}),
        )
        .await;
        // Also seed the later frames (they exist in the DB but the
        // cursor walk must stop at index 3).
        insert(
            &pool,
            4,
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "gamma"}}),
        )
        .await;
        insert(
            &pool,
            5,
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        )
        .await;

        let states = reconstruct_state_up_to_cursor(&pool, "c-snap", 3)
            .await
            .unwrap();
        assert_eq!(states.len(), 1, "expected 1 unstopped block");
        assert_eq!(states[0].kind, ContentBlockKind::Text);
        assert_eq!(states[0].accumulated_text, "alpha beta ");
        assert!(!states[0].stopped);

        let events = synthesize_snapshot_events(&states);
        assert_eq!(events.len(), 1);
        match &events[0] {
            AnthropicEvent::ContentBlockSnapshot { index, block } => {
                assert_eq!(*index, 0);
                match block {
                    ContentBlockStub::Text { text } => {
                        assert_eq!(text, "alpha beta ");
                    }
                    other => panic!("expected text block, got {:?}", other),
                }
            }
            other => panic!("expected content_block_snapshot, got {:?}", other),
        }
    }

    // ---------------------------------------------------------------
    // 3. Cursor mid tool_use input_json stream → 1 snapshot with
    //    accumulated_input_json = concat of partial_json deltas seen.
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn mid_tool_use_input_json_produces_snapshot_with_raw_prefix() {
        let pool = fresh_pool().await;
        insert(
            &pool,
            0,
            "message_start",
            json!({"type": "message_start", "message": {
                "id": "msg_1", "type": "message", "role": "assistant", "content": []
            }}),
        )
        .await;
        insert(
            &pool,
            1,
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "tool_use", "id": "toolu_1", "name": "search", "input": {}}
            }),
        )
        .await;
        insert(
            &pool,
            2,
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"q\":\""}}),
        )
        .await;
        insert(
            &pool,
            3,
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "rust strea"}}),
        )
        .await;
        // Additional deltas + stop exist later in the log, but the
        // cursor only goes up to index 3.
        insert(
            &pool,
            4,
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "ming\"}"}}),
        )
        .await;
        insert(
            &pool,
            5,
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        )
        .await;

        let states = reconstruct_state_up_to_cursor(&pool, "c-snap", 3)
            .await
            .unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].kind, ContentBlockKind::ToolUse);
        assert_eq!(states[0].accumulated_input_json, "{\"q\":\"rust strea");
        assert_eq!(states[0].tool_use_id.as_deref(), Some("toolu_1"));
        assert_eq!(states[0].accumulated_text, "search"); // name stashed here

        let events = synthesize_snapshot_events(&states);
        assert_eq!(events.len(), 1);
        match &events[0] {
            AnthropicEvent::ContentBlockSnapshot { index, block } => {
                assert_eq!(*index, 0);
                match block {
                    ContentBlockStub::ToolUse { id, name, input } => {
                        assert_eq!(id, "toolu_1");
                        assert_eq!(name, "search");
                        // Raw accumulated JSON is shipped as a JSON
                        // STRING in the `input` field — not parsed —
                        // so mid-stream partials round-trip safely.
                        assert_eq!(
                            input,
                            &serde_json::Value::String("{\"q\":\"rust strea".to_string())
                        );
                    }
                    other => panic!("expected tool_use block, got {:?}", other),
                }
            }
            other => panic!("expected content_block_snapshot, got {:?}", other),
        }
    }

    // ---------------------------------------------------------------
    // 4. Cursor mid thinking block → 1 snapshot with accumulated
    //    thinking text + signature (if a signature_delta was seen).
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn mid_thinking_block_snapshots_text_and_signature() {
        let pool = fresh_pool().await;
        insert(
            &pool,
            0,
            "message_start",
            json!({"type": "message_start", "message": {
                "id": "msg_1", "type": "message", "role": "assistant", "content": []
            }}),
        )
        .await;
        insert(
            &pool,
            1,
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "thinking", "thinking": "", "signature": ""}
            }),
        )
        .await;
        insert(
            &pool,
            2,
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Step 1. "}}),
        )
        .await;
        insert(
            &pool,
            3,
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Step 2."}}),
        )
        .await;
        insert(
            &pool,
            4,
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig_abc123"}}),
        )
        .await;

        // Cursor at index 4: client has seen both thinking deltas AND
        // the signature. Block is still unstopped.
        let states = reconstruct_state_up_to_cursor(&pool, "c-snap", 4)
            .await
            .unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].kind, ContentBlockKind::Thinking);
        assert_eq!(states[0].accumulated_text, "Step 1. Step 2.");
        assert_eq!(states[0].signature.as_deref(), Some("sig_abc123"));

        let events = synthesize_snapshot_events(&states);
        assert_eq!(events.len(), 1);
        match &events[0] {
            AnthropicEvent::ContentBlockSnapshot { index, block } => {
                assert_eq!(*index, 0);
                match block {
                    ContentBlockStub::Thinking {
                        thinking,
                        signature,
                    } => {
                        assert_eq!(thinking, "Step 1. Step 2.");
                        assert_eq!(signature, "sig_abc123");
                    }
                    other => panic!("expected thinking block, got {:?}", other),
                }
            }
            other => panic!("expected content_block_snapshot, got {:?}", other),
        }
    }

    // ---------------------------------------------------------------
    // 5. Multiple in-progress blocks: text block 0 stopped, tool_use
    //    block 1 in progress → 1 snapshot for block 1 only.
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn only_unstopped_blocks_appear_in_snapshot() {
        let pool = fresh_pool().await;
        insert(
            &pool,
            0,
            "message_start",
            json!({"type": "message_start", "message": {
                "id": "msg_1", "type": "message", "role": "assistant", "content": []
            }}),
        )
        .await;
        insert(
            &pool,
            1,
            "content_block_start",
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        )
        .await;
        insert(
            &pool,
            2,
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "done"}}),
        )
        .await;
        insert(
            &pool,
            3,
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        )
        .await;
        insert(
            &pool,
            4,
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": 1,
                "content_block": {"type": "tool_use", "id": "toolu_2", "name": "calc", "input": {}}
            }),
        )
        .await;
        insert(
            &pool,
            5,
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"x\":1"}}),
        )
        .await;

        // Cursor at index 5: block 0 is stopped, block 1 is in progress.
        let states = reconstruct_state_up_to_cursor(&pool, "c-snap", 5)
            .await
            .unwrap();
        assert_eq!(
            states.len(),
            1,
            "expected 1 unstopped block, got {:?}",
            states
        );
        assert_eq!(states[0].index, 1);
        assert_eq!(states[0].kind, ContentBlockKind::ToolUse);
        assert_eq!(states[0].accumulated_input_json, "{\"x\":1");
    }

    // ---------------------------------------------------------------
    // 6. Fresh stream (cursor = -1) → reconstruct returns empty.
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn fresh_stream_cursor_skips_reconstruction() {
        let pool = fresh_pool().await;
        // Even with half-rendered state in the DB, cursor=-1 means the
        // client hasn't seen anything yet — no snapshot needed.
        insert(
            &pool,
            0,
            "content_block_start",
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        )
        .await;
        insert(
            &pool,
            1,
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "x"}}),
        )
        .await;

        let states = reconstruct_state_up_to_cursor(&pool, "c-snap", -1)
            .await
            .unwrap();
        assert!(states.is_empty());
    }

    // ---------------------------------------------------------------
    // 7. Cursor exactly at a content_block_stop → 0 snapshots.
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn cursor_on_stop_frame_marks_block_stopped() {
        let pool = fresh_pool().await;
        insert(
            &pool,
            0,
            "message_start",
            json!({"type": "message_start", "message": {
                "id": "msg_1", "type": "message", "role": "assistant", "content": []
            }}),
        )
        .await;
        insert(
            &pool,
            1,
            "content_block_start",
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        )
        .await;
        insert(
            &pool,
            2,
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "hi"}}),
        )
        .await;
        insert(
            &pool,
            3,
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        )
        .await;

        // Cursor exactly on the stop frame: the client saw the stop,
        // block is stopped.
        let states = reconstruct_state_up_to_cursor(&pool, "c-snap", 3)
            .await
            .unwrap();
        assert!(
            states.is_empty(),
            "cursor at stop → no unstopped blocks, got {:?}",
            states
        );
    }

    // ---------------------------------------------------------------
    // 8. message_stop marks all outstanding blocks stopped.
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn message_stop_closes_all_outstanding_blocks() {
        let pool = fresh_pool().await;
        insert(
            &pool,
            0,
            "message_start",
            json!({"type": "message_start", "message": {
                "id": "msg_1", "type": "message", "role": "assistant", "content": []
            }}),
        )
        .await;
        // Two blocks, both missing their content_block_stop.
        insert(
            &pool,
            1,
            "content_block_start",
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        )
        .await;
        insert(
            &pool,
            2,
            "content_block_start",
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
        )
        .await;
        // A message_stop defensively closes them.
        insert(&pool, 3, "message_stop", json!({"type": "message_stop"})).await;

        let states = reconstruct_state_up_to_cursor(&pool, "c-snap", 3)
            .await
            .unwrap();
        assert!(states.is_empty());
    }

    // ---------------------------------------------------------------
    // Counter label assertion — make sure synthesize_snapshot_events
    // emits the observability signal with the right block_kind.
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn synthesize_emits_counter_per_block_kind() {
        crate::observability::install_for_test();
        let before = crate::observability::render_prometheus().unwrap_or_default();
        let before_text = count_label(
            &before,
            METRIC_STREAM_SNAPSHOT_EMITTED,
            "block_kind",
            "text",
        )
        .unwrap_or(0.0);

        let state = ContentBlockState {
            index: 0,
            kind: ContentBlockKind::Text,
            accumulated_text: "hello".to_string(),
            accumulated_input_json: String::new(),
            signature: None,
            tool_use_id: None,
            tool_result_is_error: false,
            stopped: false,
        };
        let out = synthesize_snapshot_events(&[state]);
        assert_eq!(out.len(), 1);

        let after = crate::observability::render_prometheus().unwrap();
        let after_text = count_label(&after, METRIC_STREAM_SNAPSHOT_EMITTED, "block_kind", "text")
            .unwrap_or(0.0);
        assert_eq!((after_text - before_text) as u64, 1);
    }

    /// Tiny Prometheus scraper: find a labeled counter value.
    fn count_label(text: &str, name: &str, label_key: &str, label_val: &str) -> Option<f64> {
        let needle = format!("{}=\"{}\"", label_key, label_val);
        for line in text.lines() {
            if line.starts_with('#') {
                continue;
            }
            if !line.starts_with(name) {
                continue;
            }
            if !line.contains(&needle) {
                continue;
            }
            if let Some(val) = line
                .split_whitespace()
                .next_back()
                .and_then(|s| s.parse::<f64>().ok())
            {
                return Some(val);
            }
        }
        None
    }
}
