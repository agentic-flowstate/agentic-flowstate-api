use anyhow::{bail, Context, Result};
use chrono::Utc;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use ticketing_system::text_normalization::{
    normalize_daily_research_output, split_daily_research_output, DailyResearchSections,
};
use tokio_util::sync::CancellationToken;

use crate::agents::executor::run_codex_agent_turn;
use crate::agents::prompts::load_prompt;
use crate::agents::working_dir::resolve_working_dir;
use crate::agents::AgentType;
use crate::package_updates::{self, PackageUpdateScanReport};

const POLL_SECONDS: u64 = 60;

pub fn spawn_dailies_scheduler(pool: Arc<SqlitePool>, token: CancellationToken) {
    tokio::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(POLL_SECONDS));

        loop {
            tokio::select! {
                _ = token.cancelled() => break,
                _ = interval.tick() => {}
            }

            if let Err(e) = run_due_once(pool.clone()).await {
                tracing::error!("[DAILIES] scheduler tick failed: {}", e);
            }
        }
    });
}

pub async fn run_due_once(pool: Arc<SqlitePool>) -> Result<()> {
    let now = Utc::now().timestamp();
    let expired = ticketing_system::dailies::complete_expired_dailies(&pool, now).await?;
    if expired > 0 {
        tracing::info!(
            "[DAILIES] completed {} expired daily automation(s)",
            expired
        );
    }

    let due = ticketing_system::dailies::due_dailies(&pool, now, 3).await?;
    for daily in due {
        if let Err(e) = spawn_daily_run(pool.clone(), daily, now).await {
            tracing::error!("[DAILIES] failed to spawn daily run: {}", e);
        }
    }

    Ok(())
}

pub async fn spawn_daily_run(
    pool: Arc<SqlitePool>,
    daily: ticketing_system::Daily,
    scheduled_for: i64,
) -> Result<ticketing_system::DailyRun> {
    let run = ticketing_system::dailies::start_run(&pool, &daily.daily_id, scheduled_for).await?;
    let run_clone = run.clone();
    tokio::spawn(async move {
        if let Err(e) = execute_daily_run(pool.clone(), daily.clone(), run_clone.clone()).await {
            tracing::error!(
                "[DAILIES] run {} for {} failed before result update: {}",
                run_clone.run_id,
                daily.daily_id,
                e
            );
            let _ = ticketing_system::dailies::complete_run(
                &pool,
                &run_clone.run_id,
                ticketing_system::DailyRunResult {
                    status: "failed".to_string(),
                    agent_run_id: None,
                    artifact_id: None,
                    summary: None,
                    lookup_summary: None,
                    sources_summary: None,
                    error: Some(e.to_string()),
                    silent: false,
                },
            )
            .await;
        }
    });

    Ok(run)
}

