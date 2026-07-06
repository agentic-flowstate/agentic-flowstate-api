use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, ExitStatus, Stdio};
use std::sync::{
    atomic::{AtomicU64, Ordering as AtomicOrdering},
    Arc,
};
use ticketing_system::token_usage::TokenUsageBreakdown;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task::JoinHandle;

const DEFAULT_CODEX_MODEL: &str = "gpt-5.5";
const DEFAULT_MODEL_PROVIDER: &str = "openai";
const APP_SERVER_CLIENT_NAME: &str = "agentic_flowstate_api";
const APP_SERVER_CLIENT_TITLE: &str = "Agentic Flowstate API";
const APP_SERVER_CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_APP_SERVER_RUST_LOG: &str = "warn";
const REQUIRED_CODEX_PATH_ENTRIES: &[&str] = &[
    "/opt/homebrew/opt/node@20/bin",
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/usr/bin",
    "/bin",
    "/usr/sbin",
    "/sbin",
];
const ATOMIC_WRITE_MAX_TEMP_ATTEMPTS: usize = 32;

static ATOMIC_WRITE_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

// Scoped workspace and ticket-router turns are MCP-only surfaces. Disable
// shell, plugin, and tool-discovery features so Codex cannot widen itself
// back into desktop or local-file access during those turns.
const RESTRICTED_MCP_ONLY_DISABLED_FEATURES: &[&str] = &[
    "apply_patch_freeform",
    "apply_patch_streaming_events",
    "apps",
    "artifact",
    "browser_use",
    "code_mode",
    "code_mode_only",
    "codex_git_commit",
    "codex_hooks",
    "computer_use",
    "enable_mcp_apps",
    "exec_permission_approvals",
    "image_generation",
    "in_app_browser",
    "memories",
    "multi_agent",
    "multi_agent_v2",
    "plugins",
    "plugin_hooks",
    "remote_plugin",
    "remote_control",
    "skill_env_var_dependency_prompt",
    "skill_mcp_dependency_install",
    "shell_tool",
    "shell_snapshot",
    "shell_zsh_fork",
    "tool_search",
    "tool_suggest",
    "unified_exec",
    "workspace_dependencies",
];

const SCOPED_MCP_ALLOWED_TOOLS: &[&str] = &[
    "list_user_organizations",
    "list_epics",
    "get_epic",
    "list_slices",
    "get_slice",
    "list_tickets",
    "list_tickets_by_due_date",
    "get_ticket",
    "search_tickets",
    "ensure_work_ticket",
    "update_ticket",
    "update_ticket_status",
    "create_slice_tickets",
    "add_ticket_relationship",
    "remove_ticket_relationship",
    "create_artifact",
    "get_artifact",
    "list_artifacts",
    "search_artifacts",
    "get_document",
    "list_documents",
    "search_documents",
    "exa_search",
    "exa_get_contents",
];

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum CodexSandboxMode {
    ReadOnly,
    DangerFullAccess,
}

impl CodexSandboxMode {
    fn as_app_server_sandbox(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::DangerFullAccess => "danger-full-access",
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum CodexToolProfile {
    Default,
    ConfiguredMcpOnly,
    RestrictedMcpOnly,
    NoTools,
}

impl CodexToolProfile {
    fn config_overrides(self) -> &'static [&'static str] {
        match self {
            Self::Default => &[],
            Self::ConfiguredMcpOnly | Self::RestrictedMcpOnly | Self::NoTools => {
                &["web_search=\"disabled\""]
            }
        }
    }

    fn disabled_features(self) -> &'static [&'static str] {
        match self {
            Self::Default => &[],
            Self::ConfiguredMcpOnly | Self::RestrictedMcpOnly | Self::NoTools => {
                RESTRICTED_MCP_ONLY_DISABLED_FEATURES
            }
        }
    }
}

fn agentic_mcp_binary() -> Result<PathBuf, String> {
    if let Some(binary) = std::env::var_os("AGENTIC_MCP_COMMAND") {
        return validate_agentic_mcp_binary(PathBuf::from(binary), "AGENTIC_MCP_COMMAND");
    }

    let binary = dirs::home_dir()
        .map(|home| {
            home.join(".agentic-flowstate")
                .join("bin")
                .join("agentic_mcp")
        })
        .ok_or_else(|| "Failed to resolve home directory for agentic-mcp binary".to_string())?;

    validate_agentic_mcp_binary(binary, "default agentic-mcp install")
}

fn validate_agentic_mcp_binary(binary: PathBuf, source: &str) -> Result<PathBuf, String> {
    if !binary.is_file() {
        return Err(format!(
            "Required agentic-mcp binary from {source} not found at {}. Build agentic-flowstate-mcp and install it to ~/.agentic-flowstate/bin/agentic_mcp or set AGENTIC_MCP_COMMAND.",
            binary.display()
        ));
    }

    Ok(binary)
}

