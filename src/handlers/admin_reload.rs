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
        .join("projects/agentic-flowstate/agentic-flowstate-setup/setup.sh");

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

    // Spawn in a NEW SESSION (setsid) so the script survives launchctl bootout
    // killing our process group during step 7 (LaunchAgents restart).
    // Without setsid, setup.sh is a child of the API server — when bootout
    // kills our process group, setup.sh dies and services never restart.
    {
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new("bash");
        cmd.arg("-l")
            .arg(&setup_script)
            .env("PATH", &full_path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(log_file))
            .stderr(std::process::Stdio::from(stderr_file));
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        match cmd.spawn() {
            Ok(_) => {
                tracing::info!("Setup script spawned for full reload (detached via setsid)");
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
const APP_DIR: &str = "/Users/jarvisgpt/projects/agentic-flowstate/agentic-flowstate-app";
const CODE_SIGN_IDENTITY: &str = "Apple Distribution: Alexander Lewis (M3C97KFGK9)";
const PROVISIONING_PROFILE: &str = "Agentic Flowstate App Store";
const APPLE_ID: &str = "atlewisftw@gmail.com";

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

    // Shell script: xcodegen + xcodebuild + devicectl install (USB)
    // OR auto-fallback to TestFlight when device is unavailable (remote user).
    //
    // Device availability is checked via connectionProperties.transportType —
    // paired-but-disconnected devices are skipped. DerivedData path is found
    // dynamically (never hardcoded — the hash changes on xcodegen regeneration).
    let script = format!(
        r#"
set -e
cd {app_dir}

echo "=== Step 1: Generating Xcode project ==="
xcodegen generate 2>&1
echo ""

echo "=== Checking device connectivity ==="
TMPJSON=$(mktemp /tmp/devicectl_XXXXXX.json)
xcrun devicectl list devices --json-output "$TMPJSON" 2>/dev/null || true

# Find a device that is ACTUALLY CONNECTED (has connectionProperties.transportType).
# Devices remain in the device list even when unavailable (paired but not USB-connected).
# We MUST check transportType to avoid trying to build/install to an unreachable device.
DEVICE_INFO=$(python3 -c "
import json
with open('$TMPJSON') as f:
    data = json.load(f)
devices = data.get('result', {{}}).get('devices', [])
for d in devices:
    conn = d.get('connectionProperties', {{}})
    transport = conn.get('transportType', '')
    if not transport:
        continue
    udid = d.get('hardwareProperties', {{}}).get('udid', '')
    core_id = d.get('identifier', '')
    name = d.get('deviceProperties', {{}}).get('name', 'Unknown')
    if udid:
        print(udid + '|' + core_id + '|' + name + '|' + transport)
        break
" 2>/dev/null || echo "")

rm -f "$TMPJSON"

if [ -n "$DEVICE_INFO" ]; then
    # ==========================================
    # DIRECT USB INSTALL — device is connected
    # ==========================================
    DEVICE_ID=$(echo "$DEVICE_INFO" | cut -d'|' -f1)
    CORE_ID=$(echo "$DEVICE_INFO" | cut -d'|' -f2)
    DEVICE_NAME=$(echo "$DEVICE_INFO" | cut -d'|' -f3)
    TRANSPORT=$(echo "$DEVICE_INFO" | cut -d'|' -f4)

    echo "Found device: $DEVICE_NAME ($DEVICE_ID) via $TRANSPORT"
    echo ""

    echo "=== Step 2/3: Building for device ==="
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

    # Dynamic app path — never hardcode the DerivedData hash (it changes on regen)
    APP_PATH=$(find "$HOME/Library/Developer/Xcode/DerivedData" -name "AgenticFlowstate.app" -path "*/Debug-iphoneos/*" -maxdepth 4 2>/dev/null | head -1)
    if [ -z "$APP_PATH" ]; then
        echo "ERROR: Built .app not found in DerivedData"
        exit 1
    fi
    echo "App: $APP_PATH"
    xcrun devicectl device install app --device "$CORE_ID" "$APP_PATH" 2>&1

    rm -f "$HOME/.agentic-flowstate/pending_ios_install.json"
    echo ""
    echo "=== iOS install complete ==="
else
    # ==========================================
    # TESTFLIGHT DEPLOY — no connected device
    # ==========================================
    echo "No connected iOS device — deploying via TestFlight"
    echo ""

    echo "=== Step 2/5: Bumping build number ==="
    CURRENT_BUILD=$(grep -m1 'CURRENT_PROJECT_VERSION:' project.yml | sed 's/.*CURRENT_PROJECT_VERSION: *//' | tr -d '[:space:]')
    NEW_BUILD=$((CURRENT_BUILD + 1))
    sed -i '' "s/CURRENT_PROJECT_VERSION: .*/CURRENT_PROJECT_VERSION: $NEW_BUILD/" project.yml
    sed -i '' "s/CFBundleVersion: .*/CFBundleVersion: \"$NEW_BUILD\"/" project.yml
    echo "Build: $CURRENT_BUILD -> $NEW_BUILD"
    xcodegen generate 2>&1

    git add project.yml 2>&1 || true
    git commit -m "Bump build to $NEW_BUILD for TestFlight" 2>&1 || true
    git push 2>&1 || true
    echo ""

    echo "=== Step 3/5: Archiving (Release) ==="
    xcodebuild archive \
        -project AgenticFlowstate.xcodeproj \
        -scheme AgenticFlowstate \
        -archivePath "build/AgenticFlowstate.xcarchive" \
        -destination "generic/platform=iOS" \
        -configuration Release \
        CODE_SIGN_STYLE=Manual \
        "CODE_SIGN_IDENTITY={sign_id}" \
        "PROVISIONING_PROFILE_SPECIFIER={prov}" \
        2>&1 | tail -30
    echo ""

    echo "=== Step 4/5: Exporting IPA ==="
    xcodebuild -exportArchive \
        -archivePath "build/AgenticFlowstate.xcarchive" \
        -exportOptionsPlist "ExportOptions.plist" \
        -exportPath "build/export" \
        2>&1 | tail -10
    echo ""

    if [ ! -f "build/export/AgenticFlowstate.ipa" ]; then
        echo "ERROR: IPA not found after export"
        exit 1
    fi

    echo "=== Step 5/5: Uploading to TestFlight ==="
    xcrun altool --upload-app \
        -f "build/export/AgenticFlowstate.ipa" \
        -t ios \
        -u "{apple_id}" \
        -p @keychain:AC_PASSWORD \
        2>&1

    rm -f "$HOME/.agentic-flowstate/pending_ios_install.json"
    echo ""
    echo "=== TestFlight upload complete ==="
    echo "You will receive a TestFlight notification when build $NEW_BUILD is ready."
fi
"#,
        app_dir = APP_DIR,
        sign_id = CODE_SIGN_IDENTITY,
        prov = PROVISIONING_PROFILE,
        apple_id = APPLE_ID,
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
            (
                StatusCode::OK,
                Json(json!({"status": "ios_install_started"})),
            )
                .into_response()
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
/// Flags persist indefinitely until the user acts on them (confirm or dismiss)
/// via the DELETE /api/admin/pending-restart endpoint.
pub async fn get_pending_restart() -> Response {
    let home = dirs::home_dir().unwrap_or_default();
    let pending_path = home.join(".agentic-flowstate/pending_restart.json");
    let ios_pending_path = home.join(".agentic-flowstate/pending_ios_install.json");

    // Check for pending restart flag
    if let Ok(content) = fs::read_to_string(&pending_path) {
        if let Ok(data) = serde_json::from_str::<serde_json::Value>(&content) {
            return (
                StatusCode::OK,
                Json(json!({
                    "pending": true,
                    "type": data.get("type").and_then(|v| v.as_str()).unwrap_or("restart"),
                    "requested_at": data.get("requested_at"),
                    "requested_by": data.get("requested_by"),
                    "service": data.get("service")
                })),
            )
                .into_response();
        }
    }

    // Check for pending iOS install flag
    if let Ok(content) = fs::read_to_string(&ios_pending_path) {
        if let Ok(data) = serde_json::from_str::<serde_json::Value>(&content) {
            return (
                StatusCode::OK,
                Json(json!({
                    "pending": true,
                    "type": "ios_install",
                    "requested_at": data.get("requested_at"),
                })),
            )
                .into_response();
        }
    }

    (
        StatusCode::OK,
        Json(json!({
            "pending": false
        })),
    )
        .into_response()
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

    // Spawn restart via a FULLY DETACHED process (setsid) so it survives
    // launchctl bootout killing this server's process group.
    //
    // Previous bug: the bash script was a child of the API server. When
    // bootout killed the API server, it also killed the entire process group,
    // so the bootstrap command never ran — leaving the service permanently down.
    //
    // Fix: use setsid to create a new session + process group, then exec bash.
    // The restart script logs to /tmp/agentic-restart-debug.log for diagnostics.
    let uid = std::process::Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "501".to_string());
    let log = "/tmp/agentic-restart-debug.log";
    let binary = format!(
        "{}/.agentic-flowstate/bin/agentic_api",
        std::env::var("HOME").unwrap_or_else(|_| "/Users/jarvisgpt".to_string())
    );
    let script = format!(
        r#"exec > '{log}' 2>&1
echo "[$(date -u +%Y-%m-%dT%H:%M:%SZ)] restart_api: script started (pid=$$, pgid=$(ps -o pgid= -p $$))"
sleep 1

# Atomic binary replacement — creates a new inode so the kernel's
# code signing cache treats it as a fresh binary.
echo "[$(date -u +%Y-%m-%dT%H:%M:%SZ)] restart_api: atomic binary replacement + codesign"
cp '{binary}' '{binary}.tmp'
mv '{binary}.tmp' '{binary}'
codesign -f -s - '{binary}' 2>&1
xattr -d com.apple.provenance '{binary}' 2>/dev/null || true

# kickstart -k kills and restarts without deregistering the service.
# Safe after atomic replacement fixes the code signing cache.
echo "[$(date -u +%Y-%m-%dT%H:%M:%SZ)] restart_api: kickstart -k gui/{uid}/com.agentic.api"
launchctl kickstart -k gui/{uid}/com.agentic.api 2>&1
rc=$?
echo "[$(date -u +%Y-%m-%dT%H:%M:%SZ)] restart_api: kickstart returned $rc"

sleep 2
if launchctl list com.agentic.api >/dev/null 2>&1; then
    echo "[$(date -u +%Y-%m-%dT%H:%M:%SZ)] restart_api: SUCCESS — service is running"
else
    echo "[$(date -u +%Y-%m-%dT%H:%M:%SZ)] restart_api: FAILED — service not found after kickstart"
fi
echo "[$(date -u +%Y-%m-%dT%H:%M:%SZ)] restart_api: script complete""#,
        uid = uid,
        binary = binary,
        log = log,
    );
    tracing::info!(
        "Executing approved restart via atomic-replace + kickstart (debug logging to {})",
        log
    );
    // Use pre_exec(setsid) to create a new session so this script survives
    // kickstart killing our process group.
    {
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new("bash");
        cmd.args(["-c", &script])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let _ = cmd.spawn();
    }

    (StatusCode::OK, Json(json!({"status": "restarting"}))).into_response()
}
