use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use ticketing_system::token_usage::TokenUsageBreakdown;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task::JoinHandle;

use super::codex_app_server::{agentic_mcp_binary, launchd_safe_path, terminate_child_process};

pub const FABLE_MODEL: &str = "claude-fable-5";
pub const FABLE_EFFORT: &str = "max";
pub const FABLE_MCP_PROFILE: &str = "fable-coordinator";
pub const FABLE_ALLOWED_MCP_TOOLS: &[&str] = &[
    "mcp__agentic-mcp__list_conversations",
    "mcp__agentic-mcp__get_conversation",
    "mcp__agentic-mcp__create_child_conversations",
    "mcp__agentic-mcp__get_conversation_processing_status",
    "mcp__agentic-mcp__get_runner_queue_capacity",
    "mcp__agentic-mcp__list_message_tool_calls",
    "mcp__agentic-mcp__get_tool_call",
    "mcp__agentic-mcp__get_child_agent_context",
    "mcp__agentic-mcp__list_organizations",
    "mcp__agentic-mcp__list_epics",
    "mcp__agentic-mcp__get_epic",
    "mcp__agentic-mcp__list_slices",
    "mcp__agentic-mcp__get_slice",
    "mcp__agentic-mcp__list_tickets",
    "mcp__agentic-mcp__list_tickets_by_due_date",
    "mcp__agentic-mcp__get_ticket",
    "mcp__agentic-mcp__search_tickets",
    "mcp__agentic-mcp__ensure_work_ticket",
    "mcp__agentic-mcp__create_slice_tickets",
    "mcp__agentic-mcp__update_ticket",
    "mcp__agentic-mcp__update_ticket_status",
    "mcp__agentic-mcp__add_ticket_relationship",
    "mcp__agentic-mcp__remove_ticket_relationship",
    "mcp__agentic-mcp__attach_ticket_documentation",
    "mcp__agentic-mcp__list_repos",
    "mcp__agentic-mcp__get_repo",
    "mcp__agentic-mcp__create_artifact",
    "mcp__agentic-mcp__update_artifact",
    "mcp__agentic-mcp__get_artifact",
    "mcp__agentic-mcp__list_artifacts",
    "mcp__agentic-mcp__search_artifacts",
    "mcp__agentic-mcp__agent_broadcast_post",
    "mcp__agentic-mcp__agent_broadcast_list",
    "mcp__agentic-mcp__agent_broadcast_update",
    "mcp__agentic-mcp__agent_broadcast_expire",
];

#[derive(Debug, Clone)]
pub enum ClaudeCodeEvent {
    SessionStarted {
        session_id: String,
    },
    AgentMessageDelta {
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
        duration_ms: u64,
    },
}

pub struct ClaudeCodeOptions<'a> {
    pub system_prompt: &'a str,
    pub prompt: &'a str,
    pub session_id: &'a str,
    pub resume: bool,
    pub conversation_id: &'a str,
    pub user_id: &'a str,
}

pub struct RunningClaudeCode {
    child: Arc<Mutex<Child>>,
    pub events: mpsc::Receiver<ClaudeCodeEvent>,
    stdout_task: JoinHandle<Result<ClaudeTurnCompletion, String>>,
    stderr_task: JoinHandle<Result<String, std::io::Error>>,
}

#[derive(Clone)]
pub struct ClaudeCodeTurnHandle {
    child: Arc<Mutex<Child>>,
}

impl ClaudeCodeTurnHandle {
    pub async fn terminate(&self) -> Result<(), String> {
        terminate_child_process(&self.child).await
    }
}

pub struct ClaudeCodeOutcome {
    pub exit_status: ExitStatus,
    pub stderr_text: String,
    completion: ClaudeTurnCompletion,
}

impl ClaudeCodeOutcome {
    pub fn success(&self) -> bool {
        self.exit_status.success()
            && self.completion.subtype == "success"
            && !self.completion.is_error
    }

