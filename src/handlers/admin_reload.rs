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

// ---- iOS Install (lightweight, no full rebuild) ----

const IOS_INSTALL_LOG_PATH: &str = "/tmp/agentic-ios-install.log";
const APP_DIR: &str = "/Users/jarvisgpt/projects/agentic-flowstate-app";

/// POST /api/admin/ios-install — build and install the iOS app only (no MCP/API/frontend rebuild).
/// Spawns xcodegen + xcodebuild + devicectl install as a detached process.
/// Much faster than a full reload — typically 15-30 seconds.
pub async fn ios_install() -> Response {
    // Clear previous log
    let _ = fs::write(IOS_INSTALL_LOG_PATH, "");

    let log_file = match fs::File::create(IOS_INSTALL_LOG_PATH) {
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
                Json(json!({"error": format!("Failed to clone log file: {}", e)})),
            )
                .into_response();
        }
    };

    let path = std::env::var("PATH").unwrap_or_default();
    let full_path = format!(
        "/opt/homebrew/bin:/opt/homebrew/sbin:/usr/bin:/usr/local/bin:{}",
        path
    );

    // Shell script that does xcodegen + xcodebuild + devicectl install.
    // Uses a temp JSON file for device list (--json-output /dev/stdout produces
    // trailing non-JSON output that breaks python's json.load).
    let script = format!(
        r#"
set -e
cd {app_dir}

echo "=== Step 1/3: Generating Xcode project ==="
xcodegen generate 2>&1
echo ""

echo "=== Step 2/3: Building for device ==="
# Find connected device — write JSON to temp file to avoid parsing issues
TMPJSON=$(mktemp /tmp/devicectl_XXXXXX.json)
xcrun devicectl list devices --json-output "$TMPJSON" 2>/dev/null || true

DEVICE_ID=$(python3 -c "
import json
with open('$TMPJSON') as f:
    data = json.load(f)
devices = data.get('result', {{}}).get('devices', [])
for d in devices:
    udid = d.get('hardwareProperties', {{}}).get('udid', '')
    if udid:
        print(udid)
        break
" 2>/dev/null || echo "")

CORE_ID=$(python3 -c "
import json
with open('$TMPJSON') as f:
    data = json.load(f)
devices = data.get('result', {{}}).get('devices', [])
for d in devices:
    cid = d.get('identifier', '')
    if cid:
        print(cid)
        break
" 2>/dev/null || echo "")

DEVICE_NAME=$(python3 -c "
import json
with open('$TMPJSON') as f:
    data = json.load(f)
devices = data.get('result', {{}}).get('devices', [])
for d in devices:
    name = d.get('deviceProperties', {{}}).get('name', '')
    if name:
        print(name)
        break
" 2>/dev/null || echo "Unknown")

rm -f "$TMPJSON"

if [ -z "$DEVICE_ID" ]; then
    echo "No connected iOS device found. Connect your iPhone via USB and retry."
    exit 1
fi

echo "Found device: $DEVICE_NAME ($DEVICE_ID)"
echo ""

xcodebuild build \
    -project AgenticFlowstate.xcodeproj \
    -scheme AgenticFlowstate \
    -destination "platform=iOS,id=$DEVICE_ID" \
    -configuration Debug \
    -allowProvisioningUpdates \
    -allowProvisioningDeviceRegistration \
    2>&1 | tail -20

echo ""
echo "=== Step 3/3: Installing to $DEVICE_NAME ==="
APP_PATH="$HOME/Library/Developer/Xcode/DerivedData/AgenticFlowstate-efyacuebtfljofhcznnfkbraddlm/Build/Products/Debug-iphoneos/AgenticFlowstate.app"
xcrun devicectl device install app --device "$CORE_ID" "$APP_PATH" 2>&1

# Clear pending install flag
rm -f "$HOME/.agentic-flowstate/pending_ios_install.json"

echo ""
echo "=== iOS install complete ==="
"#,
        app_dir = APP_DIR
    );

    match std::process::Command::new("bash")
        .arg("-c")
        .arg(&script)
        .env("PATH", &full_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log_file))
        .stderr(std::process::Stdio::from(stderr_file))
        .spawn()
    {
        Ok(_) => {
            tracing::info!("iOS install spawned (lightweight, no full rebuild)");
            (StatusCode::OK, Json(json!({"status": "ios_install_started"}))).into_response()
        }
        Err(e) => {
            tracing::error!("Failed to spawn iOS install: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to start iOS install: {}", e)})),
            )
                .into_response()
        }
    }
}

/// GET /api/admin/ios-install/log — read the current iOS install log output
pub async fn ios_install_log() -> Response {
    match fs::read_to_string(IOS_INSTALL_LOG_PATH) {
        Ok(content) => (StatusCode::OK, Json(json!({"log": content}))).into_response(),
        Err(_) => (StatusCode::OK, Json(json!({"log": ""}))).into_response(),
    }
}

