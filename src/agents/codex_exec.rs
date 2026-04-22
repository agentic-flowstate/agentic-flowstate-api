use serde_json::{json, Value};
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;

const DEFAULT_CODEX_MODEL: &str = "gpt-5.4";

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum CodexSandboxMode {
    ReadOnly,
    DangerFullAccess,
}

impl CodexSandboxMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::DangerFullAccess => "danger-full-access",
        }
    }
}

pub struct CodexExecOptions<'a> {
    pub model: &'a str,
    pub reasoning_effort: &'a str,
    pub system_prompt: &'a str,
    pub working_dir: &'a Path,
    pub prompt: &'a str,
    pub sandbox: CodexSandboxMode,
    pub bypass_approvals_and_sandbox: bool,
    pub resume_session_id: Option<&'a str>,
    pub ephemeral: bool,
}

#[derive(Debug, Clone)]
pub enum CodexExecEvent {
    ThreadStarted {
        thread_id: String,
    },
    AgentMessage {
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
        input_tokens: i64,
        output_tokens: i64,
    },
}

pub struct RunningCodexExec {
    child: Arc<Mutex<Child>>,
    pub events: mpsc::Receiver<CodexExecEvent>,
    stdout_task: JoinHandle<Result<(), String>>,
    stderr_task: JoinHandle<Result<String, std::io::Error>>,
}

pub struct CodexExecOutcome {
    pub exit_status: ExitStatus,
    pub stderr_text: String,
}

pub fn resolve_codex_model(model: &str) -> &str {
    match model {
        "" | "haiku" | "opus" | "claude-opus-4-7" => DEFAULT_CODEX_MODEL,
        legacy if legacy.starts_with("claude-") => DEFAULT_CODEX_MODEL,
        other => other,
    }
}

pub fn normalize_reasoning_effort(effort: &str) -> &str {
    match effort {
        "minimal" => "minimal",
        "low" => "low",
        "medium" => "medium",
        "high" => "high",
        "xhigh" => "xhigh",
        "max" => "xhigh",
        _ => "medium",
    }
}

pub async fn spawn_codex_exec(options: CodexExecOptions<'_>) -> Result<RunningCodexExec, String> {
    let normalized_effort = normalize_reasoning_effort(options.reasoning_effort);

    let mut command = Command::new("codex");
    command.current_dir(options.working_dir);

    if let Some(session_id) = options.resume_session_id {
        command
            .arg("exec")
            .arg("resume")
            .arg("--json")
            .arg("--skip-git-repo-check")
            .arg(session_id);
    } else {
        command
            .arg("exec")
            .arg("--json")
            .arg("--skip-git-repo-check")
            .arg("-C")
            .arg(options.working_dir);

        if options.ephemeral {
            command.arg("--ephemeral");
        }
    }

    command
        .arg("-m")
        .arg(resolve_codex_model(options.model))
        .arg("-c")
        .arg("forced_login_method=\"chatgpt\"")
        .arg("-c")
        .arg(format!("model_reasoning_effort=\"{normalized_effort}\""));

    if !options.system_prompt.is_empty() {
        let developer_instructions = serde_json::to_string(options.system_prompt)
            .map_err(|e| format!("Failed to encode Codex developer instructions: {e}"))?;
        command
            .arg("-c")
            .arg(format!("developer_instructions={developer_instructions}"));
    }

    if options.bypass_approvals_and_sandbox {
        command.arg("--dangerously-bypass-approvals-and-sandbox");
    } else if options.resume_session_id.is_none() {
        command.arg("--sandbox").arg(options.sandbox.as_str());
    }

    let mut child = command
        .arg(options.prompt)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to start codex exec: {e}"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to capture codex stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "Failed to capture codex stderr".to_string())?;

    let child = Arc::new(Mutex::new(child));
    let (tx, rx) = mpsc::channel(128);

    let stdout_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Some(line) = lines
            .next_line()
            .await
            .map_err(|e| format!("Failed reading codex stdout: {e}"))?
        {
            if let Some(event) = parse_codex_event(&line) {
                if tx.send(event).await.is_err() {
                    break;
                }
            }
        }
        Ok(())
    });

    let stderr_task = tokio::spawn(async move {
        let mut stderr_text = String::new();
        let mut reader = BufReader::new(stderr);
        reader.read_to_string(&mut stderr_text).await?;
        Ok::<String, std::io::Error>(stderr_text)
    });

    Ok(RunningCodexExec {
        child,
        events: rx,
        stdout_task,
        stderr_task,
    })
}

impl RunningCodexExec {
    pub fn child(&self) -> Arc<Mutex<Child>> {
        self.child.clone()
    }

    pub async fn wait(self) -> Result<CodexExecOutcome, String> {
        let stdout_result = self
            .stdout_task
            .await
            .map_err(|e| format!("Failed joining codex stdout reader: {e}"))?;
        stdout_result?;

        let exit_status = {
            let mut child = self.child.lock().await;
            child
                .wait()
                .await
                .map_err(|e| format!("Failed waiting for codex exec: {e}"))?
        };

        let stderr_text = self
            .stderr_task
            .await
            .map_err(|e| format!("Failed joining codex stderr reader: {e}"))?
            .map_err(|e| format!("Failed reading codex stderr: {e}"))?;

        Ok(CodexExecOutcome {
            exit_status,
            stderr_text,
        })
    }
}