async fn execute_daily_run(
    pool: Arc<SqlitePool>,
    daily: ticketing_system::Daily,
    run: ticketing_system::DailyRun,
) -> Result<()> {
    if is_package_update_daily(&daily) {
        return execute_package_update_run(pool, daily, run).await;
    }

    let agent_type = agent_type_for_daily(&daily)?;
    let working_dir = resolve_daily_working_dir(&pool, &agent_type, &daily.organization).await?;
    let session_id = uuid::Uuid::new_v4().to_string();
    let prior_runs =
        ticketing_system::dailies::recent_completed_runs(&pool, &daily.daily_id, 3).await?;
    let prompt = build_run_prompt(&daily, &run, &prior_runs);
    let system_prompt = build_system_prompt(&daily)?;

    ticketing_system::agent_runs::create_agent_run(
        &pool,
        ticketing_system::CreateAgentRunRequest {
            session_id: session_id.clone(),
            organization: Some(daily.organization.clone()),
            epic_id: None,
            slice_id: None,
            ticket_id: None,
            agent_type: agent_type.as_str().to_string(),
            input_message: prompt.clone(),
        },
    )
    .await
    .context("Failed to create agent run for daily")?;

    ticketing_system::dailies::attach_agent_run(&pool, &run.run_id, &session_id)
        .await
        .context("Failed to attach daily run to agent run")?;

    tracing::info!(
        "[DAILIES] started run {} for {} with agent_run_id={}",
        run.run_id,
        daily.daily_id,
        session_id
    );

    let turn = run_codex_agent_turn(
        &agent_type,
        &working_dir,
        &system_prompt,
        &prompt,
        None,
        true,
        None,
        &session_id,
    )
    .await;

    match turn {
        Ok(turn) => {
            let output_summary = normalize_daily_research_output(&turn.output_summary);
            let sections = split_daily_research_output(&output_summary);
            let artifact_content = compose_daily_artifact(&sections);
            let completed_at = Utc::now().to_rfc3339();
            ticketing_system::agent_runs::update_agent_run(
                &pool,
                &ticketing_system::AgentRun {
                    session_id: session_id.clone(),
                    organization: Some(daily.organization.clone()),
                    epic_id: None,
                    slice_id: None,
                    ticket_id: None,
                    agent_type: agent_type.as_str().to_string(),
                    status: "completed".to_string(),
                    started_at: run_started_at(&run),
                    completed_at: Some(completed_at),
                    input_message: prompt.clone(),
                    output_summary: Some(output_summary.clone()),
                    tool_call_count: turn.tool_call_count,
                    cc_session_id: turn.runtime_session_id.clone(),
                },
            )
            .await
            .context("Failed to update completed daily agent run")?;

            tracing::info!(
                "[DAILIES] completed run {} for {} tool_calls={}",
                run.run_id,
                daily.daily_id,
                turn.tool_call_count
            );

            let artifact = ticketing_system::artifacts::create_artifact(
                &pool,
                ticketing_system::CreateArtifactRequest {
                    title: artifact_title(&daily),
                    content: artifact_content,
                    artifact_type: "research".to_string(),
                    created_by: "daily-research".to_string(),
                    source_step_id: Some(run.run_id.clone()),
                    organization: daily.organization.clone(),
                    epic_id: None,
                    slice_id: None,
                    ticket_id: None,
                    agent_run_id: Some(session_id.clone()),
                },
            )
            .await
            .context("Failed to create daily research artifact")?;

            ticketing_system::dailies::complete_run(
                &pool,
                &run.run_id,
                ticketing_system::DailyRunResult {
                    status: "completed".to_string(),
                    agent_run_id: Some(session_id),
                    artifact_id: Some(artifact.artifact_id),
                    summary: Some(sections.report),
                    lookup_summary: sections.lookup_summary,
                    sources_summary: sections.sources_summary,
                    error: None,
                    silent: false,
                },
            )
            .await?;
        }
        Err(e) => {
            let completed_at = Utc::now().to_rfc3339();
            let error = e.to_string();
            tracing::warn!(
                "[DAILIES] failed run {} for {}: {}",
                run.run_id,
                daily.daily_id,
                error
            );
            let _ = ticketing_system::agent_runs::update_agent_run(
                &pool,
                &ticketing_system::AgentRun {
                    session_id: session_id.clone(),
                    organization: Some(daily.organization.clone()),
                    epic_id: None,
                    slice_id: None,
                    ticket_id: None,
                    agent_type: agent_type.as_str().to_string(),
                    status: "failed".to_string(),
                    started_at: run_started_at(&run),
                    completed_at: Some(completed_at),
                    input_message: prompt,
                    output_summary: Some(error.clone()),
                    tool_call_count: 0,
                    cc_session_id: None,
                },
            )
            .await;

            ticketing_system::dailies::complete_run(
                &pool,
                &run.run_id,
                ticketing_system::DailyRunResult {
                    status: "failed".to_string(),
                    agent_run_id: Some(session_id),
                    artifact_id: None,
                    summary: None,
                    lookup_summary: None,
                    sources_summary: None,
                    error: Some(error),
                    silent: false,
                },
            )
            .await?;
        }
    }

    Ok(())
}

