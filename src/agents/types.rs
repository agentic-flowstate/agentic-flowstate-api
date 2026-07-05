use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Agent configuration loaded from agents.json
#[derive(Debug, Clone, Deserialize)]
pub struct AgentConfig {
    pub model: String,
    #[serde(default)]
    pub max_turns: Option<i32>,
    #[allow(dead_code)] // Present in JSON config but prompts loaded by agent type name
    pub prompt_file: String,
    pub tools: Vec<String>,
    /// Optional working directory template. Supports `{{ORG_REPO:type}}` for org-scoped repo resolution.
    /// If not set, defaults to the base projects directory.
    #[serde(default)]
    pub working_dir: Option<String>,
    /// Effort level for adaptive thinking: "low", "medium", "high", "xhigh", "max".
    /// Defaults to "xhigh" if not set.
    #[serde(default = "default_effort")]
    pub effort: String,
}

fn default_effort() -> String {
    "xhigh".to_string()
}

/// Root config structure from agents.json
#[derive(Debug, Clone, Deserialize)]
pub struct AgentsConfig {
    pub models: HashMap<String, String>,
    pub agents: HashMap<String, AgentConfig>,
}

/// Global config loaded once at startup
static CONFIG: Lazy<AgentsConfig> = Lazy::new(|| {
    let config_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("agents.json");
    let config_str = std::fs::read_to_string(&config_path)
        .unwrap_or_else(|e| panic!("Failed to read agents.json at {:?}: {}", config_path, e));
    serde_json::from_str(&config_str)
        .unwrap_or_else(|e| panic!("Failed to parse agents.json: {}", e))
});

