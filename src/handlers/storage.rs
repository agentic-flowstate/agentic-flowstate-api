use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::Utc;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{Row, SqlitePool};
use std::{
    collections::{HashMap, VecDeque},
    ffi::CString,
    fs,
    path::{Path as FsPath, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
};
use ticketing_system::{conversations, Conversation, CreateConversationRequest};
use uuid::Uuid;

use super::chat_client_manager::ChatClientManager;
use super::chat_stream::{self, ChatCodexOptions, ChatConfig, ChatRuntime};
use crate::agents::prompts::load_prompt;
use crate::agents::AgentType;
use crate::auth_middleware::AuthenticatedUser;

static STORAGE_JOBS: Lazy<Mutex<HashMap<String, StorageJob>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

const ORICO_ROOT: &str = "/Volumes/ORICO";
const CARGO_AUTO: &str = "/Volumes/ORICO/MacMiniBuilds/cargo-auto";
const XCODE_DERIVED_DATA: &str = "/Users/jarvisgpt/Library/Developer/Xcode/DerivedData";
const XCODE_DEVICE_SUPPORT: &str = "/Users/jarvisgpt/Library/Developer/Xcode/iOS DeviceSupport";
const USER_CORE_SIMULATOR: &str = "/Users/jarvisgpt/Library/Developer/CoreSimulator";
const USER_SIMULATOR_DEVICES: &str = "/Users/jarvisgpt/Library/Developer/CoreSimulator/Devices";
const SYSTEM_CORE_SIMULATOR: &str = "/Library/Developer/CoreSimulator";
const HOMEBREW_CACHE: &str = "/Users/jarvisgpt/Library/Caches/Homebrew";
const PLAYWRIGHT_CACHE: &str = "/Users/jarvisgpt/Library/Caches/ms-playwright";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageScanResponse {
    generated_at: String,
    volumes: Vec<StorageVolume>,
    buckets: Vec<StorageBucket>,
    actions: Vec<StorageActionSummary>,
    notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageVolume {
    id: String,
    name: String,
    mount_path: String,
    total_bytes: u64,
    used_bytes: u64,
    available_bytes: u64,
    percent_used: f64,
    is_external: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageBucket {
    id: String,
    title: String,
    category: String,
    path: String,
    bytes: u64,
    exists: bool,
    risk_level: RiskLevel,
    action_id: Option<String>,
    detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageActionSummary {
    id: String,
    title: String,
    risk_level: RiskLevel,
    estimated_reclaim_bytes: Option<u64>,
    detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
    ReviewOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageJobStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageJob {
    id: String,
    action_id: String,
    action_title: String,
    status: StorageJobStatus,
    started_at: String,
    completed_at: Option<String>,
    reclaimed_bytes: Option<u64>,
    error: Option<String>,
    steps: Vec<StorageJobStep>,
    scan: Option<StorageScanResponse>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageJobStep {
    timestamp: String,
    level: String,
    title: String,
    detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StorageInvestigationConversationResponse {
    conversation: Conversation,
    status: &'static str,
}

#[derive(Debug, Clone, Copy)]
enum StorageActionKind {
    PurgeCargoAuto,
    PurgeXcodeDerivedData,
    PurgeXcodeDeviceSupport,
    DeleteUnavailableSimulators,
    DeleteAllSimulatorDevices,
    HomebrewCleanup,
    PurgePlaywrightCache,
}

impl StorageActionKind {
    fn from_id(id: &str) -> Option<Self> {
        match id {
            "purge_cargo_auto" => Some(Self::PurgeCargoAuto),
            "purge_xcode_derived_data" => Some(Self::PurgeXcodeDerivedData),
            "purge_xcode_device_support" => Some(Self::PurgeXcodeDeviceSupport),
            "delete_unavailable_simulators" => Some(Self::DeleteUnavailableSimulators),
            "delete_all_simulator_devices" => Some(Self::DeleteAllSimulatorDevices),
            "homebrew_cleanup" => Some(Self::HomebrewCleanup),
            "purge_playwright_cache" => Some(Self::PurgePlaywrightCache),
            _ => None,
        }
    }

    fn id(self) -> &'static str {
        match self {
            Self::PurgeCargoAuto => "purge_cargo_auto",
            Self::PurgeXcodeDerivedData => "purge_xcode_derived_data",
            Self::PurgeXcodeDeviceSupport => "purge_xcode_device_support",
            Self::DeleteUnavailableSimulators => "delete_unavailable_simulators",
            Self::DeleteAllSimulatorDevices => "delete_all_simulator_devices",
            Self::HomebrewCleanup => "homebrew_cleanup",
            Self::PurgePlaywrightCache => "purge_playwright_cache",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::PurgeCargoAuto => "Clear cargo-auto build outputs",
            Self::PurgeXcodeDerivedData => "Clear Xcode DerivedData",
            Self::PurgeXcodeDeviceSupport => "Clear iOS DeviceSupport",
            Self::DeleteUnavailableSimulators => "Delete unavailable simulators",
            Self::DeleteAllSimulatorDevices => "Delete all simulator devices",
            Self::HomebrewCleanup => "Run Homebrew cleanup",
            Self::PurgePlaywrightCache => "Clear Playwright browser cache",
        }
    }
}

/// GET /api/admin/storage/scan
pub async fn scan_storage() -> Response {
    match tokio::task::spawn_blocking(|| build_scan_with_progress(None)).await {
        Ok(Ok(scan)) => (StatusCode::OK, Json(scan)).into_response(),
        Ok(Err(e)) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Storage scan failed: {e}"),
        ),
        Err(e) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Storage scan task failed: {e}"),
        ),
    }
}

/// POST /api/admin/storage/scan/start
pub async fn start_storage_scan() -> Response {
    let job = StorageJob {
        id: Uuid::new_v4().to_string(),
        action_id: "scan_storage".to_string(),
        action_title: "Scan Mac Mini storage".to_string(),
        status: StorageJobStatus::Queued,
        started_at: Utc::now().to_rfc3339(),
        completed_at: None,
        reclaimed_bytes: None,
        error: None,
        steps: vec![StorageJobStep::new(
            "info",
            "Queued",
            "Manual storage scan queued from Agentic Flowstate.",
        )],
        scan: None,
    };
    let job_id = job.id.clone();

    {
        let mut jobs = STORAGE_JOBS.lock().expect("storage jobs lock poisoned");
        jobs.insert(job_id.clone(), job.clone());
    }

    std::thread::spawn(move || run_storage_scan(job_id));

    (StatusCode::OK, Json(job)).into_response()
}

/// POST /api/admin/storage/actions/:action_id/start
pub async fn start_storage_action(Path(action_id): Path<String>) -> Response {
    let Some(action) = StorageActionKind::from_id(&action_id) else {
        return json_error(
            StatusCode::NOT_FOUND,
            format!("Unknown action: {action_id}"),
        );
    };

    let job = StorageJob {
        id: Uuid::new_v4().to_string(),
        action_id: action.id().to_string(),
        action_title: action.title().to_string(),
        status: StorageJobStatus::Queued,
        started_at: Utc::now().to_rfc3339(),
        completed_at: None,
        reclaimed_bytes: None,
        error: None,
        steps: vec![StorageJobStep::new(
            "info",
            "Queued",
            "Manual storage action queued from Agentic Flowstate.",
        )],
        scan: None,
    };
    let job_id = job.id.clone();

    {
        let mut jobs = STORAGE_JOBS.lock().expect("storage jobs lock poisoned");
        jobs.insert(job_id.clone(), job.clone());
    }

    std::thread::spawn(move || run_storage_action(job_id, action));

    (StatusCode::OK, Json(job)).into_response()
}

/// GET /api/admin/storage/jobs/:job_id
pub async fn get_storage_job(Path(job_id): Path<String>) -> Response {
    let jobs = STORAGE_JOBS.lock().expect("storage jobs lock poisoned");
    match jobs.get(&job_id) {
        Some(job) => (StatusCode::OK, Json(job.clone())).into_response(),
        None => json_error(
            StatusCode::NOT_FOUND,
            format!("Storage job not found: {job_id}"),
        ),
    }
}

/// POST /api/admin/storage/investigation/start
pub async fn start_storage_investigation_conversation(
    State(db): State<Arc<SqlitePool>>,
    State(manager): State<Arc<ChatClientManager>>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Response {
    let message = match build_storage_investigation_message(&db).await {
        Ok(message) => message,
        Err(e) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to build storage investigation context: {e}"),
            )
        }
    };

    let title = format!("Mac Mini storage review {}", Utc::now().format("%Y-%m-%d"));
    let conversation = match conversations::create_conversation(
        &db,
        CreateConversationRequest {
            user_id: user.user_id.clone(),
            organization: "agentic-flowstate".to_string(),
            title,
            session_id: None,
            agent: Some("scoped-workspace".to_string()),
            conversation_type: Some("storage_investigation".to_string()),
            parent_conversation_id: None,
            conversation_role: None,
            child_sort_order: None,
        },
    )
    .await
    {
        Ok(conversation) => conversation,
        Err(e) => {
            tracing::error!("Failed to create storage investigation conversation: {e}");
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to create storage investigation conversation".to_string(),
            );
        }
    };

    let display_name = lookup_display_name(&db, &user.user_id).await;
    let mut prompt_vars = HashMap::new();
    prompt_vars.insert("USER_ID".to_string(), user.user_id.clone());
    prompt_vars.insert("USER_NAME".to_string(), display_name);

    let agent_type = AgentType::ScopedWorkspace;
    let config = ChatConfig {
        agent_type: agent_type.clone(),
        runtime: ChatRuntime::CodexAppServer,
        prompt_name: "scoped-workspace",
        working_dir: PathBuf::from("/Users/jarvisgpt/projects"),
        prompt_vars,
        codex_options: ChatCodexOptions::default_for_agent(&agent_type),
    };

    let submit_response = chat_stream::submit(
        db,
        manager,
        message,
        Some(conversation.id.clone()),
        config,
        user.user_id,
        None,
        None,
    )
    .await;

    if submit_response.status() != StatusCode::ACCEPTED {
        return submit_response;
    }

    (
        StatusCode::CREATED,
        Json(StorageInvestigationConversationResponse {
            conversation,
            status: "queued",
        }),
    )
        .into_response()
}

fn build_scan_with_progress(job_id: Option<&str>) -> anyhow::Result<StorageScanResponse> {
    push_scan_step(
        job_id,
        "Measuring volumes",
        "Reading APFS volume usage for Macintosh HD Data and ORICO.",
    );
    let mut volumes = Vec::new();
    if let Some(volume) = volume_stats(
        "macintosh_data",
        "Macintosh HD Data",
        "/System/Volumes/Data",
        false,
    )? {
        volumes.push(volume);
    }
    if let Some(volume) = volume_stats("orico", "ORICO", ORICO_ROOT, true)? {
        volumes.push(volume);
    }

    let bucket_defs = [
        (
            "cargo_auto",
            "cargo-auto build outputs",
            "Rust",
            CARGO_AUTO,
            RiskLevel::Low,
            Some(StorageActionKind::PurgeCargoAuto),
            "Generated Rust target directories from storage-guard. Safe to remove; next builds rebuild them.",
        ),
        (
            "cargo_targets",
            "stable Cargo targets",
            "Rust",
            "/Volumes/ORICO/MacMiniBuilds/cargo-targets",
            RiskLevel::ReviewOnly,
            None,
            "Shared generated Rust targets. Review before removing because services may rely on recent release outputs.",
        ),
        (
            "agentic_runtime",
            "Agentic runtime state",
            "Runtime",
            "/Volumes/ORICO/MacMiniBuilds/agentic-flowstate-runtime",
            RiskLevel::ReviewOnly,
            None,
            "Codex app-server/runtime state. Do not bulk-delete without a separate migration plan.",
        ),
        (
            "user_simulators",
            "user simulator devices",
            "Xcode",
            USER_CORE_SIMULATOR,
            RiskLevel::Medium,
            Some(StorageActionKind::DeleteUnavailableSimulators),
            "Simulator devices and caches. The action removes only devices Xcode marks unavailable.",
        ),
        (
            "user_simulator_devices",
            "all simulator device data",
            "Xcode",
            USER_SIMULATOR_DEVICES,
            RiskLevel::High,
            Some(StorageActionKind::DeleteAllSimulatorDevices),
            "Installed apps, test data, and state for every simulator device. The action deletes all simulator devices; Xcode recreates devices later, but simulator app/data state is lost.",
        ),
        (
            "system_simulators",
            "installed simulator runtimes",
            "Xcode",
            SYSTEM_CORE_SIMULATOR,
            RiskLevel::ReviewOnly,
            None,
            "Installed simulator runtimes and dyld caches. Remove specific runtimes from Xcode when they are no longer needed.",
        ),
        (
            "xcode_app_external",
            "Xcode on ORICO",
            "Applications",
            "/Volumes/ORICO/Applications/Xcode.app",
            RiskLevel::ReviewOnly,
            None,
            "Xcode.app has been moved off the internal disk. Command-line tooling still resolves through /Applications/Xcode.app.",
        ),
        (
            "xcode_app_launcher",
            "Xcode launcher symlink",
            "Applications",
            "/Applications/Xcode.app",
            RiskLevel::ReviewOnly,
            None,
            "Internal /Applications entry points at the ORICO Xcode bundle so xcode-select remains stable.",
        ),
        (
            "docker_app_external",
            "Docker Desktop on ORICO",
            "Applications",
            "/Volumes/ORICO/Applications/Docker.app",
            RiskLevel::ReviewOnly,
            None,
            "Docker Desktop.app has been moved off the internal disk. Docker's container/image disk should still be managed from Docker settings, not by moving files directly.",
        ),
        (
            "docker_app_launcher",
            "Docker launcher symlink",
            "Applications",
            "/Applications/Docker.app",
            RiskLevel::ReviewOnly,
            None,
            "Internal /Applications entry points at the ORICO Docker bundle.",
        ),
        (
            "xcode_derived_data",
            "Xcode DerivedData",
            "Xcode",
            XCODE_DERIVED_DATA,
            RiskLevel::Low,
            Some(StorageActionKind::PurgeXcodeDerivedData),
            "Generated Xcode build products. Safe to remove; Xcode rebuilds them.",
        ),
        (
            "xcode_device_support",
            "iOS DeviceSupport",
            "Xcode",
            XCODE_DEVICE_SUPPORT,
            RiskLevel::Medium,
            Some(StorageActionKind::PurgeXcodeDeviceSupport),
            "Device debugging support copied by Xcode. Safe to remove, but physical-device debugging may re-download symbols.",
        ),
        (
            "homebrew_cache",
            "Homebrew cache",
            "Package manager",
            HOMEBREW_CACHE,
            RiskLevel::Low,
            Some(StorageActionKind::HomebrewCleanup),
            "Downloaded bottles and old package cache. Homebrew cleanup removes stale cache and old versions.",
        ),
        (
            "homebrew_prefix",
            "Homebrew prefix",
            "Package manager",
            "/opt/homebrew",
            RiskLevel::ReviewOnly,
            None,
            "Installed tools and libraries. Do not delete directly; use Homebrew cleanup or uninstall specific packages.",
        ),
        (
            "playwright_cache",
            "Playwright browser cache",
            "Developer cache",
            PLAYWRIGHT_CACHE,
            RiskLevel::Low,
            Some(StorageActionKind::PurgePlaywrightCache),
            "Downloaded browser binaries for tests. Safe to remove; Playwright re-downloads browsers when needed.",
        ),
        (
            "league_app",
            "League of Legends on ORICO",
            "Applications",
            "/Volumes/ORICO/Applications/League of Legends.app",
            RiskLevel::ReviewOnly,
            None,
            "Large app bundle moved off the internal disk. Launch and patching may be slower than internal SSD, but gameplay is usually CPU/GPU/network-bound after load.",
        ),
        (
            "league_app_launcher",
            "League launcher symlink",
            "Applications",
            "/Applications/League of Legends.app",
            RiskLevel::ReviewOnly,
            None,
            "Internal /Applications entry points at the ORICO app bundle so normal launch paths keep working.",
        ),
        (
            "external_apps",
            "ORICO application bundles",
            "Applications",
            "/Volumes/ORICO/Applications",
            RiskLevel::ReviewOnly,
            None,
            "Applications already moved off the internal disk.",
        ),
    ];

    let mut buckets = Vec::with_capacity(bucket_defs.len());
    for (id, title, category, path, risk_level, action, detail) in bucket_defs {
        push_scan_step(
            job_id,
            &format!("Measuring {title}"),
            &format!("Scanning {path}."),
        );
        let path_ref = FsPath::new(path);
        let exists = path_ref.exists();
        let bytes = if exists { directory_size(path_ref)? } else { 0 };
        buckets.push(StorageBucket {
            id: id.to_string(),
            title: title.to_string(),
            category: category.to_string(),
            path: path.to_string(),
            bytes,
            exists,
            risk_level,
            action_id: action.map(|a| a.id().to_string()),
            detail: detail.to_string(),
        });
    }

    let actions = actions_from_buckets(&buckets);
    push_scan_step(
        job_id,
        "Preparing results",
        "Calculating manual actions and notes.",
    );
    Ok(StorageScanResponse {
        generated_at: Utc::now().to_rfc3339(),
        volumes,
        buckets,
        actions,
        notes: vec![
            "Finder System Data includes Xcode simulators, device support, caches, package-manager data, logs, and developer runtime state.".to_string(),
            "No automatic cleanup is scheduled here. Every cleanup must be started manually from this tab.".to_string(),
            "League of Legends now lives on ORICO with an /Applications symlink. Expect slower launch/patch/load phases than the internal SSD, but not a meaningful frame-rate hit after loading.".to_string(),
            "Xcode.app and Docker.app now live on ORICO. The large remaining Xcode space is simulator devices/runtimes, not the app bundle.".to_string(),
            "CoreSimulator runtimes are intentionally not symlinked by this tool. Use Xcode's component manager for runtimes, and use the explicit simulator-device cleanup action when you want to reclaim device state.".to_string(),
        ],
    })
}

fn run_storage_scan(job_id: String) {
    update_job(&job_id, |job| {
        job.status = StorageJobStatus::Running;
        job.steps.push(StorageJobStep::new(
            "info",
            "Started",
            "Scanning fixed Mac Mini storage buckets.",
        ));
    });

    match build_scan_with_progress(Some(&job_id)) {
        Ok(scan) => update_job(&job_id, |job| {
            job.status = StorageJobStatus::Succeeded;
            job.completed_at = Some(Utc::now().to_rfc3339());
            job.scan = Some(scan);
            job.steps.push(StorageJobStep::new(
                "success",
                "Completed",
                "Storage scan completed and results are ready.",
            ));
        }),
        Err(e) => update_job(&job_id, |job| {
            job.status = StorageJobStatus::Failed;
            job.completed_at = Some(Utc::now().to_rfc3339());
            job.error = Some(e.to_string());
            job.steps
                .push(StorageJobStep::new("error", "Failed", &e.to_string()));
        }),
    }
}

fn push_scan_step(job_id: Option<&str>, title: &str, detail: &str) {
    if let Some(job_id) = job_id {
        update_job(job_id, |job| {
            job.steps.push(StorageJobStep::new("info", title, detail));
        });
    }
}

fn actions_from_buckets(buckets: &[StorageBucket]) -> Vec<StorageActionSummary> {
    [
        (
            StorageActionKind::PurgeCargoAuto,
            RiskLevel::Low,
            "Deletes generated cargo-auto target directories on ORICO.",
        ),
        (
            StorageActionKind::PurgeXcodeDerivedData,
            RiskLevel::Low,
            "Deletes Xcode DerivedData for rebuildable build products.",
        ),
        (
            StorageActionKind::PurgeXcodeDeviceSupport,
            RiskLevel::Medium,
            "Deletes copied iOS DeviceSupport symbols; Xcode may recreate them.",
        ),
        (
            StorageActionKind::DeleteUnavailableSimulators,
            RiskLevel::Medium,
            "Runs xcrun simctl delete unavailable; keeps currently available simulators.",
        ),
        (
            StorageActionKind::DeleteAllSimulatorDevices,
            RiskLevel::High,
            "Runs xcrun simctl shutdown all and xcrun simctl delete all. Removes simulator devices, apps, and simulator-local data; runtimes stay installed.",
        ),
        (
            StorageActionKind::HomebrewCleanup,
            RiskLevel::Low,
            "Runs brew cleanup --prune=all for stale package cache and old versions.",
        ),
        (
            StorageActionKind::PurgePlaywrightCache,
            RiskLevel::Low,
            "Deletes downloaded Playwright browsers; tests can re-download them.",
        ),
    ]
    .into_iter()
    .map(|(kind, risk_level, detail)| StorageActionSummary {
        id: kind.id().to_string(),
        title: kind.title().to_string(),
        risk_level,
        estimated_reclaim_bytes: estimated_reclaim_bytes_for_action(kind, buckets),
        detail: detail.to_string(),
    })
    .collect()
}

fn estimated_reclaim_bytes_for_action(
    kind: StorageActionKind,
    buckets: &[StorageBucket],
) -> Option<u64> {
    let bucket_id = match kind {
        StorageActionKind::PurgeCargoAuto => Some("cargo_auto"),
        StorageActionKind::PurgeXcodeDerivedData => Some("xcode_derived_data"),
        StorageActionKind::PurgeXcodeDeviceSupport => Some("xcode_device_support"),
        StorageActionKind::DeleteUnavailableSimulators => None,
        StorageActionKind::DeleteAllSimulatorDevices => Some("user_simulator_devices"),
        StorageActionKind::HomebrewCleanup => Some("homebrew_cache"),
        StorageActionKind::PurgePlaywrightCache => Some("playwright_cache"),
    }?;

    buckets
        .iter()
        .find(|bucket| bucket.id == bucket_id)
        .map(|bucket| bucket.bytes)
}

fn volume_stats(
    id: &str,
    name: &str,
    mount_path: &str,
    is_external: bool,
) -> anyhow::Result<Option<StorageVolume>> {
    if !FsPath::new(mount_path).exists() {
        return Ok(None);
    }

    let c_path = CString::new(mount_path)?;
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    let rc = unsafe { libc::statfs(c_path.as_ptr(), stat.as_mut_ptr()) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).map_err(Into::into);
    }
    let stat = unsafe { stat.assume_init() };
    let block_size = stat.f_bsize as u64;
    let total_bytes = stat.f_blocks as u64 * block_size;
    let available_bytes = stat.f_bavail as u64 * block_size;
    let used_bytes = total_bytes.saturating_sub(available_bytes);
    let percent_used = if total_bytes == 0 {
        0.0
    } else {
        used_bytes as f64 / total_bytes as f64
    };

    Ok(Some(StorageVolume {
        id: id.to_string(),
        name: name.to_string(),
        mount_path: mount_path.to_string(),
        total_bytes,
        used_bytes,
        available_bytes,
        percent_used,
        is_external,
    }))
}

async fn build_storage_investigation_message(pool: &SqlitePool) -> anyhow::Result<String> {
    let mut vars = HashMap::new();
    vars.insert("GENERATED_AT".to_string(), Utc::now().to_rfc3339());
    vars.insert(
        "CURRENT_STORAGE_CONTEXT".to_string(),
        build_current_storage_context()?,
    );
    vars.insert(
        "LATEST_SCAN_CONTEXT".to_string(),
        latest_completed_scan_context(),
    );
    vars.insert(
        "RECENT_STORAGE_TICKETS".to_string(),
        load_recent_storage_tickets(pool).await?,
    );
    vars.insert(
        "RECENT_STORAGE_ARTIFACTS".to_string(),
        load_recent_storage_artifacts(pool).await?,
    );

    load_prompt("storage-investigation-conversation", vars)
}

fn build_current_storage_context() -> anyhow::Result<String> {
    let mut lines = Vec::new();

    lines.push("Volumes:".to_string());
    for (id, name, path, external) in [
        (
            "macintosh_data",
            "Macintosh HD Data",
            "/System/Volumes/Data",
            false,
        ),
        ("orico", "ORICO", ORICO_ROOT, true),
    ] {
        match volume_stats(id, name, path, external)? {
            Some(volume) => lines.push(format!(
                "- {} ({}): {} free, {} used of {} ({:.0}% used)",
                volume.name,
                volume.mount_path,
                format_bytes(volume.available_bytes),
                format_bytes(volume.used_bytes),
                format_bytes(volume.total_bytes),
                volume.percent_used * 100.0
            )),
            None => lines.push(format!("- {name} ({path}): not mounted")),
        }
    }

    lines.push(String::new());
    lines.push("Application relocation status:".to_string());
    for (label, path) in [
        ("Xcode launcher", "/Applications/Xcode.app"),
        (
            "Xcode ORICO bundle",
            "/Volumes/ORICO/Applications/Xcode.app",
        ),
        ("Docker launcher", "/Applications/Docker.app"),
        (
            "Docker ORICO bundle",
            "/Volumes/ORICO/Applications/Docker.app",
        ),
        ("League launcher", "/Applications/League of Legends.app"),
        (
            "League ORICO bundle",
            "/Volumes/ORICO/Applications/League of Legends.app",
        ),
        ("KiCad launcher", "/Applications/KiCad"),
        ("FreeCAD launcher", "/Applications/FreeCAD.app"),
        ("OpenSCAD launcher", "/Applications/OpenSCAD-2021.01.app"),
        ("BambuStudio launcher", "/Applications/BambuStudio.app"),
        ("OrcaSlicer launcher", "/Applications/OrcaSlicer.app"),
        ("PrusaSlicer launcher", "/Applications/PrusaSlicer.app"),
    ] {
        lines.push(format!("- {label}: {}", path_status(path)));
    }

    lines.push(String::new());
    lines.push(
        "This handoff endpoint does not recursively scan directory sizes; use the Storage tab scan job when fresh bucket measurements are needed."
            .to_string(),
    );

    Ok(lines.join("\n"))
}

fn latest_completed_scan_context() -> String {
    let latest = {
        let jobs = STORAGE_JOBS.lock().expect("storage jobs lock poisoned");
        jobs.values()
            .filter_map(|job| job.scan.clone())
            .max_by(|a, b| a.generated_at.cmp(&b.generated_at))
    };

    let Some(scan) = latest else {
        return "No completed storage scan is currently cached in API memory. Ask the user to run a Storage tab scan if exact current bucket sizes are needed.".to_string();
    };

    let mut lines = Vec::new();
    lines.push(format!("Latest cached scan: {}", scan.generated_at));
    lines.push("Volumes from latest scan:".to_string());
    for volume in &scan.volumes {
        lines.push(format!(
            "- {}: {} free, {} used of {} ({:.0}% used)",
            volume.name,
            format_bytes(volume.available_bytes),
            format_bytes(volume.used_bytes),
            format_bytes(volume.total_bytes),
            volume.percent_used * 100.0
        ));
    }

    let mut buckets = scan.buckets.clone();
    buckets.sort_by(|a, b| b.bytes.cmp(&a.bytes));
    lines.push("Largest buckets from latest scan:".to_string());
    for bucket in buckets.into_iter().take(10) {
        lines.push(format!(
            "- {} [{}]: {} at {} ({})",
            bucket.title,
            bucket.category,
            format_bytes(bucket.bytes),
            bucket.path,
            bucket.risk_level.label()
        ));
    }

    lines.join("\n")
}

async fn load_recent_storage_tickets(pool: &SqlitePool) -> anyhow::Result<String> {
    let rows = sqlx::query(
        r#"
        SELECT ticket_id, title, status, updated_at, COALESCE(description, '') AS description
        FROM tickets
        WHERE organization = 'agentic-flowstate'
          AND (
            lower(title || ' ' || COALESCE(description, '') || ' ' || COALESCE(guidance, '') || ' ' || COALESCE(repository, '')) LIKE '%storage%'
            OR lower(title || ' ' || COALESCE(description, '') || ' ' || COALESCE(guidance, '') || ' ' || COALESCE(repository, '')) LIKE '%orico%'
            OR lower(title || ' ' || COALESCE(description, '') || ' ' || COALESCE(guidance, '') || ' ' || COALESCE(repository, '')) LIKE '%simulator%'
            OR lower(title || ' ' || COALESCE(description, '') || ' ' || COALESCE(guidance, '') || ' ' || COALESCE(repository, '')) LIKE '%xcode%'
            OR lower(title || ' ' || COALESCE(description, '') || ' ' || COALESCE(guidance, '') || ' ' || COALESCE(repository, '')) LIKE '%docker%'
            OR lower(title || ' ' || COALESCE(description, '') || ' ' || COALESCE(guidance, '') || ' ' || COALESCE(repository, '')) LIKE '%cargo-auto%'
            OR lower(title || ' ' || COALESCE(description, '') || ' ' || COALESCE(guidance, '') || ' ' || COALESCE(repository, '')) LIKE '%daisydisk%'
            OR lower(title || ' ' || COALESCE(description, '') || ' ' || COALESCE(guidance, '') || ' ' || COALESCE(repository, '')) LIKE '%mac mini%'
          )
        ORDER BY updated_at DESC
        LIMIT 14
        "#,
    )
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        return Ok("No recent matching storage tickets found.".to_string());
    }

    Ok(rows
        .into_iter()
        .map(|row| {
            let ticket_id: String = row.get("ticket_id");
            let title: String = row.get("title");
            let status: String = row.get("status");
            let updated_at: i64 = row.get("updated_at");
            let description: String = row.get("description");
            format!(
                "- {ticket_id} [{status}] updated {}: {title}\n  {}",
                format_epoch(updated_at),
                truncate_chars(&description, 220)
            )
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

async fn load_recent_storage_artifacts(pool: &SqlitePool) -> anyhow::Result<String> {
    let rows = sqlx::query(
        r#"
        SELECT artifact_id, title, artifact_type, ticket_id, updated_at
        FROM artifacts
        WHERE organization = 'agentic-flowstate'
          AND (
            lower(title || ' ' || content) LIKE '%storage%'
            OR lower(title || ' ' || content) LIKE '%orico%'
            OR lower(title || ' ' || content) LIKE '%simulator%'
            OR lower(title || ' ' || content) LIKE '%xcode%'
            OR lower(title || ' ' || content) LIKE '%docker%'
            OR lower(title || ' ' || content) LIKE '%cargo-auto%'
            OR lower(title || ' ' || content) LIKE '%daisydisk%'
            OR lower(title || ' ' || content) LIKE '%mac mini%'
          )
        ORDER BY updated_at DESC
        LIMIT 10
        "#,
    )
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        return Ok("No recent matching storage artifacts found.".to_string());
    }

    Ok(rows
        .into_iter()
        .map(|row| {
            let artifact_id: String = row.get("artifact_id");
            let title: String = row.get("title");
            let artifact_type: String = row.get("artifact_type");
            let ticket_id: Option<String> = row.get("ticket_id");
            let updated_at: i64 = row.get("updated_at");
            format!(
                "- {artifact_id} [{artifact_type}] updated {}{}: {title}",
                format_epoch(updated_at),
                ticket_id
                    .as_deref()
                    .map(|id| format!(" ticket {id}"))
                    .unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

async fn lookup_display_name(db: &SqlitePool, user_id: &str) -> String {
    match ticketing_system::users::get_user(db, user_id).await {
        Ok(Some(user)) => user.name,
        _ => user_id.to_string(),
    }
}

fn path_status(path: &str) -> String {
    let path_ref = FsPath::new(path);
    match fs::symlink_metadata(path_ref) {
        Ok(metadata) if metadata.file_type().is_symlink() => match fs::read_link(path_ref) {
            Ok(target) => format!("symlink -> {}", target.display()),
            Err(e) => format!("symlink; read_link failed: {e}"),
        },
        Ok(metadata) if metadata.is_dir() => "directory present".to_string(),
        Ok(metadata) if metadata.is_file() => "file present".to_string(),
        Ok(_) => "present".to_string(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "missing".to_string(),
        Err(e) => format!("unreadable: {e}"),
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn format_epoch(timestamp: i64) -> String {
    chrono::DateTime::<Utc>::from_timestamp(timestamp, 0)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| timestamp.to_string())
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let trimmed = value.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }

    let mut truncated = trimmed.chars().take(max_chars).collect::<String>();
    truncated.push_str("...");
    truncated
}

fn directory_size(path: &FsPath) -> anyhow::Result<u64> {
    if !path.exists() {
        return Ok(0);
    }

    let mut total = 0_u64;
    let mut queue = VecDeque::from([path.to_path_buf()]);
    while let Some(current) = queue.pop_front() {
        let metadata = fs::symlink_metadata(&current)?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_file() {
            total = total.saturating_add(metadata.len());
            continue;
        }
        if metadata.is_dir() {
            for entry in fs::read_dir(&current)? {
                queue.push_back(entry?.path());
            }
        }
    }
    Ok(total)
}

fn run_storage_action(job_id: String, action: StorageActionKind) {
    update_job(&job_id, |job| {
        job.status = StorageJobStatus::Running;
        job.steps.push(StorageJobStep::new(
            "info",
            "Started",
            &format!("Running fixed action '{}'.", action.id()),
        ));
    });

    let result = match action {
        StorageActionKind::PurgeCargoAuto => purge_directory_children_job(&job_id, CARGO_AUTO),
        StorageActionKind::PurgeXcodeDerivedData => {
            purge_directory_children_job(&job_id, XCODE_DERIVED_DATA)
        }
        StorageActionKind::PurgeXcodeDeviceSupport => {
            purge_directory_children_job(&job_id, XCODE_DEVICE_SUPPORT)
        }
        StorageActionKind::DeleteUnavailableSimulators => delete_unavailable_simulators(&job_id),
        StorageActionKind::DeleteAllSimulatorDevices => delete_all_simulator_devices(&job_id),
        StorageActionKind::HomebrewCleanup => homebrew_cleanup(&job_id),
        StorageActionKind::PurgePlaywrightCache => {
            purge_directory_children_job(&job_id, PLAYWRIGHT_CACHE)
        }
    };

    match result {
        Ok(reclaimed_bytes) => update_job(&job_id, |job| {
            job.status = StorageJobStatus::Succeeded;
            job.completed_at = Some(Utc::now().to_rfc3339());
            job.reclaimed_bytes = Some(reclaimed_bytes);
            job.steps.push(StorageJobStep::new(
                "success",
                "Completed",
                &format!("Estimated reclaimed space: {} bytes.", reclaimed_bytes),
            ));
        }),
        Err(e) => update_job(&job_id, |job| {
            job.status = StorageJobStatus::Failed;
            job.completed_at = Some(Utc::now().to_rfc3339());
            job.error = Some(e.to_string());
            job.steps
                .push(StorageJobStep::new("error", "Failed", &e.to_string()));
        }),
    }
}

fn purge_directory_children_job(job_id: &str, path: &str) -> anyhow::Result<u64> {
    ensure_allowed_delete_root(path)?;
    let root = FsPath::new(path);
    if !root.exists() {
        anyhow::bail!("Expected cleanup path does not exist: {path}");
    }
    if !root.is_dir() {
        anyhow::bail!("Expected cleanup path is not a directory: {path}");
    }

    update_job(job_id, |job| {
        job.steps.push(StorageJobStep::new(
            "info",
            "Measured before cleanup",
            &format!("Scanning {path}."),
        ));
    });
    let before = directory_size(root)?;

    update_job(job_id, |job| {
        job.steps.push(StorageJobStep::new(
            "info",
            "Deleting generated contents",
            &format!("Removing children of {path}; the directory itself is preserved."),
        ));
    });
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let child = entry.path();
        let metadata = fs::symlink_metadata(&child)?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            fs::remove_dir_all(&child)?;
        } else {
            fs::remove_file(&child)?;
        }
    }

    let after = directory_size(root)?;
    Ok(before.saturating_sub(after))
}

fn delete_unavailable_simulators(job_id: &str) -> anyhow::Result<u64> {
    let before_user = directory_size(FsPath::new(USER_CORE_SIMULATOR))?;
    let before_system = directory_size(FsPath::new(SYSTEM_CORE_SIMULATOR))?;
    update_job(job_id, |job| {
        job.steps.push(StorageJobStep::new(
            "info",
            "Running simctl",
            "Executing xcrun simctl delete unavailable.",
        ));
    });
    run_fixed_command(
        job_id,
        "/usr/bin/xcrun",
        &["simctl", "delete", "unavailable"],
    )?;
    let after_user = directory_size(FsPath::new(USER_CORE_SIMULATOR))?;
    let after_system = directory_size(FsPath::new(SYSTEM_CORE_SIMULATOR))?;
    Ok(before_user
        .saturating_add(before_system)
        .saturating_sub(after_user.saturating_add(after_system)))
}

fn delete_all_simulator_devices(job_id: &str) -> anyhow::Result<u64> {
    let root = FsPath::new(USER_SIMULATOR_DEVICES);
    let before = directory_size(root)?;
    update_job(job_id, |job| {
        job.steps.push(StorageJobStep::new(
            "warning",
            "Deleting simulator devices",
            "This removes all simulator devices, installed simulator apps, and simulator-local data. Installed runtimes remain in place.",
        ));
    });
    run_fixed_command(job_id, "/usr/bin/xcrun", &["simctl", "shutdown", "all"])?;
    run_fixed_command(job_id, "/usr/bin/xcrun", &["simctl", "delete", "all"])?;
    let after = directory_size(root)?;
    Ok(before.saturating_sub(after))
}

fn homebrew_cleanup(job_id: &str) -> anyhow::Result<u64> {
    let before_cache = directory_size(FsPath::new(HOMEBREW_CACHE))?;
    let before_prefix = directory_size(FsPath::new("/opt/homebrew"))?;
    update_job(job_id, |job| {
        job.steps.push(StorageJobStep::new(
            "info",
            "Running Homebrew cleanup",
            "Executing brew cleanup --prune=all.",
        ));
    });
    run_fixed_command(
        job_id,
        "/opt/homebrew/bin/brew",
        &["cleanup", "--prune=all"],
    )?;
    let after_cache = directory_size(FsPath::new(HOMEBREW_CACHE))?;
    let after_prefix = directory_size(FsPath::new("/opt/homebrew"))?;
    Ok(before_cache
        .saturating_add(before_prefix)
        .saturating_sub(after_cache.saturating_add(after_prefix)))
}

fn run_fixed_command(job_id: &str, program: &str, args: &[&str]) -> anyhow::Result<()> {
    let output = Command::new(program).args(args).output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = format!(
        "$ {} {}\nstdout:\n{}\nstderr:\n{}",
        program,
        args.join(" "),
        stdout.trim(),
        stderr.trim()
    );
    update_job(job_id, |job| {
        job.steps.push(StorageJobStep::new(
            if output.status.success() {
                "info"
            } else {
                "error"
            },
            "Command output",
            &detail,
        ));
    });
    if !output.status.success() {
        anyhow::bail!("Command failed: {program} {}", args.join(" "));
    }
    Ok(())
}

fn ensure_allowed_delete_root(path: &str) -> anyhow::Result<()> {
    let allowed = [
        CARGO_AUTO,
        XCODE_DERIVED_DATA,
        XCODE_DEVICE_SUPPORT,
        PLAYWRIGHT_CACHE,
    ];
    if allowed.iter().any(|allowed_path| *allowed_path == path) {
        Ok(())
    } else {
        anyhow::bail!("Refusing to delete unmanaged path: {path}");
    }
}

fn update_job(job_id: &str, update: impl FnOnce(&mut StorageJob)) {
    let mut jobs = STORAGE_JOBS.lock().expect("storage jobs lock poisoned");
    if let Some(job) = jobs.get_mut(job_id) {
        update(job);
    }
}

impl StorageJobStep {
    fn new(level: &str, title: &str, detail: &str) -> Self {
        Self {
            timestamp: Utc::now().to_rfc3339(),
            level: level.to_string(),
            title: title.to_string(),
            detail: detail.to_string(),
        }
    }
}

impl RiskLevel {
    fn label(&self) -> &'static str {
        match self {
            RiskLevel::Low => "low",
            RiskLevel::Medium => "medium",
            RiskLevel::High => "high",
            RiskLevel::ReviewOnly => "review",
        }
    }
}

fn json_error(status: StatusCode, message: String) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_ids_round_trip() {
        for id in [
            "purge_cargo_auto",
            "purge_xcode_derived_data",
            "purge_xcode_device_support",
            "delete_unavailable_simulators",
            "delete_all_simulator_devices",
            "homebrew_cleanup",
            "purge_playwright_cache",
        ] {
            assert_eq!(StorageActionKind::from_id(id).unwrap().id(), id);
        }
        assert!(StorageActionKind::from_id("rm_rf_root").is_none());
    }

    #[test]
    fn delete_roots_are_fixed() {
        assert!(ensure_allowed_delete_root(CARGO_AUTO).is_ok());
        assert!(ensure_allowed_delete_root("/").is_err());
        assert!(ensure_allowed_delete_root("/Users/jarvisgpt").is_err());
    }
}
