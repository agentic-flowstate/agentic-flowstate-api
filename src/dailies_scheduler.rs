use anyhow::{Context, Result};
use chrono::Utc;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::Arc;
use ticketing_system::daily_action_executions::{
    self, DailyActionExecutionStatus, DailyOccurrenceWindowItem,
};
use tokio_util::sync::CancellationToken;

use crate::daily_actions::{self, DailyLaunchRequest};

const DAILIES_STORAGE_ORGANIZATION: &str = "agentic-flowstate";
const PACKAGE_UPDATE_OWNER_USER_ID: &str = "alex";
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

            if let Err(error) = run_due_once(pool.clone()).await {
                tracing::error!(
                    component = "dailies_scheduler",
                    operation = "daily_action.scheduler_tick_failed",
                    error = %error,
                    "durable Dailies scheduler tick failed"
                );
                crate::system_log_helper::log_event(
                    &pool,
                    "error",
                    "dailies",
                    "Daily scheduler tick failed",
                    Some(&format!("error={error}")),
                    None,
                    None,
                )
                .await;
            }
        }
    });
}

pub async fn run_due_once(pool: Arc<SqlitePool>) -> Result<()> {
    let now = Utc::now().timestamp();
    ensure_package_update_daily(&pool, PACKAGE_UPDATE_OWNER_USER_ID).await?;

    let expired = ticketing_system::dailies::complete_expired_dailies(&pool, now).await?;
    if expired > 0 {
        tracing::info!(
            component = "dailies_scheduler",
            operation = "daily_action.expired_dailies_completed",
            expired,
            "completed expired Daily definitions"
        );
    }

    let due = ticketing_system::dailies::due_dailies(&pool, now, 25).await?;
    let mut launch_failures = 0u64;
    let mut first_failed_daily_id = None;
    for daily in due {
        if let Err(error) = launch_due_occurrence(&pool, &daily, now).await {
            launch_failures += 1;
            first_failed_daily_id.get_or_insert_with(|| daily.daily_id.clone());
            metrics::counter!(
                "api_dailies_scheduler_launches_total",
                "outcome" => "failed"
            )
            .increment(1);
            tracing::error!(
                component = "dailies_scheduler",
                operation = "daily_action.scheduler_launch_failed",
                daily_id = %daily.daily_id,
                organization = %daily.organization,
                error = %error,
                "failed to queue due Daily through durable conversation runner"
            );
        } else {
            metrics::counter!(
                "api_dailies_scheduler_launches_total",
                "outcome" => "processed"
            )
            .increment(1);
        }
    }

    if launch_failures > 0 {
        crate::system_log_helper::log_event(
            &pool,
            "error",
            "dailies",
            "Daily scheduler launch batch had failures",
            Some(&format!(
                "failure_count={launch_failures};first_daily_id={}",
                first_failed_daily_id.as_deref().unwrap_or("none")
            )),
            None,
            None,
        )
        .await;
    }

    Ok(())
}

async fn launch_due_occurrence(
    pool: &SqlitePool,
    daily: &ticketing_system::Daily,
    now: i64,
) -> Result<()> {
    let timezone = ticketing_system::users::get_occurrence_timezone(pool, &daily.user_id)
        .await?
        .context("Account has no validated occurrence timezone")?;
    let date = daily_actions::occurrence_date_for_timestamp(&timezone, now)?;
    let window =
        daily_action_executions::materialize_daily_occurrence_window_from_persisted_timezone(
            pool,
            &daily.user_id,
            &date,
            1,
        )
        .await
        .map_err(anyhow::Error::new)?;

    let Some(item) = window
        .items
        .into_iter()
        .find(|item| item.occurrence.daily_id == daily.daily_id)
    else {
        return Ok(());
    };

    if let Some(execution) = item.latest_execution {
        if execution.status == DailyActionExecutionStatus::Failed {
            tracing::warn!(
                component = "dailies_scheduler",
                operation = "daily_action.scheduler_retry_requires_user",
                daily_id = %daily.daily_id,
                occurrence_id = %item.occurrence.occurrence_id,
                execution_id = %execution.execution_id,
                "scheduler will not implicitly retry a failed Daily execution"
            );
        }
        return Ok(());
    }

    let idempotency_key = format!(
        "scheduler:{}:{}:{}",
        daily.daily_id, item.occurrence.occurrence_id, item.action.action_key
    );
    match daily_actions::launch_daily_action(
        pool,
        daily,
        DailyLaunchRequest {
            occurrence_id: item.occurrence.occurrence_id,
            action_key: item.action.action_key,
            idempotency_key,
            retry_of_execution_id: None,
            admission_context: "dailies_scheduler",
        },
    )
    .await
    {
        Ok(result) => {
            tracing::info!(
                component = "dailies_scheduler",
                operation = "daily_action.scheduler_admitted",
                daily_id = %daily.daily_id,
                execution_id = %result.execution.execution_id,
                created = result.created,
                replayed = result.replayed,
                deduplicated = result.deduplicated,
                "scheduler admitted Daily to durable conversation queue"
            );
            Ok(())
        }
        Err(error) => Err(anyhow::anyhow!("{error:?}")),
    }
}

pub async fn materialize_run_now_occurrence(
    pool: &SqlitePool,
    daily: &ticketing_system::Daily,
) -> Result<Option<DailyOccurrenceWindowItem>> {
    let timezone = ticketing_system::users::get_occurrence_timezone(pool, &daily.user_id)
        .await?
        .context("Account has no validated occurrence timezone")?;
    let target = daily.next_run_at.unwrap_or_else(|| Utc::now().timestamp());
    let date = daily_actions::occurrence_date_for_timestamp(&timezone, target)?;
    let window =
        daily_action_executions::materialize_daily_occurrence_window_from_persisted_timezone(
            pool,
            &daily.user_id,
            &date,
            1,
        )
        .await
        .map_err(anyhow::Error::new)?;
    Ok(window
        .items
        .into_iter()
        .find(|item| item.occurrence.daily_id == daily.daily_id))
}

pub async fn ensure_package_update_daily(pool: &SqlitePool, user_id: &str) -> Result<()> {
    if user_id != PACKAGE_UPDATE_OWNER_USER_ID {
        return Ok(());
    }

    let dailies = ticketing_system::dailies::list_dailies(pool, Some(user_id), None).await?;
    if dailies.iter().any(|daily| daily.kind == "package-updates") {
        return Ok(());
    }

    let stored_instructions =
        crate::agents::prompts::load_prompt("package-update-daily-instructions", HashMap::new())?;
    ticketing_system::dailies::create_daily(
        pool,
        ticketing_system::CreateDailyRequest {
            user_id: user_id.to_string(),
            organization: DAILIES_STORAGE_ORGANIZATION.to_string(),
            title: "Package Updates".to_string(),
            description: "Daily Mac Mini package update check with approve or deny review."
                .to_string(),
            kind: "package-updates".to_string(),
            tags: vec!["Mac Mini".to_string(), "Packages".to_string()],
            cadence_unit: "day".to_string(),
            cadence_interval: 1,
            run_until: None,
            next_run_at: Some(Utc::now().timestamp()),
            unread_pause_threshold: 0,
            agent_type: "package-update-review".to_string(),
            prompt: stored_instructions,
            search_query: "local mac mini package updates".to_string(),
            max_age_hours: None,
        },
    )
    .await?;

    tracing::info!(
        component = "dailies_scheduler",
        operation = "daily_action.default_package_daily_created",
        user_id,
        "created default package update Daily"
    );

    Ok(())
}
