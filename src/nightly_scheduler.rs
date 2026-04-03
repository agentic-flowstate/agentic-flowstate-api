use std::collections::HashMap;
use std::sync::Arc;
use chrono::{Local, NaiveTime};
use ticketing_system::{nightly_runs, tickets, Ticket, SqlitePool};
use tokio_util::sync::CancellationToken;

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

/// Placeholder for orchestrator dispatch (T-A6C15CBB).
/// Will be replaced with actual per-org Opus orchestrator calls.
async fn run_orchestrator_dispatch(
    db_pool: &SqlitePool,
    run: &ticketing_system::NightlyRun,
    org_groups: Vec<OrgTicketGroup>,
) -> anyhow::Result<()> {
    tracing::info!(
        "[nightly] Orchestrator dispatch placeholder — {} org group(s), run_id={}",
        org_groups.len(),
        run.id
    );

    // TODO(T-A6C15CBB): Replace with actual orchestrator + execution pipeline.
    // For now, mark the run as completed so the scheduler doesn't re-trigger.
    nightly_runs::update_run_status(db_pool, run.id, "completed", 0, 0).await?;

    Ok(())
}
