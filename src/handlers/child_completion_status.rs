use serde_json::Value;
use ticketing_system::ConversationMessage;

const CHILD_COMPLETION_STATUS: &str = "child_completion_status";
const CHILD_COMPLETION_RELAY: &str = "child_completion_relay";
const COORDINATOR_CHILD_COMPLETION_WAKE: &str = "coordinator_child_completion_wake";

#[derive(Debug, Clone)]
struct ChildCompletionStatus {
    child_conversation_id: String,
    child_title: String,
    child_agent: String,
    child_assistant_message_id: Option<String>,
    terminal_status: String,
    conversation_type: Option<String>,
    summary: Option<String>,
}

/// Legacy child completion relays were queued into the parent as `role=user`
/// with the full child transcript in `content`. Treat metadata as authoritative
/// and collapse those rows before they reach app rendering or prompt history.
pub fn sanitize_message_for_display(mut message: ConversationMessage) -> ConversationMessage {
    let Some(status) = child_completion_status_from_metadata(message.metadata.as_deref()) else {
        return message;
    };

    let summary = status
        .summary
        .clone()
        .unwrap_or_else(|| summarize_child_completion_output(&message.content, None));
    message.role = "assistant".to_string();
    message.content = format_child_completion_status_message(&status, &summary);
    message.metadata = Some(child_completion_status_metadata(&status, &summary));
    message.content_blocks = None;
    message.attachments = None;
    message
}

pub fn is_child_completion_status_message(message: &ConversationMessage) -> bool {
    child_completion_status_from_metadata(message.metadata.as_deref()).is_some()
}

pub fn is_coordinator_wake_message(message: &ConversationMessage) -> bool {
    orchestration_from_metadata(message.metadata.as_deref())
        .is_some_and(|orchestration| orchestration == COORDINATOR_CHILD_COMPLETION_WAKE)
}

pub fn is_hidden_from_chat_display(message: &ConversationMessage) -> bool {
    is_coordinator_wake_message(message)
}

fn child_completion_status_from_metadata(metadata: Option<&str>) -> Option<ChildCompletionStatus> {
    let value = orchestrated_metadata_value(metadata)?;
    let orchestration = value.get("orchestration")?.as_str()?;
    if orchestration != CHILD_COMPLETION_STATUS && orchestration != CHILD_COMPLETION_RELAY {
        return None;
    }

    Some(ChildCompletionStatus {
        child_conversation_id: string_field(&value, "child_conversation_id")?,
        child_title: string_field(&value, "child_title")?,
        child_agent: string_field(&value, "child_agent")?,
        child_assistant_message_id: optional_string_field(&value, "child_assistant_message_id"),
        terminal_status: string_field(&value, "child_terminal_status")?,
        conversation_type: optional_string_field(&value, "child_conversation_type"),
        summary: optional_string_field(&value, "summary"),
    })
}

fn orchestration_from_metadata(metadata: Option<&str>) -> Option<String> {
    let value = orchestrated_metadata_value(metadata)?;
    string_field(&value, "orchestration")
}

fn orchestrated_metadata_value(metadata: Option<&str>) -> Option<Value> {
    let metadata = metadata?;
    let value: Value = serde_json::from_str(metadata).ok()?;
    let origin = value.get("origin")?.as_str()?;
    if origin != "agent_orchestrated" {
        return None;
    }
    Some(value)
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    optional_string_field(value, key).filter(|v| !v.trim().is_empty())
}

fn optional_string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToOwned::to_owned)
}

fn format_child_completion_status_message(status: &ChildCompletionStatus, summary: &str) -> String {
    let summary = summary.trim();
    let summary = if summary.is_empty() {
        "Open the child conversation to review the full output."
    } else {
        summary
    };

    let assistant_line = status
        .child_assistant_message_id
        .as_ref()
        .map(|id| format!("\nAssistant message: `{id}`"))
        .unwrap_or_default();

    format!(
        "Child agent {terminal_status}: {child_title}\n\n{summary}\n\nOpen child chat: agenticflowstate://conversation/{child_conversation_id}?agent={child_agent}\n\nChild conversation: `{child_conversation_id}`{assistant_line}",
        terminal_status = status.terminal_status,
        child_title = status.child_title,
        child_conversation_id = status.child_conversation_id,
        child_agent = status.child_agent,
    )
}

fn child_completion_status_metadata(status: &ChildCompletionStatus, summary: &str) -> String {
    let mut value = serde_json::json!({
        "origin": "agent_orchestrated",
        "orchestrated_by": "agent-runner",
        "orchestration": CHILD_COMPLETION_STATUS,
        "display": "agent_completion_status_card",
        "child_conversation_id": status.child_conversation_id,
        "child_title": status.child_title,
        "child_agent": status.child_agent,
        "child_terminal_status": status.terminal_status,
        "summary": summary,
        "open_url": format!(
            "agenticflowstate://conversation/{}?agent={}",
            status.child_conversation_id, status.child_agent
        ),
    });

    if let Some(id) = &status.child_assistant_message_id {
        value["child_assistant_message_id"] = Value::String(id.clone());
    }
    if let Some(conversation_type) = &status.conversation_type {
        value["child_conversation_type"] = Value::String(conversation_type.clone());
    }

    value.to_string()
}

