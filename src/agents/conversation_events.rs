//! Agentic Flowstate's persisted conversation streaming vocabulary.
//!
//! This module defines the typed enum that represents the events emitted by
//! the API, along with the four `content_block_delta` sub-variants. Serde
//! serialization is configured to produce deterministic
//! wire-format frames: flat objects with a `type` field as the discriminator,
//! nested `delta` object for block deltas, snake_case field names throughout.
//!
//! - `message_start` — first frame: carries the skeleton `Message` (id, role,
//!   model, empty `content`).
//! - `content_block_start` — a new content block opens at `index`. Body is a
//!   `ContentBlock` stub (type=text/tool_use/thinking, empty fields).
//! - `content_block_delta` — partial update to the block at `index`. Body is
//!   a `delta` object, one of:
//!   - `text_delta` — `{ type:"text_delta", text:"..." }`
//!   - `input_json_delta` — `{ type:"input_json_delta", partial_json:"..." }`
//!     (streamed JSON for tool_use `input`; concatenate across deltas).
//!   - `thinking_delta` — `{ type:"thinking_delta", thinking:"..." }`
//!   - `signature_delta` — `{ type:"signature_delta", signature:"..." }`
//!     (thinking block integrity signature).
//! - `content_block_stop` — block at `index` is finalized.
//! - `message_delta` — top-level message fields changed (stop_reason,
//!   stop_sequence, usage updates).
//! - `message_stop` — message complete.
//! - `ping` — keepalive, no semantic payload.
//! - `error` — terminal error frame with an `error` object.
//!
//! This is an internal, provider-neutral protocol shared by the Rust API and
//! native clients. Agent runtimes are adapted into these events before durable
//! persistence so changing the runtime does not change reconnect behavior.

use serde::{Deserialize, Serialize};

/// The 8-event top-level streaming vocabulary.
///
/// Serialized as a flat object with `type` tag — e.g.
/// `{"type":"message_start","message":{...}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConversationEvent {
    /// First frame of a message. Payload is a skeleton `Message` with empty
    /// `content` array.
    MessageStart { message: MessageStub },
    /// A new content block has opened at `index`.
    ContentBlockStart {
        index: u32,
        content_block: ContentBlockStub,
    },
    /// Incremental update to the block at `index`.
    ContentBlockDelta {
        index: u32,
        delta: ContentBlockDelta,
    },
    /// The block at `index` is finalized.
    ContentBlockStop { index: u32 },
    /// Top-level message fields changed (stop_reason, usage, ...).
    MessageDelta {
        delta: MessageDeltaBody,
        /// Optional usage snapshot. The API emits this on the terminal
        /// `message_delta` right before `message_stop`.
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
    /// Message is complete.
    MessageStop,
    /// Transport keepalive.
    Ping,
    /// Terminal error with a stable `{type, message}` body.
    Error { error: ApiError },
    /// Synthetic resumption snapshot for a mid-stream content block
    /// (T-F8F986A9). Emitted by the resume stream replay path BEFORE
    /// forward-emission continues, ONLY for blocks whose stored log
    /// shows `content_block_start` + some deltas but no matching
    /// `content_block_stop` at the resume cursor. The `block` field
    /// carries the fully-reconstructed state of the block (accumulated
    /// text / input_json / thinking + signature) so the client can
    /// replace its half-rendered local copy authoritatively without
    /// having to replay every delta frame it already saw.
    ///
    /// Wire shape mirrors the protocol's documented snapshot event:
    /// `{"type":"content_block_snapshot","index":N,"block":{...}}`.
    ///
    /// The event is transport-only — it is NOT persisted to
    /// `conversation_events`. The resume handler stamps it with
    /// `id: <cursor>` (the client's Last-Event-ID) rather than a new
    /// event_index so the forward cursor is never polluted. On a
    /// subsequent reconnect with the same Last-Event-ID the server will
    /// re-emit an equivalent snapshot derived fresh from the DB —
    /// clients must treat snapshots as idempotent replacements, not
    /// cumulative deltas.
    ContentBlockSnapshot { index: u32, block: ContentBlockStub },
}

impl ConversationEvent {
    /// The conversation event-type string used for the `event_type`
    /// column in `conversation_events`.
    pub fn event_type(&self) -> &'static str {
        match self {
            ConversationEvent::MessageStart { .. } => "message_start",
            ConversationEvent::ContentBlockStart { .. } => "content_block_start",
            ConversationEvent::ContentBlockDelta { .. } => "content_block_delta",
            ConversationEvent::ContentBlockStop { .. } => "content_block_stop",
            ConversationEvent::MessageDelta { .. } => "message_delta",
            ConversationEvent::MessageStop => "message_stop",
            ConversationEvent::Ping => "ping",
            ConversationEvent::Error { .. } => "error",
            ConversationEvent::ContentBlockSnapshot { .. } => "content_block_snapshot",
        }
    }
}

