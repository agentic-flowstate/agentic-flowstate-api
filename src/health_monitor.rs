//! Background health monitor that checks configured endpoints and creates bug tickets on failure.
//!
//! Runs every 6 hours. Reads endpoint config from the `health_endpoints` table in SQLite.
//! When an endpoint is down, searches for an existing open bug ticket to avoid duplicates,
//! then creates one if none exists. The ticket shows up in the iOS app.
//!
//! Endpoints are managed via MCP tools: `add_health_endpoint`, `list_health_endpoints`,
//! `remove_health_endpoint`. No code changes needed to add new orgs.

use sqlx::SqlitePool;
use std::time::Duration;
use ticketing_system::models::{CreateTicketRequest, TicketStatus, TicketType};
use ticketing_system::tickets;

/// Check a single endpoint. Returns Ok(()) or Err with a description of what failed.
///
/// For `/api/health` endpoints: validates the response body contains `"status":"ok"`.
/// A 200 with wrong body content is treated as a failure (the app may be serving an
/// error page that happens to return 200).
///
/// For all other endpoints: validates HTTP 2xx/3xx status code and non-empty body.
async fn check_endpoint(client: &reqwest::Client, url: &str) -> Result<(), String> {
    let resp = client.get(url).send().await.map_err(|e| format!("{}", e))?;
    let status = resp.status().as_u16();

    if status >= 400 {
        return Err(format!("HTTP {}", status));
    }

    // Read body for content validation
    let body = resp
        .text()
        .await
        .map_err(|e| format!("failed to read body: {}", e))?;

    if body.is_empty() {
        return Err("empty response body".to_string());
    }

    // For health API endpoints, validate the JSON body actually says "ok"
    if url.contains("/api/health") {
        if !body.contains("\"status\"") || !body.contains("\"ok\"") {
            return Err(format!(
                "health endpoint returned unexpected body: {}",
                &body[..body.len().min(200)]
            ));
        }
    }

    Ok(())
}

/// Check if there's already an open/in_progress bug ticket for this endpoint.
/// Searches by exact title prefix match to avoid duplicates.
async fn has_existing_bug(
    pool: &SqlitePool,
    organization: &str,
    epic_id: &str,
    slice_id: &str,
    endpoint_name: &str,
) -> bool {
    let title_prefix = format!("[DOWN] {}", endpoint_name);
    match tickets::list_tickets(pool, organization, epic_id, slice_id).await {
        Ok(existing) => existing.iter().any(|t| {
            t.ticket_type == TicketType::Bug
                && matches!(
                    t.status,
                    TicketStatus::Open | TicketStatus::InProgress | TicketStatus::Blocked
                )
                && t.title.starts_with(&title_prefix)
        }),
        Err(e) => {
            tracing::warn!("[HEALTH_MONITOR] Failed to check existing tickets: {}", e);
            // If we can't check, assume one exists to avoid spam
            true
        }
    }
}

/// Run all health checks. Reads endpoints from the database, checks each one,
/// and creates bug tickets for any that are down.
pub async fn run_checks(pool: &SqlitePool) {
    // Load endpoints from database
    let endpoints = match ticketing_system::health_endpoints::list_endpoints(pool, true).await {
        Ok(eps) => eps,
        Err(e) => {
            tracing::error!(
                "[HEALTH_MONITOR] Failed to load endpoints from database: {}",
                e
            );
            return;
        }
    };

    if endpoints.is_empty() {
        tracing::info!("[HEALTH_MONITOR] No endpoints configured — skipping. Use add_health_endpoint MCP tool to add URLs.");
        return;
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let mut down_count = 0;
    let mut up_count = 0;

    for ep in &endpoints {
        match check_endpoint(&client, &ep.url).await {
            Ok(()) => {
                up_count += 1;
                tracing::debug!("[HEALTH_MONITOR] {} — OK", ep.name);
            }
            Err(error) => {
                down_count += 1;
                tracing::warn!("[HEALTH_MONITOR] {} — DOWN ({})", ep.name, error);

                // Check for existing open bug before creating a new one
                if has_existing_bug(pool, &ep.organization, &ep.epic_id, &ep.slice_id, &ep.name)
                    .await
                {
                    tracing::info!(
                        "[HEALTH_MONITOR] Bug ticket already exists for {}, skipping",
                        ep.name
                    );
                    continue;
                }

                // Create a bug ticket
                let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
                let req = CreateTicketRequest {
                    organization: ep.organization.clone(),
                    epic_id: ep.epic_id.clone(),
                    slice_id: ep.slice_id.clone(),
                    title: format!("[DOWN] {} — {}", ep.name, today),
                    description: Some(format!(
                        "Automated health check failure.\n\n**Endpoint:** {}\n**Error:** {}\n**Detected:** {}\n\nThis ticket was auto-created by the API server health monitor.",
                        ep.url, error, chrono::Utc::now().to_rfc3339()
                    )),
                    ticket_type: TicketType::Bug,
                    assignee: None,
                    agent: None,
                    repository: None,
                    blocked_by: None,
                    milestone_id: Some(ep.milestone_id.clone()),
                    due_date: Some(today),
                    anchored: Some(false),
                    classification: Some("manual".to_string()),
                };

                match tickets::create_ticket(pool, req).await {
                    Ok(ticket) => {
                        tracing::info!(
                            "[HEALTH_MONITOR] Created bug ticket {} for {}",
                            ticket.ticket_id,
                            ep.name
                        );
                    }
                    Err(e) => {
                        // Don't crash — just log. Most likely cause: org/epic/slice doesn't exist yet.
                        tracing::error!(
                            "[HEALTH_MONITOR] Failed to create ticket for {}: {}",
                            ep.name,
                            e
                        );
                    }
                }
            }
        }
    }

    tracing::info!(
        "[HEALTH_MONITOR] Check complete: {} up, {} down (out of {} endpoints)",
        up_count,
        down_count,
        endpoints.len()
    );
}