    pub fn failure_summary(&self) -> String {
        if let Some(message) = self.completion.error_message.as_deref() {
            return classify_claude_failure(message);
        }
        let stderr = truncated_diagnostic(&self.stderr_text);
        if stderr.is_empty() {
            format!(
                "Claude Code Fable turn {} with process status {}",
                self.completion.subtype, self.exit_status
            )
        } else {
            classify_claude_failure(&stderr)
        }
    }

    pub fn duration_ms(&self) -> u64 {
        self.completion.duration_ms
    }
}

impl RunningClaudeCode {
    pub fn turn_handle(&self) -> ClaudeCodeTurnHandle {
        ClaudeCodeTurnHandle {
            child: self.child.clone(),
        }
    }

    pub async fn terminate(&self) -> Result<(), String> {
        terminate_child_process(&self.child).await
    }

    pub async fn wait(self) -> Result<ClaudeCodeOutcome, String> {
        let completion = self
            .stdout_task
            .await
            .map_err(|error| format!("Failed joining Claude Code stdout reader: {error}"))??;
        let exit_status = self
            .child
            .lock()
            .await
            .wait()
            .await
            .map_err(|error| format!("Failed waiting for Claude Code: {error}"))?;
        let stderr_text = self
            .stderr_task
            .await
            .map_err(|error| format!("Failed joining Claude Code stderr reader: {error}"))?
            .map_err(|error| format!("Failed reading Claude Code stderr: {error}"))?;

        Ok(ClaudeCodeOutcome {
            exit_status,
            stderr_text,
            completion,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeAuthStatus {
    logged_in: bool,
    auth_method: String,
    api_provider: String,
    subscription_type: Option<String>,
}

pub async fn verify_claude_subscription_auth() -> Result<(), String> {
    if std::env::var_os("ANTHROPIC_API_KEY").is_some() {
        return Err(
            "ANTHROPIC_API_KEY is set; refusing to start Fable because API credentials override Claude subscription authentication"
                .to_string(),
        );
    }

    let binary = claude_code_binary()?;
    let output = Command::new(&binary)
        .args(["auth", "status"])
        .env("PATH", launchd_safe_path())
        .env_remove("ANTHROPIC_API_KEY")
        .output()
        .await
        .map_err(|error| format!("Failed to read Claude Code subscription auth status: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "Claude Code subscription authentication failed; run `{}` auth login from a trusted Mac Mini terminal and complete the Safari flow",
            binary.display()
        ));
    }

    let status: ClaudeAuthStatus = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("Malformed Claude Code auth status JSON: {error}"))?;
    if !status.logged_in
        || status.auth_method != "claude.ai"
        || status.api_provider != "firstParty"
        || status.subscription_type.as_deref() != Some("max")
    {
        return Err(format!(
            "Claude Code must use the first-party claude.ai Max subscription (loggedIn={}, authMethod={}, apiProvider={}, subscriptionType={})",
            status.logged_in,
            status.auth_method,
            status.api_provider,
            status.subscription_type.as_deref().unwrap_or("none")
        ));
    }

    Ok(())
}

pub async fn spawn_claude_code(
    options: ClaudeCodeOptions<'_>,
) -> Result<RunningClaudeCode, String> {
    verify_claude_subscription_auth().await?;
    uuid::Uuid::parse_str(options.session_id)
        .map_err(|_| "Fable native session id must be a valid UUID".to_string())?;

    let binary = claude_code_binary()?;
    let mcp_binary = agentic_mcp_binary()?;
    let mcp_config = build_mcp_config(&mcp_binary, options.conversation_id, options.user_id)?;
    let runtime_dir = isolated_fable_runtime_dir()?;
    let allowed_tools = FABLE_ALLOWED_MCP_TOOLS.join(",");
    let mut command = Command::new(&binary);
    command
        .arg("--print")
        .arg("--verbose")
        .arg("--model")
        .arg(FABLE_MODEL)
        .arg("--effort")
        .arg(FABLE_EFFORT)
        .arg("--output-format")
        .arg("stream-json")
        .arg("--include-partial-messages")
        .arg("--strict-mcp-config")
        .arg("--mcp-config")
        .arg(mcp_config)
        .arg("--permission-mode")
        .arg("bypassPermissions")
        .arg("--allowedTools")
        .arg(allowed_tools)
        .arg("--tools=")
        .arg("--disable-slash-commands")
        .arg("--setting-sources")
        .arg("")
        .arg("--no-chrome")
        .arg("--prompt-suggestions")
        .arg("false")
        .arg("--system-prompt")
        .arg(options.system_prompt);
    if options.resume {
        command.arg("--resume").arg(options.session_id);
    } else {
        command.arg("--session-id").arg(options.session_id);
    }
    command
        .arg(options.prompt)
        .current_dir(runtime_dir)
        .env("PATH", launchd_safe_path())
        .env_remove("ANTHROPIC_API_KEY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    tracing::info!(
        target: "agentic_api::fable",
        event = "fable.process_starting",
        conversation_id = options.conversation_id,
        session_id = options.session_id,
        resume = options.resume,
        model = FABLE_MODEL,
        effort = FABLE_EFFORT,
        auth_method = "claude.ai",
        subscription_type = "max",
        mcp_profile = FABLE_MCP_PROFILE,
        "starting subscription-authenticated Claude Code Fable process"
    );

    let mut child = command
        .spawn()
        .map_err(|error| format!("Failed to start Claude Code: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to capture Claude Code stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "Failed to capture Claude Code stderr".to_string())?;
    let child = Arc::new(Mutex::new(child));
    let (event_tx, event_rx) = mpsc::channel(256);
    let (ready_tx, ready_rx) = oneshot::channel();
    let expected_session_id = options.session_id.to_string();
    let stdout_task = tokio::spawn(read_claude_stdout(
        stdout,
        expected_session_id,
        event_tx,
        ready_tx,
    ));
    let stderr_task = tokio::spawn(async move {
        let mut stderr_text = String::new();
        BufReader::new(stderr)
            .read_to_string(&mut stderr_text)
            .await?;
        Ok::<_, std::io::Error>(stderr_text)
    });

    match tokio::time::timeout(std::time::Duration::from_secs(30), ready_rx).await {
        Ok(Ok(Ok(()))) => Ok(RunningClaudeCode {
            child,
            events: event_rx,
            stdout_task,
            stderr_task,
        }),
        Ok(Ok(Err(error))) => {
            terminate_child_process(&child).await.ok();
            let _ = stdout_task.await;
            let stderr = stderr_task
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_default();
            Err(format!("{error}: {}", truncated_diagnostic(&stderr)))
        }
        Ok(Err(error)) => {
            terminate_child_process(&child).await.ok();
            Err(format!("Claude Code startup reader exited: {error}"))
        }
        Err(_) => {
            terminate_child_process(&child).await.ok();
            Err(
                "Claude Code did not emit a valid Fable session init event within 30 seconds"
                    .to_string(),
            )
        }
    }
}

fn claude_code_binary() -> Result<PathBuf, String> {
    let binary = match std::env::var_os("CLAUDE_CODE_COMMAND") {
        Some(configured) => PathBuf::from(configured),
        None => dirs::home_dir()
            .ok_or_else(|| {
                "Cannot resolve the home directory for the required Claude Code binary".to_string()
            })?
            .join(".local/bin/claude"),
    };
    if !binary.is_file() {
        return Err(format!(
            "Required Claude Code binary not found at {}",
            binary.display()
        ));
    }
    Ok(binary)
}

fn isolated_fable_runtime_dir() -> Result<PathBuf, String> {
    let runtime_dir = std::env::temp_dir().join("agentic-flowstate-fable-runtime");
    fs::create_dir_all(&runtime_dir).map_err(|error| {
        format!(
            "Failed to create isolated Fable runtime directory {}: {error}",
            runtime_dir.display()
        )
    })?;

    for ancestor in runtime_dir.ancestors() {
        for context_path in [
            ancestor.join("CLAUDE.md"),
            ancestor.join("CLAUDE.local.md"),
            ancestor.join(".claude"),
        ] {
            if context_path.exists() {
                return Err(format!(
                    "Refusing to start Fable because isolated runtime path inherits Claude project context from {}",
                    context_path.display()
                ));
            }
        }
    }

    Ok(runtime_dir)
}

fn build_mcp_config(
    mcp_binary: &Path,
    conversation_id: &str,
    user_id: &str,
) -> Result<String, String> {
    serde_json::to_string(&json!({
        "mcpServers": {
            "agentic-mcp": {
                "type": "stdio",
                "command": mcp_binary,
                "args": [],
                "env": {
                    "AGENTIC_MCP_CONVERSATION_ID": conversation_id,
                    "AGENTIC_MCP_USER_ID": user_id,
                    "AGENTIC_MCP_PROFILE": FABLE_MCP_PROFILE,
                }
            }
        }
    }))
    .map_err(|error| format!("Failed to build strict Fable MCP configuration: {error}"))
}

async fn read_claude_stdout(
    stdout: tokio::process::ChildStdout,
    expected_session_id: String,
    event_tx: mpsc::Sender<ClaudeCodeEvent>,
    ready_tx: oneshot::Sender<Result<(), String>>,
) -> Result<ClaudeTurnCompletion, String> {
    let mut reader = BufReader::new(stdout).lines();
    let mut parser = ClaudeStreamParser::new(expected_session_id);
    let mut ready_tx = Some(ready_tx);

    while let Some(line) = reader
        .next_line()
        .await
        .map_err(|error| format!("Failed reading Claude Code stream JSON: {error}"))?
    {
        let events = match parser.parse_line(&line) {
            Ok(events) => events,
            Err(error) => {
                if let Some(tx) = ready_tx.take() {
                    let _ = tx.send(Err(error.clone()));
                }
                return Err(error);
            }
        };
        if parser.initialized && ready_tx.is_some() {
            if let Some(tx) = ready_tx.take() {
                let _ = tx.send(Ok(()));
            }
        }
        for event in events {
            if event_tx.send(event).await.is_err() {
                return Err("Claude Code event receiver closed".to_string());
            }
        }
    }

    if let Some(tx) = ready_tx.take() {
        let _ = tx.send(Err(
            "Claude Code exited before emitting a Fable session init event".to_string(),
        ));
    }
    parser
        .completion
        .ok_or_else(|| "Claude Code stream ended without a terminal result event".to_string())
}

#[derive(Debug, Clone)]
struct ClaudeTurnCompletion {
    subtype: String,
    is_error: bool,
    error_message: Option<String>,
    duration_ms: u64,
}

#[derive(Debug)]
enum PartialBlock {
    Text,
    Thinking,
    ToolUse {
        id: String,
        name: String,
        initial_input: Value,
        partial_json: String,
    },
    Ignored,
}

struct ClaudeStreamParser {
    expected_session_id: String,
    initialized: bool,
    current_message_id: Option<String>,
    blocks: HashMap<i64, PartialBlock>,
    completion: Option<ClaudeTurnCompletion>,
}

impl ClaudeStreamParser {
    fn new(expected_session_id: String) -> Self {
        Self {
            expected_session_id,
            initialized: false,
            current_message_id: None,
            blocks: HashMap::new(),
            completion: None,
        }
    }

    fn parse_line(&mut self, line: &str) -> Result<Vec<ClaudeCodeEvent>, String> {
        let value: Value = serde_json::from_str(line)
            .map_err(|error| format!("Malformed Claude Code stream JSON: {error}"))?;
        match value.get("type").and_then(Value::as_str) {
            Some("system") if value.get("subtype").and_then(Value::as_str) == Some("init") => {
                self.parse_init(&value)
            }
            Some("stream_event") => self.parse_stream_event(&value),
            Some("user") => self.parse_user_tool_results(&value),
            Some("result") => self.parse_result(&value),
            Some("assistant" | "rate_limit_event" | "system") => Ok(Vec::new()),
            Some(other) => Err(format!(
                "Unsupported Claude Code stream event type: {other}"
            )),
            None => Err("Claude Code stream event is missing type".to_string()),
        }
    }

    fn parse_init(&mut self, value: &Value) -> Result<Vec<ClaudeCodeEvent>, String> {
        let session_id = required_string(value, "session_id")?;
        if session_id != self.expected_session_id {
            return Err(format!(
                "Claude Code session continuity violation: expected {}, received {}",
                self.expected_session_id, session_id
            ));
        }
        let model = required_string(value, "model")?;
        if model != FABLE_MODEL {
            return Err(format!(
                "Claude Code model continuity violation: expected {FABLE_MODEL}, received {model}"
            ));
        }
        let api_key_source = required_string(value, "apiKeySource")?;
        if api_key_source != "none" {
            return Err(format!(
                "Claude Code reported apiKeySource={api_key_source}; refusing non-subscription Fable execution"
            ));
        }
        let connected = value
            .get("mcp_servers")
            .and_then(Value::as_array)
            .is_some_and(|servers| {
                servers.iter().any(|server| {
                    server.get("name").and_then(Value::as_str) == Some("agentic-mcp")
                        && server.get("status").and_then(Value::as_str) == Some("connected")
                })
            });
        if !connected {
            return Err(
                "Claude Code Fable process did not connect the strict agentic-mcp server"
                    .to_string(),
            );
        }

        self.initialized = true;
        Ok(vec![ClaudeCodeEvent::SessionStarted { session_id }])
    }

    fn parse_stream_event(&mut self, value: &Value) -> Result<Vec<ClaudeCodeEvent>, String> {
        let event = value
            .get("event")
            .ok_or_else(|| "Claude stream_event is missing event".to_string())?;
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                self.current_message_id = event
                    .get("message")
                    .and_then(|message| message.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                Ok(Vec::new())
            }
            Some("content_block_start") => {
                let index = required_i64(event, "index")?;
                let block = event
                    .get("content_block")
                    .ok_or_else(|| "content_block_start is missing content_block".to_string())?;
                let partial = match block.get("type").and_then(Value::as_str) {
                    Some("text") => PartialBlock::Text,
                    Some("thinking") => PartialBlock::Thinking,
                    Some("tool_use") => PartialBlock::ToolUse {
                        id: required_string(block, "id")?,
                        name: required_string(block, "name")?,
                        initial_input: block.get("input").cloned().unwrap_or_else(|| json!({})),
                        partial_json: String::new(),
                    },
                    _ => PartialBlock::Ignored,
                };
                self.blocks.insert(index, partial);
                Ok(Vec::new())
            }
            Some("content_block_delta") => self.parse_content_delta(event),
            Some("content_block_stop") => self.finish_content_block(event),
            Some("message_delta" | "message_stop") => Ok(Vec::new()),
            Some(other) => Err(format!("Unsupported Claude stream payload type: {other}")),
            None => Err("Claude stream payload is missing type".to_string()),
        }
    }

    fn parse_content_delta(&mut self, event: &Value) -> Result<Vec<ClaudeCodeEvent>, String> {
        let index = required_i64(event, "index")?;
        let delta = event
            .get("delta")
            .ok_or_else(|| "content_block_delta is missing delta".to_string())?;
        let block = self
            .blocks
            .get_mut(&index)
            .ok_or_else(|| format!("content delta references unknown block {index}"))?;
        match (block, delta.get("type").and_then(Value::as_str)) {
            (PartialBlock::Text, Some("text_delta")) => {
                let text = required_string(delta, "text")?;
                let id = self
                    .current_message_id
                    .clone()
                    .unwrap_or_else(|| format!("{}:{index}", self.expected_session_id));
                Ok(vec![ClaudeCodeEvent::AgentMessageDelta { id, text }])
            }
            (PartialBlock::Thinking, Some("thinking_delta")) => {
                Ok(vec![ClaudeCodeEvent::ReasoningDelta {
                    text: required_string(delta, "thinking")?,
                }])
            }
            (PartialBlock::Thinking, Some("signature_delta")) => Ok(Vec::new()),
            (PartialBlock::ToolUse { partial_json, .. }, Some("input_json_delta")) => {
                partial_json.push_str(&required_string(delta, "partial_json")?);
                Ok(Vec::new())
            }
            (PartialBlock::Ignored, _) => Ok(Vec::new()),
            (_, Some(other)) => Err(format!(
                "Claude content delta {other} does not match block {index}"
            )),
            (_, None) => Err("Claude content delta is missing type".to_string()),
        }
    }

    fn finish_content_block(&mut self, event: &Value) -> Result<Vec<ClaudeCodeEvent>, String> {
        let index = required_i64(event, "index")?;
        let Some(block) = self.blocks.remove(&index) else {
            return Err(format!(
                "content block stop references unknown block {index}"
            ));
        };
        match block {
            PartialBlock::ToolUse {
                id,
                name,
                initial_input,
                partial_json,
            } => {
                let input = if partial_json.trim().is_empty() {
                    initial_input
                } else {
                    serde_json::from_str(&partial_json).map_err(|error| {
                        format!("Malformed Claude tool input JSON for {name}: {error}")
                    })?
                };
                Ok(vec![ClaudeCodeEvent::ToolCallStarted { id, name, input }])
            }
            _ => Ok(Vec::new()),
        }
    }

    fn parse_user_tool_results(&mut self, value: &Value) -> Result<Vec<ClaudeCodeEvent>, String> {
        let Some(content) = value
            .get("message")
            .and_then(|message| message.get("content"))
            .and_then(Value::as_array)
        else {
            return Ok(Vec::new());
        };
        let mut events = Vec::new();
        for block in content {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let content = match block.get("content") {
                Some(Value::String(content)) => content.clone(),
                Some(content) => serde_json::to_string(content)
                    .map_err(|error| format!("Failed to encode Claude tool result: {error}"))?,
                None => String::new(),
            };
            events.push(ClaudeCodeEvent::ToolCallCompleted {
                id: required_string(block, "tool_use_id")?,
                content,
                is_error: block
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            });
        }
        Ok(events)
    }

    fn parse_result(&mut self, value: &Value) -> Result<Vec<ClaudeCodeEvent>, String> {
        let session_id = required_string(value, "session_id")?;
        if session_id != self.expected_session_id {
            return Err(format!(
                "Claude Code terminal session mismatch: expected {}, received {}",
                self.expected_session_id, session_id
            ));
        }
        let subtype = required_string(value, "subtype")?;
        let is_error = value
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(subtype != "success");
        let duration_ms = value
            .get("duration_ms")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        let error_message = is_error
            .then(|| {
                value
                    .get("result")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .flatten()
            .or_else(|| {
                value
                    .get("api_error_status")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        let usage = token_usage_from_result(value);
        self.completion = Some(ClaudeTurnCompletion {
            subtype,
            is_error,
            error_message,
            duration_ms,
        });
        Ok(vec![ClaudeCodeEvent::TurnCompleted { usage, duration_ms }])
    }
}

fn token_usage_from_result(value: &Value) -> TokenUsageBreakdown {
    let usage = value.get("usage").unwrap_or(&Value::Null);
    let input = usage
        .get("input_tokens")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let cache_creation = usage
        .get("cache_creation_input_tokens")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let cache_read = usage
        .get("cache_read_input_tokens")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let output = usage
        .get("output_tokens")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let reasoning = usage
        .get("output_tokens_details")
        .and_then(|details| details.get("thinking_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let context_window_tokens = value
        .get("modelUsage")
        .and_then(|usage| usage.get(FABLE_MODEL))
        .and_then(|model| model.get("contextWindow"))
        .and_then(Value::as_i64);
    let thread_total = input + cache_creation + cache_read + output;
    TokenUsageBreakdown {
        input_tokens: input + cache_creation,
        cached_input_tokens: cache_read,
        output_tokens: output,
        reasoning_output_tokens: reasoning,
        total_tokens: thread_total,
        thread_total_tokens: thread_total,
        context_window_tokens,
    }
}

fn required_string(value: &Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("Claude Code stream field {key} is missing or not a string"))
}

fn required_i64(value: &Value, key: &str) -> Result<i64, String> {
    value
        .get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("Claude Code stream field {key} is missing or not an integer"))
}

fn truncated_diagnostic(input: &str) -> String {
    input.trim().chars().take(2_000).collect()
}

fn classify_claude_failure(message: &str) -> String {
    let lowered = message.to_ascii_lowercase();
    if lowered.contains("401")
        || lowered.contains("authentication")
        || lowered.contains("not logged in")
        || lowered.contains("oauth")
    {
        return "Claude Code subscription authentication failed; run `~/.local/bin/claude auth login` from a trusted Mac Mini terminal and complete the Safari flow"
            .to_string();
    }
    if lowered.contains("no conversation found")
        || lowered.contains("session") && lowered.contains("not found")
    {
        return format!(
            "Fable native session continuity failed; explicit audited recovery is required: {}",
            truncated_diagnostic(message)
        );
    }
    if lowered.contains("model")
        && (lowered.contains("unavailable") || lowered.contains("not found"))
    {
        return format!(
            "Claude Fable 5 is unavailable: {}",
            truncated_diagnostic(message)
        );
    }
    format!(
        "Claude Code Fable turn failed: {}",
        truncated_diagnostic(message)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn parser() -> ClaudeStreamParser {
        ClaudeStreamParser::new("11111111-1111-4111-8111-111111111111".to_string())
    }

    fn init_line(session_id: &str, api_key_source: &str) -> String {
        json!({
            "type": "system",
            "subtype": "init",
            "session_id": session_id,
            "model": FABLE_MODEL,
            "apiKeySource": api_key_source,
            "mcp_servers": [{"name": "agentic-mcp", "status": "connected"}]
        })
        .to_string()
    }

    #[test]
    fn fable_tool_allowlist_matches_registry_and_excludes_direct_work_surfaces() {
        let configured: Value =
            serde_json::from_str(include_str!("../../agents.json")).expect("parse agents.json");
        let registry_tools = configured["agents"]["fable-coordinator"]["tools"]
            .as_array()
            .expect("Fable tools array")
            .iter()
            .map(|tool| tool.as_str().expect("tool name"))
            .collect::<HashSet<_>>();
        let runtime_tools = FABLE_ALLOWED_MCP_TOOLS
            .iter()
            .copied()
            .collect::<HashSet<_>>();

        assert_eq!(registry_tools, runtime_tools);
        assert!(runtime_tools.contains("mcp__agentic-mcp__create_child_conversations"));
        assert!(!runtime_tools.iter().any(|tool| tool.contains('*')));
        for forbidden in [
            "laminarforge",
            "research_",
            "build",
            "deploy",
            "manage_service",
            "run_setup",
            "workspace_",
            "web_automation",
            "email",
        ] {
            assert!(
                runtime_tools.iter().all(|tool| !tool.contains(forbidden)),
                "Fable runtime allowlist contains forbidden surface: {forbidden}"
            );
        }
    }

    #[test]
    fn fable_prompt_and_runtime_directory_do_not_inherit_project_context() {
        let prompt = include_str!("../../_prompts/fable-coordinator.txt").to_ascii_lowercase();
        for forbidden in [
            "{{agents_md}}",
            "{{artifact_memory_handoff}}",
            "laminarforge",
            "biotech",
            "biomedical",
            "hsv-2",
            "crispr",
            "aav",
            "cell culture",
            "microfluidic",
        ] {
            assert!(
                !prompt.contains(forbidden),
                "Fable prompt leaked forbidden domain context: {forbidden}"
            );
        }

        let runtime_dir = isolated_fable_runtime_dir().expect("isolated Fable runtime directory");
        assert!(runtime_dir.is_dir());
        assert!(runtime_dir
            .ancestors()
            .all(|ancestor| !ancestor.join("CLAUDE.md").exists()
                && !ancestor.join("CLAUDE.local.md").exists()
                && !ancestor.join(".claude").exists()));
    }

    #[test]
    fn init_requires_exact_session_model_subscription_and_mcp() {
        let mut stream_parser = parser();
        let events = stream_parser
            .parse_line(&init_line("11111111-1111-4111-8111-111111111111", "none"))
            .expect("valid init");
        assert!(stream_parser.initialized);
        assert!(matches!(
            events.as_slice(),
            [ClaudeCodeEvent::SessionStarted { session_id }]
                if session_id == "11111111-1111-4111-8111-111111111111"
        ));

        let mut api_key_parser = parser();
        assert!(api_key_parser
            .parse_line(&init_line(
                "11111111-1111-4111-8111-111111111111",
                "ANTHROPIC_API_KEY"
            ))
            .expect_err("API auth must fail")
            .contains("refusing non-subscription"));
    }

    #[test]
    fn text_thinking_and_tool_events_parse_without_duplication() {
        let mut parser = parser();
        parser
            .parse_line(&init_line("11111111-1111-4111-8111-111111111111", "none"))
            .unwrap();
        parser
            .parse_line(
                &json!({"type":"stream_event","event":{"type":"message_start","message":{"id":"msg-1"}}}).to_string(),
            )
            .unwrap();
        parser
            .parse_line(&json!({"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}).to_string())
            .unwrap();
        let text_events = parser
            .parse_line(&json!({"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}}).to_string())
            .unwrap();
        assert!(
            matches!(text_events.as_slice(), [ClaudeCodeEvent::AgentMessageDelta { id, text }] if id == "msg-1" && text == "Hello")
        );

        parser
            .parse_line(&json!({"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":""}}}).to_string())
            .unwrap();
        let thinking = parser
            .parse_line(&json!({"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"Plan"}}}).to_string())
            .unwrap();
        assert!(
            matches!(thinking.as_slice(), [ClaudeCodeEvent::ReasoningDelta { text }] if text == "Plan")
        );

        parser
            .parse_line(&json!({"type":"stream_event","event":{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"tool-1","name":"mcp__agentic-mcp__get_ticket","input":{}}}}).to_string())
            .unwrap();
        parser
            .parse_line(&json!({"type":"stream_event","event":{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"ticket_id\":\"T-1\"}"}}}).to_string())
            .unwrap();
        let tool = parser
            .parse_line(
                &json!({"type":"stream_event","event":{"type":"content_block_stop","index":2}})
                    .to_string(),
            )
            .unwrap();
        assert!(
            matches!(tool.as_slice(), [ClaudeCodeEvent::ToolCallStarted { id, name, input }] if id == "tool-1" && name == "mcp__agentic-mcp__get_ticket" && input["ticket_id"] == "T-1")
        );

        let result = parser
            .parse_line(&json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool-1","content":"done","is_error":false}]}}).to_string())
            .unwrap();
        assert!(
            matches!(result.as_slice(), [ClaudeCodeEvent::ToolCallCompleted { id, content, is_error }] if id == "tool-1" && content == "done" && !is_error)
        );
    }

    #[test]
    fn terminal_usage_and_errors_are_strict() {
        let mut parser = parser();
        let events = parser
            .parse_line(&json!({
                "type":"result",
                "subtype":"success",
                "is_error":false,
                "duration_ms":42,
                "session_id":"11111111-1111-4111-8111-111111111111",
                "usage":{"input_tokens":2,"cache_creation_input_tokens":10,"cache_read_input_tokens":5,"output_tokens":3},
                "modelUsage":{FABLE_MODEL:{"contextWindow":200000}}
            }).to_string())
            .unwrap();
        assert!(
            matches!(events.as_slice(), [ClaudeCodeEvent::TurnCompleted { usage, duration_ms }] if usage.total_tokens == 20 && *duration_ms == 42)
        );
        assert!(parser.completion.is_some());

        assert!(parser
            .parse_line("not-json")
            .expect_err("malformed JSON must fail")
            .contains("Malformed"));
    }

    #[test]
    fn failure_classification_never_falls_back() {
        assert!(
            classify_claude_failure("HTTP 401 OAuth token expired").contains("claude auth login")
        );
        assert!(
            classify_claude_failure("No conversation found with session ID abc")
                .contains("audited recovery")
        );
        assert!(classify_claude_failure("model unavailable").contains("Fable 5 is unavailable"));
    }
}
