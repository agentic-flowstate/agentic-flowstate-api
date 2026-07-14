use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

use ticketing_system::text_normalization::normalize_codex_token_delta_output;
use ticketing_system::token_usage::TokenUsageBreakdown;

use super::codex_app_server::{
    spawn_codex_app_server, CodexAppServerEvent, CodexAppServerOptions, CodexSandboxMode,
    CodexToolProfile,
};
use super::prompts::load_prompt;
use super::{AgentRun, AgentRunStatus, AgentType, EmailOutput, StreamEvent, TicketContext};

pub struct CodexAgentTurnResult {
    pub output_summary: String,
    pub tool_call_count: i32,
    pub runtime_session_id: Option<String>,
    pub usage: TokenUsageBreakdown,
}

fn codex_policy_for_agent_type(agent_type: &AgentType) -> (CodexSandboxMode, bool) {
    match agent_type {
        AgentType::Execution | AgentType::MeetingAgent | AgentType::FullAccess => {
            (CodexSandboxMode::DangerFullAccess, true)
        }
        _ => (CodexSandboxMode::ReadOnly, false),
    }
}

fn build_agent_prompt(
    agent_type: &AgentType,
    ticket_context: &TicketContext,
    previous_output: Option<&str>,
    selected_context: Option<&str>,
    sender_info: Option<&str>,
) -> String {
    let mut context_sections = Vec::new();

    if let Some(prev) = previous_output {
        let tag = match agent_type {
            AgentType::Planning => "research_output",
            AgentType::Execution => "implementation_plan",
            AgentType::Evaluation => "prior_outputs",
            AgentType::ResearchSynthesis => "research_findings",
            AgentType::TicketPlanner => "research_synthesis",
            AgentType::TicketCreator => "ticket_plan",
            AgentType::DocDrafter => "research_output",
            _ => "prior_context",
        };
        context_sections.push(format!("<{tag}>\n{prev}\n</{tag}>"));
    }

    if let Some(ctx) = selected_context {
        context_sections.push(format!("<selected_context>\n{ctx}\n</selected_context>"));
    }

    if let Some(info) = sender_info {
        context_sections.push(format!("<sender_info>\n{info}\n</sender_info>"));
    }

    if context_sections.is_empty() {
        format!(
            "Work on this ticket:\n\nTitle: {}\nIntent: {}",
            ticket_context.title, ticket_context.intent
        )
    } else {
        format!(
            "{}\n\nWork on this ticket:\n\nTitle: {}\nIntent: {}",
            context_sections.join("\n\n"),
            ticket_context.title,
            ticket_context.intent
        )
    }
}

fn finalize_output(output: &str) -> Option<String> {
    if output.trim().is_empty() {
        return None;
    }

    let full_output = normalize_codex_token_delta_output(output);
    if full_output.len() > 100000 {
        let end = (0..=100000)
            .rev()
            .find(|&i| full_output.is_char_boundary(i))
            .unwrap_or(0);
        Some(format!("{}...\n\n[Output truncated]", &full_output[..end]))
    } else {
        Some(full_output)
    }
}

pub async fn run_codex_agent_turn(
    agent_type: &AgentType,
    working_dir: &Path,
    system_prompt: &str,
    prompt: &str,
    resume_session_id: Option<&str>,
    persist_session: bool,
    event_tx: Option<mpsc::Sender<StreamEvent>>,
    result_session_id: &str,
) -> Result<CodexAgentTurnResult> {
    run_codex_agent_turn_inner(
        agent_type,
        working_dir,
        system_prompt,
        prompt,
        resume_session_id,
        persist_session,
        event_tx,
        result_session_id,
    )
    .await
}

