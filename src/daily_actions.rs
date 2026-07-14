use std::collections::HashMap;

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde_json::json;
use sqlx::SqlitePool;
use ticketing_system::conversation_turn_jobs::ConversationTurnJobPayload;
use ticketing_system::daily_action_executions::{
    self, DailyActionCompletionPolicy, DailyActionError, LaunchDailyActionExecutionRequest,
    LaunchDailyActionExecutionResult,
};
use ticketing_system::runner_capacity::RunnerQueueAdmission;

use crate::agents::prompts::load_prompt;
use crate::agents::working_dir::resolve_working_dir;
use crate::agents::AgentType;
use crate::handlers::chat_stream::{encode_codex_options_for_job, ChatCodexOptions, ChatRuntime};
use crate::package_updates;

#[derive(Debug)]
pub enum DailyLaunchError {
    PermissionDenied,
    UnsupportedAgent(String),
    QueueRejected(RunnerQueueAdmission),
    Core(DailyActionError),
    Internal(anyhow::Error),
}

impl From<DailyActionError> for DailyLaunchError {
    fn from(error: DailyActionError) -> Self {
        Self::Core(error)
    }
}

#[derive(Debug, Clone)]
pub struct DailyLaunchRequest {
    pub occurrence_id: String,
    pub action_key: String,
    pub idempotency_key: String,
    pub retry_of_execution_id: Option<String>,
    pub admission_context: &'static str,
}

pub async fn launch_daily_action(
    pool: &SqlitePool,
    daily: &ticketing_system::Daily,
    request: DailyLaunchRequest,
) -> Result<LaunchDailyActionExecutionResult, DailyLaunchError> {
    let agent_type = parse_daily_agent_type(&daily.agent_type)
        .ok_or_else(|| DailyLaunchError::UnsupportedAgent(daily.agent_type.clone()))?;
    enforce_agent_permission(pool, &daily.user_id, &agent_type).await?;

    if has_replay_or_canonical_execution(pool, daily, &request).await? {
        let payload = build_job_payload(
            pool,
            daily,
            &agent_type,
            &request.occurrence_id,
            &request.action_key,
            false,
        )
        .await?;
        return launch_in_core(pool, daily, request, payload).await;
    }

    let admission =
        ticketing_system::runner_capacity::admit_enqueue(pool, 1, request.admission_context)
            .await
            .map_err(|error| {
                DailyLaunchError::Internal(
                    error.context("Failed to inspect Daily runner queue capacity"),
                )
            })?;
    if !admission.accepted {
        return Err(DailyLaunchError::QueueRejected(admission));
    }

    let payload = build_job_payload(
        pool,
        daily,
        &agent_type,
        &request.occurrence_id,
        &request.action_key,
        true,
    )
    .await?;
    launch_in_core(pool, daily, request, payload).await
}

async fn launch_in_core(
    pool: &SqlitePool,
    daily: &ticketing_system::Daily,
    request: DailyLaunchRequest,
    payload: ConversationTurnJobPayload,
) -> Result<LaunchDailyActionExecutionResult, DailyLaunchError> {
    daily_action_executions::launch_daily_action_execution(
        pool,
        LaunchDailyActionExecutionRequest {
            user_id: daily.user_id.clone(),
            daily_id: daily.daily_id.clone(),
            occurrence_id: request.occurrence_id,
            action_key: request.action_key,
            idempotency_key: request.idempotency_key,
            retry_of_execution_id: request.retry_of_execution_id,
            completion_policy: completion_policy_for_daily(daily),
            job: payload,
        },
    )
    .await
    .map_err(DailyLaunchError::Core)
}

async fn has_replay_or_canonical_execution(
    pool: &SqlitePool,
    daily: &ticketing_system::Daily,
    request: &DailyLaunchRequest,
) -> Result<bool, DailyLaunchError> {
    let match_count: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)
        FROM daily_action_executions
        WHERE (user_id = ? AND idempotency_key = ?)
           OR (
                occurrence_id = ?
            AND daily_id = ?
            AND action_key = ?
            AND status IN ('queued', 'running', 'completed')
           )
        "#,
    )
    .bind(&daily.user_id)
    .bind(&request.idempotency_key)
    .bind(&request.occurrence_id)
    .bind(&daily.daily_id)
    .bind(&request.action_key)
    .fetch_one(pool)
    .await
    .context("Failed to inspect existing Daily execution before queue admission")
    .map_err(DailyLaunchError::Internal)?;
    Ok(match_count > 0)
}

pub fn is_package_update_daily(daily: &ticketing_system::Daily) -> bool {
    daily.kind == "package-updates" || daily.agent_type == "package-update-review"
}

fn completion_policy_for_daily(daily: &ticketing_system::Daily) -> DailyActionCompletionPolicy {
    if is_package_update_daily(daily) {
        DailyActionCompletionPolicy::Explicit
    } else {
        DailyActionCompletionPolicy::JobTerminal
    }
}

pub fn occurrence_date_for_timestamp(timezone: &str, timestamp: i64) -> anyhow::Result<String> {
    let timezone = ticketing_system::users::validate_occurrence_timezone(timezone)
        .context("Account occurrence timezone is not a valid IANA timezone")?;
    let timestamp = DateTime::<Utc>::from_timestamp(timestamp, 0)
        .context("Daily occurrence timestamp is outside the supported range")?;
    Ok(timestamp
        .with_timezone(&timezone)
        .date_naive()
        .format("%Y-%m-%d")
        .to_string())
}

