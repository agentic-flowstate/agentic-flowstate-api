//! Encode internal [`StreamEvent`]s into the Anthropic Messages API
//! streaming vocabulary.
//!
//! ## Why this module exists
//!
//! the previous SDK (the runtime SDK we consume) does NOT expose the raw
//! Anthropic SSE wire — it hands the application a `Message` enum
//! (`User` / `Assistant` / `System` / `Result`) whose `Assistant` variant
//! carries already-materialized `ContentBlock`s. Our `AgentExecutor`
//! flattens that further into `StreamEvent` (`Text`, `ToolUse`,
//! `Thinking`, `Result`, ...) so the rest of the worker can operate on a
//! simple enum.
//!
//! Persistence and the live SSE wire, however, speak the Anthropic 8-event
//! vocabulary (`message_start` / `content_block_start` /
//! `content_block_delta` / `content_block_stop` / `message_delta` /
//! `message_stop` / `ping` / `error`). This module is the single bridge
//! that reconstructs that vocabulary from the previous SDK's shape. It is NOT a
//! legacy compatibility layer — there is no other vocabulary on the wire,
//! no feature flag, no alternate writer. The encoder is a structural
//! consequence of the previous SDK's API shape.
//!
//! ## Mapping rules
//!
//! the previous SDK reports text/thinking/tool_use/tool_result content blocks on an
//! assistant turn, terminated by a `Result`. We model the turn as a single
//! Anthropic message:
//!
//! ```text
//! Status("running")              → (no wire event — worker-internal lifecycle)
//! first content block arrives    → message_start
//! Text { content }               → content_block_start(text) (if no block open)
//!                                  content_block_delta(text_delta)
//! ToolUse { id, name, input }    → close any open block
//!                                  content_block_start(tool_use {id,name,input:{}})
//!                                  content_block_delta(input_json_delta(input as JSON))
//!                                  content_block_stop
//! Thinking { content }           → close any open non-thinking block
//!                                  content_block_start(thinking) (if needed)
//!                                  content_block_delta(thinking_delta)
//! ToolResult { id, content, … }  → close any open block
//!                                  content_block_start(tool_result)
//!                                  content_block_stop
//! Result { status, is_error }    → close any open block
//!                                  message_delta(stop_reason: status)
//!                                  message_stop
//! Status("heartbeat")            → ping
//! Status("failed" | error case)  → error  (followed by message_stop if open)
//! ```
//!
//! Router result, title/org updates, user messages, and `ReplayComplete`
//! have no Anthropic equivalent. The encoder returns an empty frame list
//! for them — they are simply not persisted and not broadcast on the wire.

use crate::agents::anthropic_events::{
    AnthropicEvent, ApiError, ContentBlockDelta, ContentBlockStub, MessageDeltaBody, MessageStub,
    Usage,
};
use crate::agents::StreamEvent;

/// Which kind of content block is currently open.
#[derive(Debug, Clone, Copy, PartialEq)]
enum OpenBlock {
    None,
    Text,
    Thinking,
}

/// Stateful encoder. One instance per assistant turn. After the
/// terminal `Result`/`Error` is emitted, call [`AnthropicEventEncoder::reset`]
/// (or drop the encoder and build a fresh one) before the next turn —
/// otherwise message_start / content-block indices will leak across turns.
pub struct AnthropicEventEncoder {
    /// True after `message_start` has been emitted for the current turn.
    message_open: bool,
    /// Next content-block index to allocate.
    next_block_index: u32,
    /// Which block (if any) is currently open at `current_block_index`.
    open_kind: OpenBlock,
    /// Index of the currently-open block — only meaningful when
    /// `open_kind != None`.
    current_block_index: u32,
    /// The `message_id` attached to the most recent `message_start`.
    /// Retained across `message_stop` so callers that observe the stop
    /// frame (e.g. the silent-push fan-out) can tag it. Cleared only
    /// when the next `message_start` overwrites it.
    current_message_id: Option<String>,
    /// Canonical `conversation_messages.id` to attach to the next
    /// `message_start`. The worker must stage this after it creates the
    /// assistant placeholder row and before any content can open the
    /// stream. Keeping the SSE id equal to the DB id is load-bearing for
    /// iOS checkpoint rehydration and GRDB reconciliation.
    pending_message_id: Option<String>,
    /// Pending `client_id` (the client-supplied `Idempotency-Key` from
    /// T-A819D36B) to stamp onto the next `message_start` stub this
    /// encoder emits. Consumed on `open_message_if_needed`. The
    /// worker calls [`Self::set_pending_client_id`] at the top of each
    /// turn with the inbound header value (or `None` if the client
    /// didn't send one) so iOS's `MessageEchoService` can reconcile its
    /// optimistic-echo row when it sees the matching frame.
    pending_client_id: Option<String>,
}

