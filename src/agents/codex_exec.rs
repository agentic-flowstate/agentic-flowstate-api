use serde_json::Value;
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;

const DEFAULT_CODEX_MODEL: &str = "gpt-5.4";

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

pub async fn run_codex_text(
    model: &str,
    reasoning_effort: &str,
    system_prompt: &str,
    working_dir: &Path,
    prompt: &str,
) -> Result<String, String> {
    let developer_instructions = serde_json::to_string(system_prompt)
        .map_err(|e| format!("Failed to encode Codex developer instructions: {e}"))?;
    let normalized_effort = normalize_reasoning_effort(reasoning_effort);

    let mut child = Command::new("codex")
        .arg("exec")
        .arg("--json")
        .arg("--ephemeral")
        .arg("--skip-git-repo-check")
        .arg("--sandbox")
        .arg("read-only")
        .arg("-C")
        .arg(working_dir)
        .arg("-m")
        .arg(resolve_codex_model(model))
        .arg("-c")
        .arg("forced_login_method=\"chatgpt\"")
        .arg("-c")
        .arg(format!("model_reasoning_effort=\"{normalized_effort}\""))
        .arg("-c")
        .arg(format!("developer_instructions={developer_instructions}"))
        .arg(prompt)
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

    let stderr_task = tokio::spawn(async move {
        let mut stderr_text = String::new();
        let mut reader = BufReader::new(stderr);
        reader.read_to_string(&mut stderr_text).await?;
        Ok::<String, std::io::Error>(stderr_text)
    });

    let mut lines = BufReader::new(stdout).lines();
    let mut last_agent_message = None;

    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|e| format!("Failed reading codex stdout: {e}"))?
    {
        if let Some(text) = extract_agent_message(&line) {
            last_agent_message = Some(text);
        }
    }

    let status = child
        .wait()
        .await
        .map_err(|e| format!("Failed waiting for codex exec: {e}"))?;
    let stderr_text = stderr_task
        .await
        .map_err(|e| format!("Failed joining codex stderr reader: {e}"))?
        .map_err(|e| format!("Failed reading codex stderr: {e}"))?;

    if !status.success() {
        let stderr_text = stderr_text.trim();
        if stderr_text.is_empty() {
            return Err(format!("codex exec failed with status {status}"));
        }
        return Err(format!("codex exec failed with status {status}: {stderr_text}"));
    }

    last_agent_message.ok_or_else(|| {
        let stderr_text = stderr_text.trim();
        if stderr_text.is_empty() {
            "codex exec returned no agent_message output".to_string()
        } else {
            format!("codex exec returned no agent_message output: {stderr_text}")
        }
    })
}

fn extract_agent_message(line: &str) -> Option<String> {
    let value: Value = serde_json::from_str(line).ok()?;
    if value.get("type")?.as_str()? != "item.completed" {
        return None;
    }

    let item = value.get("item")?;
    if item.get("type")?.as_str()? != "agent_message" {
        return None;
    }

    Some(item.get("text")?.as_str()?.to_string())
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
    fn extracts_completed_agent_message_text() {
        let line = r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"hi"}}"#;
        assert_eq!(extract_agent_message(line).as_deref(), Some("hi"));
    }
}
