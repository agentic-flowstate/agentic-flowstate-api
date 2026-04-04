use std::collections::HashMap;
use std::sync::Arc;
use chrono::{Local, NaiveTime};
use cc_sdk::{ClaudeSDKClient, ClaudeCodeOptions, Message, ContentBlock, ToolsConfig, PermissionMode, McpServerConfig};
use futures::StreamExt;
use ticketing_system::{
    nightly_runs, tickets, conversations,
    NightlyRun, Ticket, CreateConversationRequest, AddMessageRequest, SqlitePool,
};
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
/// 5. Create nightly_run record (for double-run prevention)
/// 6. Hand off to orchestrator dispatch
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
// Batch execution engine
// ---------------------------------------------------------------------------

/// Global concurrency limiter: max 5 ticket pipelines running simultaneously.
static PIPELINE_SEMAPHORE: once_cell::sync::Lazy<tokio::sync::Semaphore> =
    once_cell::sync::Lazy::new(|| tokio::sync::Semaphore::new(5));

/// Execute all org plans: batches run sequentially, tickets within a batch
/// run in parallel (up to the global semaphore limit).
async fn run_batch_execution(
    db_pool: &SqlitePool,
    run: &NightlyRun,
    plans: Vec<OrgExecutionPlan>,
) -> anyhow::Result<()> {
    let total_tickets: usize = plans
        .iter()
        .map(|p| p.batches.iter().map(|b| b.len()).sum::<usize>())
        .sum();
    tracing::info!(
        "[nightly] Starting batch execution — {} plan(s), {} total ticket(s), run_id={}",
        plans.len(),
        total_tickets,
        run.id
    );

    // Build a lookup map: ticket_id -> SchedulerTicket
    let mut ticket_map: HashMap<String, SchedulerTicket> = HashMap::new();
    for plan in &plans {
        for t in &plan.tickets {
            ticket_map.insert(t.ticket_id.clone(), t.clone());
        }
    }

    // Merge all org plans into a unified batch sequence.
    // Find the max batch count across all orgs, then for each batch level,
    // merge all org batches at that level.
    let max_batches = plans.iter().map(|p| p.batches.len()).max().unwrap_or(0);

    let mut completed_count: i64 = 0;
    let mut failed_count: i64 = 0;

    for batch_idx in 0..max_batches {
        // Collect all ticket IDs across all orgs at this batch level
        let mut batch_ticket_ids: Vec<String> = Vec::new();
        for plan in &plans {
            if let Some(batch) = plan.batches.get(batch_idx) {
                batch_ticket_ids.extend(batch.clone());
            }
        }

        if batch_ticket_ids.is_empty() {
            continue;
        }

        tracing::info!(
            "[nightly] Executing batch {} with {} ticket(s)",
            batch_idx + 1,
            batch_ticket_ids.len()
        );

        // Run all tickets in this batch in parallel (up to semaphore limit)
        let mut join_set = tokio::task::JoinSet::new();

        for ticket_id in batch_ticket_ids {
            let pool = db_pool.clone();
            let run_id = run.id;
            let ticket = match ticket_map.get(&ticket_id) {
                Some(t) => t.clone(),
                None => {
                    tracing::error!("[nightly] Ticket {} not found in lookup map", ticket_id);
                    continue;
                }
            };

            join_set.spawn(async move {
                // Acquire semaphore permit
                let _permit = PIPELINE_SEMAPHORE.acquire().await.unwrap();
                run_ticket_pipeline(&pool, run_id, &ticket).await
            });
        }

        // Wait for all tickets in this batch to complete
        while let Some(result) = join_set.join_next().await {
            match result {
                Ok(Ok(true)) => completed_count += 1,
                Ok(Ok(false)) => failed_count += 1,
                Ok(Err(e)) => {
                    tracing::error!("[nightly] Pipeline error: {}", e);
                    failed_count += 1;
                }
                Err(e) => {
                    tracing::error!("[nightly] Pipeline task panicked: {}", e);
                    failed_count += 1;
                }
            }
        }

        tracing::info!(
            "[nightly] Batch {} complete. Running totals: {} completed, {} failed",
            batch_idx + 1,
            completed_count,
            failed_count
        );
    }

    // Finalize the run
    let status = if failed_count == 0 { "completed" } else { "completed_with_failures" };
    nightly_runs::update_run_status(db_pool, run.id, status, completed_count, failed_count).await?;

    tracing::info!(
        "[nightly] Run {} finished: {} completed, {} failed",
        run.id,
        completed_count,
        failed_count
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Per-ticket pipeline
// ---------------------------------------------------------------------------

/// Run the 3-agent pipeline for a single ticket.
/// Creates a conversation visible in the chat tab with real agent output.
/// Returns Ok(true) on success, Ok(false) on failure.
async fn run_ticket_pipeline(
    db_pool: &SqlitePool,
    run_id: i64,
    ticket: &SchedulerTicket,
) -> anyhow::Result<bool> {
    tracing::info!(
        "[nightly] Starting pipeline for {} — {}",
        ticket.ticket_id,
        ticket.title
    );

    // Create conversation so pipeline is visible in chat tab
    let conv = conversations::create_conversation(db_pool, CreateConversationRequest {
        user_id: "alex".to_string(),
        organization: ticket.organization.clone(),
        title: ticket.title.clone(),
        session_id: None,
        agent: Some("nightly-scheduler".to_string()),
    }).await?;

    // Link conversation to ticket
    conversations::set_router_result(
        db_pool, &conv.id,
        Some(&ticket.ticket_id),
        Some(&ticket.organization),
    ).await?;

    // Add user message with ticket context
    let user_content = format!(
        "Nightly pipeline — {}\n\n**Ticket:** {}\n**Organization:** {}\n**Epic:** {} / **Slice:** {}\n**Classification:** {}\n**Repository:** {}\n\n{}",
        ticket.title, ticket.ticket_id, ticket.organization,
        ticket.epic_id, ticket.slice_id, ticket.classification,
        ticket.repository.as_deref().unwrap_or("none"),
        ticket.description.as_deref().unwrap_or("(no description)")
    );
    conversations::add_message(db_pool, &conv.id, AddMessageRequest {
        role: "user".to_string(),
        content: user_content,
        attachments: None,
    }).await?;

    // Create assistant message placeholder — updated as each agent completes
    let assistant_msg = conversations::add_message(db_pool, &conv.id, AddMessageRequest {
        role: "assistant".to_string(),
        content: "Starting nightly pipeline...".to_string(),
        attachments: None,
    }).await?;

    let mut output_sections: Vec<String> = Vec::new();

    // Helper closure to update the assistant message with accumulated output
    let update_output = |db: &SqlitePool, msg_id: &str, sections: &[String]| {
        let content = sections.join("\n\n---\n\n");
        let db = db.clone();
        let msg_id = msg_id.to_string();
        async move {
            let _ = conversations::update_message(&db, &msg_id, &content).await;
        }
    };

    // Re-check ticket status before starting (may have changed since orchestrator)
    if let Some(current) = tickets::get_ticket_by_id(db_pool, &ticket.ticket_id).await? {
        if current.status.as_str() != "open" {
            tracing::info!(
                "[nightly] Ticket {} is no longer open (status: {}), skipping",
                ticket.ticket_id,
                current.status
            );
            output_sections.push(format!(
                "Skipped — ticket status changed to **{}** before pipeline started.",
                current.status
            ));
            update_output(db_pool, &assistant_msg.id, &output_sections).await;
            conversations::archive_conversation(db_pool, "alex", &conv.id).await?;
            return Ok(true);
        }
    }

    // Move ticket to in_progress
    if let Err(e) = tickets::update_ticket_status(
        db_pool, &ticket.organization, &ticket.epic_id,
        &ticket.slice_id, &ticket.ticket_id, "in_progress",
    ).await {
        tracing::warn!("[nightly] Failed to set {} to in_progress: {}", ticket.ticket_id, e);
    }

    // Set up worktree if ticket has a repository
    let (worktree_path, branch_name, repo_path) = if let Some(ref repo_name) = ticket.repository {
        match setup_worktree(db_pool, &ticket.ticket_id, repo_name).await {
            Ok((wt, br, rp)) => (Some(wt), Some(br), Some(rp)),
            Err(e) => {
                tracing::error!(
                    "[nightly] Failed to create worktree for {}: {}",
                    ticket.ticket_id, e
                );
                output_sections.push(format!("## Pipeline Failed\n\nWorktree creation failed: {}", e));
                update_output(db_pool, &assistant_msg.id, &output_sections).await;
                nightly_runs::increment_failed(db_pool, run_id).await?;
                return Ok(false);
            }
        }
    } else {
        (None, None, None)
    };

    let working_dir = worktree_path
        .as_deref()
        .map(std::path::Path::new)
        .unwrap_or(std::path::Path::new("/Users/jarvisgpt/projects"));

    // ---- Agent 1: Codebase Research ----
    let codebase_output = match run_codebase_research(ticket, working_dir).await {
        Ok(output) => {
            tracing::info!(
                "[nightly] Codebase research complete for {} ({} chars)",
                ticket.ticket_id,
                output.len()
            );
            output_sections.push(format!("## Codebase Research\n\n{}", output));
            update_output(db_pool, &assistant_msg.id, &output_sections).await;
            Some(output)
        }
        Err(e) => {
            tracing::error!(
                "[nightly] Codebase research failed for {}: {}",
                ticket.ticket_id, e
            );
            output_sections.push(format!("## Codebase Research\n\nFailed: {}", e));
            update_output(db_pool, &assistant_msg.id, &output_sections).await;
            None
        }
    };

    // ---- Agent 2: Exa Research ----
    let exa_output = match run_exa_research(ticket, working_dir, codebase_output.as_deref()).await {
        Ok(output) => {
            tracing::info!(
                "[nightly] Exa research complete for {} ({} chars)",
                ticket.ticket_id,
                output.len()
            );
            output_sections.push(format!("## Web Research\n\n{}", output));
            update_output(db_pool, &assistant_msg.id, &output_sections).await;
            Some(output)
        }
        Err(e) => {
            tracing::error!(
                "[nightly] Exa research failed for {}: {}",
                ticket.ticket_id, e
            );
            output_sections.push(format!("## Web Research\n\nFailed: {}", e));
            update_output(db_pool, &assistant_msg.id, &output_sections).await;
            None
        }
    };

    // ---- Agent 3: Full-Access Execution ----
    let execution_result = run_full_access_execution(
        ticket,
        working_dir,
        worktree_path.as_deref(),
        branch_name.as_deref(),
        codebase_output.as_deref(),
        exa_output.as_deref(),
    )
    .await;

    // Determine outcome and update conversation
    let success = match &execution_result {
        Ok(output) => {
            tracing::info!("[nightly] Execution complete for {}", ticket.ticket_id);
            output_sections.push(format!("## Execution\n\n{}", output));
            update_output(db_pool, &assistant_msg.id, &output_sections).await;
            true
        }
        Err(e) => {
            tracing::error!(
                "[nightly] Execution failed for {}: {}",
                ticket.ticket_id, e
            );
            output_sections.push(format!("## Execution\n\nFailed: {}", e));
            update_output(db_pool, &assistant_msg.id, &output_sections).await;
            false
        }
    };

    // Cleanup worktree
    if let (Some(ref wt_path), Some(ref rp)) = (&worktree_path, &repo_path) {
        cleanup_worktree(rp, wt_path).await;
    }

    // Archive conversation on success, leave open on failure
    if success {
        conversations::archive_conversation(db_pool, "alex", &conv.id).await?;
        nightly_runs::increment_completed(db_pool, run_id).await?;
    } else {
        nightly_runs::increment_failed(db_pool, run_id).await?;
    }

    Ok(success)
}

// ---------------------------------------------------------------------------
// Worktree management
// ---------------------------------------------------------------------------

/// Create a git worktree for isolated pipeline execution.
/// Returns (worktree_path, branch_name, repo_path).
async fn setup_worktree(
    db_pool: &SqlitePool,
    ticket_id: &str,
    repo_name: &str,
) -> anyhow::Result<(String, String, String)> {
    let repo = ticketing_system::repositories::get_repository(db_pool, repo_name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Repository '{}' not found in registry", repo_name))?;

    let repo_path = repo
        .local_path
        .ok_or_else(|| anyhow::anyhow!("Repository '{}' has no local_path", repo_name))?;

    let branch_name = format!("nightly/{}", ticket_id);
    let worktree_path = format!("/tmp/nightly/{}", ticket_id);

    // Clean up any stale worktree from a previous failed run
    if std::path::Path::new(&worktree_path).exists() {
        tracing::warn!("[nightly] Stale worktree found at {}, removing", worktree_path);
        let _ = tokio::process::Command::new("git")
            .args(["worktree", "remove", "--force", &worktree_path])
            .current_dir(&repo_path)
            .output()
            .await;
        // Also clean up the stale branch if it exists
        let _ = tokio::process::Command::new("git")
            .args(["branch", "-D", &branch_name])
            .current_dir(&repo_path)
            .output()
            .await;
    }

    // Create the worktree
    let output = tokio::process::Command::new("git")
        .args(["worktree", "add", "-b", &branch_name, &worktree_path])
        .current_dir(&repo_path)
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git worktree add failed: {}", stderr);
    }

    tracing::info!(
        "[nightly] Created worktree at {} on branch {}",
        worktree_path,
        branch_name
    );

    Ok((worktree_path, branch_name, repo_path))
}

/// Cleanup a worktree after pipeline execution.
/// Checks for commits first — keeps the branch if there are changes.
async fn cleanup_worktree(repo_path: &str, worktree_path: &str) {
    // Check if there are any commits on the worktree branch beyond main
    let has_commits = tokio::process::Command::new("git")
        .args(["log", "main..HEAD", "--oneline"])
        .current_dir(worktree_path)
        .output()
        .await
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    if has_commits {
        tracing::info!(
            "[nightly] Worktree {} has commits, keeping branch for review",
            worktree_path
        );
        // Remove the worktree directory but keep the branch
        let _ = tokio::process::Command::new("git")
            .args(["worktree", "remove", worktree_path])
            .current_dir(repo_path)
            .output()
            .await;
    } else {
        tracing::info!(
            "[nightly] Worktree {} has no commits, cleaning up",
            worktree_path
        );
        // Remove worktree and delete the branch
        let _ = tokio::process::Command::new("git")
            .args(["worktree", "remove", "--force", worktree_path])
            .current_dir(repo_path)
            .output()
            .await;
        // Extract branch name from worktree path
        if let Some(ticket_id) = worktree_path.split('/').last() {
            let branch = format!("nightly/{}", ticket_id);
            let _ = tokio::process::Command::new("git")
                .args(["branch", "-D", &branch])
                .current_dir(repo_path)
                .output()
                .await;
        }
    }
}

// ---------------------------------------------------------------------------
// Agent pipeline stages
// ---------------------------------------------------------------------------

/// MCP binary path for agents that need MCP tools.
fn mcp_binary_path() -> std::path::PathBuf {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../agentic-flowstate-mcp/target/release/agentic_mcp");
    path.canonicalize().unwrap_or(path)
}

/// Agent 1: Codebase Research — explores the repo and produces structured findings.
async fn run_codebase_research(
    ticket: &SchedulerTicket,
    working_dir: &std::path::Path,
) -> anyhow::Result<String> {
    let mut vars = HashMap::new();
    vars.insert("ticket_id".to_string(), ticket.ticket_id.clone());
    vars.insert("ticket_title".to_string(), ticket.title.clone());
    vars.insert("organization".to_string(), ticket.organization.clone());
    vars.insert("classification".to_string(), ticket.classification.clone());
    if let Some(ref desc) = ticket.description {
        vars.insert("ticket_description".to_string(), desc.clone());
    }

    let system_prompt = load_prompt("nightly-codebase-research", vars)?;

    let tools: Vec<String> = vec![
        "Bash".into(), "Read".into(), "Glob".into(), "Grep".into(),
    ];

    let options = ClaudeCodeOptions::builder()
        .system_prompt(&system_prompt)
        .model("claude-opus-4-6")
        .tools(ToolsConfig::list(tools.clone()))
        .allowed_tools(tools)
        .permission_mode(PermissionMode::BypassPermissions)
        .cwd(working_dir)
        .add_extra_arg("effort", Some("high".to_string()))
        .build();

    run_agent_to_completion("codebase-research", &ticket.ticket_id, options).await
}

/// Agent 2: Exa Research — augments codebase findings with web research.
async fn run_exa_research(
    ticket: &SchedulerTicket,
    working_dir: &std::path::Path,
    codebase_output: Option<&str>,
) -> anyhow::Result<String> {
    let mut vars = HashMap::new();
    vars.insert("ticket_id".to_string(), ticket.ticket_id.clone());
    vars.insert("ticket_title".to_string(), ticket.title.clone());
    vars.insert("organization".to_string(), ticket.organization.clone());
    vars.insert("classification".to_string(), ticket.classification.clone());
    if let Some(ref desc) = ticket.description {
        vars.insert("ticket_description".to_string(), desc.clone());
    }
    if let Some(codebase) = codebase_output {
        vars.insert("codebase_research".to_string(), codebase.to_string());
    }

    let system_prompt = load_prompt("nightly-exa-research", vars)?;

    let tools: Vec<String> = vec![
        "mcp__agentic-mcp__exa_search".into(),
        "mcp__agentic-mcp__exa_get_contents".into(),
        "Read".into(),
        "Glob".into(),
        "Grep".into(),
    ];

    let mcp_binary = mcp_binary_path();
    let options = ClaudeCodeOptions::builder()
        .system_prompt(&system_prompt)
        .model("claude-opus-4-6")
        .tools(ToolsConfig::list(tools.clone()))
        .allowed_tools(tools)
        .permission_mode(PermissionMode::BypassPermissions)
        .cwd(working_dir)
        .add_mcp_server(
            "agentic-mcp",
            McpServerConfig::Stdio {
                command: mcp_binary.to_string_lossy().to_string(),
                args: None,
                env: None,
            },
        )
        .add_extra_arg("effort", Some("high".to_string()))
        .build();

    run_agent_to_completion("exa-research", &ticket.ticket_id, options).await
}

/// Agent 3: Full-Access Execution — does the work with all tools and CLAUDE.md context.
async fn run_full_access_execution(
    ticket: &SchedulerTicket,
    working_dir: &std::path::Path,
    worktree_path: Option<&str>,
    branch_name: Option<&str>,
    codebase_output: Option<&str>,
    exa_output: Option<&str>,
) -> anyhow::Result<String> {
    // Load CLAUDE.md
    let claude_md = std::fs::read_to_string("/Users/jarvisgpt/projects/CLAUDE.md")
        .unwrap_or_else(|e| format!("(Failed to read CLAUDE.md: {})", e));

    // Build nightly context injection
    let mut context_vars = HashMap::new();
    context_vars.insert("branch_name".to_string(),
        branch_name.unwrap_or("(no branch)").to_string());
    context_vars.insert("repo_name".to_string(),
        ticket.repository.clone().unwrap_or_else(|| "(no repo)".to_string()));
    context_vars.insert("worktree_path".to_string(),
        worktree_path.unwrap_or("(no worktree)").to_string());
    context_vars.insert("ticket_id".to_string(), ticket.ticket_id.clone());
    context_vars.insert("ticket_title".to_string(), ticket.title.clone());
    context_vars.insert("organization".to_string(), ticket.organization.clone());
    context_vars.insert("epic_id".to_string(), ticket.epic_id.clone());
    context_vars.insert("slice_id".to_string(), ticket.slice_id.clone());
    context_vars.insert("classification".to_string(), ticket.classification.clone());
    if let Some(ref desc) = ticket.description {
        context_vars.insert("ticket_description".to_string(), desc.clone());
    }
    if let Some(codebase) = codebase_output {
        context_vars.insert("codebase_research".to_string(), codebase.to_string());
    }
    if let Some(exa) = exa_output {
        context_vars.insert("exa_research".to_string(), exa.to_string());
    }
    if ticket.classification == "manual" {
        context_vars.insert("manual_ticket".to_string(), "true".to_string());
    }

    let nightly_context = load_prompt("nightly-context", context_vars)?;

    // Build the full-access system prompt: CLAUDE.MD + nightly context
    let mut full_access_vars = HashMap::new();
    full_access_vars.insert("claude_md".to_string(), claude_md);
    full_access_vars.insert("context".to_string(), nightly_context);

    let system_prompt = load_prompt("full-access", full_access_vars)?;

    // Full-access tools
    let tools: Vec<String> = vec![
        "Read".into(), "Write".into(), "Edit".into(), "Bash".into(),
        "Glob".into(), "Grep".into(), "Task".into(),
        "mcp__agentic-mcp__*".into(),
    ];

    let mcp_binary = mcp_binary_path();
    let options = ClaudeCodeOptions::builder()
        .system_prompt(&system_prompt)
        .model("claude-opus-4-6")
        .tools(ToolsConfig::list(tools.clone()))
        .allowed_tools(tools)
        .permission_mode(PermissionMode::BypassPermissions)
        .cwd(working_dir)
        .add_mcp_server(
            "agentic-mcp",
            McpServerConfig::Stdio {
                command: mcp_binary.to_string_lossy().to_string(),
                args: None,
                env: None,
            },
        )
        .add_extra_arg("effort", Some("max".to_string()))
        .build();

    let user_msg = format!(
        "Execute ticket {} — {}.\n\nClassification: {}\n\nWork on this ticket now. Follow the research findings provided in your context.",
        ticket.ticket_id,
        ticket.title,
        ticket.classification,
    );

    run_agent_to_completion_with_message("full-access", &ticket.ticket_id, options, &user_msg).await
}

/// Generic agent runner: connect, send default message, stream to completion, return text.
async fn run_agent_to_completion(
    agent_name: &str,
    ticket_id: &str,
    options: ClaudeCodeOptions,
) -> anyhow::Result<String> {
    let msg = format!("Analyze and research ticket {}. Follow your system prompt instructions.", ticket_id);
    run_agent_to_completion_with_message(agent_name, ticket_id, options, &msg).await
}

/// Generic agent runner with custom user message.
async fn run_agent_to_completion_with_message(
    agent_name: &str,
    ticket_id: &str,
    options: ClaudeCodeOptions,
    user_message: &str,
) -> anyhow::Result<String> {
    tracing::info!("[nightly] Starting {} agent for {}", agent_name, ticket_id);

    let mut sdk_client = ClaudeSDKClient::new(options);
    sdk_client.connect(None).await
        .map_err(|e| anyhow::anyhow!("Failed to connect {} agent for {}: {}", agent_name, ticket_id, e))?;

    sdk_client.send_user_message(user_message.to_string()).await
        .map_err(|e| anyhow::anyhow!("Failed to send to {} agent for {}: {}", agent_name, ticket_id, e))?;

    let mut response_stream = sdk_client.receive_messages().await;
    let mut output_parts = Vec::new();
    let mut tool_call_count = 0u32;

    while let Some(msg_result) = response_stream.next().await {
        match msg_result {
            Ok(Message::Assistant { message: assistant_msg }) => {
                for block in &assistant_msg.content {
                    match block {
                        ContentBlock::Text(text) => {
                            output_parts.push(text.text.clone());
                        }
                        ContentBlock::ToolUse(tool_use) => {
                            tool_call_count += 1;
                            if tool_call_count <= 5 || tool_call_count % 10 == 0 {
                                tracing::debug!(
                                    "[nightly] {} for {}: tool #{} — {}",
                                    agent_name, ticket_id, tool_call_count, tool_use.name
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }
            Ok(Message::Result { is_error, .. }) => {
                if is_error {
                    tracing::error!("[nightly] {} agent returned error for {}", agent_name, ticket_id);
                }
                break;
            }
            Err(e) => {
                tracing::error!("[nightly] {} stream error for {}: {}", agent_name, ticket_id, e);
                return Err(anyhow::anyhow!("{} stream error: {}", agent_name, e));
            }
            _ => {}
        }
    }

    let output = output_parts.join("");
    tracing::info!(
        "[nightly] {} agent for {} complete: {} chars, {} tool calls",
        agent_name,
        ticket_id,
        output.len(),
        tool_call_count
    );

    if output.is_empty() {
        Err(anyhow::anyhow!("No output from {} agent for {}", agent_name, ticket_id))
    } else {
        Ok(output)
    }
}