impl AnthropicEventEncoder {
    pub fn new(_conversation_id: &str) -> Self {
        Self {
            message_open: false,
            next_block_index: 0,
            open_kind: OpenBlock::None,
            current_block_index: 0,
            current_message_id: None,
            pending_message_id: None,
            pending_client_id: None,
        }
    }

    /// Stage the canonical assistant `conversation_messages.id` that must
    /// appear on the next `message_start` frame. This is intentionally
    /// required: emitting a synthetic stream id creates a second assistant
    /// identity on iOS when `/messages` and `/checkpoint.recent_events`
    /// are merged during an active reconnect.
    pub fn set_pending_message_id(&mut self, message_id: impl Into<String>) {
        self.pending_message_id = Some(message_id.into());
    }

    /// Stage the `client_id` (Idempotency-Key) that should be echoed on
    /// the next `message_start` frame this encoder emits
    /// (T-A819D36B). The value is consumed the first time
    /// `open_message_if_needed` fires, so callers MUST call this BEFORE
    /// feeding the first `StreamEvent` of a new turn into
    /// [`Self::translate`]. Pass `None` to clear a prior staged value —
    /// a missing `Idempotency-Key` header MUST propagate end-to-end as
    /// `None` (no fallbacks, per the ticket ground rules).
    pub fn set_pending_client_id(&mut self, client_id: Option<String>) {
        self.pending_client_id = client_id;
    }

    /// Return the `message_id` of the most-recently-opened Anthropic
    /// message (format `msg_<conv_short>_<n>`), or `None` if
    /// `message_start` has never been emitted for this encoder.
    ///
    /// This is the ID that was attached to the most recent `message_start`
    /// frame and is the one external consumers should tag as
    /// `last_message_id` on downstream signals (e.g. the silent-push
    /// fan-out in T-90C7FAC4). The value is stable across a
    /// `message_start` / `message_stop` pair so callers who observe a
    /// `message_stop` in the encoder output can read it immediately
    /// afterward.
    pub fn current_message_id(&self) -> Option<&str> {
        self.current_message_id.as_deref()
    }

    /// Encode a single internal `StreamEvent` into the sequence of
    /// Anthropic events that should be emitted on the wire.
    ///
    /// Returns an empty `Vec` for events that have no Anthropic
    /// equivalent (router events, title updates, replay markers).
    pub fn encode(&mut self, event: &StreamEvent) -> Vec<AnthropicEvent> {
        match event {
            StreamEvent::Text { content } => self.on_text(content),
            StreamEvent::Thinking { content } => self.on_thinking(content),
            StreamEvent::ToolUse { id, name, input } => self.on_tool_use(id, name, input),
            StreamEvent::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => self.on_tool_result(tool_use_id, content, *is_error),
            StreamEvent::Result {
                status, is_error, ..
            } => self.on_result(status, *is_error),
            StreamEvent::Status { status, message } => self.on_status(status, message.as_deref()),
            // Router + metadata events have no Anthropic equivalent —
            // they are not persisted and not broadcast on the wire.
            StreamEvent::RouterResult { .. }
            | StreamEvent::UserMessage { .. }
            | StreamEvent::ReplayComplete { .. }
            | StreamEvent::TitleUpdate { .. }
            | StreamEvent::OrgUpdate { .. } => Vec::new(),
        }
    }

    fn open_message_if_needed(&mut self, out: &mut Vec<AnthropicEvent>) {
        if !self.message_open {
            let id = self
                .pending_message_id
                .take()
                .expect("pending assistant message id must be staged before message_start");
            self.current_message_id = Some(id.clone());
            // Consume the pending Idempotency-Key (T-A819D36B). `.take()`
            // leaves `None` behind so a second message within the same
            // turn (shouldn't happen, but guard it) doesn't re-echo a
            // stale client_id.
            let client_id = self.pending_client_id.take();
            out.push(AnthropicEvent::MessageStart {
                message: MessageStub::new_assistant(id, None).with_client_id(client_id),
            });
            self.message_open = true;
            self.next_block_index = 0;
            self.open_kind = OpenBlock::None;
        }
    }