fn summarize_child_completion_output(child_output: &str, error_message: Option<&str>) -> String {
    if let Some(error_message) = error_message.filter(|message| !message.trim().is_empty()) {
        return truncate_summary_sentence(error_message.trim());
    }

    let normalized = child_output.replace("\r\n", "\n");
    if let Some(section) = extract_summary_section(&normalized) {
        return truncate_summary_sentence(&section);
    }

    let fallback = normalized
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .find(|line| {
            !line.starts_with('#')
                && !line.starts_with("Coordinator instruction:")
                && !line.starts_with("Final output:")
                && !line.starts_with("Child conversation:")
                && !line.starts_with("Agent:")
                && !line.starts_with("Status:")
                && !line.starts_with("Assistant message:")
        })
        .unwrap_or("Open the child conversation to review the full output.");

    truncate_summary_sentence(fallback)
}

fn extract_summary_section(output: &str) -> Option<String> {
    let lines: Vec<&str> = output.lines().collect();
    for (idx, line) in lines.iter().enumerate() {
        if !is_summary_heading(line) {
            continue;
        }

        let mut section = Vec::new();
        for next in lines.iter().skip(idx + 1) {
            let trimmed = next.trim();
            if trimmed.is_empty() {
                if !section.is_empty() {
                    break;
                }
                continue;
            }
            if is_markdown_heading(trimmed) && !section.is_empty() {
                break;
            }
            if is_numbered_section_heading(trimmed) && !section.is_empty() {
                break;
            }
            section.push(trimmed);
            if section.join(" ").chars().count() >= 220 {
                break;
            }
        }

        let summary = section.join(" ");
        if !summary.trim().is_empty() {
            return Some(summary);
        }
    }

    None
}

fn is_summary_heading(line: &str) -> bool {
    let normalized = line
        .trim()
        .trim_start_matches('#')
        .trim()
        .trim_start_matches(|c: char| c.is_ascii_digit() || c == '.')
        .trim()
        .to_ascii_lowercase();
    normalized == "summary"
}

fn is_markdown_heading(line: &str) -> bool {
    line.starts_with('#')
}

fn is_numbered_section_heading(line: &str) -> bool {
    let mut chars = line.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_digit() && chars.next() == Some('.')
}

fn truncate_summary_sentence(input: &str) -> String {
    let normalized = input.split_whitespace().collect::<Vec<_>>().join(" ");
    const MAX_CHARS: usize = 220;
    if normalized.chars().count() <= MAX_CHARS {
        return normalized;
    }

    let mut out = String::new();
    for ch in normalized.chars().take(MAX_CHARS.saturating_sub(1)) {
        out.push(ch);
    }
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_legacy_relay_to_compact_assistant_message() {
        let metadata = serde_json::json!({
            "origin": "agent_orchestrated",
            "orchestration": "child_completion_relay",
            "child_conversation_id": "child-1",
            "child_title": "Schema Design",
            "child_agent": "codebase-research",
            "child_terminal_status": "completed",
            "child_assistant_message_id": "assistant-1"
        })
        .to_string();
        let message = ConversationMessage {
            id: "message-1".to_string(),
            conversation_id: "parent-1".to_string(),
            role: "user".to_string(),
            content: "Child conversation finished.\n\nFinal output:\n\n### 1. Summary\nBuilt the schema foundation.\n\n### 2. Files".to_string(),
            attachments: None,
            metadata: Some(metadata),
            tool_call_summaries: None,
            content_blocks: None,
            assistant_turn_duration_seconds: None,
            created_at: 1,
            message_index: 1,
        };

        let sanitized = sanitize_message_for_display(message);

        assert_eq!(sanitized.role, "assistant");
        assert!(sanitized
            .content
            .contains("Child agent completed: Schema Design"));
        assert!(sanitized.content.contains("Built the schema foundation."));
        assert!(!sanitized.content.contains("Final output:"));
        assert!(sanitized
            .metadata
            .unwrap()
            .contains(CHILD_COMPLETION_STATUS));
    }

    #[test]
    fn coordinator_wake_messages_are_hidden_from_chat_display() {
        let metadata = serde_json::json!({
            "origin": "agent_orchestrated",
            "orchestration": "coordinator_child_completion_wake",
            "child_conversation_id": "child-1",
            "child_title": "Queue-return behavior smoke test",
            "child_agent": "full-access",
            "child_terminal_status": "completed"
        })
        .to_string();
        let message = ConversationMessage {
            id: "message-1".to_string(),
            conversation_id: "parent-1".to_string(),
            role: "user".to_string(),
            content: "Coordinator wake: child agent completed.".to_string(),
            attachments: None,
            metadata: Some(metadata),
            tool_call_summaries: None,
            content_blocks: None,
            assistant_turn_duration_seconds: None,
            created_at: 1,
            message_index: 1,
        };

        assert!(is_coordinator_wake_message(&message));
        assert!(is_hidden_from_chat_display(&message));
    }
}