pub struct CodexAppServerOptions<'a> {
    pub model: &'a str,
    pub reasoning_effort: &'a str,
    pub system_prompt: &'a str,
    pub working_dir: &'a Path,
    pub prompt: &'a str,
    pub sandbox: CodexSandboxMode,
    pub bypass_approvals_and_sandbox: bool,
    pub resume_session_id: Option<&'a str>,
    pub ephemeral: bool,
    pub tool_profile: CodexToolProfile,
    pub scoped_user_id: Option<&'a str>,
    pub current_conversation_id: Option<&'a str>,
    pub scoped_email_id: Option<i64>,
    pub approved_mcp_tools: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum CodexAppServerEvent {
    ThreadStarted {
        thread_id: String,
    },
    AgentMessageDelta {
        id: String,
        text: String,
    },
    AgentMessageCompleted {
        id: String,
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    ToolCallStarted {
        id: String,
        name: String,
        input: Value,
    },
    ToolCallCompleted {
        id: String,
        content: String,
        is_error: bool,
    },
    TurnCompleted {
        usage: TokenUsageBreakdown,
    },
}

#[derive(Debug, Clone)]
struct TurnCompletion {
    status: String,
    error_message: Option<String>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct AppServerErrorNotification {
    message: String,
    will_retry: bool,
}

pub struct RunningCodexAppServer {
    child: Arc<Mutex<Child>>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    pub events: mpsc::Receiver<CodexAppServerEvent>,
    stdout_task: JoinHandle<Result<(), String>>,
    stderr_task: JoinHandle<Result<String, std::io::Error>>,
    completion: Arc<Mutex<Option<TurnCompletion>>>,
}

#[derive(Clone)]
pub struct CodexAppServerTurnHandle {
    child: Arc<Mutex<Child>>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
}

impl CodexAppServerTurnHandle {
    pub async fn terminate(&self) -> Result<(), String> {
        close_stdin(&self.stdin).await;
        terminate_child_process(&self.child).await
    }
}

#[derive(Default)]
struct AgentMessageTextCollector {
    delta_text: String,
    completed_text: Option<String>,
}

impl AgentMessageTextCollector {
    fn push_delta(&mut self, text: &str) {
        self.delta_text.push_str(text);
    }

    fn set_completed(&mut self, text: String) {
        self.completed_text = Some(text);
    }

    fn finish(self) -> Option<String> {
        self.completed_text.or_else(|| {
            if self.delta_text.trim().is_empty() {
                None
            } else {
                Some(self.delta_text)
            }
        })
    }
}

pub struct CodexAppServerOutcome {
    pub exit_status: ExitStatus,
    pub stderr_text: String,
    turn_completion: Option<TurnCompletion>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexAccountRateLimits {
    pub rate_limits: CodexRateLimitSnapshot,
    #[serde(default)]
    pub rate_limits_by_limit_id: Option<HashMap<String, CodexRateLimitSnapshot>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexRateLimitSnapshot {
    pub limit_id: Option<String>,
    pub limit_name: Option<String>,
    pub primary: Option<CodexRateLimitWindow>,
    pub secondary: Option<CodexRateLimitWindow>,
    pub credits: Option<CodexCreditsSnapshot>,
    pub plan_type: Option<String>,
    pub rate_limit_reached_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexRateLimitWindow {
    pub used_percent: i32,
    pub window_duration_mins: Option<i64>,
    pub resets_at: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexCreditsSnapshot {
    pub has_credits: bool,
    pub unlimited: bool,
    pub balance: Option<String>,
}

impl CodexAppServerOutcome {
    pub fn success(&self) -> bool {
        self.turn_completion
            .as_ref()
            .map(|completion| completion.status == "completed")
            .unwrap_or_else(|| self.exit_status.success())
    }

    pub fn failure_summary(&self, runtime_name: &str) -> String {
        if let Some(completion) = &self.turn_completion {
            if let Some(message) = &completion.error_message {
                return format!("{runtime_name} turn {}: {message}", completion.status);
            }
            return format!("{runtime_name} turn {}", completion.status);
        }

        let stderr_text = self.stderr_text.trim();
        if stderr_text.is_empty() {
            format!("{runtime_name} failed with status {}", self.exit_status)
        } else {
            format!(
                "{runtime_name} failed with status {}: {stderr_text}",
                self.exit_status
            )
        }
    }
}

fn append_app_server_stderr(error: String, stderr_text: &str) -> String {
    let stderr_text = stderr_text.trim();
    if stderr_text.is_empty() {
        error
    } else {
        format!("{error}: {stderr_text}")
    }
}

pub fn resolve_codex_model(model: &str) -> &str {
    match model {
        "" => DEFAULT_CODEX_MODEL,
        other => other,
    }
}

pub fn normalize_reasoning_effort(effort: &str) -> &str {
    match effort {
        "none" => "none",
        "minimal" => "minimal",
        "low" => "low",
        "medium" => "medium",
        "high" => "high",
        "xhigh" => "xhigh",
        "max" => "xhigh",
        _ => "medium",
    }
}

fn launchd_safe_path_from(existing_path: Option<&OsStr>) -> OsString {
    let mut entries: Vec<String> = REQUIRED_CODEX_PATH_ENTRIES
        .iter()
        .map(|entry| entry.to_string())
        .collect();

    if let Some(existing) = existing_path {
        for entry in std::env::split_paths(&existing) {
            let entry = entry.to_string_lossy().to_string();
            if !entry.is_empty() && !entries.iter().any(|existing| existing == &entry) {
                entries.push(entry);
            }
        }
    }

    OsString::from(entries.join(":"))
}

pub(crate) fn launchd_safe_path() -> OsString {
    launchd_safe_path_from(std::env::var_os("PATH").as_deref())
}

fn config_string_literal(value: &str) -> Result<String, String> {
    serde_json::to_string(value)
        .map_err(|e| format!("Failed to encode Codex config string value: {e}"))
}

fn config_key_literal(value: &str) -> Result<String, String> {
    if !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Ok(value.to_string());
    }
    config_string_literal(value)
}

fn working_dir_trust_overrides(path: &Path) -> Result<Vec<String>, String> {
    let mut paths = vec![path.to_string_lossy().to_string()];

    if let Ok(canonical) = path.canonicalize() {
        let canonical = canonical.to_string_lossy().to_string();
        if !paths.iter().any(|existing| existing == &canonical) {
            paths.push(canonical);
        }
    }

    paths
        .into_iter()
        .map(|path| {
            let quoted_path = config_string_literal(&path)?;
            Ok(format!("projects.{quoted_path}.trust_level=\"trusted\""))
        })
        .collect()
}

fn mcp_server_command_override(server_name: &str, command: &str) -> Result<String, String> {
    let quoted_name = config_key_literal(server_name)?;
    let quoted_command = config_string_literal(command)?;
    Ok(format!(
        "mcp_servers.{quoted_name}.command={quoted_command}"
    ))
}

fn mcp_server_env_override(server_name: &str, key: &str, value: &str) -> Result<String, String> {
    let quoted_name = config_key_literal(server_name)?;
    let quoted_value = config_string_literal(value)?;
    Ok(format!(
        "mcp_servers.{quoted_name}.env.{key}={quoted_value}"
    ))
}

fn mcp_tool_config_overrides(
    profile: CodexToolProfile,
    approved_mcp_tools: &[String],
) -> Result<Vec<String>, String> {
    if profile == CodexToolProfile::NoTools {
        return Ok(Vec::new());
    }

    let mut tools: Vec<&str> = approved_mcp_tools
        .iter()
        .map(String::as_str)
        .filter(|tool| !tool.trim().is_empty())
        .collect();
    if tools.is_empty() && profile == CodexToolProfile::RestrictedMcpOnly {
        tools = SCOPED_MCP_ALLOWED_TOOLS.to_vec();
    }

    let quoted_name = config_key_literal("agentic-mcp")?;
    if tools.iter().any(|tool| *tool == "*") {
        return Ok(vec![
            format!("mcp_servers.{quoted_name}.default_tools_enabled=true"),
            format!("mcp_servers.{quoted_name}.default_tools_approval_mode=\"approve\""),
        ]);
    }

    tools.sort_unstable();
    tools.dedup();

    let enabled_tools = tools
        .iter()
        .map(|tool| config_string_literal(tool))
        .collect::<Result<Vec<_>, _>>()?
        .join(", ");
    let mut overrides = vec![format!(
        "mcp_servers.{quoted_name}.enabled_tools=[{enabled_tools}]"
    )];

    for tool in tools {
        let quoted_tool = config_key_literal(tool)?;
        overrides.push(format!(
            "mcp_servers.{quoted_name}.tools.{quoted_tool}.enabled=true"
        ));
        overrides.push(format!(
            "mcp_servers.{quoted_name}.tools.{quoted_tool}.approval_mode=\"approve\""
        ));
    }

    Ok(overrides)
}

fn default_codex_home() -> Result<PathBuf, String> {
    if let Some(home) = std::env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(home));
    }

    dirs::home_dir()
        .map(|home| home.join(".codex"))
        .ok_or_else(|| "Failed to resolve home directory for CODEX_HOME".to_string())
}

fn app_server_codex_home(profile: CodexToolProfile) -> Result<PathBuf, String> {
    match profile {
        CodexToolProfile::Default => {
            if let Some(home) = std::env::var_os("AGENTIC_CODEX_HOME") {
                return Ok(PathBuf::from(home));
            }

            dirs::home_dir()
                .map(|home| {
                    home.join(".agentic-flowstate")
                        .join("codex-app-server-home")
                })
                .ok_or_else(|| {
                    "Failed to resolve home directory for AGENTIC_CODEX_HOME".to_string()
                })
        }
        CodexToolProfile::ConfiguredMcpOnly => {
            if let Some(home) = std::env::var_os("AGENTIC_CODEX_CONFIGURED_MCP_HOME") {
                return Ok(PathBuf::from(home));
            }

            dirs::home_dir()
                .map(|home| {
                    home.join(".agentic-flowstate")
                        .join("codex-configured-mcp-home")
                })
                .ok_or_else(|| {
                    "Failed to resolve home directory for AGENTIC_CODEX_CONFIGURED_MCP_HOME"
                        .to_string()
                })
        }
        CodexToolProfile::RestrictedMcpOnly => {
            if let Some(home) = std::env::var_os("AGENTIC_CODEX_RESTRICTED_HOME") {
                return Ok(PathBuf::from(home));
            }

            dirs::home_dir()
                .map(|home| {
                    home.join(".agentic-flowstate")
                        .join("codex-restricted-home")
                })
                .ok_or_else(|| {
                    "Failed to resolve home directory for AGENTIC_CODEX_RESTRICTED_HOME".to_string()
                })
        }
        CodexToolProfile::NoTools => {
            if let Some(home) = std::env::var_os("AGENTIC_CODEX_NO_TOOLS_HOME") {
                return Ok(PathBuf::from(home));
            }

            dirs::home_dir()
                .map(|home| home.join(".agentic-flowstate").join("codex-no-tools-home"))
                .ok_or_else(|| {
                    "Failed to resolve home directory for AGENTIC_CODEX_NO_TOOLS_HOME".to_string()
                })
        }
    }
}

pub fn app_server_generated_images_dir(profile: CodexToolProfile) -> Result<PathBuf, String> {
    Ok(app_server_codex_home(profile)?.join("generated_images"))
}

fn app_server_sqlite_home(target_home: &Path) -> PathBuf {
    target_home.join("sqlite-state")
}

fn restricted_runtime_working_dir() -> Result<PathBuf, String> {
    let dir = dirs::home_dir()
        .map(|home| {
            home.join(".agentic-flowstate")
                .join("codex-scoped-workspace")
        })
        .ok_or_else(|| {
            "Failed to resolve home directory for scoped Codex runtime working directory"
                .to_string()
        })?;
    ensure_directory(&dir, "scoped Codex runtime working directory")?;
    Ok(dir)
}

fn effective_working_dir(options: &CodexAppServerOptions<'_>) -> Result<PathBuf, String> {
    match options.tool_profile {
        CodexToolProfile::Default => Ok(options.working_dir.to_path_buf()),
        CodexToolProfile::ConfiguredMcpOnly
        | CodexToolProfile::RestrictedMcpOnly
        | CodexToolProfile::NoTools => restricted_runtime_working_dir(),
    }
}

fn prepare_codex_app_server_home(
    agentic_mcp_command: &Path,
    profile: CodexToolProfile,
) -> Result<PathBuf, String> {
    let source_home = default_codex_home()?;
    let target_home = app_server_codex_home(profile)?;
    ensure_directory(&target_home, "Codex app-server home")?;
    let sqlite_home = app_server_sqlite_home(&target_home);
    ensure_directory(&sqlite_home, "Codex app-server SQLite state home")?;

    let source_auth = source_home.join("auth.json");
    if !source_auth.is_file() {
        return Err(format!(
            "Required Codex auth file not found at {}",
            source_auth.display()
        ));
    }
    copy_file_atomically(
        &source_auth,
        &target_home.join("auth.json"),
        "Codex app-server auth",
    )?;

    let source_agents = source_home.join("AGENTS.md");
    let target_agents = target_home.join("AGENTS.md");
    if profile == CodexToolProfile::Default && source_agents.is_file() {
        std::fs::copy(&source_agents, target_home.join("AGENTS.md")).map_err(|e| {
            format!(
                "Failed to copy Codex AGENTS.md from {} to {}: {e}",
                source_agents.display(),
                target_home.join("AGENTS.md").display()
            )
        })?;
    } else if profile != CodexToolProfile::Default && target_agents.exists() {
        std::fs::remove_file(&target_agents).map_err(|e| {
            format!(
                "Failed to remove AGENTS.md from restricted Codex home at {}: {e}",
                target_agents.display()
            )
        })?;
    }

    let config = build_app_server_config(&source_home, &sqlite_home, agentic_mcp_command, profile)?;
    write_file_atomically(
        &target_home.join("config.toml"),
        &config,
        "Codex app-server config",
    )?;

    Ok(target_home)
}

fn write_file_atomically(path: &Path, contents: &str, label: &str) -> Result<(), String> {
    write_bytes_atomically(path, contents.as_bytes(), label)
}

fn write_bytes_atomically(path: &Path, contents: &[u8], label: &str) -> Result<(), String> {
    let parent = path.parent().ok_or_else(|| {
        format!(
            "Failed to resolve parent directory for {label} at {}",
            path.display()
        )
    })?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            format!(
                "Failed to resolve file name for {label} at {}",
                path.display()
            )
        })?;
    write_bytes_atomically_with_temp_candidates(
        path,
        contents,
        label,
        atomic_temp_path_candidates(parent, file_name, label),
    )
}

fn write_file_atomically_with_temp_candidates<I>(
    path: &Path,
    contents: &str,
    label: &str,
    temp_paths: I,
) -> Result<(), String>
where
    I: IntoIterator<Item = Result<PathBuf, String>>,
{
    write_bytes_atomically_with_temp_candidates(path, contents.as_bytes(), label, temp_paths)
}

fn write_bytes_atomically_with_temp_candidates<I>(
    path: &Path,
    contents: &[u8],
    label: &str,
    temp_paths: I,
) -> Result<(), String>
where
    I: IntoIterator<Item = Result<PathBuf, String>>,
{
    let mut cleanup_temp_path: Option<PathBuf> = None;
    let write_result = (|| -> Result<(), String> {
        let (temp_path, mut file) = create_atomic_temp_file_from_candidates(temp_paths, label)?;
        cleanup_temp_path = Some(temp_path.clone());
        #[cfg(unix)]
        std::fs::set_permissions(&temp_path, std::fs::Permissions::from_mode(0o600)).map_err(
            |e| {
                format!(
                    "Failed to set permissions on temporary {label} at {}: {e}",
                    temp_path.display()
                )
            },
        )?;
        file.write_all(contents).map_err(|e| {
            format!(
                "Failed to write temporary {label} at {}: {e}",
                temp_path.display()
            )
        })?;
        file.sync_all().map_err(|e| {
            format!(
                "Failed to sync temporary {label} at {}: {e}",
                temp_path.display()
            )
        })?;
        drop(file);
        std::fs::rename(&temp_path, path).map_err(|e| {
            format!(
                "Failed to replace {label} at {} from {}: {e}",
                path.display(),
                temp_path.display()
            )
        })?;
        Ok(())
    })();

    if write_result.is_err() {
        if let Some(temp_path) = cleanup_temp_path {
            let _ = std::fs::remove_file(&temp_path);
        }
    }

    write_result
}

fn copy_file_atomically(source: &Path, target: &Path, label: &str) -> Result<(), String> {
    let contents = std::fs::read(source)
        .map_err(|e| format!("Failed to read {label} from {}: {e}", source.display()))?;
    write_bytes_atomically(target, &contents, label).map_err(|e| {
        format!(
            "Failed to replace {label} at {} from {}: {e}",
            target.display(),
            source.display()
        )
    })
}

fn atomic_temp_path_candidates<'a>(
    parent: &'a Path,
    file_name: &'a str,
    label: &'a str,
) -> impl Iterator<Item = Result<PathBuf, String>> + 'a {
    (0..ATOMIC_WRITE_MAX_TEMP_ATTEMPTS).map(move |_| {
        let counter = ATOMIC_WRITE_TEMP_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
        atomic_temp_path(parent, file_name, label, counter)
    })
}

