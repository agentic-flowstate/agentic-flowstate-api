use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use once_cell::sync::Lazy;

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
    Email,
    WorkspaceManager,
    MeetingNotes,
    TicketAssistant,
    /// EXA-powered deep research agent - uses EXA API for web search/content + Anthropic for analysis
    ExaResearch,
    /// Critically evaluates and synthesizes research findings into structured, actionable output
    ResearchSynthesis,
    /// Plans follow-up tickets by checking existing system for duplicates and producing a mermaid graph
    TicketPlanner,
    /// Creates follow-up tickets from an approved ticket plan
    TicketCreator,
    /// Drafts policy documents, checklists, and training materials into the documentation repo
    DocDrafter,
    /// Personal life planner agent — daily planning, nutrition, training, project management
    LifePlanner,
    /// Selects the best next ticket to work on for a given organization
    PullTicket,
}

impl AgentType {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentType::Planning => "planning",
            AgentType::Execution => "execution",
            AgentType::Evaluation => "evaluation",
            AgentType::Email => "email",
            AgentType::WorkspaceManager => "workspace-manager",
            AgentType::MeetingNotes => "meeting-notes",
            AgentType::TicketAssistant => "ticket-assistant",
            AgentType::ExaResearch => "exa-research",
            AgentType::ResearchSynthesis => "research-synthesis",
            AgentType::TicketPlanner => "ticket-planner",
            AgentType::TicketCreator => "ticket-creator",
            AgentType::DocDrafter => "doc-drafter",
            AgentType::LifePlanner => "life-planner",
            AgentType::PullTicket => "pull-ticket",
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

    pub fn model(&self) -> &str {
        let config = self.config();
        AgentsConfig::get().resolve_model(&config.model)
    }

    pub fn max_turns(&self) -> Option<i32> {
        self.config().max_turns
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
        let subject = email_content[subject_start + 9..subject_end].trim().to_string();

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
    pub ticket_id: String,
    pub epic_id: String,
    pub slice_id: String,
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
    /// Explicit pipeline step ID to bind this agent run to.
    /// When set, the streaming handler manages the full step lifecycle
    /// (transition through Running → Completed/Failed) and advances the pipeline.
    #[serde(default)]
    pub step_id: Option<String>,
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

/// Structured streaming event for agent execution
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// Text content from the assistant
    Text { content: String },
    /// Tool use request
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// Tool result
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
    /// Thinking content (extended thinking)
    Thinking { content: String },
    /// Agent run status update
    Status { status: String, message: Option<String> },
    /// Final result
    Result {
        session_id: String,
        status: String,
        is_error: bool,
    },
    /// Sent after all historical events have been replayed during reconnection
    ReplayComplete {
        total_events: usize,
        agent_status: String,
    },
}