async fn execute_package_update_run(
    pool: Arc<SqlitePool>,
    daily: ticketing_system::Daily,
    run: ticketing_system::DailyRun,
) -> Result<()> {
    let report = package_updates::scan_available_updates()
        .await
        .context("Failed to scan package updates")?;
    let report_json =
        serde_json::to_string_pretty(&report).context("Failed to encode package update report")?;

    if report.updates.is_empty() {
        ticketing_system::dailies::complete_run(
            &pool,
            &run.run_id,
            ticketing_system::DailyRunResult {
                status: "completed".to_string(),
                agent_run_id: None,
                artifact_id: None,
                summary: Some("No package updates available.".to_string()),
                lookup_summary: Some(package_update_lookup_summary(&report)),
                sources_summary: None,
                error: None,
                silent: true,
            },
        )
        .await?;
        tracing::info!(
            "[DAILIES] package update run {} completed silently with no updates",
            run.run_id
        );
        return Ok(());
    }

    let review = ticketing_system::package_update_reviews::create_review(
        &pool,
        ticketing_system::CreatePackageUpdateReviewRequest {
            run_id: run.run_id.clone(),
            daily_id: daily.daily_id.clone(),
            update_count: report.updates.len() as i64,
            scanner_report_json: report_json.clone(),
        },
    )
    .await
    .context("Failed to create package update review")?;

    let agent_type = AgentType::PackageUpdateReview;
    let working_dir = resolve_daily_working_dir(&pool, &agent_type, &daily.organization).await?;
    let session_id = uuid::Uuid::new_v4().to_string();
    let prompt = build_package_update_prompt(&daily, &run, &review.review_id, &report_json);
    let system_prompt = load_prompt("package-update-review", HashMap::new())
        .context("Failed to load package update review prompt")?;

    ticketing_system::agent_runs::create_agent_run(
        &pool,
        ticketing_system::CreateAgentRunRequest {
            session_id: session_id.clone(),
            organization: Some(daily.organization.clone()),
            epic_id: None,
            slice_id: None,
            ticket_id: None,
            agent_type: agent_type.as_str().to_string(),
            input_message: prompt.clone(),
        },
    )
    .await
    .context("Failed to create package update review agent run")?;

    ticketing_system::dailies::attach_agent_run(&pool, &run.run_id, &session_id)
        .await
        .context("Failed to attach package update daily run to agent run")?;

    let turn = run_codex_agent_turn(
        &agent_type,
        &working_dir,
        &system_prompt,
        &prompt,
        None,
        true,
        None,
        &session_id,
    )
    .await;

    match turn {
        Ok(turn) => {
            let summary = turn.output_summary.trim().to_string();
            let completed_at = Utc::now().to_rfc3339();
            ticketing_system::agent_runs::update_agent_run(
                &pool,
                &ticketing_system::AgentRun {
                    session_id: session_id.clone(),
                    organization: Some(daily.organization.clone()),
                    epic_id: None,
                    slice_id: None,
                    ticket_id: None,
                    agent_type: agent_type.as_str().to_string(),
                    status: "completed".to_string(),
                    started_at: run_started_at(&run),
                    completed_at: Some(completed_at),
                    input_message: prompt.clone(),
                    output_summary: Some(summary.clone()),
                    tool_call_count: turn.tool_call_count,
                    cc_session_id: turn.runtime_session_id.clone(),
                },
            )
            .await
            .context("Failed to update completed package update agent run")?;

            ticketing_system::package_update_reviews::set_agent_summary(
                &pool,
                &review.review_id,
                &summary,
            )
            .await?;

            let artifact = ticketing_system::artifacts::create_artifact(
                &pool,
                ticketing_system::CreateArtifactRequest {
                    title: format!("Package update review - {}", Utc::now().format("%Y-%m-%d")),
                    content: format!(
                        "{}\n\n# Scanner Report\n\n```json\n{}\n```",
                        summary, report_json
                    ),
                    artifact_type: "agent-output".to_string(),
                    created_by: "package-update-review".to_string(),
                    source_step_id: Some(run.run_id.clone()),
                    organization: daily.organization.clone(),
                    epic_id: None,
                    slice_id: None,
                    ticket_id: None,
                    agent_run_id: Some(session_id.clone()),
                },
            )
            .await
            .context("Failed to create package update review artifact")?;

            ticketing_system::dailies::complete_run(
                &pool,
                &run.run_id,
                ticketing_system::DailyRunResult {
                    status: "completed".to_string(),
                    agent_run_id: Some(session_id),
                    artifact_id: Some(artifact.artifact_id),
                    summary: Some(summary),
                    lookup_summary: Some(package_update_lookup_summary(&report)),
                    sources_summary: None,
                    error: None,
                    silent: false,
                },
            )
            .await?;
        }
        Err(e) => {
            let completed_at = Utc::now().to_rfc3339();
            let error = e.to_string();
            let _ = ticketing_system::agent_runs::update_agent_run(
                &pool,
                &ticketing_system::AgentRun {
                    session_id: session_id.clone(),
                    organization: Some(daily.organization.clone()),
                    epic_id: None,
                    slice_id: None,
                    ticket_id: None,
                    agent_type: agent_type.as_str().to_string(),
                    status: "failed".to_string(),
                    started_at: run_started_at(&run),
                    completed_at: Some(completed_at),
                    input_message: prompt,
                    output_summary: Some(error.clone()),
                    tool_call_count: 0,
                    cc_session_id: None,
                },
            )
            .await;

            ticketing_system::dailies::complete_run(
                &pool,
                &run.run_id,
                ticketing_system::DailyRunResult {
                    status: "failed".to_string(),
                    agent_run_id: Some(session_id),
                    artifact_id: None,
                    summary: None,
                    lookup_summary: Some(package_update_lookup_summary(&report)),
                    sources_summary: None,
                    error: Some(error),
                    silent: false,
                },
            )
            .await?;
        }
    }

    Ok(())
}