async fn run_codex_agent_turn_inner(
    agent_type: &AgentType,
    working_dir: &Path,
    system_prompt: &str,
    prompt: &str,
    resume_session_id: Option<&str>,
    persist_session: bool,
    event_tx: Option<mpsc::Sender<StreamEvent>>,
    result_session_id: &str,
) -> Result<CodexAgentTurnResult> {
    let (sandbox, bypass_approvals_and_sandbox) = codex_policy_for_agent_type(agent_type);
    let mut turn = spawn_codex_app_server(CodexAppServerOptions {
        model: agent_type.model(),
        reasoning_effort: agent_type.effort(),
        system_prompt,
        working_dir,
        prompt,
        sandbox,
        bypass_approvals_and_sandbox,
        resume_session_id,
        ephemeral: !persist_session,
        state_owner_id: result_session_id,
        tool_profile: CodexToolProfile::Worker,
        scoped_user_id: None,
        current_conversation_id: None,
        scoped_email_id: None,
        approved_mcp_tools: agent_type.approved_mcp_tool_names(),
    })
    .await
    .map_err(anyhow::Error::msg)?;

    let mut message_order: Vec<String> = Vec::new();
    let mut agent_messages: HashMap<String, String> = HashMap::new();
    let mut tool_call_count = 0;
    let mut runtime_session_id = resume_session_id.map(str::to_string);
    let mut usage = TokenUsageBreakdown::default();
    let mut streamed_agent_message_items: HashSet<String> = HashSet::new();

    loop {
        let event = turn.events.recv().await;
        let Some(event) = event else {
            break;
        };

        match event {
            CodexAppServerEvent::ThreadStarted { thread_id } => {
                runtime_session_id = Some(thread_id);
            }
            CodexAppServerEvent::AgentMessageDelta { id, text } => {
                if text.is_empty() {
                    continue;
                }
                if !agent_messages.contains_key(&id) {
                    message_order.push(id.clone());
                    agent_messages.insert(id.clone(), String::new());
                }
                if let Some(message) = agent_messages.get_mut(&id) {
                    message.push_str(&text);
                }
                streamed_agent_message_items.insert(id);
                if let Some(ref tx) = event_tx {
                    let _ = tx.send(StreamEvent::Text { content: text }).await;
                }
            }
            CodexAppServerEvent::AgentMessageCompleted { id, text } => {
                if text.is_empty() {
                    continue;
                }
                if !agent_messages.contains_key(&id) {
                    message_order.push(id.clone());
                }
                agent_messages.insert(id.clone(), text.clone());
                if let Some(ref tx) = event_tx {
                    if !streamed_agent_message_items.contains(&id) {
                        let _ = tx.send(StreamEvent::Text { content: text }).await;
                    }
                }
            }
            CodexAppServerEvent::ReasoningDelta { text, .. } => {
                if let Some(ref tx) = event_tx {
                    let _ = tx.send(StreamEvent::Thinking { content: text }).await;
                }
            }
            CodexAppServerEvent::ToolCallStarted { id, name, input } => {
                tool_call_count += 1;
                if let Some(ref tx) = event_tx {
                    let _ = tx.send(StreamEvent::ToolUse { id, name, input }).await;
                }
            }
            CodexAppServerEvent::ToolCallCompleted {
                id,
                content,
                is_error,
            } => {
                if let Some(ref tx) = event_tx {
                    let _ = tx
                        .send(StreamEvent::ToolResult {
                            tool_use_id: id,
                            content,
                            is_error,
                        })
                        .await;
                }
            }
            CodexAppServerEvent::TurnCompleted { usage: event_usage } => {
                usage = event_usage;
            }
        }
    }

    let outcome = turn.wait().await.map_err(anyhow::Error::msg)?;
    if !outcome.success() {
        anyhow::bail!("{}", outcome.failure_summary("codex app-server"));
    }

    let final_message = message_order
        .iter()
        .rev()
        .find_map(|id| agent_messages.get(id))
        .ok_or_else(|| anyhow::anyhow!("No output from agent"))?;
    let output_summary =
        finalize_output(final_message).ok_or_else(|| anyhow::anyhow!("No output from agent"))?;
    let emitted_session_id = runtime_session_id
        .clone()
        .unwrap_or_else(|| result_session_id.to_string());

    if let Some(ref tx) = event_tx {
        let _ = tx
            .send(StreamEvent::Result {
                session_id: emitted_session_id,
                status: "success".to_string(),
                is_error: false,
            })
            .await;
    }

    Ok(CodexAgentTurnResult {
        output_summary,
        tool_call_count,
        runtime_session_id,
        usage,
    })
}

pub struct AgentExecutor {
    working_dir: PathBuf,
}

impl AgentExecutor {
    pub fn new(working_dir: PathBuf) -> Self {
        Self { working_dir }
    }

    pub async fn execute(
        &self,
        agent_type: AgentType,
        ticket_context: TicketContext,
        previous_output: Option<String>,
        selected_context: Option<String>,
        sender_info: Option<String>,
        event_tx: Option<mpsc::Sender<StreamEvent>>,
    ) -> Result<AgentRun> {
        let started_at = chrono::Utc::now().to_rfc3339();
        let session_id = uuid::Uuid::new_v4().to_string();

        let mut vars = HashMap::new();
        vars.insert("epic_id".to_string(), ticket_context.epic_id.clone());
        vars.insert("slice_id".to_string(), ticket_context.slice_id.clone());
        vars.insert("ticket_id".to_string(), ticket_context.ticket_id.clone());
        vars.insert("ticket_title".to_string(), ticket_context.title.clone());
        vars.insert("ticket_intent".to_string(), ticket_context.intent.clone());

        let system_prompt =
            load_prompt(agent_type.as_str(), vars).context("Failed to load agent prompt")?;
        let prompt = build_agent_prompt(
            &agent_type,
            &ticket_context,
            previous_output.as_deref(),
            selected_context.as_deref(),
            sender_info.as_deref(),
        );

        tracing::info!(
            "Starting agent execution via codex app-server: type={}, ticket={}, model={}",
            agent_type.as_str(),
            ticket_context.ticket_id,
            agent_type.model()
        );
        tracing::info!("System prompt length: {} chars", system_prompt.len());
        tracing::info!("Working dir: {:?}", self.working_dir);
        tracing::info!("Tools config: {:?}", agent_type.allowed_tools());
        tracing::info!("Max turns: {:?}", agent_type.max_turns());

        let turn = run_codex_agent_turn(
            &agent_type,
            &self.working_dir,
            &system_prompt,
            &prompt,
            None,
            true,
            event_tx,
            &session_id,
        )
        .await?;

        let completed_at = chrono::Utc::now().to_rfc3339();
        let email_output = if agent_type == AgentType::Email {
            EmailOutput::parse(&turn.output_summary)
        } else {
            None
        };

        Ok(AgentRun {
            session_id,
            organization: None,
            ticket_id: Some(ticket_context.ticket_id),
            epic_id: Some(ticket_context.epic_id),
            slice_id: Some(ticket_context.slice_id),
            agent_type: agent_type.as_str().to_string(),
            status: AgentRunStatus::Completed,
            started_at,
            completed_at: Some(completed_at),
            input_message: ticket_context.intent,
            output_summary: Some(turn.output_summary),
            email_output,
            tool_call_count: turn.tool_call_count,
            runtime_session_id: turn.runtime_session_id,
        })
    }
}
