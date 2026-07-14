use anyhow::{Context, Result};
use chrono::Utc;
use once_cell::sync::Lazy;
use serde::Serialize;
use sqlx::SqlitePool;
use std::sync::Arc;
use ticketing_system::models::{
    CreateTicketRequest, SystemLog, SystemLogIncident, SystemLogIncidentStatus, Ticket, TicketType,
};
use ticketing_system::system_logs::SystemLogIncidentScanState;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

pub const SCANNER_KEY: &str = "api-system-log-incident-triage";
pub const SCANNER_STALE_AFTER_SECONDS: i64 = (POLL_SECONDS as i64) * 3;
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
    pub ticketed_incidents: usize,
    pub already_addressed: usize,
    pub fixed_refreshed: u64,
    pub last_scanned_log_id: i64,
    pub scanner_status: Option<SystemLogIncidentScanState>,
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
                Ok(summary) if summary.scanned_logs > 0 || summary.ticketed_incidents > 0 => {
                    tracing::info!(
                        target: "agentic_api::log_incidents",
                        scanned_logs = summary.scanned_logs,
                        skipped_logs = summary.skipped_logs,
                        upserted_incidents = summary.upserted_incidents,
                        created_incidents = summary.created_incidents,
                        ticketed_incidents = summary.ticketed_incidents,
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

    ticketing_system::system_logs::advance_system_log_incident_scan_cursor(
        pool.as_ref(),
        SCANNER_KEY,
        last_seen_log_id,
    )
    .await?;
    summary.last_scanned_log_id = last_seen_log_id;

    let incidents = ticketing_system::system_logs::list_system_log_incidents_with_runtime_status(
        pool.as_ref(),
        Some(SystemLogIncidentStatus::Unaddressed),
        MAX_INCIDENTS_PER_TICK,
    )
    .await?;

    for item in incidents {
        match ticket_incident_for_review(pool.as_ref(), &item.incident).await? {
            TicketOutcome::Ticketed => summary.ticketed_incidents += 1,
            TicketOutcome::AlreadyAddressed => summary.already_addressed += 1,
        }
    }

    summary.scanner_status = ticketing_system::system_logs::get_system_log_incident_scan_state(
        pool.as_ref(),
        SCANNER_KEY,
        SCANNER_STALE_AFTER_SECONDS,
    )
    .await?;

    Ok(summary)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TicketOutcome {
    Ticketed,
    AlreadyAddressed,
}

async fn ticket_incident_for_review(
    pool: &SqlitePool,
    incident: &SystemLogIncident,
) -> Result<TicketOutcome> {
    let ticket = ensure_ticket(pool, incident).await?;
    let linked = ticketing_system::system_logs::link_system_log_incident_ticket_if_unaddressed(
        pool,
        &incident.incident_id,
        OWNER_AGENT,
        &ticket.ticket_id,
    )
    .await?;
    let Some(linked) = linked else {
        return Ok(TicketOutcome::AlreadyAddressed);
    };
    if linked.status != SystemLogIncidentStatus::Unaddressed {
        return Ok(TicketOutcome::AlreadyAddressed);
    }

    ticketing_system::system_logs::set_system_log_incident_status(
        pool,
        &linked.incident_id,
        SystemLogIncidentStatus::Investigating,
    )
    .await?;

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
            "failed to mark incident ticket in_progress after auto-ticketing incident"
        );
    }

    Ok(TicketOutcome::Ticketed)
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
            anchored: Some(false),
            classification: Some("automated".to_string()),
        },
    )
    .await
}

fn should_skip_log(log: &SystemLog) -> bool {
    let component = log.component.trim().to_ascii_lowercase();
    let message = log.message.trim().to_ascii_lowercase();
    component == "disk_pressure"
        || component.contains("health")
        || message.contains("[health_monitor]")
        || message.contains("health endpoint")
        || message.contains("automated health check")
        || is_readiness_probe_503_request_log(&message)
}

fn is_readiness_probe_503_request_log(message: &str) -> bool {
    let message = message.trim();
    message.starts_with("get /health/ready ")
        && (message.contains("→ 503") || message.contains("-> 503"))
        && !message.contains("/api/")
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
        assert!(should_skip_log(&test_log(
            "api",
            "GET /health/ready → 503 (0ms)"
        )));
        assert!(!should_skip_log(&test_log("chat", "Codex runtime failed")));
    }

    #[test]
    fn disk_pressure_logs_are_owned_by_the_disk_pressure_monitor() {
        assert!(should_skip_log(&test_log(
            "disk_pressure",
            "Writable volume disk pressure detected"
        )));
    }

    #[test]
    fn readiness_probe_skip_does_not_hide_non_health_api_failures() {
        assert!(!should_skip_log(&test_log(
            "api",
            "GET /api/health/ready → 503 (0ms)"
        )));
        assert!(!should_skip_log(&test_log(
            "api",
            "GET /health/ready-check → 503 (0ms)"
        )));
    }

    #[test]
    fn incident_titles_are_bounded() {
        let title = truncate_for_title(&"a ".repeat(200), 32);
        assert!(title.chars().count() <= 35);
        assert!(title.ends_with("..."));
    }
}