fn is_package_update_daily(daily: &ticketing_system::Daily) -> bool {
    daily.kind == "package-updates" || daily.agent_type == "package-update-review"
}

fn agent_type_for_daily(daily: &ticketing_system::Daily) -> Result<AgentType> {
    match daily.agent_type.as_str() {
        "daily-research" => Ok(AgentType::DailyResearch),
        "exa-research" => Ok(AgentType::ExaResearch),
        other => bail!("Unsupported daily agent_type: {}", other),
    }
}

async fn resolve_daily_working_dir(
    pool: &SqlitePool,
    agent_type: &AgentType,
    organization: &str,
) -> Result<PathBuf> {
    resolve_working_dir(pool, agent_type, organization)
        .await
        .context("Failed to resolve daily agent working directory")
}

fn build_system_prompt(daily: &ticketing_system::Daily) -> Result<String> {
    let mut vars = HashMap::new();
    vars.insert("daily_id".to_string(), daily.daily_id.clone());
    vars.insert("daily_title".to_string(), daily.title.clone());
    vars.insert("daily_description".to_string(), daily.description.clone());
    vars.insert("search_query".to_string(), daily.search_query.clone());
    vars.insert(
        "max_age_hours".to_string(),
        daily
            .max_age_hours
            .map(|v| v.to_string())
            .unwrap_or_else(|| "not set".to_string()),
    );
    load_prompt("daily-research", vars).context("Failed to load daily-research prompt")
}

fn build_run_prompt(
    daily: &ticketing_system::Daily,
    run: &ticketing_system::DailyRun,
    prior_runs: &[ticketing_system::DailyRun],
) -> String {
    format!(
        "Run this Daily research automation now.\n\nDaily ID: {}\nRun ID: {}\nTitle: {}\nDescription: {}\nSearch query: {}\nMax age hours: {}\n\nInstructions:\n{}\n\nPrior completed runs to use as the baseline:\n{}\n\nRun discipline:\n- Search fresh every time, but treat the prior runs as already-read context.\n- Lead with material deltas. If nothing meaningful changed, say \"No material change\" and keep the brief short.\n- Do not restate old background unless it changed, becomes newly relevant, or resolves a prior watch item.\n- Vary the research enough to check the Daily's primary query, authoritative source pages, and targeted delta queries from prior watch items.\n- Put provenance in Sources only. Do not include lookup notes, search logs, queries-run sections, or tool transcripts.\n\nReturn markdown only. Do not create artifacts; the API will persist your final output.",
        daily.daily_id,
        run.run_id,
        daily.title,
        daily.description,
        daily.search_query,
        daily
            .max_age_hours
            .map(|v| v.to_string())
            .unwrap_or_else(|| "not set".to_string()),
        daily.prompt,
        build_prior_run_context(prior_runs)
    )
}

