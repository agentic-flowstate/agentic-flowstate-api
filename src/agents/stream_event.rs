//! The internal `StreamEvent` enum emitted by the agent runtime.
//!
//! The agent runtime produces `StreamEvent`s; the [`AnthropicEventEncoder`]
//! consumes the enum and re-expresses each event as a sequence of
//! Anthropic vocabulary frames (see `crate::handlers::anthropic_event_encoder`).
//! `StreamEvent` itself is not persisted — only the encoded Anthropic
//! frames land in `conversation_events`.
//!
//! [`AnthropicEventEncoder`]: crate::handlers::anthropic_event_encoder::AnthropicEventEncoder

use serde::Serialize;

/// Structured streaming event for agent execution.
#[derive(Debug, Clone, Serialize)]
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
    /// Final deterministic ticket-preflight decision.
    RouterResult {
        enriched_message: String,
        ticket_id: Option<String>,
        organization: Option<String>,
        skipped: bool,
    },
}
