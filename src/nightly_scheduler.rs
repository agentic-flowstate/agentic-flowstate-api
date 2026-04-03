use std::collections::HashMap;
use std::sync::Arc;
use chrono::{Local, NaiveTime};
use cc_sdk::{ClaudeSDKClient, ClaudeCodeOptions, Message, ContentBlock, ToolsConfig, PermissionMode};
use futures::StreamExt;
use ticketing_system::{nightly_runs, tickets, NightlyRun, Ticket, SqlitePool};
use tokio_util::sync::CancellationToken;

use crate::agents::prompts::load_prompt;

/// Ticket data grouped by organization, ready for orchestrator dispatch.
#[derive(Debug)]
pub struct OrgTicketGroup {
    pub organization: String,
    pub tickets: Vec<SchedulerTicket>,
}

/// Ticket info the scheduler passes to orchestrators and pipelines.
#[derive(Debug, Clone)]
pub struct SchedulerTicket {
    pub ticket_id: String,
    pub organization: String,
    pub epic_id: String,
    pub slice_id: String,
    pub title: String,
    pub description: Option<String>,
    pub classification: String,
    pub repository: Option<String>,
    pub blocked_by: Vec<String>,
}

impl From<&Ticket> for SchedulerTicket {
    fn from(t: &Ticket) -> Self {
        Self {
            ticket_id: t.ticket_id.clone(),
            organization: t.organization.clone(),
            epic_id: t.epic_id.clone(),
            slice_id: t.slice_id.clone(),
            title: t.title.clone(),
            description: t.description.clone(),
            classification: t.classification.clone().unwrap_or_else(|| "automated".to_string()),
            repository: t.repository.clone(),
            blocked_by: t.blocked_by.clone().unwrap_or_default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Start the nightly scheduler background task.
/// Fires at 12:01 AM local time. On startup, runs a catch-up if today's run
/// hasn't happened yet and it's past 12:01 AM.
pub fn start_nightly_scheduler(db_pool: Arc<SqlitePool>, shutdown: CancellationToken) {
    tokio::spawn(scheduler_loop(db_pool, shutdown));
}

async fn scheduler_loop(db_pool: Arc<SqlitePool>, shutdown: CancellationToken) {
    tracing::info!("[nightly] Scheduler started");

    // Startup recovery: mark any orphaned running nightly runs as failed
    match nightly_runs::mark_running_as_failed(&db_pool).await {
        Ok(count) if count > 0 => {
            tracing::warn!("[nightly] Marked {} orphaned nightly run(s) as failed from previous run", count);
        }
        Ok(_) => {}
        Err(e) => {
            tracing::error!("[nightly] Failed to clean up orphaned nightly runs: {}", e);
        }
    }

    // Startup catch-up: if today's run doesn't exist and it's past 12:01 AM, run now
    let today = Local::now().format("%Y-%m-%d").to_string();
    let now_time = Local::now().time();
    let trigger_time = NaiveTime::from_hms_opt(0, 1, 0).unwrap();

    if now_time >= trigger_time {
        match nightly_runs::get_run_by_date(&db_pool, &today).await {
            Ok(None) => {
                tracing::info!("[nightly] Catch-up: no run for {} yet, triggering now", today);
                if let Err(e) = run_nightly_cycle(&db_pool).await {
                    tracing::error!("[nightly] Catch-up cycle failed: {}", e);
                }
            }
            Ok(Some(run)) => {
                tracing::info!("[nightly] Run for {} already exists (status: {}), skipping catch-up", today, run.status);
            }
            Err(e) => {
                tracing::error!("[nightly] Failed to check today's run: {}", e);
            }
        }
    }

    // Main timer loop: sleep until next 12:01 AM, then run
    loop {
        let sleep_duration = duration_until_next_trigger();
        tracing::info!(
            "[nightly] Next run in {:.1} hours",
            sleep_duration.as_secs_f64() / 3600.0
        );

        tokio::select! {
            _ = shutdown.cancelled() => {
                tracing::info!("[nightly] Scheduler shutting down");
                break;
            }
            _ = tokio::time::sleep(sleep_duration) => {
                tracing::info!("[nightly] Timer fired, starting nightly cycle");
                if let Err(e) = run_nightly_cycle(&db_pool).await {
                    tracing::error!("[nightly] Nightly cycle failed: {}", e);
                }
            }
        }
    }
}

/// Calculate duration from now until the next 12:01 AM local time.
fn duration_until_next_trigger() -> std::time::Duration {
    let now = Local::now();
    let trigger_time = NaiveTime::from_hms_opt(0, 1, 0).unwrap();

    let today_trigger = now.date_naive().and_time(trigger_time);
    let next_trigger = if now.naive_local() >= today_trigger {
        // Already past 12:01 AM today — schedule for tomorrow
        today_trigger + chrono::Duration::days(1)
    } else {
        today_trigger
    };

    let delta = next_trigger - now.naive_local();
    delta.to_std().unwrap_or(std::time::Duration::from_secs(60))
}

// ---------------------------------------------------------------------------
// Nightly cycle
// ---------------------------------------------------------------------------

/// Main nightly cycle entry point.
/// 1. Check double-run prevention
/// 2. Query overdue + due-today open tickets
/// 3. Filter out done/in_progress/blocked
/// 4. Group by organization
/// 5. Create nightly_run + nightly_run_tickets records
/// 6. Hand off to orchestrator dispatch (ticket #4)
pub async fn run_nightly_cycle(db_pool: &SqlitePool) -> anyhow::Result<()> {
    let today = Local::now().format("%Y-%m-%d").to_string();

    // Double-run prevention: UNIQUE(run_date) will reject duplicates,
    // but check first to avoid noisy errors
    if let Some(existing) = nightly_runs::get_run_by_date(db_pool, &today).await? {
        tracing::info!(
            "[nightly] Run for {} already exists (id={}, status={}), skipping",
            today, existing.id, existing.status
        );
        return Ok(());
    }

    // Query all overdue + due-today tickets
    let tomorrow = {
        let today_date = chrono::NaiveDate::parse_from_str(&today, "%Y-%m-%d")?;
        (today_date + chrono::Duration::days(1)).format("%Y-%m-%d").to_string()
    };

    let all_tickets = tickets::list_tickets_by_due_date(
        db_pool,
        None,        // all orgs
        None,        // no lower bound (includes overdue)
        Some(&tomorrow), // up to end of today
        false,       // exclude done
    )
    .await?;

    // Filter: only open tickets (not in_progress, not blocked)
    let eligible: Vec<&Ticket> = all_tickets
        .iter()
        .filter(|t| t.status.as_str() == "open")
        .collect();

    if eligible.is_empty() {
        tracing::info!("[nightly] No eligible tickets for {}", today);
        // Still create a run record so we don't re-trigger on catch-up
        nightly_runs::create_run(db_pool, &today, 0).await?;
        // Mark immediately completed
        let run = nightly_runs::get_run_by_date(db_pool, &today).await?.unwrap();
        nightly_runs::update_run_status(db_pool, run.id, "completed", 0, 0).await?;
        return Ok(());
    }

    tracing::info!(
        "[nightly] Found {} eligible ticket(s) for {}",
        eligible.len(),
        today
    );

    // Create the nightly run record
    let run = nightly_runs::create_run(db_pool, &today, eligible.len() as i64).await?;

    // Create run ticket records
    for ticket in &eligible {
        let classification = ticket
            .classification
            .as_deref()
            .unwrap_or("automated");

        if let Err(e) = nightly_runs::create_run_ticket(
            db_pool,
            run.id,
            &ticket.ticket_id,
            &ticket.organization,
            classification,
        )
        .await
        {
            tracing::error!(
                "[nightly] Failed to create run ticket for {}: {}",
                ticket.ticket_id, e
            );
        }
    }

    // Group by organization
    let org_groups = group_by_org(&eligible);

    tracing::info!(
        "[nightly] Grouped into {} organization(s): {}",
        org_groups.len(),
        org_groups
            .iter()
            .map(|g| format!("{}({})", g.organization, g.tickets.len()))
            .collect::<Vec<_>>()
            .join(", ")
    );

    // Dispatch to orchestrators (implemented in ticket #4)
    // For now, log and mark the run. The orchestrator dispatch will be
    // called here once T-A6C15CBB is complete.
    run_orchestrator_dispatch(db_pool, &run, org_groups).await?;

    Ok(())
}

/// Group eligible tickets by organization.
fn group_by_org(tickets: &[&Ticket]) -> Vec<OrgTicketGroup> {
    let mut map: HashMap<String, Vec<SchedulerTicket>> = HashMap::new();

    for ticket in tickets {
        let st = SchedulerTicket::from(*ticket);
        map.entry(ticket.organization.clone())
            .or_default()
            .push(st);
    }

    map.into_iter()
        .map(|(org, tickets)| OrgTicketGroup {
            organization: org,
            tickets,
        })
        .collect()
}

/// Orchestrator execution plan — batches of ticket IDs per org.
#[derive(Debug)]
pub struct OrgExecutionPlan {
    pub organization: String,
    pub batches: Vec<Vec<String>>,
    pub tickets: Vec<SchedulerTicket>,
}

// ---------------------------------------------------------------------------
// Orchestrator dispatch
// ---------------------------------------------------------------------------

/// Run per-org orchestrator agents in parallel. Each receives its org's ticket
/// data and outputs a batched execution plan as structured JSON.
async fn run_orchestrator_dispatch(
    db_pool: &SqlitePool,
    run: &NightlyRun,
    org_groups: Vec<OrgTicketGroup>,
) -> anyhow::Result<()> {
    tracing::info!(
        "[nightly] Dispatching orchestrators for {} org(s), run_id={}",
        org_groups.len(),
        run.id
    );

    // Fire all org orchestrators in parallel
    let mut join_set = tokio::task::JoinSet::new();

    for group in org_groups {
        let pool = db_pool.clone();
        let run_id = run.id;
        join_set.spawn(async move {
            run_org_orchestrator(&pool, run_id, group).await
        });
    }

    // Collect results
    let mut all_plans = Vec::new();

    while let Some(result) = join_set.join_next().await {
        match result {
            Ok(Ok(plan)) => {
                tracing::info!(
                    "[nightly] Orchestrator for '{}' produced {} batch(es)",
                    plan.organization,
                    plan.batches.len()
                );
                all_plans.push(plan);
            }
            Ok(Err(e)) => {
                tracing::error!("[nightly] Orchestrator failed: {}", e);
            }
            Err(e) => {
                tracing::error!("[nightly] Orchestrator task panicked: {}", e);
            }
        }
    }

    if all_plans.is_empty() {
        tracing::warn!("[nightly] All orchestrators failed, marking run as failed");
        nightly_runs::update_run_status(db_pool, run.id, "failed", 0, 0).await?;
        return Ok(());
    }

    // Hand off to batch execution engine (T-E350DA66)
    run_batch_execution(db_pool, run, all_plans).await?;

    Ok(())
}

/// Run a single org's orchestrator: format ticket data, call Opus, parse JSON.
async fn run_org_orchestrator(
    db_pool: &SqlitePool,
    run_id: i64,
    group: OrgTicketGroup,
) -> anyhow::Result<OrgExecutionPlan> {
    let org = &group.organization;
    tracing::info!(
        "[nightly] Running orchestrator for '{}' with {} ticket(s)",
        org,
        group.tickets.len()
    );

    // Format ticket data for the prompt
    let tickets_text = format_tickets_for_orchestrator(&group.tickets);

    // Load prompt template
    let mut vars = HashMap::new();
    vars.insert("organization".to_string(), org.clone());
    vars.insert("tickets".to_string(), tickets_text);

    let system_prompt = load_prompt("nightly-orchestrator", vars)?;

    // Call Opus via cc-sdk (no tools, single turn)
    let options = ClaudeCodeOptions::builder()
        .system_prompt(&system_prompt)
        .model("claude-opus-4-6")
        .tools(ToolsConfig::none())
        .max_turns(1)
        .permission_mode(PermissionMode::BypassPermissions)
        .cwd(std::path::Path::new("/tmp"))
        .build();

    let mut sdk_client = ClaudeSDKClient::new(options);
    sdk_client.connect(None).await
        .map_err(|e| anyhow::anyhow!("Failed to connect orchestrator for {}: {}", org, e))?;

    let user_msg = format!(
        "Analyze these {} tickets and produce the batched execution plan.",
        group.tickets.len()
    );
    sdk_client.send_user_message(user_msg).await
        .map_err(|e| anyhow::anyhow!("Failed to send to orchestrator for {}: {}", org, e))?;

    let mut response_stream = sdk_client.receive_messages().await;
    let mut output_parts = Vec::new();

    while let Some(msg_result) = response_stream.next().await {
        match msg_result {
            Ok(Message::Assistant { message: assistant_msg }) => {
                for block in &assistant_msg.content {
                    if let ContentBlock::Text(text) = block {
                        output_parts.push(text.text.clone());
                    }
                }
            }
            Ok(Message::Result { .. }) => break,
            Err(e) => {
                tracing::error!("[nightly] Orchestrator stream error for {}: {}", org, e);
                break;
            }
            _ => {}
        }
    }

    let raw_output = output_parts.join("");
    tracing::debug!("[nightly] Orchestrator raw output for {}: {}", org, &raw_output[..raw_output.len().min(500)]);

    // Parse JSON output
    let batches = parse_orchestrator_output(&raw_output, &group.tickets)?;

    // Update nightly_run_tickets with batch numbers
    for (batch_idx, batch) in batches.iter().enumerate() {
        for ticket_id in batch {
            if let Err(e) = nightly_runs::set_batch_number(
                db_pool,
                run_id,
                ticket_id,
                (batch_idx + 1) as i64,
            )
            .await
            {
                tracing::error!(
                    "[nightly] Failed to set batch number for {}: {}",
                    ticket_id, e
                );
            }
        }
    }

    tracing::info!(
        "[nightly] Orchestrator for '{}' complete: {} batch(es) with {} total ticket(s)",
        org,
        batches.len(),
        batches.iter().map(|b| b.len()).sum::<usize>()
    );

    Ok(OrgExecutionPlan {
        organization: org.clone(),
        batches,
        tickets: group.tickets,
    })
}

/// Format ticket data as structured text for the orchestrator prompt.
fn format_tickets_for_orchestrator(tickets: &[SchedulerTicket]) -> String {
    let mut lines = Vec::new();

    for t in tickets {
        lines.push(format!("### {} — {}", t.ticket_id, t.title));
        lines.push(format!("- Classification: {}", t.classification));
        if let Some(ref repo) = t.repository {
            lines.push(format!("- Repository: {}", repo));
        } else {
            lines.push("- Repository: none (research only)".to_string());
        }
        if !t.blocked_by.is_empty() {
            lines.push(format!("- Blocked by: {}", t.blocked_by.join(", ")));
        }
        if let Some(ref desc) = t.description {
            let short = if desc.len() > 200 { &desc[..200] } else { desc };
            lines.push(format!("- Description: {}", short));
        }
        lines.push(String::new());
    }

    lines.join("\n")
}

/// Parse orchestrator JSON output. Falls back to all-in-one-batch on failure.
fn parse_orchestrator_output(
    raw: &str,
    tickets: &[SchedulerTicket],
) -> anyhow::Result<Vec<Vec<String>>> {
    // Try to extract JSON from the response (may be wrapped in markdown fences)
    let json_str = extract_json(raw);

    match serde_json::from_str::<serde_json::Value>(&json_str) {
        Ok(val) => {
            if let Some(batches_arr) = val.get("batches").and_then(|b| b.as_array()) {
                let mut batches = Vec::new();
                for batch in batches_arr {
                    if let Some(ids) = batch.as_array() {
                        let ticket_ids: Vec<String> = ids
                            .iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect();
                        if !ticket_ids.is_empty() {
                            batches.push(ticket_ids);
                        }
                    }
                }
                if batches.is_empty() {
                    tracing::warn!("[nightly] Orchestrator returned empty batches, falling back");
                    Ok(fallback_single_batch(tickets))
                } else {
                    Ok(batches)
                }
            } else {
                tracing::warn!("[nightly] Orchestrator JSON missing 'batches' key, falling back");
                Ok(fallback_single_batch(tickets))
            }
        }
        Err(e) => {
            tracing::warn!(
                "[nightly] Failed to parse orchestrator JSON ({}), falling back to single batch",
                e
            );
            Ok(fallback_single_batch(tickets))
        }
    }
}

/// Extract JSON from a response that may include markdown fences or preamble.
fn extract_json(raw: &str) -> String {
    let trimmed = raw.trim();

    // Try direct parse first
    if trimmed.starts_with('{') {
        return trimmed.to_string();
    }

    // Try extracting from ```json ... ``` fences
    if let Some(start) = trimmed.find("```json") {
        let after_fence = &trimmed[start + 7..];
        if let Some(end) = after_fence.find("```") {
            return after_fence[..end].trim().to_string();
        }
    }

    // Try extracting from ``` ... ``` fences
    if let Some(start) = trimmed.find("```") {
        let after_fence = &trimmed[start + 3..];
        if let Some(end) = after_fence.find("```") {
            let inner = after_fence[..end].trim();
            if inner.starts_with('{') {
                return inner.to_string();
            }
        }
    }

    // Try finding first { to last }
    if let Some(start) = trimmed.find('{') {
        if let Some(end) = trimmed.rfind('}') {
            if end > start {
                return trimmed[start..=end].to_string();
            }
        }
    }

    trimmed.to_string()
}

/// Fallback: put all tickets in a single batch.
fn fallback_single_batch(tickets: &[SchedulerTicket]) -> Vec<Vec<String>> {
    vec![tickets.iter().map(|t| t.ticket_id.clone()).collect()]
}

// ---------------------------------------------------------------------------
// Batch execution placeholder (T-E350DA66)
// ---------------------------------------------------------------------------

/// Placeholder for batch execution engine. Will be replaced by T-E350DA66.
async fn run_batch_execution(
    db_pool: &SqlitePool,
    run: &NightlyRun,
    plans: Vec<OrgExecutionPlan>,
) -> anyhow::Result<()> {
    let total_tickets: usize = plans.iter().map(|p| p.batches.iter().map(|b| b.len()).sum::<usize>()).sum();
    tracing::info!(
        "[nightly] Batch execution placeholder — {} plan(s), {} total ticket(s), run_id={}",
        plans.len(),
        total_tickets,
        run.id
    );

    // TODO(T-E350DA66): Replace with actual batch execution engine.
    // For now, mark the run as completed.
    nightly_runs::update_run_status(db_pool, run.id, "completed", 0, 0).await?;

    Ok(())
}