    fn close_open_block(&mut self, out: &mut Vec<AnthropicEvent>) {
        if self.open_kind != OpenBlock::None {
            out.push(AnthropicEvent::ContentBlockStop {
                index: self.current_block_index,
            });
            self.open_kind = OpenBlock::None;
        }
    }

    fn on_text(&mut self, content: &str) -> Vec<AnthropicEvent> {
        let mut out = Vec::with_capacity(3);
        self.open_message_if_needed(&mut out);
        if self.open_kind != OpenBlock::Text {
            self.close_open_block(&mut out);
            let idx = self.next_block_index;
            self.next_block_index += 1;
            out.push(AnthropicEvent::ContentBlockStart {
                index: idx,
                content_block: ContentBlockStub::Text {
                    text: String::new(),
                },
            });
            self.current_block_index = idx;
            self.open_kind = OpenBlock::Text;
        }
        out.push(AnthropicEvent::ContentBlockDelta {
            index: self.current_block_index,
            delta: ContentBlockDelta::TextDelta {
                text: content.to_string(),
            },
        });
        out
    }

    fn on_thinking(&mut self, content: &str) -> Vec<AnthropicEvent> {
        let mut out = Vec::with_capacity(3);
        self.open_message_if_needed(&mut out);
        if self.open_kind != OpenBlock::Thinking {
            self.close_open_block(&mut out);
            let idx = self.next_block_index;
            self.next_block_index += 1;
            out.push(AnthropicEvent::ContentBlockStart {
                index: idx,
                content_block: ContentBlockStub::Thinking {
                    thinking: String::new(),
                    signature: String::new(),
                },
            });
            self.current_block_index = idx;
            self.open_kind = OpenBlock::Thinking;
        }
        out.push(AnthropicEvent::ContentBlockDelta {
            index: self.current_block_index,
            delta: ContentBlockDelta::ThinkingDelta {
                thinking: content.to_string(),
            },
        });
        out
    }