// ---- Pending Restart Flag ----

/// GET /api/admin/pending-restart — check if there's a pending restart/setup request.
///
/// MCP handlers write `~/.agentic-flowstate/pending_restart.json` when a restart
/// is needed. The iOS app polls this to detect when to show the restart UI.
///
/// Stale flags (older than 10 minutes) are auto-cleaned. This prevents ghost
/// prompts when a restart already happened but the flag wasn't cleaned up.
pub async fn get_pending_restart() -> Response {
    let home = dirs::home_dir().unwrap_or_default();
    let pending_path = home.join(".agentic-flowstate/pending_restart.json");
    let ios_pending_path = home.join(".agentic-flowstate/pending_ios_install.json");
    let max_age = std::time::Duration::from_secs(600); // 10 minutes

    // Check for pending restart flag
    if let Ok(content) = fs::read_to_string(&pending_path) {
        // Check file age — auto-clean stale flags
        if is_stale_flag(&pending_path, max_age) {
            tracing::info!("Auto-cleaning stale pending_restart.json (older than 10 min)");
            let _ = fs::remove_file(&pending_path);
        } else if let Ok(data) = serde_json::from_str::<serde_json::Value>(&content) {
            return (StatusCode::OK, Json(json!({
                "pending": true,
                "type": data.get("type").and_then(|v| v.as_str()).unwrap_or("restart"),
                "requested_at": data.get("requested_at"),
                "requested_by": data.get("requested_by"),
                "service": data.get("service")
            }))).into_response();
        }
    }

    // Check for pending iOS install flag
    if let Ok(content) = fs::read_to_string(&ios_pending_path) {
        if is_stale_flag(&ios_pending_path, max_age) {
            tracing::info!("Auto-cleaning stale pending_ios_install.json (older than 10 min)");
            let _ = fs::remove_file(&ios_pending_path);
        } else if let Ok(data) = serde_json::from_str::<serde_json::Value>(&content) {
            return (StatusCode::OK, Json(json!({
                "pending": true,
                "type": "ios_install",
                "requested_at": data.get("requested_at"),
            }))).into_response();
        }
    }

    (StatusCode::OK, Json(json!({
        "pending": false
    }))).into_response()
}

/// Check if a flag file is older than `max_age` based on filesystem modification time.
fn is_stale_flag(path: &std::path::Path, max_age: std::time::Duration) -> bool {
    if let Ok(metadata) = fs::metadata(path) {
        if let Ok(modified) = metadata.modified() {
            if let Ok(elapsed) = modified.elapsed() {
                return elapsed > max_age;
            }
        }
    }
    false
}

/// DELETE /api/admin/pending-restart — clear ALL pending flag files.
/// Called after the restart/install has been executed.
pub async fn clear_pending_restart() -> Response {
    let home = dirs::home_dir().unwrap_or_default();
    let _ = fs::remove_file(home.join(".agentic-flowstate/pending_restart.json"));
    let _ = fs::remove_file(home.join(".agentic-flowstate/pending_ios_install.json"));
    (StatusCode::OK, Json(json!({"cleared": true}))).into_response()
}

/// POST /api/admin/restart — trigger API server restart via launchctl kickstart -k.
///
/// Called by the iOS app AFTER the user approves a pending restart.
/// Spawns the restart in a background thread with a small delay so the HTTP
/// response can be sent before the process gets killed.
pub async fn restart_api() -> Response {
    // Clear the pending restart flag first
    let home = dirs::home_dir().unwrap_or_default();
    let pending_path = home.join(".agentic-flowstate/pending_restart.json");
    let _ = fs::remove_file(&pending_path);

    // Spawn restart as a detached process so it survives this server dying.
    // After cargo build replaces the binary, macOS 26's code signing monitor
    // caches the old hash. "kickstart -k" fails with SIGKILL (Code Signature
    // Invalid) / Launch Constraint Violation. bootout+bootstrap fully resets
    // the cached code signing state.
    let uid = std::process::Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "501".to_string());
    let plist_path = format!(
        "{}/Library/LaunchAgents/com.agentic.api.plist",
        std::env::var("HOME").unwrap_or_else(|_| "/Users/jarvisgpt".to_string())
    );
    let script = format!(
        "sleep 1; \
         launchctl bootout gui/{uid}/com.agentic.api 2>/dev/null; \
         i=0; while launchctl list com.agentic.api >/dev/null 2>&1 && [ $i -lt 30 ]; do sleep 1; i=$((i+1)); done; \
         launchctl bootstrap gui/{uid} '{plist}'",
        uid = uid,
        plist = plist_path,
    );
    tracing::info!("Executing approved restart via bootout+bootstrap (code-sign safe)");
    let _ = std::process::Command::new("bash")
        .args(["-c", &script])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();

    (StatusCode::OK, Json(json!({"status": "restarting"}))).into_response()
}