fn atomic_temp_path(
    parent: &Path,
    file_name: &str,
    label: &str,
    counter: u64,
) -> Result<PathBuf, String> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| format!("Failed to build temporary {label} path: {e}"))?
        .as_nanos();

    Ok(parent.join(format!(
        ".{file_name}.{}.{}.{}.tmp",
        std::process::id(),
        nanos,
        counter
    )))
}

fn create_atomic_temp_file_from_candidates<I>(
    temp_paths: I,
    label: &str,
) -> Result<(PathBuf, std::fs::File), String>
where
    I: IntoIterator<Item = Result<PathBuf, String>>,
{
    let mut attempted = 0usize;
    let mut last_collision: Option<PathBuf> = None;

    for temp_path in temp_paths {
        attempted += 1;
        let temp_path = temp_path?;
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => return Ok((temp_path, file)),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                last_collision = Some(temp_path);
            }
            Err(e) => {
                return Err(format!(
                    "Failed to create temporary {label} at {}: {e}",
                    temp_path.display()
                ));
            }
        }
    }

    match last_collision {
        Some(path) => Err(format!(
            "Failed to create temporary {label} after {attempted} attempts; last collision at {}",
            path.display()
        )),
        None => Err(format!(
            "Failed to create temporary {label}: no temporary paths generated"
        )),
    }
}

fn ensure_directory(path: &Path, label: &str) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let target = std::fs::read_link(path).map_err(|e| {
                format!("Failed to read {label} symlink at {}: {e}", path.display())
            })?;
            let resolved_target = if target.is_absolute() {
                target
            } else {
                path.parent().unwrap_or_else(|| Path::new(".")).join(target)
            };

            match std::fs::metadata(&resolved_target) {
                Ok(target_metadata) if target_metadata.is_dir() => Ok(()),
                Ok(_) => Err(format!(
                    "{label} at {} points to non-directory target {}",
                    path.display(),
                    resolved_target.display()
                )),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(format!(
                    "{label} at {} is a broken symlink to missing target {}",
                    path.display(),
                    resolved_target.display()
                )),
                Err(e) => Err(format!(
                    "Failed to inspect {label} symlink target {} from {}: {e}",
                    resolved_target.display(),
                    path.display()
                )),
            }
        }
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(format!(
            "{label} path exists but is not a directory: {}",
            path.display()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir_all(path)
            .map_err(|e| format!("Failed to create {label} at {}: {e}", path.display())),
        Err(e) => Err(format!(
            "Failed to inspect {label} at {}: {e}",
            path.display()
        )),
    }
}

fn build_app_server_config(
    source_home: &Path,
    sqlite_home: &Path,
    agentic_mcp_command: &Path,
    profile: CodexToolProfile,
) -> Result<String, String> {
    let mut root = toml::map::Map::new();
    root.insert(
        "model".to_string(),
        toml::Value::String(DEFAULT_CODEX_MODEL.to_string()),
    );
    root.insert(
        "model_reasoning_effort".to_string(),
        toml::Value::String("medium".to_string()),
    );
    root.insert(
        "web_search".to_string(),
        toml::Value::String("disabled".to_string()),
    );
    root.insert(
        "sqlite_home".to_string(),
        toml::Value::String(sqlite_home.to_string_lossy().to_string()),
    );

    if profile != CodexToolProfile::NoTools {
        let mut agentic_mcp = source_agentic_mcp_table(source_home)?.unwrap_or_default();
        // The API runs Codex app-server turns unattended. User-facing approval
        // prompts from the interactive Codex config cannot be answered here, so
        // enforce safety through route auth + MCP scope checks instead.
        agentic_mcp.remove("tools");
        agentic_mcp.remove("enabled_tools");
        agentic_mcp.remove("disabled_tools");
        agentic_mcp.remove("default_tools_approval_mode");
        agentic_mcp.remove("default_tools_enabled");
        agentic_mcp.insert(
            "command".to_string(),
            toml::Value::String(agentic_mcp_command.to_string_lossy().to_string()),
        );
        agentic_mcp.insert("startup_timeout_sec".to_string(), toml::Value::Integer(30));
        upsert_agentic_mcp_env(&mut agentic_mcp, source_home)?;

        let mut mcp_servers = toml::map::Map::new();
        mcp_servers.insert("agentic-mcp".to_string(), toml::Value::Table(agentic_mcp));
        root.insert("mcp_servers".to_string(), toml::Value::Table(mcp_servers));
    }

    toml::to_string(&toml::Value::Table(root))
        .map_err(|e| format!("Failed to encode Codex app-server config: {e}"))
}

fn upsert_agentic_mcp_env(
    agentic_mcp: &mut toml::map::Map<String, toml::Value>,
    source_home: &Path,
) -> Result<(), String> {
    match agentic_mcp.remove("env") {
        Some(toml::Value::Table(mut env)) => {
            env.insert(
                "CODEX_HOME".to_string(),
                toml::Value::String(source_home.to_string_lossy().to_string()),
            );
            agentic_mcp.insert("env".to_string(), toml::Value::Table(env));
        }
        Some(_) => {
            return Err(
                "Source Codex config has mcp_servers.agentic-mcp.env, but it is not a table"
                    .to_string(),
            );
        }
        None => {
            let mut env = toml::map::Map::new();
            env.insert(
                "CODEX_HOME".to_string(),
                toml::Value::String(source_home.to_string_lossy().to_string()),
            );
            agentic_mcp.insert("env".to_string(), toml::Value::Table(env));
        }
    }

    Ok(())
}

fn source_agentic_mcp_table(
    source_home: &Path,
) -> Result<Option<toml::map::Map<String, toml::Value>>, String> {
    let config_path = source_home.join("config.toml");
    if !config_path.is_file() {
        return Ok(None);
    }

    let config = std::fs::read_to_string(&config_path).map_err(|e| {
        format!(
            "Failed to read source Codex config at {}: {e}",
            config_path.display()
        )
    })?;
    let parsed: toml::Value = toml::from_str(&config).map_err(|e| {
        format!(
            "Failed to parse source Codex config at {}: {e}",
            config_path.display()
        )
    })?;

    Ok(parsed
        .get("mcp_servers")
        .and_then(|servers| servers.get("agentic-mcp"))
        .and_then(|server| server.as_table())
        .cloned())
}

