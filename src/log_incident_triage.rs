use anyhow::{Context, Result};
use chrono::Utc;
use once_cell::sync::Lazy;
use serde::Serialize;
use serde_json::json;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::Arc;
use ticketing_system::conversation_turn_jobs::ConversationTurnJobPayload;
use ticketing_system::conversations::{
    self, InitialChildTurnJobRequest, CHILD_CONVERSATION_ID_PLACEHOLDER,
    PARENT_CONVERSATION_ID_PLACEHOLDER,
};
use ticketing_system::models::{
    CreateChildConversationRequest, CreateConversationRequest, CreateTicketRequest, SystemLog,
    SystemLogIncident, SystemLogIncidentStatus, Ticket, TicketType,
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::agents::prompts::load_prompt;
use crate::agents::AgentType;
use crate::handlers::chat_stream::{encode_codex_options_for_job, ChatCodexOptions, ChatRuntime};

const SCANNER_KEY: &str = "api-system-log-incident-triage";
const OWNER_USER_ID: &str = "alex";
const ORGANIZATION: &str = "agentic-flowstate";
const EPIC_ID: &str = "backend";
const SLICE_ID: &str = "mcp-server";
const MILESTONE_ID: &str = "T-D8C67BA3";
const REPOSITORY: &str = "agentic-flowstate-api";
const OWNER_AGENT: &str = "full-access";
const MAX_LOGS_PER_TICK: i64 = 200;
const MAX_INCIDENTS_PER_TICK: i64 = 5;
const POLL_SECONDS: u64 = 60;

static TRIAGE_RUN_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

#[derive(Debug, Clone, Default, Serialize)]
pub struct LogIncidentTriageSummary {
    pub scanned_logs: usize,
    pub skipped_logs: usize,
    pub upserted_incidents: usize,
    pub created_incidents: usize,
    pub queued_investigations: usize,
    pub already_addressed: usize,
    pub queue_rejected: usize,
    pub fixed_refreshed: u64,
    pub last_scanned_log_id: i64,
}

pub fn spawn_system_log_incident_triage(pool: Arc<SqlitePool>, token: CancellationToken) {
    tokio::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_secs(20)).await;
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(POLL_SECONDS));

        loop {
            tokio::select! {
                _ = token.cancelled() => break,
                _ = interval.tick() => {}
            }

            match run_once(pool.clone()).await {
                Ok(summary) if summary.scanned_logs > 0 || summary.queued_investigations > 0 => {
                    tracing::info!(
                        target: "agentic_api::log_incidents",
                        scanned_logs = summary.scanned_logs,
                        skipped_logs = summary.skipped_logs,
                        upserted_incidents = summary.upserted_incidents,
                        created_incidents = summary.created_incidents,
                        queued_investigations = summary.queued_investigations,
                        fixed_refreshed = summary.fixed_refreshed,
                        "system log incident triage tick completed"
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::error!(
                        target: "agentic_api::log_incidents",
                        error = %error,
                        "system log incident triage tick failed"
                    );
                }
            }
        }
    });
}