/// Skeleton `Message` emitted inside `message_start`.
///
/// The API sends the full message header with an empty `content` array;
/// the content is streamed via `content_block_*` frames. Fields that aren't
/// known yet (`stop_reason`, `stop_sequence`) are null.
///
/// `client_id` (T-A819D36B) carries the `Idempotency-Key` the iOS client
/// supplied when POSTing the user message that triggered this assistant
/// turn. iOS's `MessageEchoService.reconcileServerMessageStart` matches on
/// this value to lock the optimistic-echo row to the server's canonical
/// `id`. When no Idempotency-Key was supplied, the field is `None` and
/// serialization skips it entirely — iOS tolerates both absent and `null`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageStub {
    pub id: String,
    /// Always `"message"` for this frame.
    #[serde(rename = "type")]
    pub message_type: String,
    /// Always `"assistant"` — server-to-client stream only carries assistant
    /// messages.
    pub role: String,
    /// Model identifier (e.g., `"gpt-5.5"`). Optional because the runtime adapter
    /// doesn't always surface it; we pass `None` when unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Empty at message_start; populated via content_block_* frames.
    pub content: Vec<ContentBlockStub>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequence: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Optimistic-echo reconciliation key (T-A819D36B). Carries the
    /// `Idempotency-Key` the client supplied on the POST that triggered
    /// this assistant turn. iOS's `MessageEchoService` matches on this
    /// value to promote its local-echo row. `None` when absent on the
    /// request — serialization skips the field in that case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
}

impl MessageStub {
    /// Build a fresh `message_start` stub for an assistant message.
    /// `model` can be `None` when the runtime adapter doesn't surface the model id.
    /// `client_id` defaults to `None`; use [`Self::with_client_id`] to
    /// attach the optimistic-echo reconciliation key (T-A819D36B).
    pub fn new_assistant(id: impl Into<String>, model: Option<String>) -> Self {
        Self {
            id: id.into(),
            message_type: "message".to_string(),
            role: "assistant".to_string(),
            model,
            content: Vec::new(),
            stop_reason: None,
            stop_sequence: None,
            usage: None,
            client_id: None,
        }
    }

    /// Attach a `client_id` (the request's `Idempotency-Key`) to this
    /// stub so the emitted `message_start` frame echoes it back to the
    /// client. Used by the turn pipeline to thread the header value from
    /// the HTTP layer all the way into the SSE payload iOS decodes.
    pub fn with_client_id(mut self, client_id: Option<String>) -> Self {
        self.client_id = client_id;
        self
    }
}

/// `content_block_start` body. Each variant is serialized with a flat
/// `type` tag — e.g. `{"type":"text","text":""}`,
/// `{"type":"tool_use","id":"...","name":"...","input":{}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlockStub {
    /// Text block — starts empty, filled by `text_delta` updates.
    Text {
        #[serde(default)]
        text: String,
    },
    /// Tool-use block — `input` starts as empty object, filled by
    /// `input_json_delta` updates (concatenate partial_json, then parse).
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: serde_json::Value,
    },
    /// Thinking (extended-reasoning) block — starts empty, filled by
    /// `thinking_delta` + `signature_delta` updates.
    Thinking {
        #[serde(default)]
        thinking: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        signature: String,
    },
    /// Tool-result block. In the protocol's REST API this appears on USER
    /// messages (the reply sent after a tool_use). The runtime adapter surfaces the
    /// result inline on the assistant stream as a separate Message with
    /// `role: "user"` content. We emit it as a `content_block_start`
    /// with the full result as initial content — no delta follow-ups.
    ToolResult {
        tool_use_id: String,
        /// Inline text form of the result — the runtime adapter flattens structured
        /// results into a JSON string before we see them.
        #[serde(default)]
        content: String,
        /// Mirrors the protocol's `is_error` flag on tool_result blocks.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
}

/// The four `content_block_delta` sub-variants. Serialized as a flat object
/// with `type` — e.g. `{"type":"text_delta","text":"..."}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlockDelta {
    /// Incremental text content for a `text` block.
    TextDelta { text: String },
    /// Incremental JSON string for a `tool_use` block's `input`. Multiple
    /// deltas concatenate into a single JSON string that the client parses
    /// after the matching `content_block_stop`.
    InputJsonDelta { partial_json: String },
    /// Incremental thinking text for a `thinking` block.
    ThinkingDelta { thinking: String },
    /// Integrity signature for a `thinking` block — carries an opaque token
    /// that the server can use to verify the block wasn't tampered with.
    SignatureDelta { signature: String },
}

