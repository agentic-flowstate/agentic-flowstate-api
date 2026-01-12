//! Hook utilities for capturing tool results from cc-sdk.
//!
//! The Claude Code CLI executes tools internally and doesn't stream ToolResult blocks.
//! To capture tool outputs, we use PostToolUse hooks which fire immediately after
//! each tool completes with the full response.
//!
//! Tool results are stored directly to the database (not just the channel) to ensure
//! they are persisted even if the SSE connection drops.

use cc_sdk::{
    ClaudeCodeOptions, HookCallback, HookInput, HookContext,
    HookJSONOutput, SyncHookJSONOutput, HookMatcher, SdkError,
};
use tokio::sync::mpsc;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use async_trait::async_trait;

use super::StreamEvent;

/// Hook that captures tool results from PostToolUse events.
///
/// Stores results directly to database AND sends to channel (if open).
/// This ensures tool results are persisted even if the SSE connection drops.
struct ToolResultHook {
    event_tx: mpsc::Sender<StreamEvent>,
    db: SqlitePool,
    session_id: String,
    /// Atomic counter for event indices to ensure unique ordering.
    /// Starts at a high number to avoid collisions with main stream events.
    event_index: Arc<AtomicI32>,
}

#[async_trait]
impl HookCallback for ToolResultHook {
    async fn execute(
        &self,
        input: &HookInput,
        tool_use_id: Option<&str>,
        _context: &HookContext,
    ) -> Result<HookJSONOutput, SdkError> {
        if let HookInput::PostToolUse(post) = input {
            // Convert tool_response to string for the event
            let content = match &post.tool_response {
                serde_json::Value::String(s) => s.clone(),
                other => serde_json::to_string(other).unwrap_or_default(),
            };

            // Determine if this was an error (check for error indicators in response)
            let is_error = post.tool_response.get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let event = StreamEvent::ToolResult {
                tool_use_id: tool_use_id.unwrap_or("unknown").to_string(),
                content,
                is_error,
            };

            // Get next event index (atomic increment)
            let idx = self.event_index.fetch_add(1, Ordering::SeqCst);

            // FIRST: Store directly to database (guaranteed persistence)
            if let Ok(json) = serde_json::to_string(&event) {
                if let Err(e) = ticketing_system::agent_runs::store_event(
                    &self.db,
                    &self.session_id,
                    idx,
                    "tool_result",
                    &json,
                ).await {
                    tracing::warn!(
                        "Failed to store tool_result to DB: {} (session={}, tool={})",
                        e, self.session_id, post.tool_name
                    );
                } else {
                    tracing::debug!(
                        "Stored tool_result to DB: session={}, idx={}, tool={}",
                        self.session_id, idx, post.tool_name
                    );
                }
            }

            // SECOND: Try to send to channel (may fail if SSE disconnected - that's OK)
            if let Err(e) = self.event_tx.send(event).await {
                // This is expected if the client disconnected - the DB store above ensures we don't lose data
                tracing::debug!(
                    "Channel closed for tool_result (expected if SSE disconnected): {} (session={}, tool={})",
                    e, self.session_id, post.tool_name
                );
            }

            tracing::info!(
                "PostToolUse hook: tool={}, response_len={}, session={}",
                post.tool_name,
                serde_json::to_string(&post.tool_response).map(|s| s.len()).unwrap_or(0),
                self.session_id
            );
        }

        // Return default response - don't block or modify anything
        Ok(HookJSONOutput::Sync(SyncHookJSONOutput::default()))
    }
}

/// Configuration for tool result capture hooks.
pub struct HookConfig {
    pub db: SqlitePool,
    pub session_id: String,
    /// Starting event index for hook-generated events.
    /// Should be set high enough to avoid collision with main stream events.
    pub event_index_start: i32,
}

/// Configure a PostToolUse hook on ClaudeCodeOptions to capture tool results.
///
/// This sets up a hook that fires after each tool execution, storing the
/// tool response directly to the database AND forwarding to the event channel.
///
/// Direct DB storage ensures tool results are persisted even if the SSE connection
/// drops before the hook fires.
pub fn configure_tool_result_hook(
    options: &mut ClaudeCodeOptions,
    event_tx: mpsc::Sender<StreamEvent>,
    config: HookConfig,
) {
    let event_index = Arc::new(AtomicI32::new(config.event_index_start));

    let hook = ToolResultHook {
        event_tx,
        db: config.db,
        session_id: config.session_id.clone(),
        event_index,
    };

    let mut hooks_map: HashMap<String, Vec<HookMatcher>> = HashMap::new();
    hooks_map.insert(
        "PostToolUse".to_string(),
        vec![HookMatcher {
            matcher: None, // Match all tools
            hooks: vec![Arc::new(hook) as Arc<dyn HookCallback>],
        }],
    );

    options.hooks = Some(hooks_map);
    tracing::info!(
        "Configured PostToolUse hook for tool result capture (session={}, start_idx={})",
        config.session_id,
        config.event_index_start
    );
}