fn build_package_update_prompt(
    daily: &ticketing_system::Daily,
    run: &ticketing_system::DailyRun,
    review_id: &str,
    report_json: &str,
) -> String {
    format!(
        "A daily Mac Mini package-update scan found available updates.\n\nDaily ID: {}\nRun ID: {}\nReview ID: {}\nTitle: {}\n\nYour job:\n- Tell Alex concisely what updates are available and why they may matter.\n- Mention Codex/OpenAI CLI updates prominently if present.\n- Do not file tickets, do not run shell commands, do not update packages.\n- Keep it short enough to read in the Dailies tab.\n- End with one plain sentence that the app will offer Approve and Deny actions.\n\nScanner report:\n```json\n{}\n```\n\nReturn markdown only.",
        daily.daily_id, run.run_id, review_id, daily.title, report_json
    )
}

fn package_update_lookup_summary(report: &PackageUpdateScanReport) -> String {
    let manager_lines = report
        .managers
        .iter()
        .map(|manager| {
            format!(
                "- {}: {} update{}",
                manager.manager,
                manager.update_count,
                if manager.update_count == 1 { "" } else { "s" }
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "Host: {}\nTimestamp: {}\nUpdates found: {}\n\n{}",
        report.host,
        report.timestamp,
        report.updates.len(),
        manager_lines
    )
}

fn build_prior_run_context(prior_runs: &[ticketing_system::DailyRun]) -> String {
    if prior_runs.is_empty() {
        return "No prior completed runs are available. Establish the baseline in this run, then make later runs delta-first.".to_string();
    }

    prior_runs
        .iter()
        .map(|run| {
            let summary = run
                .summary
                .as_deref()
                .map(|value| truncate_for_prompt(value, 1800))
                .unwrap_or_else(|| "No prior summary stored.".to_string());
            let sources = run
                .sources_summary
                .as_deref()
                .map(|value| truncate_for_prompt(value, 900));

            let mut block = format!(
                "## Previous Run {}\n- Scheduled: {}\n- Completed: {}\n\n### Prior Report\n{}",
                run.run_id,
                prompt_time(Some(run.scheduled_for)),
                prompt_time(run.completed_at),
                summary
            );

            if let Some(sources) = sources {
                block.push_str("\n\n### Prior Sources\n");
                block.push_str(&sources);
            }

            block
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn prompt_time(timestamp: Option<i64>) -> String {
    timestamp
        .and_then(|ts| chrono::DateTime::from_timestamp(ts, 0))
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| "not set".to_string())
}

fn truncate_for_prompt(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{}...\n[truncated]", truncated.trim_end())
    } else {
        truncated
    }
}

fn compose_daily_artifact(sections: &DailyResearchSections) -> String {
    let mut parts = vec![sections.report.clone()];

    if let Some(sources) = &sections.sources_summary {
        parts.push(format!("# Sources\n\n{}", sources));
    }

    parts.join("\n\n")
}

fn artifact_title(daily: &ticketing_system::Daily) -> String {
    format!(
        "{} - {}",
        daily.title,
        Utc::now().format("%Y-%m-%d %H:%M UTC")
    )
}

fn run_started_at(run: &ticketing_system::DailyRun) -> String {
    run.started_at
        .and_then(|ts| chrono::DateTime::from_timestamp(ts, 0))
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| Utc::now().to_rfc3339())
}
