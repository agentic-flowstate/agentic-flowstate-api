//! Translate internal [`StreamEvent`]s into the Anthropic Messages API
//! streaming vocabulary.
//!
//! The `ConversationWorker` emits a custom `StreamEvent` enum (`Text`,
//! `ToolUse`, `Status`, ...) that the iOS app has consumed for months.
//! T-49352BF5 layers the Anthropic 8-event vocabulary on top without
//! breaking that path — every internal event is re-expressed as the
//! equivalent sequence of Anthropic events, and `ConversationWorker` can
//! then write either vocabulary (or both, dual-write).
//!
//! ## Mapping rules
//!
//! cc-sdk streams text/thinking/tool_use/tool_result content blocks on an
//! assistant turn, terminated by a `Result`. We model the turn as a single
//! Anthropic message:
//!
//! ```text
//! Status("running")              → (no v2 event — worker-internal lifecycle)
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
//! Router events (`RouterText`, `RouterToolUse`, `RouterToolResult`,
//! `RouterResult`), title/org updates, user messages, and
//! `ReplayComplete` aren't part of the Anthropic vocabulary — the
//! translator keeps them OFF the v2 wire entirely. They still ride the
//! v1 stream because `EVENT_VOCAB_MODE != modern_only` keeps the legacy
//! writer active.

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

/// Stateful translator. One instance per assistant turn. After the
/// terminal `Result`/`Error` is emitted, call [`AnthropicTranslator::reset`]
/// (or drop the translator and build a fresh one) before the next turn —
/// otherwise message_start / content-block indices will leak across turns.
pub struct AnthropicTranslator {
    /// True after `message_start` has been emitted for the current turn.
    message_open: bool,
    /// Next content-block index to allocate.
    next_block_index: u32,
    /// Which block (if any) is currently open at `current_block_index`.
    open_kind: OpenBlock,
    /// Index of the currently-open block — only meaningful when
    /// `open_kind != None`.
    current_block_index: u32,
    /// Per-conversation monotonic message counter, used to fabricate a
    /// stable `message_id` when cc-sdk doesn't give us one. Format
    /// `msg_<conv_short>_<n>`.
    message_counter: u32,
    /// Short prefix of the conversation id, cached for message_id
    /// construction.
    conv_short: String,
}

impl AnthropicTranslator {
    pub fn new(conversation_id: &str) -> Self {
        let conv_short = conversation_id
            .chars()
            .take(8)
            .collect::<String>();
        Self {
            message_open: false,
            next_block_index: 0,
            open_kind: OpenBlock::None,
            current_block_index: 0,
            message_counter: 0,
            conv_short,
        }
    }

    /// True if the translator is currently inside a logical assistant
    /// message (has emitted `message_start` but not yet `message_stop`).
    pub fn is_message_open(&self) -> bool {
        self.message_open
    }

    /// Close the current turn and zero counters so the next translator
    /// call starts a fresh Anthropic message. Caller must have already
    /// drained the terminal events via [`Self::translate`] — `reset`
    /// does NOT flush anything.
    pub fn reset(&mut self) {
        self.message_open = false;
        self.next_block_index = 0;
        self.open_kind = OpenBlock::None;
        self.current_block_index = 0;
    }

    /// Translate a single internal `StreamEvent` into the sequence of
    /// Anthropic events that should be emitted on the v2 wire.
    ///
    /// Returns an empty `Vec` for events that have no Anthropic
    /// equivalent (router events, title updates, replay markers).
    pub fn translate(&mut self, event: &StreamEvent) -> Vec<AnthropicEvent> {
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
            // they flow only on the legacy (v1) channel.
            StreamEvent::RouterText { .. }
            | StreamEvent::RouterToolUse { .. }
            | StreamEvent::RouterToolResult { .. }
            | StreamEvent::RouterResult { .. }
            | StreamEvent::UserMessage { .. }
            | StreamEvent::ReplayComplete { .. }
            | StreamEvent::TitleUpdate { .. }
            | StreamEvent::OrgUpdate { .. } => Vec::new(),
        }
    }

    fn open_message_if_needed(&mut self, out: &mut Vec<AnthropicEvent>) {
        if !self.message_open {
            self.message_counter += 1;
            let id = format!("msg_{}_{}", self.conv_short, self.message_counter);
            out.push(AnthropicEvent::MessageStart {
                message: MessageStub::new_assistant(id, None),
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
        // Serialize the input as a single input_json_delta — cc-sdk
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
            // cc-sdk can fire Result without any preceding content (e.g.,
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
            // when cc-sdk sends a proper Result — if we see a bare
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
    /// The worker computes token usage from cc-sdk's `Result` metadata;
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

    #[test]
    fn text_only_turn_produces_canonical_sequence() {
        let mut tx = AnthropicTranslator::new("conv-abcdef1234");
        let mut all = Vec::new();
        all.extend(tx.translate(&StreamEvent::Text {
            content: "Hello".into(),
        }));
        all.extend(tx.translate(&StreamEvent::Text {
            content: ", world".into(),
        }));
        all.extend(tx.translate(&StreamEvent::Result {
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
        let mut tx = AnthropicTranslator::new("conv-abcd1234");
        let mut all = Vec::new();
        all.extend(tx.translate(&StreamEvent::ToolUse {
            id: "toolu_1".into(),
            name: "search".into(),
            input: json!({"q": "rust"}),
        }));
        all.extend(tx.translate(&StreamEvent::Result {
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
        let mut tx = AnthropicTranslator::new("conv-9999");
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
        .flat_map(|e| tx.translate(e))
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
        let mut tx = AnthropicTranslator::new("c");
        let out = tx.translate(&StreamEvent::Status {
            status: "heartbeat".into(),
            message: None,
        });
        assert_eq!(event_types(&out), vec!["ping"]);
    }

    #[test]
    fn router_events_are_not_emitted_on_v2() {
        let mut tx = AnthropicTranslator::new("c");
        assert!(tx
            .translate(&StreamEvent::RouterText {
                content: "x".into(),
            })
            .is_empty());
        assert!(tx
            .translate(&StreamEvent::RouterToolUse {
                id: "a".into(),
                name: "b".into(),
                input: json!({}),
            })
            .is_empty());
        assert!(tx
            .translate(&StreamEvent::TitleUpdate {
                title: "t".into(),
            })
            .is_empty());
        assert!(tx
            .translate(&StreamEvent::ReplayComplete {
                total_events: 5,
                agent_status: "running".into(),
            })
            .is_empty());
    }

    #[test]
    fn failed_status_produces_error_and_message_stop() {
        let mut tx = AnthropicTranslator::new("c");
        // Open a text block first
        let _ = tx.translate(&StreamEvent::Text {
            content: "hi".into(),
        });
        let err = tx.translate(&StreamEvent::Status {
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
    fn tool_use_after_text_closes_text_block() {
        let mut tx = AnthropicTranslator::new("c");
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
        .flat_map(|e| tx.translate(e))
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