pub async fn run_once(pool: Arc<SqlitePool>) -> Result<LogIncidentTriageSummary> {
    let _guard = TRIAGE_RUN_LOCK.lock().await;
    let mut summary = LogIncidentTriageSummary::default();

    summary.fixed_refreshed =
        ticketing_system::system_logs::refresh_fixed_system_log_incidents(pool.as_ref()).await?;

    let cursor = ticketing_system::system_logs::get_system_log_incident_scan_cursor(
        pool.as_ref(),
        SCANNER_KEY,
    )
    .await?;
    let logs = ticketing_system::system_logs::list_system_log_incident_candidate_logs_after(
        pool.as_ref(),
        cursor,
        MAX_LOGS_PER_TICK,
    )
    .await?;

    let mut last_seen_log_id = cursor;
    for log in logs {
        summary.scanned_logs += 1;
        last_seen_log_id = last_seen_log_id.max(log.id);
        if should_skip_log(&log) {
            summary.skipped_logs += 1;
            continue;
        }

        let incident = ticketing_system::system_logs::upsert_incident_for_error_log(
            pool.as_ref(),
            log.id,
            &[],
        )
        .await
        .with_context(|| format!("upsert system log incident for log {}", log.id))?;
        summary.upserted_incidents += 1;
        if incident.occurrence_count <= 1 && incident.first_log_id == log.id {
            summary.created_incidents += 1;
        }
    }

    if last_seen_log_id > cursor {
        ticketing_system::system_logs::advance_system_log_incident_scan_cursor(
            pool.as_ref(),
            SCANNER_KEY,
            last_seen_log_id,
        )
        .await?;
        summary.last_scanned_log_id = last_seen_log_id;
    } else {
        summary.last_scanned_log_id = cursor;
    }

    let incidents = ticketing_system::system_logs::list_system_log_incidents_with_runtime_status(
        pool.as_ref(),
        Some(SystemLogIncidentStatus::Unaddressed),
        MAX_INCIDENTS_PER_TICK,
    )
    .await?;

    for item in incidents {
        match queue_investigation(pool.as_ref(), &item.incident).await? {
            QueueOutcome::Queued => summary.queued_investigations += 1,
            QueueOutcome::AlreadyAddressed => summary.already_addressed += 1,
            QueueOutcome::Rejected => summary.queue_rejected += 1,
        }
    }

    Ok(summary)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueueOutcome {
    Queued,
    AlreadyAddressed,
    Rejected,
}

async fn queue_investigation(
    pool: &SqlitePool,
    incident: &SystemLogIncident,
) -> Result<QueueOutcome> {
    let admission =
        crate::handlers::runner_capacity::admit_enqueue(pool, 1, "system_log_incident_triage")
            .await
            .context("inspect runner queue capacity for system log incident")?;
    if !admission.accepted {
        return Ok(QueueOutcome::Rejected);
    }

    let ticket = ensure_ticket(pool, incident).await?;
    let linked = ticketing_system::system_logs::link_system_log_incident_ticket_if_unaddressed(
        pool,
        &incident.incident_id,
        OWNER_AGENT,
        &ticket.ticket_id,
    )
    .await?;
    let Some(linked) = linked else {
        return Ok(QueueOutcome::AlreadyAddressed);
    };
    if linked.status != SystemLogIncidentStatus::Unaddressed || linked.job_id.is_some() {
        return Ok(QueueOutcome::AlreadyAddressed);
    }

    let created = create_investigation_child(pool, &linked, &ticket).await?;
    let Some(queued) = created.queued_jobs.first() else {
        anyhow::bail!("incident investigation child creation returned no queued job");
    };

    let marked = ticketing_system::system_logs::mark_system_log_incident_investigating_once(
        pool,
        &linked.incident_id,
        OWNER_AGENT,
        &ticket.ticket_id,
        &queued.child_conversation_id,
        &queued.job_id,
    )
    .await?;

    if marked.is_none() {
        return Ok(QueueOutcome::AlreadyAddressed);
    }

    if let Err(error) = ticketing_system::tickets::update_ticket_status(
        pool,
        ORGANIZATION,
        EPIC_ID,
        SLICE_ID,
        &ticket.ticket_id,
        "in_progress",
    )
    .await
    {
        tracing::warn!(
            target: "agentic_api::log_incidents",
            ticket_id = %ticket.ticket_id,
            error = %error,
            "failed to mark incident ticket in_progress after queueing investigation"
        );
    }

    if let Err(error) = crate::handlers::conversations::publish_conversation_run_status(
        pool,
        &queued.child_conversation_id,
    )
    .await
    {
        tracing::warn!(
            target: "agentic_api::log_incidents",
            conversation_id = %queued.child_conversation_id,
            error = %error,
            "failed to publish incident investigation run status"
        );
    }

    Ok(QueueOutcome::Queued)
}

async fn ensure_ticket(pool: &SqlitePool, incident: &SystemLogIncident) -> Result<Ticket> {
    if let Some(ticket_id) = incident.ticket_id.as_deref() {
        if let Some(ticket) = ticketing_system::tickets::get_ticket_by_id(pool, ticket_id).await? {
            return Ok(ticket);
        }
    }

    let today = Utc::now().format("%Y-%m-%d").to_string();
    ticketing_system::tickets::create_ticket(
        pool,
        CreateTicketRequest {
            organization: ORGANIZATION.to_string(),
            epic_id: EPIC_ID.to_string(),
            slice_id: SLICE_ID.to_string(),
            title: incident_ticket_title(incident),
            description: Some(incident_ticket_description(incident)),
            ticket_type: TicketType::Bug,
            assignee: None,
            agent: Some(OWNER_AGENT.to_string()),
            repository: Some(REPOSITORY.to_string()),
            blocked_by: None,
            milestone_id: Some(MILESTONE_ID.to_string()),
            due_date: Some(today),
            classification: Some("automated".to_string()),
        },
    )
    .await
}

async fn create_investigation_child(
    pool: &SqlitePool,
    incident: &SystemLogIncident,
    ticket: &Ticket,
) -> Result<conversations::CreatedMultiAgentConversationWithJobs> {
    let parent = CreateConversationRequest {
        user_id: OWNER_USER_ID.to_string(),
        organization: ORGANIZATION.to_string(),
        title: format!("System log incident {}", incident.incident_id),
        session_id: None,
        agent: Some(OWNER_AGENT.to_string()),
        conversation_type: Some("system-log-incident".to_string()),
        parent_conversation_id: None,
        conversation_role: Some("multi_agent_parent".to_string()),
        child_sort_order: None,
    };
    let children = vec![CreateChildConversationRequest {
        title: "Incident investigation".to_string(),
        agent: Some(OWNER_AGENT.to_string()),
        conversation_type: Some("incident-investigation".to_string()),
        child_sort_order: Some(0),
    }];

    let agent_type = AgentType::FullAccess;
    let codex_options = ChatCodexOptions::default_for_agent(&agent_type);
    let payload = ConversationTurnJobPayload {
        user_id: OWNER_USER_ID.to_string(),
        message: investigation_message(incident, ticket)?,
        agent_type: OWNER_AGENT.to_string(),
        runtime: ChatRuntime::CodexAppServer.as_job_runtime().to_string(),
        prompt_name: OWNER_AGENT.to_string(),
        working_dir: "/Users/jarvisgpt/projects".to_string(),
        prompt_vars: encode_codex_options_for_job(HashMap::new(), &codex_options),
        images_json: None,
        client_id: Some(format!("system-log-incident:{}", incident.incident_id)),
        message_metadata: Some(
            json!({
                "origin": "system_log_incident_triage",
                "orchestration": "incident_investigation",
                "incident_id": incident.incident_id,
                "fingerprint": incident.fingerprint,
                "ticket_id": ticket.ticket_id,
                "parent_conversation_id": PARENT_CONVERSATION_ID_PLACEHOLDER,
                "child_conversation_id": CHILD_CONVERSATION_ID_PLACEHOLDER,
            })
            .to_string(),
        ),
    };
    let initial_turns = vec![InitialChildTurnJobRequest {
        child_index: 0,
        payload,
    }];

    conversations::create_multi_agent_conversation_with_initial_turn_jobs(
        pool,
        parent,
        children,
        initial_turns,
    )
    .await
}

fn should_skip_log(log: &SystemLog) -> bool {
    let component = log.component.trim().to_ascii_lowercase();
    let message = log.message.trim().to_ascii_lowercase();
    component.contains("health")
        || message.contains("[health_monitor]")
        || message.contains("health endpoint")
        || message.contains("automated health check")
}

fn incident_ticket_title(incident: &SystemLogIncident) -> String {
    format!(
        "[System log] {}: {}",
        incident.component,
        truncate_for_title(&incident.sample_message, 96)
    )
}

fn incident_ticket_description(incident: &SystemLogIncident) -> String {
    format!(
        "Automated system log incident triage.\n\nIncident: {}\nFingerprint: {}\nLevel: {}\nComponent: {}\nOccurrences: {}\nFirst log ID: {}\nLast log ID: {}\nFirst seen: {}\nLast seen: {}\n\nMessage:\n{}\n\nDetail:\n{}",
        incident.incident_id,
        incident.fingerprint,
        incident.level,
        incident.component,
        incident.occurrence_count,
        incident.first_log_id,
        incident.last_log_id,
        incident.first_seen_at,
        incident.last_seen_at,
        incident.sample_message,
        incident.sample_detail.as_deref().unwrap_or("(none)")
    )
}

fn investigation_message(incident: &SystemLogIncident, ticket: &Ticket) -> Result<String> {
    let mut vars = HashMap::new();
    vars.insert("incident_id".to_string(), incident.incident_id.clone());
    vars.insert("ticket_id".to_string(), ticket.ticket_id.clone());
    vars.insert("level".to_string(), incident.level.clone());
    vars.insert("component".to_string(), incident.component.clone());
    vars.insert("fingerprint".to_string(), incident.fingerprint.clone());
    vars.insert(
        "occurrence_count".to_string(),
        incident.occurrence_count.to_string(),
    );
    vars.insert(
        "first_log_id".to_string(),
        incident.first_log_id.to_string(),
    );
    vars.insert("last_log_id".to_string(), incident.last_log_id.to_string());
    vars.insert("message".to_string(), incident.sample_message.clone());
    vars.insert(
        "detail".to_string(),
        incident
            .sample_detail
            .clone()
            .unwrap_or_else(|| "(none)".to_string()),
    );

    load_prompt("system-log-incident-investigation", vars)
}

fn truncate_for_title(value: &str, max_chars: usize) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= max_chars {
        return normalized;
    }
    let mut truncated = normalized.chars().take(max_chars).collect::<String>();
    truncated.push_str("...");
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_log(component: &str, message: &str) -> SystemLog {
        SystemLog {
            id: 1,
            level: "error".to_string(),
            component: component.to_string(),
            message: message.to_string(),
            detail: None,
            user_id: None,
            session_id: None,
            created_at: 1,
            created_at_iso: "1970-01-01T00:00:01+00:00".to_string(),
        }
    }

    #[test]
    fn health_monitor_logs_are_skipped_until_classified() {
        assert!(should_skip_log(&test_log(
            "health_monitor",
            "[HEALTH_MONITOR] api down"
        )));
        assert!(should_skip_log(&test_log(
            "api",
            "health endpoint returned unexpected body"
        )));
        assert!(!should_skip_log(&test_log("chat", "Codex runtime failed")));
    }

    #[test]
    fn incident_titles_are_bounded() {
        let title = truncate_for_title(&"a ".repeat(200), 32);
        assert!(title.chars().count() <= 35);
        assert!(title.ends_with("..."));
    }
}
