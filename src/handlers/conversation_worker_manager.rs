use once_cell::sync::Lazy;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, Mutex};

use super::chat_client_manager::ChatClientManager;
use super::conversation_worker::{ConversationWorker, WorkerMessage};

/// Channel buffer size for worker message queues.
const WORKER_CHANNEL_SIZE: usize = 16;

/// Handle to a running ConversationWorker.
struct WorkerHandle {
    message_tx: mpsc::Sender<WorkerMessage>,
    #[allow(dead_code)]
    created_at: Instant,
}

/// Manages per-conversation workers. One long-lived worker per conversation.
///
/// Workers are created on demand and shut down when idle (10 minutes)
/// or when the message channel closes.
pub struct ConversationWorkerManager {
    workers: Mutex<HashMap<String, WorkerHandle>>,
}

/// Global worker manager instance.
pub static WORKER_MANAGER: Lazy<ConversationWorkerManager> =
    Lazy::new(ConversationWorkerManager::new);

impl ConversationWorkerManager {
    pub fn new() -> Self {
        Self {
            workers: Mutex::new(HashMap::new()),
        }
    }

    /// Whether a live worker currently exists for the conversation.
    pub async fn has_worker(&self, conversation_id: &str) -> bool {
        let workers = self.workers.lock().await;
        workers
            .get(conversation_id)
            .map(|handle| !handle.message_tx.is_closed())
            .unwrap_or(false)
    }

    /// Get or create a worker for the given conversation.
    /// Returns a sender to push messages to the worker's queue.
    pub async fn get_or_create(
        &self,
        conversation_id: String,
        db: Arc<SqlitePool>,
        chat_manager: Arc<ChatClientManager>,
    ) -> mpsc::Sender<WorkerMessage> {
        let mut workers = self.workers.lock().await;

        // Check for existing live worker
        if let Some(handle) = workers.get(&conversation_id) {
            if !handle.message_tx.is_closed() {
                return handle.message_tx.clone();
            }
            // Worker is dead — remove stale handle
            tracing::info!("[WORKER-MGR] Removing stale worker for {}", conversation_id);
        }

        // Spawn new worker
        let (message_tx, message_rx) = mpsc::channel::<WorkerMessage>(WORKER_CHANNEL_SIZE);

        let worker = ConversationWorker::new(db, conversation_id.clone(), chat_manager, message_rx);

        let conv_id_for_cleanup = conversation_id.clone();
        tokio::spawn(async move {
            worker.run().await;
            // Worker exited — it will be cleaned up on next get_or_create
            tracing::info!("[WORKER-MGR] Worker task ended for {}", conv_id_for_cleanup);
        });

        workers.insert(
            conversation_id,
            WorkerHandle {
                message_tx: message_tx.clone(),
                created_at: Instant::now(),
            },
        );

        message_tx
    }
}