impl AgentsConfig {
    pub fn get() -> &'static AgentsConfig {
        &CONFIG
    }

    /// Resolve model alias to full model ID
    pub fn resolve_model<'a>(&'a self, alias: &'a str) -> &'a str {
        self.models.get(alias).map(|s| s.as_str()).unwrap_or(alias)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum AgentType {
    Planning,
    Execution,
    Evaluation,
    ConversationEvaluator,
    Feedback,
    Email,
    EmailClassifier,
    WorkspaceManager,
    MeetingNotes,
    TicketAssistant,
    /// EXA-powered deep research agent - uses EXA API for web search/content and model analysis
    ExaResearch,
    /// Recurring research agent invoked by Dailies automation
    DailyResearch,
    /// Recurring package-update summarizer invoked by Dailies automation
    PackageUpdateReview,
    /// Critically evaluates and synthesizes research findings into structured, actionable output
    ResearchSynthesis,
    /// Plans follow-up tickets by checking existing system for duplicates and producing a mermaid graph
    TicketPlanner,
    /// Creates follow-up tickets from an approved ticket plan
    TicketCreator,
    /// Drafts policy documents, checklists, and training materials into the documentation repo
    DocDrafter,
    /// Personal home planner agent — daily planning, nutrition, training, project management
    HomePlanner,
    /// Selects the best next ticket to work on for a given organization
    PullTicket,
    /// Local codebase + CLI research agent — explores files, runs commands, produces implementation plan
    CodebaseResearch,
    /// Manages documentation references on tickets - finds and attaches relevant docs
    DocManager,
    /// Post-meeting agent — processes transcript, takes action (create tickets, send emails, etc.)
    MeetingAgent,
    /// Full-access agent — every MCP tool + all built-in tools
    FullAccess,
    /// Scoped workspace manager — restricted tool set for external users (no home/daily plan/focus/code)
    ScopedWorkspace,
}

impl AgentType {
    pub fn from_chat_agent_key(key: &str) -> Option<Self> {
        match key {
            // "codex" is the model/runtime label users and tool agents naturally reach for
            // during the migration, but the runnable Agentic chat agent is full-access.
            "codex" => Some(AgentType::FullAccess),
            "full-access" => Some(AgentType::FullAccess),
            "home-planner" => Some(AgentType::HomePlanner),
            "workspace-manager" => Some(AgentType::WorkspaceManager),
            "meeting-agent" => Some(AgentType::MeetingAgent),
            "scoped-workspace" => Some(AgentType::ScopedWorkspace),
            "conversation-evaluator" => Some(AgentType::ConversationEvaluator),
            "feedback" => Some(AgentType::Feedback),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            AgentType::Planning => "planning",
            AgentType::Execution => "execution",
            AgentType::Evaluation => "evaluation",
            AgentType::ConversationEvaluator => "conversation-evaluator",
            AgentType::Feedback => "feedback",
            AgentType::Email => "email",
            AgentType::EmailClassifier => "email-classifier",
            AgentType::WorkspaceManager => "workspace-manager",
            AgentType::MeetingNotes => "meeting-notes",
            AgentType::TicketAssistant => "ticket-assistant",
            AgentType::ExaResearch => "exa-research",
            AgentType::DailyResearch => "daily-research",
            AgentType::PackageUpdateReview => "package-update-review",
            AgentType::ResearchSynthesis => "research-synthesis",
            AgentType::TicketPlanner => "ticket-planner",
            AgentType::TicketCreator => "ticket-creator",
            AgentType::DocDrafter => "doc-drafter",
            AgentType::HomePlanner => "home-planner",
            AgentType::PullTicket => "pull-ticket",
            AgentType::CodebaseResearch => "codebase-research",
            AgentType::DocManager => "doc-manager",
            AgentType::MeetingAgent => "meeting-agent",
            AgentType::FullAccess => "full-access",
            AgentType::ScopedWorkspace => "scoped-workspace",
        }
    }

    pub fn working_dir_template(&self) -> Option<&str> {
        self.config().working_dir.as_deref()
    }

    pub fn config(&self) -> &AgentConfig {
        AgentsConfig::get()
            .agents
            .get(self.as_str())
            .unwrap_or_else(|| panic!("No config for agent type: {}", self.as_str()))
    }

    pub fn allowed_tools(&self) -> Vec<&str> {
        self.config().tools.iter().map(|s| s.as_str()).collect()
    }

    pub fn approved_mcp_tool_names(&self) -> Vec<String> {
        self.allowed_tools()
            .into_iter()
            .filter_map(|tool| tool.strip_prefix("mcp__agentic-mcp__").map(str::to_string))
            .collect()
    }

    pub fn model(&self) -> &str {
        let config = self.config();
        AgentsConfig::get().resolve_model(&config.model)
    }

    pub fn max_turns(&self) -> Option<i32> {
        self.config().max_turns
    }

    pub fn effort(&self) -> &str {
        &self.config().effort
    }
}

/// Structured email output parsed from agent response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailOutput {
    pub to: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cc: Option<String>,
    pub subject: String,
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

impl EmailOutput {
    /// Parse email output from agent response containing XML-like tags
    /// Expected format:
    /// <email>
    /// <to>...</to>
    /// <cc>...</cc> (optional)
    /// <subject>...</subject>
    /// <body>...</body>
    /// </email>
    /// <notes>...</notes>
    pub fn parse(text: &str) -> Option<Self> {
        // Extract content between <email>...</email>
        let email_start = text.find("<email>")?;
        let email_end = text.find("</email>")?;
        let email_content = &text[email_start + 7..email_end];

        // Extract to
        let to_start = email_content.find("<to>")?;
        let to_end = email_content.find("</to>")?;
        let to = email_content[to_start + 4..to_end].trim().to_string();

        // Extract cc (optional)
        let cc = if let Some(cc_start) = email_content.find("<cc>") {
            if let Some(cc_end) = email_content.find("</cc>") {
                Some(email_content[cc_start + 4..cc_end].trim().to_string())
            } else {
                None
            }
        } else {
            None
        };

        // Extract subject
        let subject_start = email_content.find("<subject>")?;
        let subject_end = email_content.find("</subject>")?;
        let subject = email_content[subject_start + 9..subject_end]
            .trim()
            .to_string();

        // Extract body
        let body_start = email_content.find("<body>")?;
        let body_end = email_content.find("</body>")?;
        let body = email_content[body_start + 6..body_end].trim().to_string();

        // Extract notes (optional, outside of <email> tag)
        let notes = if let Some(notes_start) = text.find("<notes>") {
            if let Some(notes_end) = text.find("</notes>") {
                Some(text[notes_start + 7..notes_end].trim().to_string())
            } else {
                None
            }
        } else {
            None
        };

        Some(EmailOutput {
            to,
            cc,
            subject,
            body,
            notes,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRun {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticket_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epic_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slice_id: Option<String>,
    /// Agent type as string to support legacy/unknown types in history
    pub agent_type: String,
    pub status: AgentRunStatus,
    pub started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    pub input_message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_summary: Option<String>,
    /// Structured email output (only for email agent type)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email_output: Option<EmailOutput>,
    pub tool_call_count: i32,
    /// Runtime session ID for resuming after API restart
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cc_session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AgentRunStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl AgentRunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentRunStatus::Running => "running",
            AgentRunStatus::Completed => "completed",
            AgentRunStatus::Failed => "failed",
            AgentRunStatus::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone)]
pub struct TicketContext {
    pub epic_id: String,
    pub slice_id: String,
    pub ticket_id: String,
    pub title: String,
    pub intent: String,
}

#[derive(Debug, Deserialize)]
pub struct RunAgentRequest {
    pub agent_type: AgentType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_session_id: Option<String>,
    /// For email agent: select multiple previous agent runs to include as context
    #[serde(default)]
    pub selected_session_ids: Vec<String>,
    /// For ticket-assistant: custom user question to ask about the ticket
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_input_message: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RunAgentResponse {
    pub session_id: String,
    pub status: String,
}

/// Request to send a follow-up message to an existing agent session
#[derive(Debug, Deserialize)]
pub struct SendMessageRequest {
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct AgentRunsResponse {
    pub runs: Vec<AgentRun>,
}

// `StreamEvent` moved to `crate::agents::stream_event` (re-exported from
// `crate::agents::StreamEvent`) so the backfill binary can mount just the
// enum without pulling in this file's `AgentsConfig`/`once_cell::Lazy`
// static initializer. The compatibility re-export in `agents/mod.rs`
// keeps every existing `use crate::agents::StreamEvent` import working.

#[cfg(test)]
mod tests {
    use super::{AgentType, AgentsConfig};

    #[test]
    fn home_planner_has_no_email_tools() {
        let tools = AgentType::HomePlanner.allowed_tools();
        let disallowed = [
            "mcp__agentic-mcp__list_emails",
            "mcp__agentic-mcp__get_email",
            "mcp__agentic-mcp__search_emails",
            "mcp__agentic-mcp__read_email_content",
            "mcp__agentic-mcp__send_email",
            "mcp__agentic-mcp__list_email_threads",
            "mcp__agentic-mcp__get_email_thread",
            "mcp__agentic-mcp__list_drafts",
            "mcp__agentic-mcp__get_draft",
            "mcp__agentic-mcp__create_draft",
            "mcp__agentic-mcp__update_draft",
            "mcp__agentic-mcp__delete_draft",
            "mcp__agentic-mcp__send_draft",
            "mcp__agentic-mcp__list_email_accounts",
        ];

        for tool in disallowed {
            assert!(
                !tools.contains(&tool),
                "home-planner must not be an email agent: {tool}"
            );
        }
    }

    #[test]
    fn removed_email_guard_tools_are_not_explicitly_allowlisted() {
        let removed = [
            "mcp__agentic-mcp__prepare_email_for_agent_intake",
            "mcp__agentic-mcp__read_guarded_email_content",
            "mcp__agentic-mcp__get_safe_agent_email_payload",
            "mcp__agentic-mcp__check_email_agent_action_gate",
        ];

        for (agent_name, config) in &AgentsConfig::get().agents {
            for tool in removed {
                assert!(
                    !config.tools.iter().any(|allowed| allowed == tool),
                    "{agent_name} explicitly allowlists removed email guard tool {tool}"
                );
            }
        }
    }
}