    fn on_tool_use(
        &mut self,
        id: &str,
        name: &str,
        input: &serde_json::Value,
    ) -> Vec<AnthropicEvent> {
        let mut out = Vec::with_capacity(4);
        self.open_message_if_needed(&mut out);
        self.close_open_block(&mut out);

        let idx = self.next_block_index;
        self.next_block_index += 1;

        // content_block_start opens with an empty input; the input is
        // streamed as an input_json_delta so downstream consumers that
        // parse Anthropic's wire format get the exact shape they expect.
        out.push(AnthropicEvent::ContentBlockStart {
            index: idx,
            content_block: ContentBlockStub::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input: serde_json::json!({}),
            },
        });
        // Serialize the input as a single input_json_delta — the previous SDK
        // gives us the complete input atomically, so one frame is enough.
        let partial_json = serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string());
        out.push(AnthropicEvent::ContentBlockDelta {
            index: idx,
            delta: ContentBlockDelta::InputJsonDelta { partial_json },
        });
        out.push(AnthropicEvent::ContentBlockStop { index: idx });

        // Tool-use blocks are self-contained — the block is closed
        // before we move on. Leave `open_kind = None` so the next event
        // opens a fresh block.
        self.open_kind = OpenBlock::None;
        out
    }

    fn on_tool_result(
        &mut self,
        tool_use_id: &str,
        content: &str,
        is_error: bool,
    ) -> Vec<AnthropicEvent> {
        let mut out = Vec::with_capacity(3);
        self.open_message_if_needed(&mut out);
        self.close_open_block(&mut out);

        let idx = self.next_block_index;
        self.next_block_index += 1;
        out.push(AnthropicEvent::ContentBlockStart {
            index: idx,
            content_block: ContentBlockStub::ToolResult {
                tool_use_id: tool_use_id.to_string(),
                content: content.to_string(),
                is_error,
            },
        });
        out.push(AnthropicEvent::ContentBlockStop { index: idx });

        self.open_kind = OpenBlock::None;
        out
    }

    fn on_result(&mut self, status: &str, is_error: bool) -> Vec<AnthropicEvent> {
        let mut out = Vec::with_capacity(3);
        self.close_open_block(&mut out);
        if !self.message_open {
            // the previous SDK can fire Result without any preceding content (e.g.,
            // an empty error turn). Open+close the message anyway so the
            // Anthropic stream remains well-formed.
            self.open_message_if_needed(&mut out);
        }
        let stop_reason = if is_error {
            "error".to_string()
        } else {
            match status {
                "success" | "completed" => "end_turn".to_string(),
                "max_tokens" => "max_tokens".to_string(),
                "cancelled" => "cancelled".to_string(),
                other => other.to_string(),
            }
        };
        out.push(AnthropicEvent::MessageDelta {
            delta: MessageDeltaBody {
                stop_reason: Some(stop_reason),
                stop_sequence: None,
            },
            usage: None,
        });
        out.push(AnthropicEvent::MessageStop);
        self.message_open = false;
        self.next_block_index = 0;
        self.open_kind = OpenBlock::None;
        out
    }

    fn on_status(&mut self, status: &str, message: Option<&str>) -> Vec<AnthropicEvent> {
        match status {
            // Heartbeats map cleanly to Anthropic's `ping` frame.
            "heartbeat" => vec![AnthropicEvent::Ping],
            // Failure states become an `error` frame + a terminal
            // `message_stop` if a message was open.
            "failed" | "resume_failed" | "timeout" => {
                let mut out = Vec::with_capacity(3);
                self.close_open_block(&mut out);
                out.push(AnthropicEvent::Error {
                    error: ApiError {
                        error_type: status.to_string(),
                        message: message.unwrap_or(status).to_string(),
                    },
                });
                if self.message_open {
                    out.push(AnthropicEvent::MessageStop);
                    self.message_open = false;
                    self.next_block_index = 0;
                }
                out
            }
            // "running" / "completed" are worker-lifecycle markers, not
            // Anthropic events. Completion maps to the on_result path
            // when the previous SDK sends a proper Result — if we see a bare
            // Status("completed") without a Result (e.g., the trailing
            // status after the stream ends cleanly), synthesize a
            // message_stop if a message is still open.
            "completed" => {
                if self.message_open {
                    let mut out = Vec::with_capacity(3);
                    self.close_open_block(&mut out);
                    out.push(AnthropicEvent::MessageDelta {
                        delta: MessageDeltaBody {
                            stop_reason: Some("end_turn".to_string()),
                            stop_sequence: None,
                        },
                        usage: None,
                    });
                    out.push(AnthropicEvent::MessageStop);
                    self.message_open = false;
                    self.next_block_index = 0;
                    self.open_kind = OpenBlock::None;
                    out
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }

    /// Emit a pure `ping` frame without touching any other state.
    /// Used by the worker's 15-second heartbeat tick separately from
    /// StreamEvent::Status("heartbeat") — callers should prefer the
    /// Status path, but this exists for direct emission.
    #[allow(dead_code)]
    pub fn ping() -> AnthropicEvent {
        AnthropicEvent::Ping
    }

    /// Attach a final usage snapshot to a just-emitted `message_delta`.
    /// The worker computes token usage from the previous SDK's `Result` metadata;
    /// this is a small helper so callers can override the `usage: None`
    /// we emit from `on_result`.
    #[allow(dead_code)]
    pub fn with_usage(ev: &mut AnthropicEvent, usage: Usage) {
        if let AnthropicEvent::MessageDelta { usage: u, .. } = ev {
            *u = Some(usage);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::StreamEvent;
    use serde_json::json;

    fn event_types(events: &[AnthropicEvent]) -> Vec<&'static str> {
        events.iter().map(|e| e.event_type()).collect()
    }

    fn encoder_with_message_id(message_id: &str) -> AnthropicEventEncoder {
        let mut tx = AnthropicEventEncoder::new("conv-test");
        tx.set_pending_message_id(message_id.to_string());
        tx
    }

    #[test]
    fn text_only_turn_produces_canonical_sequence() {
        let mut tx = encoder_with_message_id("asst-text");
        let mut all = Vec::new();
        all.extend(tx.encode(&StreamEvent::Text {
            content: "Hello".into(),
        }));
        all.extend(tx.encode(&StreamEvent::Text {
            content: ", world".into(),
        }));
        all.extend(tx.encode(&StreamEvent::Result {
            session_id: "s1".into(),
            status: "success".into(),
            is_error: false,
        }));

        assert_eq!(
            event_types(&all),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
    }

    #[test]
    fn tool_use_turn_produces_input_json_delta() {
        let mut tx = encoder_with_message_id("asst-tool");
        let mut all = Vec::new();
        all.extend(tx.encode(&StreamEvent::ToolUse {
            id: "toolu_1".into(),
            name: "search".into(),
            input: json!({"q": "rust"}),
        }));
        all.extend(tx.encode(&StreamEvent::Result {
            session_id: "s1".into(),
            status: "success".into(),
            is_error: false,
        }));

        // message_start, content_block_start(tool_use), content_block_delta(input_json_delta),
        // content_block_stop, message_delta, message_stop
        assert_eq!(
            event_types(&all),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );

        // Verify the inner delta is input_json_delta with the serialized input
        if let AnthropicEvent::ContentBlockDelta { delta, .. } = &all[2] {
            if let ContentBlockDelta::InputJsonDelta { partial_json } = delta {
                let parsed: serde_json::Value = serde_json::from_str(partial_json).unwrap();
                assert_eq!(parsed, json!({"q": "rust"}));
            } else {
                panic!("expected input_json_delta, got {:?}", delta);
            }
        } else {
            panic!("expected content_block_delta at index 2");
        }
    }

    #[test]
    fn thinking_block_uses_thinking_delta() {
        let mut tx = encoder_with_message_id("asst-thinking");
        let all: Vec<_> = [
            StreamEvent::Thinking {
                content: "Let me see...".into(),
            },
            StreamEvent::Text {
                content: "Answer.".into(),
            },
            StreamEvent::Result {
                session_id: "s1".into(),
                status: "success".into(),
                is_error: false,
            },
        ]
        .iter()
        .flat_map(|e| tx.encode(e))
        .collect();

        // Thinking block opens, gets a thinking_delta, then CLOSES (content_block_stop)
        // before Text opens a new text block.
        assert_eq!(
            event_types(&all),
            vec![
                "message_start",
                "content_block_start", // thinking
                "content_block_delta", // thinking_delta
                "content_block_stop",  // thinking closes
                "content_block_start", // text
                "content_block_delta", // text_delta
                "content_block_stop",  // text closes on result
                "message_delta",
                "message_stop",
            ]
        );
        if let AnthropicEvent::ContentBlockDelta { delta, .. } = &all[2] {
            assert!(matches!(delta, ContentBlockDelta::ThinkingDelta { .. }));
        } else {
            panic!("expected thinking_delta");
        }
    }

    #[test]
    fn heartbeat_maps_to_ping() {
        let mut tx = AnthropicEventEncoder::new("c");
        let out = tx.encode(&StreamEvent::Status {
            status: "heartbeat".into(),
            message: None,
        });
        assert_eq!(event_types(&out), vec!["ping"]);
    }

    #[test]
    fn router_events_are_not_emitted_on_wire() {
        let mut tx = AnthropicEventEncoder::new("c");
        assert!(tx
            .encode(&StreamEvent::RouterResult {
                enriched_message: "x".into(),
                ticket_id: None,
                organization: None,
                skipped: true,
            })
            .is_empty());
        assert!(tx
            .encode(&StreamEvent::TitleUpdate { title: "t".into() })
            .is_empty());
        assert!(tx
            .encode(&StreamEvent::ReplayComplete {
                total_events: 5,
                agent_status: "running".into(),
            })
            .is_empty());
    }

    #[test]
    fn failed_status_produces_error_and_message_stop() {
        let mut tx = encoder_with_message_id("asst-failed");
        // Open a text block first
        let _ = tx.encode(&StreamEvent::Text {
            content: "hi".into(),
        });
        let err = tx.encode(&StreamEvent::Status {
            status: "failed".into(),
            message: Some("boom".into()),
        });
        // expected: content_block_stop, error, message_stop
        assert_eq!(
            event_types(&err),
            vec!["content_block_stop", "error", "message_stop"]
        );
    }

    #[test]
    fn message_start_uses_pending_message_id() {
        let mut tx = encoder_with_message_id("assistant-db-id");
        let all: Vec<_> = tx.encode(&StreamEvent::Text {
            content: "hi".into(),
        });
        let ms = &all[0];
        assert_eq!(ms.event_type(), "message_start");
        let v = serde_json::to_value(ms).unwrap();
        assert_eq!(v["message"]["id"], "assistant-db-id");
    }

    /// T-A819D36B: the Idempotency-Key staged via
    /// `set_pending_client_id` must appear on the `message_start` frame's
    /// `client_id` field, which serializes as snake_case `client_id` in
    /// the wire JSON (iOS's `MessageEchoService` decodes this exact key).
    #[test]
    fn message_start_echoes_pending_client_id() {
        let mut tx = encoder_with_message_id("asst-idempo");
        tx.set_pending_client_id(Some("test-abc-123".to_string()));
        let all: Vec<_> = tx.encode(&StreamEvent::Text {
            content: "hi".into(),
        });
        // First frame must be message_start carrying client_id.
        let ms = &all[0];
        assert_eq!(ms.event_type(), "message_start");
        let v = serde_json::to_value(ms).unwrap();
        assert_eq!(v["type"], "message_start");
        assert_eq!(v["message"]["client_id"], "test-abc-123");
    }

    /// T-A819D36B: when no Idempotency-Key was supplied the encoder
    /// propagates `None` and serde skips the field entirely — iOS tolerates
    /// both "field absent" and "field: null", but the contract the artifact
    /// documents is "absent".
    #[test]
    fn message_start_omits_client_id_when_none() {
        let mut tx = encoder_with_message_id("asst-no-idempo");
        // No call to set_pending_client_id — default is None.
        let all: Vec<_> = tx.encode(&StreamEvent::Text {
            content: "hi".into(),
        });
        let ms = &all[0];
        assert_eq!(ms.event_type(), "message_start");
        let v = serde_json::to_value(ms).unwrap();
        assert!(
            v["message"].get("client_id").is_none(),
            "client_id must be serde-skipped when None, got {:?}",
            v
        );
    }

    /// T-A819D36B: the staged client_id is consumed by the FIRST
    /// `message_start` and does not leak onto subsequent ones. If the
    /// encoder is re-used for a second turn (same worker, same
    /// encoder instance) without restaging — once `on_result` has
    /// closed the first turn — the second turn's message_start must
    /// NOT echo the first turn's key.
    #[test]
    fn pending_client_id_is_consumed_once() {
        let mut tx = encoder_with_message_id("asst-once-1");
        tx.set_pending_client_id(Some("key-1".to_string()));

        let turn1: Vec<_> = [
            StreamEvent::Text {
                content: "a".into(),
            },
            StreamEvent::Result {
                session_id: "s".into(),
                status: "success".into(),
                is_error: false,
            },
        ]
        .iter()
        .flat_map(|e| tx.encode(e))
        .collect();
        // The `Result` event already closes the message via on_result;
        // the worker stages a fresh assistant row id before the next turn.
        tx.set_pending_message_id("asst-once-2");

        // Second turn — NO set_pending_client_id, so client_id must be absent.
        let turn2: Vec<_> = tx.encode(&StreamEvent::Text {
            content: "b".into(),
        });

        let turn1_json = serde_json::to_value(&turn1[0]).unwrap();
        assert_eq!(turn1_json["message"]["client_id"], "key-1");

        let turn2_json = serde_json::to_value(&turn2[0]).unwrap();
        assert!(
            turn2_json["message"].get("client_id").is_none(),
            "second turn must not leak first turn's client_id, got {:?}",
            turn2_json
        );
    }

    #[test]
    fn tool_use_after_text_closes_text_block() {
        let mut tx = encoder_with_message_id("asst-mixed");
        let all: Vec<_> = [
            StreamEvent::Text {
                content: "I'll search.".into(),
            },
            StreamEvent::ToolUse {
                id: "t1".into(),
                name: "search".into(),
                input: json!({}),
            },
            StreamEvent::Text {
                content: "Done.".into(),
            },
            StreamEvent::Result {
                session_id: "s".into(),
                status: "success".into(),
                is_error: false,
            },
        ]
        .iter()
        .flat_map(|e| tx.encode(e))
        .collect();
        assert_eq!(
            event_types(&all),
            vec![
                "message_start",
                "content_block_start", // text 0
                "content_block_delta", // text_delta
                "content_block_stop",  // text 0 closes before tool_use
                "content_block_start", // tool_use 1
                "content_block_delta", // input_json_delta
                "content_block_stop",  // tool_use 1 closes
                "content_block_start", // text 2
                "content_block_delta", // text_delta
                "content_block_stop",  // text 2 closes on result
                "message_delta",
                "message_stop",
            ]
        );
    }
}
