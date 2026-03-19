use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use std::fs;

const RELOAD_LOG_PATH: &str = "/tmp/agentic-reload.log";

/// POST /api/admin/reload — trigger a full setup rebuild (detached process)
pub async fn reload_services() -> Response {
    let setup_script = dirs::home_dir()
        .unwrap_or_default()
        .join("projects/agentic-flowstate-setup/setup.sh");

    if !setup_script.exists() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "Setup script not found"})),
        )
            .into_response();
    }

    // Clear previous log
    let _ = fs::write(RELOAD_LOG_PATH, "");

    // Pipe stdout+stderr to the log file so the app can poll it
    let log_file = match fs::File::create(RELOAD_LOG_PATH) {
        Ok(f) => f,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to create log file: {}", e)})),
            )
                .into_response();
        }
    };
    let stderr_file = match log_file.try_clone() {
        Ok(f) => f,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to clone log file handle: {}", e)})),
            )
                .into_response();
        }
    };

    // Build a PATH that includes Homebrew locations (the detached process
    // doesn't inherit the user's interactive shell PATH).
    let path = std::env::var("PATH").unwrap_or_default();
    let full_path = format!(
        "/opt/homebrew/opt/node@20/bin:/opt/homebrew/bin:/opt/homebrew/sbin:{}",
        path
    );

    // Spawn detached — the script will rebuild and restart services (including this API server).
    // Output goes to the log file so the app can read progress.
    match std::process::Command::new("bash")
        .arg("-l")
        .arg(&setup_script)
        .env("PATH", &full_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log_file))
        .stderr(std::process::Stdio::from(stderr_file))
        .spawn()
    {
        Ok(_) => {
            tracing::info!("Setup script spawned for full reload");
            (StatusCode::OK, Json(json!({"status": "reload_started"}))).into_response()
        }
        Err(e) => {
            tracing::error!("Failed to spawn setup script: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to start reload: {}", e)})),
            )
                .into_response()
        }
    }
}

/// GET /api/admin/reload/log — read the current reload log output
pub async fn reload_log() -> Response {
    match fs::read_to_string(RELOAD_LOG_PATH) {
        Ok(content) => (StatusCode::OK, Json(json!({"log": content}))).into_response(),
        Err(_) => (StatusCode::OK, Json(json!({"log": ""}))).into_response(),
    }
}