fn build_codex_app_server_command(
    options: &CodexAppServerOptions<'_>,
    agentic_mcp_command: &Path,
    codex_home: &Path,
) -> Result<StdCommand, String> {
    let working_dir = effective_working_dir(options)?;
    let mut command = StdCommand::new("codex");
    command.current_dir(&working_dir);
    command.env("PATH", launchd_safe_path());
    command.env("CODEX_HOME", codex_home);
    command.env(
        "RUST_LOG",
        std::env::var("AGENTIC_CODEX_APP_SERVER_RUST_LOG")
            .unwrap_or_else(|_| DEFAULT_APP_SERVER_RUST_LOG.to_string()),
    );
    command
        .arg("app-server")
        .arg("--listen")
        .arg("stdio://")
        .arg("-c")
        .arg("forced_login_method=\"chatgpt\"");

    for trust_override in working_dir_trust_overrides(&working_dir)? {
        command.arg("-c").arg(trust_override);
    }

    if options.tool_profile != CodexToolProfile::NoTools {
        command.arg("-c").arg(mcp_server_command_override(
            "agentic-mcp",
            agentic_mcp_command.to_string_lossy().as_ref(),
        )?);
        for override_arg in
            mcp_tool_config_overrides(options.tool_profile, &options.approved_mcp_tools)?
        {
            command.arg("-c").arg(override_arg);
        }
        if let Some(conversation_id) = options.current_conversation_id {
            command.arg("-c").arg(mcp_server_env_override(
                "agentic-mcp",
                "AGENTIC_MCP_CONVERSATION_ID",
                conversation_id,
            )?);
        }
        if let Some(user_id) = options.scoped_user_id {
            command.arg("-c").arg(mcp_server_env_override(
                "agentic-mcp",
                "AGENTIC_MCP_USER_ID",
                user_id,
            )?);
        }
        if let Some(email_id) = options.scoped_email_id {
            command.arg("-c").arg(mcp_server_env_override(
                "agentic-mcp",
                "AGENTIC_MCP_EMAIL_SCOPE_ID",
                &email_id.to_string(),
            )?);
        }
    }

    if options.tool_profile == CodexToolProfile::RestrictedMcpOnly {
        if options.scoped_user_id.is_none() {
            return Err(
                "Restricted MCP profile requires scoped_user_id for tool enforcement".to_string(),
            );
        }
        command.arg("-c").arg(mcp_server_env_override(
            "agentic-mcp",
            "AGENTIC_MCP_PROFILE",
            "scoped-workspace",
        )?);
    }

    for config_override in options.tool_profile.config_overrides() {
        command.arg("-c").arg(config_override);
    }

    for feature in options.tool_profile.disabled_features() {
        command.arg("--disable").arg(feature);
    }

    #[cfg(unix)]
    unsafe {
        // Give each Codex app-server turn its own process group so Stop can
        // terminate the whole turn tree, not just the direct app-server parent.
        command.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    Ok(command)
}

pub async fn spawn_codex_app_server(
    options: CodexAppServerOptions<'_>,
) -> Result<RunningCodexAppServer, String> {
    let normalized_effort = normalize_reasoning_effort(options.reasoning_effort).to_string();
    let agentic_mcp_command = agentic_mcp_binary()?;
    let codex_home = prepare_codex_app_server_home(&agentic_mcp_command, options.tool_profile)?;
    let working_dir = effective_working_dir(&options)?;
    let mut command = Command::from(build_codex_app_server_command(
        &options,
        &agentic_mcp_command,
        &codex_home,
    )?);

    tracing::info!(
        "[CODEX] Launching app-server: cwd={}, codex_home={}, model={}, effort={}, sandbox={}, profile={:?}, mcp_servers={:?}, disabled_features={:?}",
        working_dir.display(),
        codex_home.display(),
        resolve_codex_model(options.model),
        normalized_effort,
        effective_sandbox(&options).as_app_server_sandbox(),
        options.tool_profile,
        ["agentic-mcp"],
        options.tool_profile.disabled_features(),
    );

    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to start codex app-server: {e}"))?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "Failed to capture codex app-server stdin".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to capture codex app-server stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "Failed to capture codex app-server stderr".to_string())?;

    let child = Arc::new(Mutex::new(child));
    let stdin = Arc::new(Mutex::new(Some(stdin)));
    let completion = Arc::new(Mutex::new(None));
    let (tx, rx) = mpsc::channel(128);
    let (ready_tx, ready_rx) = oneshot::channel();

    let startup = AppServerStartup {
        model: resolve_codex_model(options.model).to_string(),
        reasoning_effort: normalized_effort,
        system_prompt: options.system_prompt.to_string(),
        working_dir: working_dir.to_string_lossy().to_string(),
        prompt: options.prompt.to_string(),
        sandbox: effective_sandbox(&options),
        resume_session_id: options.resume_session_id.map(str::to_string),
        ephemeral: options.ephemeral,
    };

    let stdout_task = tokio::spawn(run_app_server_stdout(
        stdout,
        stdin.clone(),
        child.clone(),
        completion.clone(),
        tx,
        startup,
        ready_tx,
    ));

    let stderr_task = tokio::spawn(async move {
        let mut stderr_text = String::new();
        let mut reader = BufReader::new(stderr);
        reader.read_to_string(&mut stderr_text).await?;
        Ok::<String, std::io::Error>(stderr_text)
    });

    match ready_rx.await {
        Ok(Ok(())) => Ok(RunningCodexAppServer {
            child,
            stdin,
            events: rx,
            stdout_task,
            stderr_task,
            completion,
        }),
        Ok(Err(e)) => {
            Err(
                finalize_failed_app_server_startup(e, &child, &stdin, stdout_task, stderr_task)
                    .await,
            )
        }
        Err(e) => Err(finalize_failed_app_server_startup(
            format!("codex app-server startup task exited: {e}"),
            &child,
            &stdin,
            stdout_task,
            stderr_task,
        )
        .await),
    }
}

async fn finalize_failed_app_server_startup(
    error: String,
    child: &Arc<Mutex<Child>>,
    stdin: &Arc<Mutex<Option<ChildStdin>>>,
    stdout_task: JoinHandle<Result<(), String>>,
    stderr_task: JoinHandle<Result<String, std::io::Error>>,
) -> String {
    close_stdin(stdin).await;
    let _ = terminate_child_process(child).await;
    let _ = child.lock().await.wait().await;

    if let Err(e) = stdout_task.await {
        tracing::warn!("[CODEX] Failed joining startup stdout task: {}", e);
    }

    let stderr_text = match stderr_task.await {
        Ok(Ok(text)) => text,
        Ok(Err(e)) => {
            tracing::warn!("[CODEX] Failed reading startup stderr: {}", e);
            String::new()
        }
        Err(e) => {
            tracing::warn!("[CODEX] Failed joining startup stderr task: {}", e);
            String::new()
        }
    };

    append_app_server_stderr(error, &stderr_text)
}

pub async fn read_codex_account_rate_limits() -> Result<CodexAccountRateLimits, String> {
    let agentic_mcp_command = agentic_mcp_binary()?;
    let codex_home =
        prepare_codex_app_server_home(&agentic_mcp_command, CodexToolProfile::Default)?;
    let options = CodexAppServerOptions {
        model: DEFAULT_CODEX_MODEL,
        reasoning_effort: "medium",
        system_prompt: "",
        working_dir: &codex_home,
        prompt: "",
        sandbox: CodexSandboxMode::ReadOnly,
        bypass_approvals_and_sandbox: false,
        resume_session_id: None,
        ephemeral: true,
        tool_profile: CodexToolProfile::Default,
        scoped_user_id: None,
        current_conversation_id: None,
        scoped_email_id: None,
        approved_mcp_tools: Vec::new(),
    };
    let mut command = Command::from(build_codex_app_server_command(
        &options,
        &agentic_mcp_command,
        &codex_home,
    )?);

    tracing::info!(
        "[CODEX] Reading account rate limits: codex_home={}",
        codex_home.display()
    );

    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to start codex app-server for account rate limits: {e}"))?;

    let stdin = Arc::new(Mutex::new(Some(child.stdin.take().ok_or_else(|| {
        "Failed to capture codex app-server stdin for account rate limits".to_string()
    })?)));
    let stdout = child.stdout.take().ok_or_else(|| {
        "Failed to capture codex app-server stdout for account rate limits".to_string()
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        "Failed to capture codex app-server stderr for account rate limits".to_string()
    })?;

    let child = Arc::new(Mutex::new(child));
    let stderr_task = tokio::spawn(async move {
        let mut stderr_text = String::new();
        let mut reader = BufReader::new(stderr);
        reader.read_to_string(&mut stderr_text).await?;
        Ok::<String, std::io::Error>(stderr_text)
    });

    let result = async {
        let mut lines = BufReader::new(stdout).lines();
        let (tx, _rx) = mpsc::channel(16);
        let mut latest_usage = TokenUsageBreakdown::default();

        send_app_server_message(
            &stdin,
            json!({
                "id": 1,
                "method": "initialize",
                "params": {
                    "clientInfo": {
                        "name": APP_SERVER_CLIENT_NAME,
                        "title": APP_SERVER_CLIENT_TITLE,
                        "version": APP_SERVER_CLIENT_VERSION
                    },
                    "capabilities": {
                        "experimentalApi": true
                    }
                }
            }),
        )
        .await?;
        read_response(&mut lines, &stdin, &tx, &mut latest_usage, 1, "initialize").await?;

        send_app_server_message(
            &stdin,
            json!({
                "method": "initialized",
                "params": {}
            }),
        )
        .await?;

        send_app_server_message(
            &stdin,
            json!({
                "id": 2,
                "method": "account/rateLimits/read",
                "params": null
            }),
        )
        .await?;
        let value = read_response(
            &mut lines,
            &stdin,
            &tx,
            &mut latest_usage,
            2,
            "account/rateLimits/read",
        )
        .await?;

        serde_json::from_value::<CodexAccountRateLimits>(value)
            .map_err(|e| format!("Failed parsing Codex account rate limits: {e}"))
    }
    .await;

    close_stdin(&stdin).await;
    let _ = terminate_child_process(&child).await;
    let _ = child.lock().await.wait().await;
    let stderr_text = stderr_task
        .await
        .map_err(|e| format!("Failed joining app-server stderr reader: {e}"))?
        .map_err(|e| format!("Failed reading app-server stderr: {e}"))?;

    result.map_err(|e| append_app_server_stderr(e, &stderr_text))
}

fn effective_sandbox(options: &CodexAppServerOptions<'_>) -> CodexSandboxMode {
    if matches!(
        options.tool_profile,
        CodexToolProfile::RestrictedMcpOnly | CodexToolProfile::NoTools
    ) {
        return CodexSandboxMode::ReadOnly;
    }
    if options.bypass_approvals_and_sandbox {
        CodexSandboxMode::DangerFullAccess
    } else {
        options.sandbox
    }
}

struct AppServerStartup {
    model: String,
    reasoning_effort: String,
    system_prompt: String,
    working_dir: String,
    prompt: String,
    sandbox: CodexSandboxMode,
    resume_session_id: Option<String>,
    ephemeral: bool,
}

async fn run_app_server_stdout(
    stdout: tokio::process::ChildStdout,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    child: Arc<Mutex<Child>>,
    completion: Arc<Mutex<Option<TurnCompletion>>>,
    tx: mpsc::Sender<CodexAppServerEvent>,
    startup: AppServerStartup,
    ready_tx: oneshot::Sender<Result<(), String>>,
) -> Result<(), String> {
    let mut lines = BufReader::new(stdout).lines();
    let mut latest_usage = TokenUsageBreakdown::default();

    let startup_result =
        bootstrap_app_server(&stdin, &mut lines, &tx, &mut latest_usage, &startup).await;
    let startup_ok = startup_result.is_ok();
    let _ = ready_tx.send(startup_result);
    if !startup_ok {
        return Ok(());
    }

    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|e| format!("Failed reading codex app-server stdout: {e}"))?
    {
        let should_close =
            handle_app_server_message(&line, None, &tx, &stdin, &completion, &mut latest_usage)
                .await?;

        if should_close {
            close_stdin(&stdin).await;
            break;
        }
    }

    let _ = terminate_child_process(&child).await;
    Ok(())
}

async fn bootstrap_app_server(
    stdin: &Arc<Mutex<Option<ChildStdin>>>,
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    tx: &mpsc::Sender<CodexAppServerEvent>,
    latest_usage: &mut TokenUsageBreakdown,
    startup: &AppServerStartup,
) -> Result<(), String> {
    send_app_server_message(
        stdin,
        json!({
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": APP_SERVER_CLIENT_NAME,
                    "title": APP_SERVER_CLIENT_TITLE,
                    "version": APP_SERVER_CLIENT_VERSION
                },
                "capabilities": {
                    "experimentalApi": true
                }
            }
        }),
    )
    .await?;
    read_response(lines, stdin, tx, latest_usage, 1, "initialize").await?;

    send_app_server_message(
        stdin,
        json!({
            "method": "initialized",
            "params": {}
        }),
    )
    .await?;

    let thread_method = if startup.resume_session_id.is_some() {
        "thread/resume"
    } else {
        "thread/start"
    };
    let mut thread_params = json!({
        "cwd": startup.working_dir.clone(),
        "model": startup.model.clone(),
        "modelProvider": DEFAULT_MODEL_PROVIDER,
        "approvalPolicy": "never",
        "sandbox": startup.sandbox.as_app_server_sandbox(),
        "developerInstructions": startup.system_prompt.clone(),
        "serviceName": APP_SERVER_CLIENT_NAME,
        "personality": "pragmatic"
    });
    if let Some(session_id) = &startup.resume_session_id {
        thread_params["threadId"] = json!(session_id);
        thread_params["excludeTurns"] = json!(true);
    } else {
        thread_params["ephemeral"] = json!(startup.ephemeral);
        thread_params["sessionStartSource"] = json!("startup");
    }

    send_app_server_message(
        stdin,
        json!({
            "id": 2,
            "method": thread_method,
            "params": thread_params
        }),
    )
    .await?;
    let thread_result = read_response(lines, stdin, tx, latest_usage, 2, thread_method).await?;
    let thread_id = thread_result
        .get("thread")
        .and_then(|thread| thread.get("id"))
        .and_then(|id| id.as_str())
        .ok_or_else(|| format!("{thread_method} response did not include thread.id"))?
        .to_string();

    let _ = tx
        .send(CodexAppServerEvent::ThreadStarted {
            thread_id: thread_id.clone(),
        })
        .await;

    send_app_server_message(
        stdin,
        json!({
            "id": 3,
            "method": "turn/start",
            "params": {
                "threadId": thread_id,
                "cwd": startup.working_dir.clone(),
                "model": startup.model.clone(),
                "effort": startup.reasoning_effort.clone(),
                "approvalPolicy": "never",
                "input": [
                    {
                        "type": "text",
                        "text": startup.prompt.clone()
                    }
                ]
            }
        }),
    )
    .await?;
    read_response(lines, stdin, tx, latest_usage, 3, "turn/start").await?;

    Ok(())
}

