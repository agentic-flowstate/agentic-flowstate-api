use anyhow::{bail, Context, Result};
use chrono::Utc;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use ticketing_system::text_normalization::normalize_daily_research_output;
use tokio_util::sync::CancellationToken;

use crate::agents::executor::run_codex_agent_turn;
use crate::agents::prompts::load_prompt;
use crate::agents::working_dir::resolve_working_dir;
use crate::agents::AgentType;

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
                    error: Some(e.to_string()),
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
    let agent_type = agent_type_for_daily(&daily)?;
    let working_dir = resolve_daily_working_dir(&pool, &agent_type, &daily.organization).await?;
    let session_id = uuid::Uuid::new_v4().to_string();
    let prompt = build_run_prompt(&daily, &run);
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

            let artifact = ticketing_system::artifacts::create_artifact(
                &pool,
                ticketing_system::CreateArtifactRequest {
                    title: artifact_title(&daily),
                    content: output_summary.clone(),
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
                    summary: Some(output_summary),
                    error: None,
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
                    error: Some(error),
                },
            )
            .await?;
        }
    }

    Ok(())
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

fn build_run_prompt(daily: &ticketing_system::Daily, run: &ticketing_system::DailyRun) -> String {
    format!(
        "Run this Daily research automation now.\n\nDaily ID: {}\nRun ID: {}\nTitle: {}\nDescription: {}\nSearch query: {}\nMax age hours: {}\n\nInstructions:\n{}\n\nUse fresh search. Return the full report in markdown. Do not create artifacts; the API will persist your final output.",
        daily.daily_id,
        run.run_id,
        daily.title,
        daily.description,
        daily.search_query,
        daily
            .max_age_hours
            .map(|v| v.to_string())
            .unwrap_or_else(|| "not set".to_string()),
        daily.prompt
    )
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
