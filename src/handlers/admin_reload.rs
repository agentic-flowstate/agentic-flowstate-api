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
    -scheme AgenticFlowstate_iOS \
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
