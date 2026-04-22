use cc_sdk::ClaudeSDKClient;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;
use tokio::process::Child;
use tokio::sync::Mutex;

pub struct ChatClient {
    pub sdk_client: ClaudeSDKClient,
    pub session_id: Option<String>,
    pub last_used: Instant,
}

/// Manages persistent ClaudeSDKClient instances per conversation.
/// Each client keeps a Claude CLI subprocess alive for the duration
/// of the conversation, enabling true multi-turn via a single subprocess.
pub struct ChatClientManager {
    clients: Mutex<HashMap<String, Arc<Mutex<ChatClient>>>>,
    codex_turns: Mutex<HashMap<String, Arc<Mutex<Child>>>>,
    cancelled_codex_turns: Mutex<HashSet<String>>,
}

impl ChatClientManager {
    pub fn new() -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
            codex_turns: Mutex::new(HashMap::new()),
            cancelled_codex_turns: Mutex::new(HashSet::new()),
        }
    }

    /// Get an existing client for a conversation, if one exists and is connected.
    pub async fn get(&self, conversation_id: &str) -> Option<Arc<Mutex<ChatClient>>> {
        let clients = self.clients.lock().await;
        clients.get(conversation_id).cloned()
    }

    /// Insert a new client for a conversation.
    pub async fn insert(
        &self,
        conversation_id: String,
        client: ChatClient,
    ) -> Arc<Mutex<ChatClient>> {
        let arc = Arc::new(Mutex::new(client));
        let mut clients = self.clients.lock().await;
        clients.insert(conversation_id, arc.clone());
        arc
    }

    /// Remove a client (e.g., conversation ended or client died).
    pub async fn remove(&self, conversation_id: &str) {
        let mut clients = self.clients.lock().await;
        clients.remove(conversation_id);
    }

    /// Register a live Codex CLI subprocess for a conversation turn so the
    /// cancel endpoint can kill it.
    pub async fn insert_codex_turn(&self, conversation_id: String, child: Arc<Mutex<Child>>) {
        let mut turns = self.codex_turns.lock().await;
        turns.insert(conversation_id.clone(), child);
        drop(turns);

        let mut cancelled = self.cancelled_codex_turns.lock().await;
        cancelled.remove(&conversation_id);
    }

    /// Remove a live Codex CLI subprocess handle after the turn ends.
    pub async fn remove_codex_turn(&self, conversation_id: &str) {
        let mut turns = self.codex_turns.lock().await;
        turns.remove(conversation_id);
    }

    /// Consume the "user cancelled this Codex turn" marker.
    pub async fn take_cancelled_codex_turn(&self, conversation_id: &str) -> bool {
        let mut cancelled = self.cancelled_codex_turns.lock().await;
        cancelled.remove(conversation_id)
    }

    /// Interrupt a running conversation's agent via cc-sdk interrupt().
    /// Returns Ok(true) if interrupted, Ok(false) if no active client found.
    pub async fn interrupt(&self, conversation_id: &str) -> Result<bool, String> {
        let client_arc = {
            let clients = self.clients.lock().await;
            clients.get(conversation_id).cloned()
        };
        if let Some(client_arc) = client_arc {
            let mut client = client_arc.lock().await;
            client
                .sdk_client
                .interrupt()
                .await
                .map_err(|e| format!("Interrupt failed: {}", e))?;
            Ok(true)
        } else {
            let child_arc = {
                let turns = self.codex_turns.lock().await;
                turns.get(conversation_id).cloned()
            };

            if let Some(child_arc) = child_arc {
                let mut cancelled = self.cancelled_codex_turns.lock().await;
                cancelled.insert(conversation_id.to_string());
                drop(cancelled);

                let mut child = child_arc.lock().await;
                child
                    .start_kill()
                    .map_err(|e| format!("Interrupt failed: {}", e))?;
                Ok(true)
            } else {
                Ok(false)
            }
        }
    }
}