async fn read_response(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    stdin: &Arc<Mutex<Option<ChildStdin>>>,
    tx: &mpsc::Sender<CodexAppServerEvent>,
    latest_usage: &mut TokenUsageBreakdown,
    request_id: i64,
    method: &str,
) -> Result<Value, String> {
    let completion = Arc::new(Mutex::new(None));

    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|e| format!("Failed reading codex app-server stdout: {e}"))?
    {
        let value: Value = serde_json::from_str(&line)
            .map_err(|e| format!("Failed parsing codex app-server JSON: {e}: {line}"))?;

        if value.get("id").and_then(|id| id.as_i64()) == Some(request_id) {
            if let Some(error) = value.get("error") {
                let message = error
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                return Err(format!("codex app-server {method} failed: {message}"));
            }

            return value
                .get("result")
                .cloned()
                .ok_or_else(|| format!("codex app-server {method} response missing result"));
        }

        if value.get("id").is_some() && value.get("method").is_some() {
            reject_server_request(stdin, &value).await?;
            continue;
        }

        handle_app_server_value(&value, tx, &completion, latest_usage).await?;
    }

    Err(format!(
        "codex app-server closed stdout before {method} response"
    ))
}

async fn handle_app_server_message(
    line: &str,
    expected_response_id: Option<i64>,
    tx: &mpsc::Sender<CodexAppServerEvent>,
    stdin: &Arc<Mutex<Option<ChildStdin>>>,
    completion: &Arc<Mutex<Option<TurnCompletion>>>,
    latest_usage: &mut TokenUsageBreakdown,
) -> Result<bool, String> {
    let value: Value = serde_json::from_str(line)
        .map_err(|e| format!("Failed parsing codex app-server JSON: {e}: {line}"))?;

    if expected_response_id.and_then(|expected| {
        value
            .get("id")
            .and_then(|id| id.as_i64())
            .map(|id| id == expected)
    }) == Some(true)
    {
        return Ok(false);
    }

    if value.get("id").is_some() && value.get("method").is_some() {
        reject_server_request(stdin, &value).await?;
        return Ok(false);
    }

    handle_app_server_value(&value, tx, completion, latest_usage).await
}

async fn handle_app_server_value(
    value: &Value,
    tx: &mpsc::Sender<CodexAppServerEvent>,
    completion: &Arc<Mutex<Option<TurnCompletion>>>,
    latest_usage: &mut TokenUsageBreakdown,
) -> Result<bool, String> {
    let Some(method) = value.get("method").and_then(|v| v.as_str()) else {
        return Ok(false);
    };
    let params = value.get("params").unwrap_or(&Value::Null);

    match method {
        "thread/started" => {
            if let Some(thread_id) = params
                .get("thread")
                .and_then(|thread| thread.get("id"))
                .and_then(|id| id.as_str())
            {
                let _ = tx
                    .send(CodexAppServerEvent::ThreadStarted {
                        thread_id: thread_id.to_string(),
                    })
                    .await;
            }
        }
        "thread/tokenUsage/updated" => {
            *latest_usage = parse_token_usage(params);
        }
        "item/agentMessage/delta" => {
            if let Some(item_id) = params.get("itemId").and_then(|v| v.as_str()) {
                if let Some(delta) = params.get("delta").and_then(|v| v.as_str()) {
                    let _ = tx
                        .send(CodexAppServerEvent::AgentMessageDelta {
                            id: item_id.to_string(),
                            text: delta.to_string(),
                        })
                        .await;
                }
            }
        }
        "item/reasoningText/delta" | "item/reasoningSummaryText/delta" => {
            if let Some(delta) = params.get("delta").and_then(|v| v.as_str()) {
                let _ = tx
                    .send(CodexAppServerEvent::ReasoningDelta {
                        text: delta.to_string(),
                    })
                    .await;
            }
        }
        "item/started" => {
            if let Some(event) = parse_item_started(params.get("item").unwrap_or(&Value::Null)) {
                let _ = tx.send(event).await;
            }
        }
        "item/completed" => {
            if let Some(event) = parse_item_completed(params.get("item").unwrap_or(&Value::Null)) {
                let _ = tx.send(event).await;
            }
        }
        "turn/completed" => {
            let parsed = parse_turn_completion(params);
            *completion.lock().await = Some(parsed);
            let _ = tx
                .send(CodexAppServerEvent::TurnCompleted {
                    usage: *latest_usage,
                })
                .await;
            return Ok(true);
        }
        "error" => {
            let error = parse_error_notification(params);
            if error.will_retry {
                tracing::warn!(
                    "[CODEX] Retryable app-server error notification: {}",
                    error.message
                );
                return Ok(false);
            }
            return Err(format!(
                "codex app-server error notification: {}",
                error.message
            ));
        }
        "mcpServer/startupStatus/updated" => {
            let status = params.get("status").and_then(|value| value.as_str());
            if status == Some("failed") {
                let name = params
                    .get("name")
                    .and_then(|value| value.as_str())
                    .unwrap_or("unknown");
                let error = params
                    .get("error")
                    .and_then(|value| value.as_str())
                    .unwrap_or("unknown MCP startup error");
                tracing::warn!("[CODEX] MCP server {name} failed to start: {error}");
            }
        }
        _ => {}
    }

    Ok(false)
}

async fn send_app_server_message(
    stdin: &Arc<Mutex<Option<ChildStdin>>>,
    message: Value,
) -> Result<(), String> {
    let mut guard = stdin.lock().await;
    let writer = guard
        .as_mut()
        .ok_or_else(|| "codex app-server stdin is closed".to_string())?;
    let mut bytes = serde_json::to_vec(&message)
        .map_err(|e| format!("Failed to encode app-server message: {e}"))?;
    bytes.push(b'\n');
    writer
        .write_all(&bytes)
        .await
        .map_err(|e| format!("Failed writing to codex app-server stdin: {e}"))?;
    writer
        .flush()
        .await
        .map_err(|e| format!("Failed flushing codex app-server stdin: {e}"))
}

async fn close_stdin(stdin: &Arc<Mutex<Option<ChildStdin>>>) {
    let _ = stdin.lock().await.take();
}

async fn reject_server_request(
    stdin: &Arc<Mutex<Option<ChildStdin>>>,
    request: &Value,
) -> Result<(), String> {
    let Some(id) = request.get("id").cloned() else {
        return Ok(());
    };
    let method = request
        .get("method")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown");
    send_app_server_message(
        stdin,
        json!({
            "id": id,
            "error": {
                "code": -32601,
                "message": format!("Agentic Flowstate API does not support interactive app-server request {method}")
            }
        }),
    )
    .await
}

impl RunningCodexAppServer {
    pub fn turn_handle(&self) -> CodexAppServerTurnHandle {
        CodexAppServerTurnHandle {
            child: self.child.clone(),
            stdin: self.stdin.clone(),
        }
    }

    pub async fn terminate(&self) -> Result<(), String> {
        close_stdin(&self.stdin).await;
        terminate_child_process(&self.child).await
    }

    pub async fn wait(self) -> Result<CodexAppServerOutcome, String> {
        let stdout_result = self
            .stdout_task
            .await
            .map_err(|e| format!("Failed joining codex app-server stdout reader: {e}"))?;
        stdout_result?;

        let exit_status = {
            let mut child = self.child.lock().await;
            child
                .wait()
                .await
                .map_err(|e| format!("Failed waiting for codex app-server: {e}"))?
        };

        let stderr_text = self
            .stderr_task
            .await
            .map_err(|e| format!("Failed joining codex app-server stderr reader: {e}"))?
            .map_err(|e| format!("Failed reading codex app-server stderr: {e}"))?;

        let turn_completion = self.completion.lock().await.clone();

        Ok(CodexAppServerOutcome {
            exit_status,
            stderr_text,
            turn_completion,
        })
    }
}

pub async fn terminate_child_process(child: &Arc<Mutex<Child>>) -> Result<(), String> {
    let mut child = child.lock().await;
    terminate_child_process_locked(&mut child)
}

fn terminate_child_process_locked(child: &mut Child) -> Result<(), String> {
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            let rc = unsafe { libc::killpg(pid as i32, libc::SIGKILL) };
            if rc == 0 {
                tracing::info!("[CODEX] Sent SIGKILL to app-server process group {}", pid);
                return Ok(());
            }

            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::ESRCH) {
                return Err(format!(
                    "Interrupt failed: killpg({}, SIGKILL) returned {}",
                    pid, err
                ));
            }

            let direct_rc = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            if direct_rc == 0 {
                tracing::info!(
                    "[CODEX] App-server process group {} was unavailable; sent SIGKILL to child pid",
                    pid
                );
                return Ok(());
            }

            let direct_err = std::io::Error::last_os_error();
            if direct_err.raw_os_error() == Some(libc::ESRCH) {
                tracing::info!(
                    "[CODEX] App-server pid {} was already gone before SIGKILL",
                    pid
                );
                return Ok(());
            }

            return Err(format!(
                "Interrupt failed: killpg({}, SIGKILL) returned {}; kill({}, SIGKILL) returned {}",
                pid, err, pid, direct_err
            ));
        }
    }

    child
        .start_kill()
        .map_err(|e| format!("Interrupt failed: {}", e))
}

pub async fn run_codex_text(
    model: &str,
    reasoning_effort: &str,
    system_prompt: &str,
    working_dir: &Path,
    prompt: &str,
) -> Result<String, String> {
    run_codex_text_with_profile(
        model,
        reasoning_effort,
        system_prompt,
        working_dir,
        prompt,
        CodexToolProfile::Default,
    )
    .await
}

async fn run_codex_text_with_profile(
    model: &str,
    reasoning_effort: &str,
    system_prompt: &str,
    working_dir: &Path,
    prompt: &str,
    tool_profile: CodexToolProfile,
) -> Result<String, String> {
    let mut running = spawn_codex_app_server(CodexAppServerOptions {
        model,
        reasoning_effort,
        system_prompt,
        working_dir,
        prompt,
        sandbox: CodexSandboxMode::ReadOnly,
        bypass_approvals_and_sandbox: false,
        resume_session_id: None,
        ephemeral: true,
        tool_profile,
        scoped_user_id: None,
        current_conversation_id: None,
        scoped_email_id: None,
        approved_mcp_tools: Vec::new(),
    })
    .await?;
    let mut agent_message = AgentMessageTextCollector::default();

    while let Some(event) = running.events.recv().await {
        match event {
            CodexAppServerEvent::AgentMessageDelta { text, .. } => {
                agent_message.push_delta(&text);
            }
            CodexAppServerEvent::AgentMessageCompleted { text, .. } => {
                agent_message.set_completed(text);
            }
            _ => {}
        }
    }

    let outcome = running.wait().await?;
    if !outcome.success() {
        return Err(outcome.failure_summary("codex app-server"));
    }

    agent_message.finish().ok_or_else(|| {
        let stderr_text = outcome.stderr_text.trim();
        if stderr_text.is_empty() {
            "codex app-server returned no agentMessage output".to_string()
        } else {
            format!("codex app-server returned no agentMessage output: {stderr_text}")
        }
    })
}

