//! The internal `StreamEvent` enum emitted by the agent runtime.
//!
//! `StreamEvent` is the sole on-the-wire representation of an agent turn
//! as seen by the legacy (v1) dual-write path: every emitted event is
//! serialized to JSON with the `serde(tag = "type", rename_all = "snake_case")`
//! shape and stored as `conversation_events.event_data`. The Anthropic
//! translator consumes the same enum shape and re-expresses each event as
//! a sequence of v2 `AnthropicEvent` frames (see
//! `crate::handlers::anthropic_translator`).
//!
//! Isolating the enum in its own small file (rather than inside the
//! bigger `types.rs`) lets the `backfill_v2_events` binary re-hydrate
//! historical v1 payloads via `crate::agents::stream_event::StreamEvent`
//! without pulling in `types.rs`'s `agents.json`-loading static init.

use serde::{Deserialize, Serialize};

/// Structured streaming event for agent execution.
///
/// `Deserialize` is derived so the v2 backfill (`bin/backfill_v2_events.rs`)
/// can re-hydrate historical v1 rows from `conversation_events.event_data`
/// and re-run them through the Anthropic translator without reimplementing
/// the translation logic. Live emission callers only need `Serialize`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// Text content from the assistant
    Text { content: String },
    /// Tool use request
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// Tool result
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
    /// Thinking content (extended thinking)
    Thinking { content: String },
    /// Agent run status update
    Status {
        status: String,
        message: Option<String>,
    },
    /// Final result
    Result {
        session_id: String,
        status: String,
        is_error: bool,
    },
    /// User follow-up message (stored so it can be replayed on reconnect)
    UserMessage { content: String },
    /// Sent after all historical events have been replayed during reconnection
    ReplayComplete {
        total_events: usize,
        agent_status: String,
    },
    /// Auto-generated conversation title (sent after first message)
    TitleUpdate { title: String },
    /// Auto-detected organization for the conversation (sent after first message)
    OrgUpdate { organization: String },
    /// Text content from the router agent (reasoning, search output)
    RouterText { content: String },
    /// Tool use by the router agent
    RouterToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// Tool result for the router agent
    RouterToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
    /// Final router decision: enriched message + ticket info
    RouterResult {
        enriched_message: String,
        ticket_id: Option<String>,
        organization: Option<String>,
        skipped: bool,
    },
}
