use axum::{extract::Json, http::StatusCode};
use serde::Deserialize;
use std::sync::OnceLock;
use std::sync::Mutex;

static DEBUG_LOG: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

fn log_store() -> &'static Mutex<Vec<String>> {
    DEBUG_LOG.get_or_init(|| Mutex::new(Vec::new()))
}

#[derive(Deserialize)]
pub struct DebugLogRequest {
    pub log: String,
}

/// POST /api/debug-log — Store a debug log entry from the iOS app
pub async fn post_debug_log(
    Json(req): Json<DebugLogRequest>,
) -> StatusCode {
    let timestamp = chrono::Utc::now().format("%H:%M:%S%.3f").to_string();
    let entry = format!("[{}] {}", timestamp, req.log);
    if let Ok(mut logs) = log_store().lock() {
        logs.push(entry);
        // Keep only last 100 entries
        if logs.len() > 100 {
            let drain = logs.len() - 100;
            logs.drain(..drain);
        }
    }
    // Also write to file for direct reading
    let _ = std::fs::write("/tmp/ios-debug.log", {
        let logs = log_store().lock().unwrap();
        logs.join("\n")
    });
    StatusCode::OK
}

/// GET /api/debug-log — Read the debug log
pub async fn get_debug_log() -> Json<serde_json::Value> {
    let logs = log_store().lock().unwrap();
    Json(serde_json::json!({
        "entries": *logs,
        "count": logs.len(),
    }))
}