fn parse_item_started(item: &Value) -> Option<CodexAppServerEvent> {
    match item.get("type")?.as_str()? {
        "commandExecution" => Some(CodexAppServerEvent::ToolCallStarted {
            id: item.get("id")?.as_str()?.to_string(),
            name: "shell".to_string(),
            input: json!({
                "command": item.get("command").and_then(|v| v.as_str()).unwrap_or_default(),
            }),
        }),
        "mcpToolCall" => Some(CodexAppServerEvent::ToolCallStarted {
            id: item.get("id")?.as_str()?.to_string(),
            name: format!(
                "mcp__{}__{}",
                item.get("server")?.as_str()?,
                item.get("tool")?.as_str()?
            ),
            input: item.get("arguments").cloned().unwrap_or(Value::Null),
        }),
        "dynamicToolCall" => Some(CodexAppServerEvent::ToolCallStarted {
            id: item.get("id")?.as_str()?.to_string(),
            name: item.get("tool")?.as_str()?.to_string(),
            input: item.get("arguments").cloned().unwrap_or(Value::Null),
        }),
        _ => None,
    }
}

fn parse_item_completed(item: &Value) -> Option<CodexAppServerEvent> {
    match item.get("type")?.as_str()? {
        "agentMessage" => Some(CodexAppServerEvent::AgentMessageCompleted {
            id: item.get("id")?.as_str()?.to_string(),
            text: item.get("text")?.as_str()?.to_string(),
        }),
        "commandExecution" => Some(CodexAppServerEvent::ToolCallCompleted {
            id: item.get("id")?.as_str()?.to_string(),
            content: extract_command_result_text(item),
            is_error: item
                .get("exitCode")
                .and_then(|v| v.as_i64())
                .map(|code| code != 0)
                .unwrap_or_else(|| item.get("status").and_then(|v| v.as_str()) == Some("failed")),
        }),
        "mcpToolCall" => Some(CodexAppServerEvent::ToolCallCompleted {
            id: item.get("id")?.as_str()?.to_string(),
            content: extract_mcp_result_text(item),
            is_error: item.get("error").map(|v| !v.is_null()).unwrap_or(false)
                || item.get("status").and_then(|v| v.as_str()) == Some("failed"),
        }),
        "dynamicToolCall" => Some(CodexAppServerEvent::ToolCallCompleted {
            id: item.get("id")?.as_str()?.to_string(),
            content: extract_dynamic_tool_result_text(item),
            is_error: item
                .get("success")
                .and_then(|v| v.as_bool())
                .map(|success| !success)
                .unwrap_or_else(|| item.get("status").and_then(|v| v.as_str()) == Some("failed")),
        }),
        _ => None,
    }
}

fn parse_token_usage(params: &Value) -> TokenUsageBreakdown {
    let token_usage = params.get("tokenUsage").unwrap_or(&Value::Null);
    let last = token_usage.get("last").unwrap_or(&Value::Null);
    let total = token_usage.get("total").unwrap_or(&Value::Null);
    let context_window_tokens = token_usage
        .get("modelContextWindow")
        .and_then(|value| value.as_i64())
        .filter(|tokens| *tokens > 0);
    let mut usage = TokenUsageBreakdown {
        input_tokens: token_count(last, "inputTokens"),
        cached_input_tokens: token_count(last, "cachedInputTokens"),
        output_tokens: token_count(last, "outputTokens"),
        reasoning_output_tokens: token_count(last, "reasoningOutputTokens"),
        total_tokens: token_count(last, "totalTokens"),
        thread_total_tokens: token_count(total, "totalTokens"),
        context_window_tokens,
    };
    usage.total_tokens = usage.total_or_derived();
    if usage.thread_total_tokens == 0 {
        usage.thread_total_tokens = usage.total_tokens;
    }
    usage
}

fn token_count(value: &Value, key: &str) -> i64 {
    value.get(key).and_then(|value| value.as_i64()).unwrap_or(0)
}

fn parse_turn_completion(params: &Value) -> TurnCompletion {
    let turn = params.get("turn").unwrap_or(&Value::Null);
    let status = turn
        .get("status")
        .and_then(|value| value.as_str())
        .unwrap_or("failed")
        .to_string();
    let error_message = turn
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(|value| value.as_str())
        .map(str::to_string);
    TurnCompletion {
        status,
        error_message,
    }
}

fn parse_error_notification(params: &Value) -> AppServerErrorNotification {
    let will_retry = params
        .get("willRetry")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let error = params.get("error").unwrap_or(&Value::Null);
    let mut message = error
        .get("message")
        .and_then(|value| value.as_str())
        .or_else(|| params.get("message").and_then(|value| value.as_str()))
        .or_else(|| error.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| {
            format!(
                "unknown app-server error payload {}",
                truncate_for_log(&compact_json(params), 500)
            )
        });

    if let Some(details) = error
        .get("additionalDetails")
        .and_then(|value| value.as_str())
        .filter(|details| !details.is_empty())
    {
        message.push_str(": ");
        message.push_str(details);
    }

    if let Some(codex_error_info) = summarize_codex_error_info(error.get("codexErrorInfo")) {
        message.push_str(" (");
        message.push_str(&codex_error_info);
        message.push(')');
    }

    AppServerErrorNotification {
        message,
        will_retry,
    }
}

fn summarize_codex_error_info(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if value.is_null() {
        return None;
    }

    let mut parts = Vec::new();
    collect_string_field(value, "code", &mut parts);
    collect_string_field(value, "type", &mut parts);
    collect_i64_field(value, "httpStatusCode", &mut parts);

    if parts.is_empty() {
        Some(truncate_for_log(&compact_json(value), 240))
    } else {
        Some(parts.join(", "))
    }
}

fn collect_string_field(value: &Value, key: &str, parts: &mut Vec<String>) {
    if let Some(field) = value.get(key).and_then(|field| field.as_str()) {
        parts.push(format!("{key}={field}"));
    }
}

fn collect_i64_field(value: &Value, key: &str, parts: &mut Vec<String>) {
    if let Some(field) = value.get(key).and_then(|field| field.as_i64()) {
        parts.push(format!("{key}={field}"));
    }
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

fn truncate_for_log(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }

    let mut truncated = value.chars().take(max_chars).collect::<String>();
    truncated.push_str("...");
    truncated
}

fn extract_command_result_text(item: &Value) -> String {
    let output = item
        .get("aggregatedOutput")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_string();
    if !output.is_empty() {
        return output;
    }

    match item.get("exitCode").and_then(|v| v.as_i64()) {
        Some(code) if code != 0 => format!("Command failed with exit code {code}"),
        Some(code) => format!("Command exited with status {code}"),
        None => String::new(),
    }
}

fn extract_mcp_result_text(item: &Value) -> String {
    if let Some(message) = item
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(|v| v.as_str())
    {
        return message.to_string();
    }

    let result = match item.get("result") {
        Some(result) if !result.is_null() => result,
        _ => return String::new(),
    };

    if let Some(content) = result.get("content").and_then(|v| v.as_array()) {
        let text_parts: Vec<&str> = content
            .iter()
            .filter(|entry| entry.get("type").and_then(|v| v.as_str()) == Some("text"))
            .filter_map(|entry| entry.get("text").and_then(|v| v.as_str()))
            .collect();
        if !text_parts.is_empty() {
            return text_parts.join("\n");
        }
    }

    serde_json::to_string(result).unwrap_or_default()
}

