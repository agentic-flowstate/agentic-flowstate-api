use std::collections::{HashMap, HashSet};
use tokio::sync::Mutex;

use crate::agents::codex_app_server::CodexAppServerTurnHandle;

/// Manages live Codex app-server turns and cancellation markers.
pub struct ChatClientManager {
    runner_generation_id: String,
    app_server_turns: Mutex<HashMap<String, CodexAppServerTurnHandle>>,
    cancelled_turns: Mutex<HashSet<String>>,
}

impl ChatClientManager {
    pub fn new() -> Self {
        Self::with_runner_generation_id(format!("api-embedded-{}", uuid::Uuid::new_v4()))
    }

    pub fn with_runner_generation_id(runner_generation_id: String) -> Self {
        Self {
            runner_generation_id,
            app_server_turns: Mutex::new(HashMap::new()),
            cancelled_turns: Mutex::new(HashSet::new()),
        }
    }

    pub fn runner_generation_id(&self) -> &str {
        &self.runner_generation_id
    }

    /// Register a live Codex app-server subprocess for a conversation turn so the
    /// cancel endpoint can kill it.
    pub async fn insert_app_server_turn(
        &self,
        conversation_id: String,
        turn_handle: CodexAppServerTurnHandle,
    ) {
        let mut turns = self.app_server_turns.lock().await;
        turns.insert(conversation_id, turn_handle);
    }

    /// Remove a live Codex app-server subprocess handle after the turn ends.
    pub async fn remove_app_server_turn(&self, conversation_id: &str) {
        let mut turns = self.app_server_turns.lock().await;
        turns.remove(conversation_id);
    }

    /// Whether a live Codex app-server subprocess is currently registered for
    /// this conversation.
    pub async fn has_app_server_turn(&self, conversation_id: &str) -> bool {
        let turns = self.app_server_turns.lock().await;
        turns.contains_key(conversation_id)
    }

    /// Mark the current turn as cancelled. The worker consumes this marker
    /// either before the queued turn starts or when an in-flight turn exits.
    pub async fn mark_cancelled_turn(&self, conversation_id: &str) {
        let mut cancelled = self.cancelled_turns.lock().await;
        cancelled.insert(conversation_id.to_string());
    }

    /// Check whether the current turn has been cancelled without consuming it.
    pub async fn is_turn_cancelled(&self, conversation_id: &str) -> bool {
        let cancelled = self.cancelled_turns.lock().await;
        cancelled.contains(conversation_id)
    }

    /// Consume the "user cancelled this turn" marker.
    pub async fn consume_cancelled_turn(&self, conversation_id: &str) -> bool {
        let mut cancelled = self.cancelled_turns.lock().await;
        cancelled.remove(conversation_id)
    }

    /// Interrupt a running conversation's Codex app-server turn.
    /// Returns Ok(true) if interrupted, Ok(false) if no active turn found.
    pub async fn interrupt(&self, conversation_id: &str) -> Result<bool, String> {
        let turn_handle = {
            let turns = self.app_server_turns.lock().await;
            turns.get(conversation_id).cloned()
        };

        if let Some(turn_handle) = turn_handle {
            turn_handle.terminate().await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ChatClientManager;

    #[tokio::test]
    async fn cancelled_turn_marker_round_trips() {
        let manager = ChatClientManager::new();

        assert!(!manager.is_turn_cancelled("conv-1").await);

        manager.mark_cancelled_turn("conv-1").await;
        assert!(manager.is_turn_cancelled("conv-1").await);
        assert!(manager.consume_cancelled_turn("conv-1").await);
        assert!(!manager.is_turn_cancelled("conv-1").await);
        assert!(!manager.consume_cancelled_turn("conv-1").await);
    }
}