fn parse_daily_agent_type(value: &str) -> Option<AgentType> {
    let agent = AgentType::from_chat_agent_key(value)
        .or_else(|| serde_json::from_value(serde_json::Value::String(value.to_string())).ok())?;
    matches!(
        agent,
        AgentType::DailyResearch
            | AgentType::Research
            | AgentType::PackageUpdateReview
            | AgentType::FullAccess
    )
    .then_some(agent)
}

async fn enforce_agent_permission(
    pool: &SqlitePool,
    user_id: &str,
    agent_type: &AgentType,
) -> Result<(), DailyLaunchError> {
    if !matches!(
        agent_type,
        AgentType::PackageUpdateReview | AgentType::FullAccess
    ) {
        return Ok(());
    }

    match ticketing_system::system_logs::is_admin(pool, user_id).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(DailyLaunchError::PermissionDenied),
        Err(error) => Err(DailyLaunchError::Internal(
            error.context("Failed to authorize privileged Daily agent"),
        )),
    }
}

async fn build_job_payload(
    pool: &SqlitePool,
    daily: &ticketing_system::Daily,
    agent_type: &AgentType,
    occurrence_id: &str,
    action_key: &str,
    include_package_scan: bool,
) -> Result<ConversationTurnJobPayload, DailyLaunchError> {
    let working_dir = resolve_working_dir(pool, agent_type, &daily.organization)
        .await
        .context("Failed to resolve Daily agent working directory")
        .map_err(DailyLaunchError::Internal)?;

    let mut message_vars = HashMap::from([
        ("DAILY_ID".to_string(), daily.daily_id.clone()),
        ("DAILY_TITLE".to_string(), daily.title.clone()),
        ("DAILY_DESCRIPTION".to_string(), daily.description.clone()),
        ("DAILY_PROMPT".to_string(), daily.prompt.clone()),
        ("OCCURRENCE_ID".to_string(), occurrence_id.to_string()),
        ("ACTION_KEY".to_string(), action_key.to_string()),
    ]);

    if is_package_update_daily(daily) && include_package_scan {
        let report = package_updates::scan_available_updates()
            .await
            .context("Failed to scan package updates before durable queue commit")
            .map_err(DailyLaunchError::Internal)?;
        message_vars.insert(
            "PACKAGE_UPDATE_REPORT".to_string(),
            serde_json::to_string_pretty(&report)
                .context("Failed to encode package update report")
                .map_err(DailyLaunchError::Internal)?,
        );
    }

    let message = load_prompt("daily-action-message", message_vars)
        .context("Failed to render Daily action message")
        .map_err(DailyLaunchError::Internal)?;
    let mut prompt_vars = HashMap::from([
        ("DAILY_ID".to_string(), daily.daily_id.clone()),
        ("DAILY_TITLE".to_string(), daily.title.clone()),
        ("DAILY_DESCRIPTION".to_string(), daily.description.clone()),
        ("SEARCH_QUERY".to_string(), daily.search_query.clone()),
        (
            "MAX_AGE_HOURS".to_string(),
            daily
                .max_age_hours
                .map(|value| value.to_string())
                .unwrap_or_else(|| "not set".to_string()),
        ),
    ]);
    prompt_vars = encode_codex_options_for_job(
        prompt_vars,
        &ChatCodexOptions::default_for_agent(agent_type),
    );

    let metadata = serde_json::to_string(&json!({
        "origin": "daily_action",
        "daily_kind": daily.kind,
    }))
    .context("Failed to encode Daily action job metadata")
    .map_err(DailyLaunchError::Internal)?;

    Ok(ConversationTurnJobPayload {
        user_id: daily.user_id.clone(),
        message,
        agent_type: agent_type.as_str().to_string(),
        runtime: ChatRuntime::CodexAppServer.as_job_runtime().to_string(),
        prompt_name: agent_type.as_str().to_string(),
        working_dir: working_dir.to_string_lossy().into_owned(),
        prompt_vars,
        images_json: None,
        client_id: None,
        message_metadata: Some(metadata),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_updates_use_explicit_completion_policy_guard() {
        let daily = ticketing_system::Daily {
            daily_id: "DLY-1".to_string(),
            user_id: "alex".to_string(),
            organization: "agentic-flowstate".to_string(),
            title: "Package Updates".to_string(),
            description: String::new(),
            kind: "package-updates".to_string(),
            status: "active".to_string(),
            cadence_unit: "day".to_string(),
            cadence_interval: 1,
            run_until: None,
            next_run_at: None,
            last_run_at: None,
            unread_pause_threshold: 0,
            consecutive_unread_runs: 0,
            agent_type: "package-update-review".to_string(),
            prompt: String::new(),
            search_query: String::new(),
            max_age_hours: None,
            pause_reason: None,
            tags: Vec::new(),
            created_at: 0,
            updated_at: 0,
        };
        assert!(is_package_update_daily(&daily));
        assert_eq!(
            completion_policy_for_daily(&daily),
            DailyActionCompletionPolicy::Explicit
        );
    }

    #[test]
    fn occurrence_dates_are_derived_in_validated_account_timezone() {
        assert_eq!(
            occurrence_date_for_timestamp("America/Bogota", 1_783_980_000).unwrap(),
            "2026-07-13"
        );
        assert!(occurrence_date_for_timestamp("UTC-5", 1_783_980_000).is_err());
    }

    #[test]
    fn unsupported_agents_do_not_fall_back_to_full_access() {
        assert!(parse_daily_agent_type("not-a-real-agent").is_none());
        assert!(parse_daily_agent_type("execution").is_none());
        assert_eq!(
            parse_daily_agent_type("daily-research"),
            Some(AgentType::DailyResearch)
        );
    }
}