pub async fn run_codex_text(
    model: &str,
    reasoning_effort: &str,
    system_prompt: &str,
    working_dir: &Path,
    prompt: &str,
) -> Result<String, String> {
    let mut running = spawn_codex_exec(CodexExecOptions {
        model,
        reasoning_effort,
        system_prompt,
        working_dir,
        prompt,
        sandbox: CodexSandboxMode::ReadOnly,
        bypass_approvals_and_sandbox: false,
        resume_session_id: None,
        ephemeral: true,
    })
    .await?;
    let mut last_agent_message = None;

    while let Some(event) = running.events.recv().await {
        if let CodexExecEvent::AgentMessage { text } = event {
            last_agent_message = Some(text);
        }
    }

    let outcome = running.wait().await?;
    let stderr_text = outcome.stderr_text.trim();

    if !outcome.exit_status.success() {
        if stderr_text.is_empty() {
            return Err(format!(
                "codex exec failed with status {}",
                outcome.exit_status
            ));
        }
        return Err(format!(
            "codex exec failed with status {}: {stderr_text}",
            outcome.exit_status
        ));
    }

    last_agent_message.ok_or_else(|| {
        if stderr_text.is_empty() {
            "codex exec returned no agent_message output".to_string()
        } else {
            format!("codex exec returned no agent_message output: {stderr_text}")
        }
    })
}

fn parse_codex_event(line: &str) -> Option<CodexExecEvent> {
    let value: Value = serde_json::from_str(line).ok()?;
    match value.get("type")?.as_str()? {
        "thread.started" => Some(CodexExecEvent::ThreadStarted {
            thread_id: value.get("thread_id")?.as_str()?.to_string(),
        }),
        "item.started" => parse_item_started(value.get("item")?),
        "item.completed" => parse_item_completed(value.get("item")?),
        "turn.completed" => Some(CodexExecEvent::TurnCompleted {
            input_tokens: value
                .get("usage")
                .and_then(|usage| usage.get("input_tokens"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
            output_tokens: value
                .get("usage")
                .and_then(|usage| usage.get("output_tokens"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
        }),
        _ => None,
    }
}

fn parse_item_started(item: &Value) -> Option<CodexExecEvent> {
    match item.get("type")?.as_str()? {
        "command_execution" => Some(CodexExecEvent::ToolCallStarted {
            id: item.get("id")?.as_str()?.to_string(),
            name: "shell".to_string(),
            input: json!({
                "command": item.get("command").and_then(|v| v.as_str()).unwrap_or_default(),
            }),
        }),
        "mcp_tool_call" => Some(CodexExecEvent::ToolCallStarted {
            id: item.get("id")?.as_str()?.to_string(),
            name: format!(
                "mcp__{}__{}",
                item.get("server")?.as_str()?,
                item.get("tool")?.as_str()?
            ),
            input: item.get("arguments").cloned().unwrap_or(Value::Null),
        }),
        _ => None,
    }
}

fn parse_item_completed(item: &Value) -> Option<CodexExecEvent> {
    match item.get("type")?.as_str()? {
        "agent_message" => Some(CodexExecEvent::AgentMessage {
            text: item.get("text")?.as_str()?.to_string(),
        }),
        "command_execution" => Some(CodexExecEvent::ToolCallCompleted {
            id: item.get("id")?.as_str()?.to_string(),
            content: extract_command_result_text(item),
            is_error: item
                .get("exit_code")
                .and_then(|v| v.as_i64())
                .map(|code| code != 0)
                .unwrap_or(false),
        }),
        "mcp_tool_call" => Some(CodexExecEvent::ToolCallCompleted {
            id: item.get("id")?.as_str()?.to_string(),
            content: extract_mcp_result_text(item),
            is_error: item.get("error").map(|v| !v.is_null()).unwrap_or(false),
        }),
        _ => None,
    }
}

fn extract_command_result_text(item: &Value) -> String {
    let output = item
        .get("aggregated_output")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_string();
    if !output.is_empty() {
        return output;
    }

    match item.get("exit_code").and_then(|v| v.as_i64()) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_legacy_models_to_gpt_5_4() {
        assert_eq!(resolve_codex_model("haiku"), "gpt-5.4");
        assert_eq!(resolve_codex_model("claude-opus-4-7"), "gpt-5.4");
        assert_eq!(resolve_codex_model("gpt-5.4"), "gpt-5.4");
    }

    #[test]
    fn normalizes_legacy_effort_labels() {
        assert_eq!(normalize_reasoning_effort("low"), "low");
        assert_eq!(normalize_reasoning_effort("xhigh"), "xhigh");
        assert_eq!(normalize_reasoning_effort("max"), "xhigh");
        assert_eq!(normalize_reasoning_effort("unknown"), "medium");
    }

    #[test]
    fn parses_completed_agent_message_text() {
        let line = r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"hi"}}"#;
        let event = parse_codex_event(line);
        match event {
            Some(CodexExecEvent::AgentMessage { text }) => assert_eq!(text, "hi"),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn parses_command_execution_events() {
        let line = r#"{"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"/bin/zsh -lc pwd","aggregated_output":"/private/tmp\n","exit_code":0,"status":"completed"}}"#;
        let event = parse_codex_event(line);
        match event {
            Some(CodexExecEvent::ToolCallCompleted {
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
        let line = r#"{"type":"item.completed","item":{"id":"item_3","type":"mcp_tool_call","server":"agentic-mcp","tool":"search_tickets","arguments":{"query":"\"T-7FAAE188\""},"result":{"content":[{"type":"text","text":"hello"}],"structured_content":null},"error":null,"status":"completed"}}"#;
        let event = parse_codex_event(line);
        match event {
            Some(CodexExecEvent::ToolCallCompleted {
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
}