/// Body of the top-level `message_delta` frame. Fields match the protocol's
/// docs — any combination can be present. Only fields the server has
/// learned about are serialized (others are skipped).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MessageDeltaBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequence: Option<String>,
}

/// Token-usage snapshot. The API surfaces this on `message_start` (initial
/// rate-limit bookkeeping) and on the terminal `message_delta` (final counts).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
}

/// Body of an `error` frame. The protocol uses
/// `{"error":{"type":"overloaded_error","message":"..."}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiError {
    #[serde(rename = "type")]
    pub error_type: String,
    pub message: String,
}

/// The valid conversation event-type strings, exposed as an array for tests
/// that want to enumerate the vocabulary. Consumed by the regression
/// guard in `conversation_worker::streaming_persistence_tests` (pinned
/// `ALLOWED_EVENT_TYPES`) and by the event-type round-trip test in this
/// module's own `tests` submodule. Both call sites are `#[cfg(test)]`,
/// so the release build sees zero non-test usages — but the test-only
/// contract is load-bearing, and deleting the constant would silently
/// break the regression guard.
#[allow(dead_code)]
pub const ALL_EVENT_TYPES: &[&str] = &[
    "message_start",
    "content_block_start",
    "content_block_delta",
    "content_block_stop",
    "message_delta",
    "message_stop",
    "ping",
    "error",
];

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Byte-exact serialization: every variant round-trips through JSON and
    /// the wire shape matches the conversation protocol contract.
    #[test]
    fn message_start_wire_shape() {
        let ev = ConversationEvent::MessageStart {
            message: MessageStub::new_assistant("msg_abc123", Some("gpt-5.5".to_string())),
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(
            v,
            json!({
                "type": "message_start",
                "message": {
                    "id": "msg_abc123",
                    "type": "message",
                    "role": "assistant",
                    "model": "gpt-5.5",
                    "content": []
                }
            })
        );
        let round: ConversationEvent = serde_json::from_value(v).unwrap();
        assert_eq!(round, ev);
    }

    /// T-A819D36B: `client_id` echoes on the wire when set, is skipped when absent.
    /// This is the contract iOS `MessageEchoService.reconcileServerMessageStart`
    /// depends on for optimistic-echo reconciliation.
    #[test]
    fn message_start_with_client_id_wire_shape() {
        let ev = ConversationEvent::MessageStart {
            message: MessageStub::new_assistant("msg_abc123", Some("gpt-5.5".to_string()))
                .with_client_id(Some("test-abc-123".to_string())),
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(
            v,
            json!({
                "type": "message_start",
                "message": {
                    "id": "msg_abc123",
                    "type": "message",
                    "role": "assistant",
                    "model": "gpt-5.5",
                    "content": [],
                    "client_id": "test-abc-123"
                }
            })
        );
        // Round-trip: deserialize preserves the client_id.
        let round: ConversationEvent = serde_json::from_value(v).unwrap();
        assert_eq!(round, ev);
    }

    #[test]
    fn message_start_without_client_id_skips_field() {
        let ev = ConversationEvent::MessageStart {
            message: MessageStub::new_assistant("msg_abc123", None).with_client_id(None),
        };
        let v = serde_json::to_value(&ev).unwrap();
        // The `client_id` key MUST NOT appear when value is None.
        assert!(
            v.get("message").and_then(|m| m.get("client_id")).is_none(),
            "client_id must be skipped when None, got: {}",
            v
        );
    }

    #[test]
    fn content_block_start_text_wire_shape() {
        let ev = ConversationEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStub::Text {
                text: String::new(),
            },
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(
            v,
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": { "type": "text", "text": "" }
            })
        );
    }

    #[test]
    fn content_block_start_tool_use_wire_shape() {
        let ev = ConversationEvent::ContentBlockStart {
            index: 1,
            content_block: ContentBlockStub::ToolUse {
                id: "toolu_01".to_string(),
                name: "get_weather".to_string(),
                input: json!({}),
            },
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(
            v,
            json!({
                "type": "content_block_start",
                "index": 1,
                "content_block": {
                    "type": "tool_use",
                    "id": "toolu_01",
                    "name": "get_weather",
                    "input": {}
                }
            })
        );
    }

    #[test]
    fn content_block_start_thinking_wire_shape() {
        let ev = ConversationEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStub::Thinking {
                thinking: String::new(),
                signature: String::new(),
            },
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(
            v,
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": { "type": "thinking", "thinking": "" }
            })
        );
    }

    #[test]
    fn content_block_delta_text_wire_shape() {
        let ev = ConversationEvent::ContentBlockDelta {
            index: 0,
            delta: ContentBlockDelta::TextDelta {
                text: "Hello".to_string(),
            },
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(
            v,
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "text_delta", "text": "Hello" }
            })
        );
        let round: ConversationEvent = serde_json::from_value(v).unwrap();
        assert_eq!(round, ev);
    }

    #[test]
    fn content_block_delta_input_json_wire_shape() {
        let ev = ConversationEvent::ContentBlockDelta {
            index: 1,
            delta: ContentBlockDelta::InputJsonDelta {
                partial_json: "{\"location\":\"SF\"}".to_string(),
            },
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(
            v,
            json!({
                "type": "content_block_delta",
                "index": 1,
                "delta": {
                    "type": "input_json_delta",
                    "partial_json": "{\"location\":\"SF\"}"
                }
            })
        );
    }

    #[test]
    fn content_block_delta_thinking_wire_shape() {
        let ev = ConversationEvent::ContentBlockDelta {
            index: 0,
            delta: ContentBlockDelta::ThinkingDelta {
                thinking: "Let me consider...".to_string(),
            },
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(
            v,
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {
                    "type": "thinking_delta",
                    "thinking": "Let me consider..."
                }
            })
        );
    }

    #[test]
    fn content_block_delta_signature_wire_shape() {
        let ev = ConversationEvent::ContentBlockDelta {
            index: 0,
            delta: ContentBlockDelta::SignatureDelta {
                signature: "sig_abc".to_string(),
            },
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(
            v,
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {
                    "type": "signature_delta",
                    "signature": "sig_abc"
                }
            })
        );
    }

    #[test]
    fn content_block_stop_wire_shape() {
        let ev = ConversationEvent::ContentBlockStop { index: 2 };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v, json!({ "type": "content_block_stop", "index": 2 }));
    }

    #[test]
    fn message_delta_wire_shape() {
        let ev = ConversationEvent::MessageDelta {
            delta: MessageDeltaBody {
                stop_reason: Some("end_turn".to_string()),
                stop_sequence: None,
            },
            usage: Some(Usage {
                input_tokens: Some(10),
                output_tokens: Some(42),
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            }),
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(
            v,
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": "end_turn" },
                "usage": { "input_tokens": 10, "output_tokens": 42 }
            })
        );
    }

    #[test]
    fn message_stop_wire_shape() {
        let ev = ConversationEvent::MessageStop;
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v, json!({ "type": "message_stop" }));
    }

    #[test]
    fn ping_wire_shape() {
        let ev = ConversationEvent::Ping;
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v, json!({ "type": "ping" }));
    }

    #[test]
    fn error_wire_shape() {
        let ev = ConversationEvent::Error {
            error: ApiError {
                error_type: "overloaded_error".to_string(),
                message: "Overloaded".to_string(),
            },
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(
            v,
            json!({
                "type": "error",
                "error": { "type": "overloaded_error", "message": "Overloaded" }
            })
        );
    }

    #[test]
    fn event_type_string_matches_tag() {
        let cases: &[(ConversationEvent, &str)] = &[
            (
                ConversationEvent::MessageStart {
                    message: MessageStub::new_assistant("x", None),
                },
                "message_start",
            ),
            (
                ConversationEvent::ContentBlockStart {
                    index: 0,
                    content_block: ContentBlockStub::Text {
                        text: String::new(),
                    },
                },
                "content_block_start",
            ),
            (
                ConversationEvent::ContentBlockDelta {
                    index: 0,
                    delta: ContentBlockDelta::TextDelta {
                        text: String::new(),
                    },
                },
                "content_block_delta",
            ),
            (
                ConversationEvent::ContentBlockStop { index: 0 },
                "content_block_stop",
            ),
            (
                ConversationEvent::MessageDelta {
                    delta: MessageDeltaBody::default(),
                    usage: None,
                },
                "message_delta",
            ),
            (ConversationEvent::MessageStop, "message_stop"),
            (ConversationEvent::Ping, "ping"),
            (
                ConversationEvent::Error {
                    error: ApiError {
                        error_type: "x".into(),
                        message: "y".into(),
                    },
                },
                "error",
            ),
        ];

        let all: std::collections::HashSet<&str> = ALL_EVENT_TYPES.iter().copied().collect();
        for (ev, expected) in cases {
            assert_eq!(ev.event_type(), *expected);
            assert!(
                all.contains(expected),
                "{} not in ALL_EVENT_TYPES",
                expected
            );
        }
    }
}