fn extract_dynamic_tool_result_text(item: &Value) -> String {
    if let Some(content_items) = item.get("contentItems").and_then(|v| v.as_array()) {
        let text_parts: Vec<&str> = content_items
            .iter()
            .filter(|entry| entry.get("type").and_then(|v| v.as_str()) == Some("inputText"))
            .filter_map(|entry| entry.get("text").and_then(|v| v.as_str()))
            .collect();
        if !text_parts.is_empty() {
            return text_parts.join("\n");
        }
    }

    serde_json::to_string(item).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc as StdArc, Mutex as StdMutex};
    use std::thread;

    fn sample_app_server_options<'a>(
        resume_session_id: Option<&'a str>,
    ) -> CodexAppServerOptions<'a> {
        CodexAppServerOptions {
            model: "",
            reasoning_effort: "medium",
            system_prompt: "",
            working_dir: Path::new("/tmp/codex-workspace"),
            prompt: "test prompt",
            sandbox: CodexSandboxMode::ReadOnly,
            bypass_approvals_and_sandbox: false,
            resume_session_id,
            ephemeral: true,
            tool_profile: CodexToolProfile::Default,
            scoped_user_id: None,
            current_conversation_id: None,
            scoped_email_id: None,
            approved_mcp_tools: Vec::new(),
        }
    }

    fn command_args(command: &StdCommand) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn command_env(command: &StdCommand, key: &str) -> Option<String> {
        command.get_envs().find_map(|(name, value)| {
            if name == OsStr::new(key) {
                value.map(|value| value.to_string_lossy().into_owned())
            } else {
                None
            }
        })
    }

    fn unique_temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "agentic-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time")
                .as_nanos()
        ))
    }

    #[test]
    fn ensure_directory_accepts_symlink_to_directory() {
        let root = unique_temp_path("codex-home-symlink-ok");
        let target = root.join("target");
        let link = root.join("link");
        std::fs::create_dir_all(&target).expect("create target");
        std::os::unix::fs::symlink(&target, &link).expect("create symlink");

        ensure_directory(&link, "Codex app-server home").expect("valid symlink");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ensure_directory_reports_broken_symlink_target() {
        let root = unique_temp_path("codex-home-broken-symlink");
        let target = root.join("missing-target");
        let link = root.join("link");
        std::fs::create_dir_all(&root).expect("create root");
        std::os::unix::fs::symlink(&target, &link).expect("create symlink");

        let error = ensure_directory(&link, "Codex app-server home").expect_err("broken symlink");

        assert!(error.contains("broken symlink"));
        assert!(error.contains(&target.display().to_string()));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn resolves_empty_model_to_gpt_5_5() {
        assert_eq!(resolve_codex_model(""), "gpt-5.5");
        assert_eq!(resolve_codex_model("gpt-5.5"), "gpt-5.5");
    }

    #[test]
    fn normalizes_legacy_effort_labels() {
        assert_eq!(normalize_reasoning_effort("none"), "none");
        assert_eq!(normalize_reasoning_effort("low"), "low");
        assert_eq!(normalize_reasoning_effort("xhigh"), "xhigh");
        assert_eq!(normalize_reasoning_effort("max"), "xhigh");
        assert_eq!(normalize_reasoning_effort("unknown"), "medium");
    }

    #[tokio::test]
    async fn terminate_child_process_falls_back_to_child_pid_without_process_group() {
        let child = Command::new("/bin/sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep child");
        let child = Arc::new(Mutex::new(child));

        terminate_child_process(&child)
            .await
            .expect("terminate child");

        let status = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            child.lock().await.wait().await
        })
        .await
        .expect("child should exit promptly after terminate")
        .expect("wait for child");
        assert!(!status.success());
    }

    #[test]
    fn launchd_safe_path_prepends_required_entries() {
        let path = launchd_safe_path_from(Some(OsStr::new("/custom/bin:/usr/bin")));
        assert_eq!(
            path.to_string_lossy(),
            "/opt/homebrew/opt/node@20/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin:/custom/bin"
        );
    }

    #[test]
    fn launchd_safe_path_handles_missing_existing_path() {
        let path = launchd_safe_path_from(None);
        assert_eq!(
            path.to_string_lossy(),
            "/opt/homebrew/opt/node@20/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
        );
    }

    #[test]
    fn build_codex_command_starts_app_server_without_exec_or_json() {
        let command = build_codex_app_server_command(
            &sample_app_server_options(None),
            Path::new("/tmp/agentic_mcp"),
            Path::new("/tmp/agentic_codex_home"),
        )
        .expect("build command");
        let args = command_args(&command);

        assert_eq!(args.first().map(String::as_str), Some("app-server"));
        assert!(args.iter().any(|arg| arg == "--listen"));
        assert!(args.iter().any(|arg| arg == "stdio://"));
        assert!(!args.iter().any(|arg| arg == "exec"));
        assert_eq!(command_env(&command, "RUST_LOG").as_deref(), Some("warn"));
    }

    #[test]
    fn app_server_command_keeps_mcp_override_and_feature_disables() {
        let mut options = sample_app_server_options(Some("session-123"));
        options.tool_profile = CodexToolProfile::RestrictedMcpOnly;
        options.scoped_user_id = Some("jakegreene");
        let command = build_codex_app_server_command(
            &options,
            Path::new("/tmp/agentic_mcp"),
            Path::new("/tmp/agentic_codex_home"),
        )
        .expect("build command");
        let args = command_args(&command);

        assert!(args
            .iter()
            .any(|arg| arg == "mcp_servers.agentic-mcp.command=\"/tmp/agentic_mcp\""));
        assert!(args
            .windows(2)
            .any(|pair| pair[0] == "--disable" && pair[1] == "apps"));
        assert!(args
            .windows(2)
            .any(|pair| pair[0] == "--disable" && pair[1] == "apply_patch_freeform"));
        assert!(args
            .windows(2)
            .any(|pair| pair[0] == "--disable" && pair[1] == "tool_search"));
        assert!(args
            .windows(2)
            .any(|pair| pair[0] == "--disable" && pair[1] == "shell_tool"));
        assert!(args
            .windows(2)
            .any(|pair| pair[0] == "--disable" && pair[1] == "browser_use"));
        assert!(args
            .windows(2)
            .any(|pair| pair[0] == "--disable" && pair[1] == "multi_agent"));
        assert_ne!(
            command.get_current_dir(),
            Some(Path::new("/tmp/codex-workspace"))
        );
    }

    #[test]
    fn restricted_profile_forces_read_only_sandbox() {
        let mut options = sample_app_server_options(None);
        options.tool_profile = CodexToolProfile::RestrictedMcpOnly;
        options.sandbox = CodexSandboxMode::DangerFullAccess;
        options.bypass_approvals_and_sandbox = true;

        assert_eq!(effective_sandbox(&options), CodexSandboxMode::ReadOnly);
    }

    #[test]
    fn no_tools_profile_forces_read_only_sandbox() {
        let mut options = sample_app_server_options(None);
        options.tool_profile = CodexToolProfile::NoTools;
        options.sandbox = CodexSandboxMode::DangerFullAccess;
        options.bypass_approvals_and_sandbox = true;

        assert_eq!(effective_sandbox(&options), CodexSandboxMode::ReadOnly);
    }

    #[test]
    fn no_tools_profile_omits_mcp_server_and_disables_shell_features() {
        let config = build_app_server_config(
            Path::new("/tmp/source-codex-home"),
            Path::new("/tmp/agentic-codex-sqlite"),
            Path::new("/tmp/agentic_mcp"),
            CodexToolProfile::NoTools,
        )
        .expect("build config");
        let parsed: toml::Value = toml::from_str(&config).expect("parse config");
        assert!(parsed.get("mcp_servers").is_none());
        assert_eq!(
            parsed.get("sqlite_home").and_then(|value| value.as_str()),
            Some("/tmp/agentic-codex-sqlite")
        );

        let mut options = sample_app_server_options(None);
        options.tool_profile = CodexToolProfile::NoTools;
        let command = build_codex_app_server_command(
            &options,
            Path::new("/tmp/agentic_mcp"),
            Path::new("/tmp/agentic_codex_home"),
        )
        .expect("build command");
        let args = command_args(&command);

        assert!(!args
            .iter()
            .any(|arg| arg.contains("mcp_servers.agentic-mcp.command")));
        assert!(args
            .windows(2)
            .any(|pair| pair[0] == "--disable" && pair[1] == "shell_tool"));
        assert!(args
            .windows(2)
            .any(|pair| pair[0] == "--disable" && pair[1] == "tool_search"));
    }

    #[test]
    fn app_server_config_points_mcp_child_at_source_codex_home() {
        let config = build_app_server_config(
            Path::new("/tmp/source-codex-home"),
            Path::new("/tmp/agentic-codex-sqlite"),
            Path::new("/tmp/agentic_mcp"),
            CodexToolProfile::Default,
        )
        .expect("build config");
        let parsed: toml::Value = toml::from_str(&config).expect("parse config");
        let agentic_mcp = parsed
            .get("mcp_servers")
            .and_then(|servers| servers.get("agentic-mcp"))
            .expect("agentic-mcp config");

        assert_eq!(
            agentic_mcp.get("command").and_then(|value| value.as_str()),
            Some("/tmp/agentic_mcp")
        );
        assert_eq!(
            agentic_mcp
                .get("env")
                .and_then(|env| env.get("CODEX_HOME"))
                .and_then(|value| value.as_str()),
            Some("/tmp/source-codex-home")
        );
        assert_eq!(
            parsed.get("sqlite_home").and_then(|value| value.as_str()),
            Some("/tmp/agentic-codex-sqlite")
        );
    }

    #[test]
    fn app_server_config_strips_source_tool_policy() {
        let source_home = std::env::temp_dir().join(format!(
            "agentic-codex-config-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time")
                .as_nanos()
        ));
        std::fs::create_dir_all(&source_home).expect("create source home");
        std::fs::write(
            source_home.join("config.toml"),
            r#"
[mcp_servers.agentic-mcp]
command = "/tmp/old_agentic_mcp"

[mcp_servers.agentic-mcp.env]
EXA_API_KEY = "keep-for-mcp"

[mcp_servers.agentic-mcp.tools.list_tickets]
approval_mode = "approve"
"#,
        )
        .expect("write source config");

        let config = build_app_server_config(
            &source_home,
            Path::new("/tmp/agentic-codex-sqlite"),
            Path::new("/tmp/agentic_mcp"),
            CodexToolProfile::RestrictedMcpOnly,
        )
        .expect("config");
        let parsed: toml::Value = toml::from_str(&config).expect("parse config");
        let agentic_mcp = parsed
            .get("mcp_servers")
            .and_then(|servers| servers.get("agentic-mcp"))
            .expect("agentic-mcp config");

        assert_eq!(
            agentic_mcp.get("command").and_then(|value| value.as_str()),
            Some("/tmp/agentic_mcp")
        );
        assert!(agentic_mcp.get("tools").is_none());
        assert!(agentic_mcp.get("enabled_tools").is_none());
        assert!(agentic_mcp.get("default_tools_enabled").is_none());
        assert!(agentic_mcp.get("default_tools_approval_mode").is_none());
        assert_eq!(
            agentic_mcp
                .get("env")
                .and_then(|env| env.get("EXA_API_KEY"))
                .and_then(|value| value.as_str()),
            Some("keep-for-mcp")
        );

        let _ = std::fs::remove_dir_all(source_home);
    }

    #[test]
    fn app_server_command_approves_requested_mcp_tools() {
        let approved = vec!["exa_search".to_string(), "exa_get_contents".to_string()];
        let mut options = sample_app_server_options(None);
        options.approved_mcp_tools = approved;
        let command = build_codex_app_server_command(
            &options,
            Path::new("/tmp/agentic_mcp"),
            Path::new("/tmp/agentic_codex_home"),
        )
        .expect("build command");
        let args = command_args(&command);

        assert!(args.iter().any(|arg| {
            arg == "mcp_servers.agentic-mcp.enabled_tools=[\"exa_get_contents\", \"exa_search\"]"
        }));
        assert!(args
            .iter()
            .any(|arg| arg == "mcp_servers.agentic-mcp.tools.exa_search.enabled=true"));
        assert!(args.iter().any(|arg| {
            arg == "mcp_servers.agentic-mcp.tools.exa_search.approval_mode=\"approve\""
        }));
    }

    #[test]
    fn app_server_command_passes_current_conversation_context_to_mcp() {
        let mut options = sample_app_server_options(None);
        options.scoped_user_id = Some("alex");
        options.current_conversation_id = Some("conv-123");
        let command = build_codex_app_server_command(
            &options,
            Path::new("/tmp/agentic_mcp"),
            Path::new("/tmp/agentic_codex_home"),
        )
        .expect("build command");
        let args = command_args(&command);

        assert!(args.iter().any(|arg| {
            arg == "mcp_servers.agentic-mcp.env.AGENTIC_MCP_CONVERSATION_ID=\"conv-123\""
        }));
        assert!(args
            .iter()
            .any(|arg| arg == "mcp_servers.agentic-mcp.env.AGENTIC_MCP_USER_ID=\"alex\""));
    }

    #[test]
    fn app_server_command_passes_scoped_email_id_to_mcp() {
        let mut options = sample_app_server_options(None);
        options.tool_profile = CodexToolProfile::ConfiguredMcpOnly;
        options.approved_mcp_tools = vec!["read_email_content".to_string()];
        options.scoped_email_id = Some(42);
        let command = build_codex_app_server_command(
            &options,
            Path::new("/tmp/agentic_mcp"),
            Path::new("/tmp/agentic_codex_home"),
        )
        .expect("build command");
        let args = command_args(&command);

        assert!(args
            .iter()
            .any(|arg| { arg == "mcp_servers.agentic-mcp.env.AGENTIC_MCP_EMAIL_SCOPE_ID=\"42\"" }));
        assert!(args
            .iter()
            .any(|arg| arg == "mcp_servers.agentic-mcp.enabled_tools=[\"read_email_content\"]"));
    }

    #[test]
    fn configured_mcp_profile_uses_explicit_tools_without_scoped_workspace_filter() {
        let approved = vec!["read_email_content".to_string()];
        let mut options = sample_app_server_options(None);
        options.tool_profile = CodexToolProfile::ConfiguredMcpOnly;
        options.approved_mcp_tools = approved;
        let command = build_codex_app_server_command(
            &options,
            Path::new("/tmp/agentic_mcp"),
            Path::new("/tmp/agentic_codex_home"),
        )
        .expect("build command");
        let args = command_args(&command);

        assert!(args
            .windows(2)
            .any(|pair| pair[0] == "--disable" && pair[1] == "shell_tool"));
        assert!(!args.iter().any(|arg| arg.contains("AGENTIC_MCP_PROFILE")));
        assert!(args
            .iter()
            .any(|arg| arg == "mcp_servers.agentic-mcp.enabled_tools=[\"read_email_content\"]"));
        assert!(args.iter().any(|arg| {
            arg == "mcp_servers.agentic-mcp.tools.read_email_content.approval_mode=\"approve\""
        }));
    }

    #[test]
    fn app_server_command_wildcard_approves_all_mcp_tools() {
        let approved = vec!["*".to_string()];
        let mut options = sample_app_server_options(None);
        options.approved_mcp_tools = approved;
        let command = build_codex_app_server_command(
            &options,
            Path::new("/tmp/agentic_mcp"),
            Path::new("/tmp/agentic_codex_home"),
        );
        let args = command_args(&command.expect("build command"));

        assert!(args
            .iter()
            .any(|arg| arg == "mcp_servers.agentic-mcp.default_tools_enabled=true"));
        assert!(args.iter().any(|arg| {
            arg == "mcp_servers.agentic-mcp.default_tools_approval_mode=\"approve\""
        }));
        assert!(!args
            .iter()
            .any(|arg| arg.contains("mcp_servers.agentic-mcp.enabled_tools")));
    }

    #[test]
    fn atomic_config_write_keeps_concurrent_readers_on_valid_toml() {
        let root = unique_temp_path("codex-config-atomic-write");
        std::fs::create_dir_all(&root).expect("create root");
        let config_path = root.join("config.toml");
        write_file_atomically(
            &config_path,
            "model = \"gpt-5.5\"\nmodel_reasoning_effort = \"medium\"\n",
            "test config",
        )
        .expect("write initial config");

        let done = StdArc::new(AtomicBool::new(false));
        let failure = StdArc::new(StdMutex::new(None::<String>));
        let reader_done = done.clone();
        let reader_failure = failure.clone();
        let reader_path = config_path.clone();
        let reader = thread::spawn(move || {
            while !reader_done.load(Ordering::SeqCst) {
                let text = match std::fs::read_to_string(&reader_path) {
                    Ok(text) => text,
                    Err(e) => {
                        *reader_failure.lock().expect("failure lock") =
                            Some(format!("read failed: {e}"));
                        break;
                    }
                };
                if let Err(e) = toml::from_str::<toml::Value>(&text) {
                    *reader_failure.lock().expect("failure lock") =
                        Some(format!("parse failed: {e}: {text:?}"));
                    break;
                }
            }
        });

        for index in 0..200 {
            let config = format!(
                "model = \"gpt-5.5\"\nmodel_reasoning_effort = \"medium\"\n\n[mcp_servers.agentic-mcp]\ncommand = \"/tmp/agentic_mcp_{index}\"\nstartup_timeout_sec = 30\n\n[mcp_servers.agentic-mcp.env]\nCODEX_HOME = \"/tmp/source-{index}\"\n"
            );
            write_file_atomically(&config_path, &config, "test config")
                .expect("atomic config write");
        }

        done.store(true, Ordering::SeqCst);
        reader.join().expect("reader join");
        assert_eq!(*failure.lock().expect("failure lock"), None);

        let temp_files = std::fs::read_dir(&root)
            .expect("read root")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(temp_files, 0);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn atomic_config_write_retries_when_temp_candidate_exists() {
        let root = unique_temp_path("codex-config-temp-collision");
        std::fs::create_dir_all(&root).expect("create root");
        let config_path = root.join("config.toml");
        let collision_path = root.join(".config.toml.collision.tmp");
        let retry_path = root.join(".config.toml.retry.tmp");
        let config = "model = \"gpt-5.5\"\nmodel_reasoning_effort = \"medium\"\n";

        std::fs::write(&collision_path, "leftover temp file").expect("write collision file");
        write_file_atomically_with_temp_candidates(
            &config_path,
            config,
            "test config",
            vec![Ok(collision_path.clone()), Ok(retry_path.clone())],
        )
        .expect("write after collision");

        assert_eq!(
            std::fs::read_to_string(&config_path).expect("read config"),
            config
        );
        assert!(collision_path.exists());
        assert!(!retry_path.exists());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn atomic_auth_write_keeps_concurrent_readers_on_valid_json() {
        let root = unique_temp_path("codex-auth-atomic-write");
        std::fs::create_dir_all(&root).expect("create root");
        let auth_path = root.join("auth.json");
        write_bytes_atomically(&auth_path, br#"{"token":"initial"}"#, "test auth")
            .expect("write initial auth");

        let done = StdArc::new(AtomicBool::new(false));
        let failure = StdArc::new(StdMutex::new(None::<String>));
        let reader_done = done.clone();
        let reader_failure = failure.clone();
        let reader_path = auth_path.clone();
        let reader = thread::spawn(move || {
            while !reader_done.load(Ordering::SeqCst) {
                let bytes = match std::fs::read(&reader_path) {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        *reader_failure.lock().expect("failure lock") =
                            Some(format!("read failed: {e}"));
                        break;
                    }
                };
                if let Err(e) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                    *reader_failure.lock().expect("failure lock") =
                        Some(format!("parse failed: {e}: {bytes:?}"));
                    break;
                }
            }
        });

        for index in 0..200 {
            let auth = format!(
                r#"{{"token":"token-{index}","refresh_token":"{}"}}"#,
                "x".repeat(2048)
            );
            write_bytes_atomically(&auth_path, auth.as_bytes(), "test auth")
                .expect("atomic auth write");
        }

        done.store(true, Ordering::SeqCst);
        reader.join().expect("reader join");
        assert_eq!(*failure.lock().expect("failure lock"), None);

        let temp_files = std::fs::read_dir(&root)
            .expect("read root")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(temp_files, 0);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn startup_error_formatter_appends_trimmed_stderr() {
        assert_eq!(
            append_app_server_stderr(
                "codex app-server closed stdout before initialize response".to_string(),
                "\nfailed to read auth.json\n"
            ),
            "codex app-server closed stdout before initialize response: failed to read auth.json"
        );
        assert_eq!(
            append_app_server_stderr(
                "codex app-server closed stdout before initialize response".to_string(),
                " \n "
            ),
            "codex app-server closed stdout before initialize response"
        );
    }

    #[test]
    fn parses_completed_agent_message_text() {
        let item = json!({"id":"item_0","type":"agentMessage","text":"hi"});
        let event = parse_item_completed(&item);
        match event {
            Some(CodexAppServerEvent::AgentMessageCompleted { id, text }) => {
                assert_eq!(id, "item_0");
                assert_eq!(text, "hi");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn text_collector_accumulates_delta_only_messages() {
        let mut collector = AgentMessageTextCollector::default();
        collector.push_delta("{\"suggestions\":[");
        collector.push_delta("{\"label\":\"Add tests\",\"message\":\"Add tests.\"}");
        collector.push_delta("]}");

        assert_eq!(
            collector.finish().as_deref(),
            Some("{\"suggestions\":[{\"label\":\"Add tests\",\"message\":\"Add tests.\"}]}")
        );
    }

    #[test]
    fn text_collector_prefers_completed_message() {
        let mut collector = AgentMessageTextCollector::default();
        collector.push_delta("partial");
        collector.set_completed("complete".to_string());

        assert_eq!(collector.finish().as_deref(), Some("complete"));
    }

    #[test]
    fn parses_command_execution_events() {
        let item = json!({
            "id":"item_1",
            "type":"commandExecution",
            "command":"/bin/zsh -lc pwd",
            "aggregatedOutput":"/private/tmp\n",
            "exitCode":0,
            "status":"completed"
        });
        let event = parse_item_completed(&item);
        match event {
            Some(CodexAppServerEvent::ToolCallCompleted {
                id,
                content,
                is_error,
            }) => {
                assert_eq!(id, "item_1");
                assert_eq!(content, "/private/tmp");
                assert!(!is_error);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn parses_mcp_tool_result_text() {
        let item = json!({
            "id":"item_3",
            "type":"mcpToolCall",
            "server":"agentic-mcp",
            "tool":"search_tickets",
            "arguments":{"query":"T-7FAAE188"},
            "result":{"content":[{"type":"text","text":"hello"}],"structuredContent":null},
            "error":null,
            "status":"completed"
        });
        let event = parse_item_completed(&item);
        match event {
            Some(CodexAppServerEvent::ToolCallCompleted {
                id,
                content,
                is_error,
            }) => {
                assert_eq!(id, "item_3");
                assert_eq!(content, "hello");
                assert!(!is_error);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn parses_token_usage_from_app_server_notification() {
        let params = json!({
            "threadId": "thread",
            "turnId": "turn",
            "tokenUsage": {
                "last": {
                    "inputTokens": 10,
                    "cachedInputTokens": 0,
                    "outputTokens": 5,
                    "reasoningOutputTokens": 2,
                    "totalTokens": 17
                },
                "total": {
                    "inputTokens": 30,
                    "cachedInputTokens": 4,
                    "outputTokens": 15,
                    "reasoningOutputTokens": 8,
                    "totalTokens": 53
                },
                "modelContextWindow": 200000
            }
        });
        assert_eq!(
            parse_token_usage(&params),
            TokenUsageBreakdown {
                input_tokens: 10,
                cached_input_tokens: 0,
                output_tokens: 5,
                reasoning_output_tokens: 2,
                total_tokens: 17,
                thread_total_tokens: 53,
                context_window_tokens: Some(200000),
            }
        );
    }

    #[test]
    fn parses_retryable_app_server_error_notification() {
        let params = json!({
            "threadId": "thread",
            "turnId": "turn",
            "willRetry": true,
            "error": {
                "message": "stream disconnected",
                "additionalDetails": "retrying sampling request",
                "codexErrorInfo": {
                    "code": "websocket_timeout",
                    "httpStatusCode": 504
                }
            }
        });

        assert_eq!(
            parse_error_notification(&params),
            AppServerErrorNotification {
                message:
                    "stream disconnected: retrying sampling request (code=websocket_timeout, httpStatusCode=504)"
                        .to_string(),
                will_retry: true,
            }
        );
    }

    #[test]
    fn parses_legacy_app_server_error_notification_message() {
        let params = json!({
            "message": "legacy failure"
        });

        assert_eq!(
            parse_error_notification(&params),
            AppServerErrorNotification {
                message: "legacy failure".to_string(),
                will_retry: false,
            }
        );
    }
}
