use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use cc_sdk::ClaudeSDKClient;

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
}

impl ChatClientManager {
    pub fn new() -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// Get an existing client for a conversation, if one exists and is connected.
    pub async fn get(&self, conversation_id: &str) -> Option<Arc<Mutex<ChatClient>>> {
        let clients = self.clients.lock().await;
        clients.get(conversation_id).cloned()
    }

    /// Insert a new client for a conversation.
    pub async fn insert(&self, conversation_id: String, client: ChatClient) -> Arc<Mutex<ChatClient>> {
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
}
