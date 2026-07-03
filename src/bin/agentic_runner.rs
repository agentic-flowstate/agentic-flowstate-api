use agentic_api::agents::AgentType;
use agentic_api::apns;
use agentic_api::handlers::chat_client_manager::ChatClientManager;
use agentic_api::handlers::chat_stream::{self, ChatAttachmentData, ChatConfig, ChatRuntime};
use agentic_api::handlers::conversation_worker::{ConversationWorker, WorkerMessage};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use futures::FutureExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use ticketing_system::{agent_runners, checkpoints, conversation_turn_jobs, conversations};
use tokio::signal;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

const RUNNER_KIND: &str = "agent-runner";
const RUNNER_HEARTBEAT_STALE_SECONDS: i64 = 90;
const RUNNER_HEARTBEAT_INTERVAL_SECONDS: u64 = 15;
const RUNNER_POLL_INTERVAL_MS: u64 = 750;
const RUNNER_RECONCILE_INTERVAL_SECONDS: u64 = 60;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "agentic_runner=debug,agentic_api=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    tracing::info!("Starting Agentic runner...");
    let db = Arc::new(ticketing_system::init_db().await?);
    init_apns()?;

    let generation_id = format!("runner-{}", uuid::Uuid::new_v4());
    agent_runners::register_generation(
        &db,
        &generation_id,
        RUNNER_KIND,
        env!("CARGO_PKG_VERSION"),
        std::process::id() as i64,
    )
    .await?;
    let drained =
        agent_runners::mark_other_generations_draining(&db, RUNNER_KIND, &generation_id).await?;
    tracing::info!(
        "Agentic runner generation {} registered (marked {} previous generation(s) draining)",
        generation_id,
        drained
    );
    match agent_runners::reconcile_stale_runner_generations(&db, RUNNER_HEARTBEAT_STALE_SECONDS)
        .await
    {
        Ok(reconciled) if reconciled.any() => {
            tracing::warn!(
                "Reconciled stale agent runner generation metadata: counts_recomputed={} generations_terminalized={}",
                reconciled.generations_recomputed,
                reconciled.generations_terminalized
            );
        }
        Ok(_) => tracing::debug!("No stale agent runner generation metadata to reconcile"),
        Err(e) => tracing::error!(
            "Failed to reconcile stale runner generation metadata: {}",
            e
        ),
    }

    let manager = Arc::new(ChatClientManager::with_runner_generation_id(
        generation_id.clone(),
    ));
    let shutdown = CancellationToken::new();
    spawn_heartbeat(db.clone(), generation_id.clone(), shutdown.child_token());
    spawn_runner_generation_reconciler(db.clone(), shutdown.child_token());
    spawn_shutdown_listener(shutdown.clone());

    let concurrency = runner_concurrency()?;
    match concurrency {
        Some(limit) => tracing::info!("Agentic runner concurrency limited to {}", limit),
        None => tracing::info!("Agentic runner concurrency is unlimited"),
    }
    let mut joins = JoinSet::<(String, Result<String>)>::new();
    let mut accepting = true;

    loop {
        if shutdown.is_cancelled() {
            accepting = false;
            let _ = agent_runners::mark_generation_draining(&db, &generation_id).await;
        }

        while accepting && concurrency_allows_claim(concurrency, joins.len()) {
            match conversation_turn_jobs::claim_next_job(&db, &generation_id).await? {
                Some(job) => {
                    let job_id = job.id.clone();
                    let claimed_at_ms = Utc::now().timestamp_millis();
                    let queue_wait_ms = claimed_at_ms.saturating_sub(job.created_at * 1000);
                    tracing::info!(
                        "Claimed conversation job {} for conversation {}",
                        job.id,
                        job.conversation_id
                    );
                    tracing::info!(
                        "[CHAT_LATENCY] phase=runner_claimed job_id={} conv={} client_id={} generation_id={} created_at={} started_at={} claimed_at_ms={} queue_wait_ms={}",
                        job.id,
                        job.conversation_id,
                        job.payload.client_id.as_deref().unwrap_or("none"),
                        generation_id,
                        job.created_at,
                        job.started_at.unwrap_or(job.updated_at),
                        claimed_at_ms,
                        queue_wait_ms
                    );
                    joins.spawn(
                        run_claimed_job(db.clone(), manager.clone(), job).map(move |r| (job_id, r)),
                    );
                }
                None => break,
            }
        }

        if !accepting && joins.is_empty() {
            break;
        }

        tokio::select! {
            Some(result) = joins.join_next(), if !joins.is_empty() => {
                match result {
                    Ok((job_id, Ok(status))) => {
                        tracing::info!("Conversation job {} finished as {}", job_id, status);
                    }
                    Ok((job_id, Err(e))) => {
                        tracing::error!("Conversation job {} failed: {}", job_id, e);
                    }
                    Err(e) => {
                        tracing::error!("Conversation job task failed to join: {}", e);
                    }
                }
            }
            _ = shutdown.cancelled() => {
                accepting = false;
                let _ = agent_runners::mark_generation_draining(&db, &generation_id).await;
            }
            _ = tokio::time::sleep(Duration::from_millis(RUNNER_POLL_INTERVAL_MS)) => {}
        }
    }

    if let Err(e) = agent_runners::mark_generation_exited(&db, &generation_id).await {
        tracing::warn!(
            "Failed to mark runner generation {} exited: {}",
            generation_id,
            e
        );
    }
    tracing::info!("Agentic runner generation {} exited cleanly", generation_id);
    Ok(())
}

fn runner_concurrency() -> Result<Option<usize>> {
    parse_runner_concurrency(std::env::var("AGENTIC_RUNNER_CONCURRENCY").ok().as_deref())
}

fn parse_runner_concurrency(value: Option<&str>) -> Result<Option<usize>> {
    let Some(raw) = value else {
        return Ok(None);
    };

    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let limit = trimmed
        .parse::<usize>()
        .with_context(|| "AGENTIC_RUNNER_CONCURRENCY must be a positive integer")?;
    if limit == 0 {
        bail!("AGENTIC_RUNNER_CONCURRENCY must be greater than zero");
    }
    Ok(Some(limit))
}

fn concurrency_allows_claim(concurrency: Option<usize>, active_jobs: usize) -> bool {
    concurrency.map(|limit| active_jobs < limit).unwrap_or(true)
}

fn env_flag_enabled(name: &str) -> bool {
    std::env::var(name)
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes"))
        .unwrap_or(false)
}

fn init_apns() -> Result<()> {
    let alert_enabled = env_flag_enabled("APNS_ALERT_ENABLED");
    if alert_enabled {
        apns::ApnsService::init_from_env().context("APNs alert push init failed")?;
        tracing::info!("APNs alert push service initialized for runner");
    } else {
        tracing::info!("APNs alert push disabled for runner");
    }

    let apns_silent = Arc::new(apns::ApnsClient::new());
    let silent_enabled = env_flag_enabled("APNS_SILENT_ENABLED");

    if silent_enabled {
        let cfg = apns_silent
            .init_from_env()
            .context("APNs silent push init failed")?;
        tracing::info!(
            "APNs silent push initialized for runner (bundle_id={}, sandbox={}, team={}, key={})",
            cfg.bundle_id,
            cfg.use_sandbox,
            cfg.team_id,
            cfg.key_id
        );
    } else {
        tracing::info!("APNs silent push disabled for runner");
    }

    match apns::silent_fanout::init_global(apns::silent_fanout::SilentPushConfig {
        enabled: silent_enabled,
        sender: apns_silent,
    }) {
        Ok(()) => tracing::info!(
            "APNs silent fan-out registry installed for runner (enabled={})",
            silent_enabled
        ),
        Err(e) => tracing::warn!("APNs silent fan-out registry already initialized: {}", e),
    }

    Ok(())
}

fn spawn_heartbeat(
    db: Arc<ticketing_system::SqlitePool>,
    generation_id: String,
    shutdown: CancellationToken,
) {
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(Duration::from_secs(RUNNER_HEARTBEAT_INTERVAL_SECONDS));
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = interval.tick() => {}
            }
            if let Err(e) = agent_runners::heartbeat_generation(&db, &generation_id).await {
                tracing::warn!(
                    "Failed to heartbeat runner generation {}: {}",
                    generation_id,
                    e
                );
            }
        }
    });
}

fn spawn_runner_generation_reconciler(
    db: Arc<ticketing_system::SqlitePool>,
    shutdown: CancellationToken,
) {
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(Duration::from_secs(RUNNER_RECONCILE_INTERVAL_SECONDS));
        interval.reset();

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = interval.tick() => {}
            }

            match agent_runners::reconcile_stale_runner_generations(
                &db,
                RUNNER_HEARTBEAT_STALE_SECONDS,
            )
            .await
            {
                Ok(reconciled) if reconciled.any() => {
                    tracing::warn!(
                        "Reconciled stale agent runner generation metadata: counts_recomputed={} generations_terminalized={}",
                        reconciled.generations_recomputed,
                        reconciled.generations_terminalized
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::error!(
                        "Failed to reconcile stale runner generation metadata: {}",
                        e
                    );
                }
            }
        }
    });
}

fn spawn_shutdown_listener(shutdown: CancellationToken) {
    tokio::spawn(async move {
        let ctrl_c = async {
            signal::ctrl_c()
                .await
                .expect("failed to install Ctrl+C handler");
        };

        #[cfg(unix)]
        let terminate = async {
            signal::unix::signal(signal::unix::SignalKind::terminate())
                .expect("failed to install signal handler")
                .recv()
                .await;
        };

        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();

        tokio::select! {
            _ = ctrl_c => tracing::info!("Runner received Ctrl+C, draining..."),
            _ = terminate => tracing::info!("Runner received SIGTERM, draining..."),
        }
        shutdown.cancel();
    });
}

async fn run_claimed_job(
    db: Arc<ticketing_system::SqlitePool>,
    manager: Arc<ChatClientManager>,
    job: conversation_turn_jobs::ConversationTurnJob,
) -> Result<String> {
    let result =
        std::panic::AssertUnwindSafe(run_claimed_job_inner(db.clone(), manager, job.clone()))
            .catch_unwind()
            .await
            .map_err(|_| anyhow::anyhow!("conversation job panicked"))
            .and_then(|inner| inner);

    match result {
        Ok(status) => {
            conversation_turn_jobs::mark_job_terminal(&db, &job.id, &status, None).await?;
            Ok(status)
        }
        Err(e) => {
            let message = e.to_string();
            let _ = checkpoints::mark_interrupted(&db, &job.conversation_id).await;
            conversation_turn_jobs::mark_job_terminal(&db, &job.id, "failed", Some(&message))
                .await?;
            Err(e)
        }
    }
}

async fn run_claimed_job_inner(
    db: Arc<ticketing_system::SqlitePool>,
    manager: Arc<ChatClientManager>,
    job: conversation_turn_jobs::ConversationTurnJob,
) -> Result<String> {
    let worker_start_ms = Utc::now().timestamp_millis();
    verify_job_conversation_owner(&db, &job).await?;
    let worker_message = worker_message_from_job(&job)?;
    tracing::info!(
        "[CHAT_LATENCY] phase=worker_starting job_id={} conv={} client_id={} worker_start_ms={} queue_to_worker_ms={}",
        job.id,
        job.conversation_id,
        job.payload.client_id.as_deref().unwrap_or("none"),
        worker_start_ms,
        worker_start_ms.saturating_sub(job.created_at * 1000)
    );
    let (tx, rx) = mpsc::channel(1);
    tx.send(worker_message)
        .await
        .map_err(|_| anyhow::anyhow!("failed to send job to conversation worker"))?;
    drop(tx);

    let worker = ConversationWorker::new(db.clone(), job.conversation_id.clone(), manager, rx);
    worker.run().await;
    tracing::info!(
        "[CHAT_LATENCY] phase=worker_finished job_id={} conv={} client_id={} finished_at_ms={} worker_duration_ms={}",
        job.id,
        job.conversation_id,
        job.payload.client_id.as_deref().unwrap_or("none"),
        Utc::now().timestamp_millis(),
        Utc::now().timestamp_millis().saturating_sub(worker_start_ms)
    );

    if let Some(terminal) = latest_terminal_event_status(&db, &job.conversation_id).await? {
        tracing::warn!(
            "[RUNNER] Conversation job {} observed terminal event after worker finish: conv={} client_id={} event_index={} event_type={} status={}",
            job.id,
            job.conversation_id,
            job.payload.client_id.as_deref().unwrap_or("none"),
            terminal.event_index,
            terminal.event_type,
            terminal.status
        );
        return Ok(terminal.status.to_string());
    }

    let checkpoint = checkpoints::get_checkpoint(&db, &job.conversation_id).await?;
    let status = match checkpoint.as_ref().map(|cp| cp.status.as_str()) {
        Some("completed") => "completed",
        Some("interrupted") | Some("cancelled") => "cancelled",
        Some("failed") | Some("timeout") => "failed",
        Some(other) => {
            tracing::warn!(
                "Conversation job {} finished with non-terminal checkpoint status {}; marking failed",
                job.id,
                other
            );
            "failed"
        }
        None => "failed",
    };

    Ok(status.to_string())
}

async fn latest_terminal_event_status(
    db: &ticketing_system::SqlitePool,
    conversation_id: &str,
) -> Result<Option<TerminalEventStatus>> {
    let rows = sqlx::query_as::<_, (i64, String, String)>(
        r#"
        SELECT event_index, event_type, event_data
        FROM conversation_events
        WHERE conversation_id = ?
        ORDER BY event_index DESC
        LIMIT 20
        "#,
    )
    .bind(conversation_id)
    .fetch_all(db)
    .await
    .context("Failed to inspect conversation terminal events")?;

    Ok(rows
        .into_iter()
        .find_map(|(event_index, event_type, event_data)| {
            terminal_status_from_event(&event_type, &event_data).map(|status| TerminalEventStatus {
                status,
                event_index,
                event_type,
            })
        }))
}

struct TerminalEventStatus {
    status: &'static str,
    event_index: i64,
    event_type: String,
}

fn terminal_status_from_event(event_type: &str, event_data: &str) -> Option<&'static str> {
    let value: serde_json::Value = serde_json::from_str(event_data).ok()?;
    if event_type == "error"
        || value.get("type").and_then(serde_json::Value::as_str) == Some("error")
    {
        return Some("failed");
    }

    if let Some(status) = value.get("status").and_then(serde_json::Value::as_str) {
        return match status {
            "failed" | "timeout" => Some("failed"),
            "cancelled" => Some("cancelled"),
            _ => None,
        };
    }

    value
        .get("delta")
        .and_then(|delta| delta.get("stop_reason"))
        .and_then(serde_json::Value::as_str)
        .and_then(|stop_reason| match stop_reason {
            "cancelled" => Some("cancelled"),
            _ => None,
        })
}

async fn verify_job_conversation_owner(
    db: &ticketing_system::SqlitePool,
    job: &conversation_turn_jobs::ConversationTurnJob,
) -> Result<()> {
    let conversation = conversations::get_conversation(db, &job.conversation_id, false)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Conversation not found for queued job"))?;
    if conversation.user_id != job.payload.user_id {
        anyhow::bail!("Conversation job user does not own the target conversation");
    }
    Ok(())
}

fn worker_message_from_job(
    job: &conversation_turn_jobs::ConversationTurnJob,
) -> Result<WorkerMessage> {
    let payload = &job.payload;
    if payload.runtime != ChatRuntime::CodexAppServer.as_job_runtime() {
        anyhow::bail!("Unsupported conversation job runtime: {}", payload.runtime);
    }

    let agent_type: AgentType = AgentType::from_chat_agent_key(&payload.agent_type)
        .or_else(|| {
            serde_json::from_value(serde_json::Value::String(payload.agent_type.clone())).ok()
        })
        .with_context(|| format!("Unsupported conversation job agent: {}", payload.agent_type))?;
    let attachments: Option<Vec<ChatAttachmentData>> = match payload.images_json.as_deref() {
        Some(json) => Some(
            serde_json::from_str(json)
                .context("Failed to deserialize conversation job attachments")?,
        ),
        None => None,
    };

    let mut prompt_vars = payload.prompt_vars.clone();
    let codex_options = chat_stream::take_codex_options_from_job(&agent_type, &mut prompt_vars);

    Ok(WorkerMessage {
        user_id: payload.user_id.clone(),
        message: payload.message.clone(),
        config: ChatConfig {
            agent_type,
            runtime: ChatRuntime::CodexAppServer,
            prompt_name: prompt_name_static(&payload.prompt_name)?,
            working_dir: PathBuf::from(&payload.working_dir),
            prompt_vars,
            codex_options,
        },
        attachments,
        completion_tx: None,
        client_id: payload.client_id.clone(),
        message_metadata: payload.message_metadata.clone(),
    })
}

fn prompt_name_static(prompt_name: &str) -> Result<&'static str> {
    match prompt_name {
        "codex" => Ok("full-access"),
        "full-access" => Ok("full-access"),
        "workspace-manager" => Ok("workspace-manager"),
        "scoped-workspace" => Ok("scoped-workspace"),
        "meeting-agent" => Ok("meeting-agent"),
        "home-planner" => Ok("home-planner"),
        "conversation-evaluator-system" => Ok("conversation-evaluator-system"),
        "planning" => Ok("planning"),
        "execution" => Ok("execution"),
        "evaluation" => Ok("evaluation"),
        "conversation-evaluator" => Ok("conversation-evaluator"),
        "feedback" => Ok("feedback"),
        "email" => Ok("email"),
        "meeting-notes" => Ok("meeting-notes"),
        "ticket-assistant" => Ok("ticket-assistant"),
        "exa-research" => Ok("exa-research"),
        "daily-research" => Ok("daily-research"),
        "package-update-review" => Ok("package-update-review"),
        "research-synthesis" => Ok("research-synthesis"),
        "ticket-planner" => Ok("ticket-planner"),
        "ticket-creator" => Ok("ticket-creator"),
        "doc-drafter" => Ok("doc-drafter"),
        "pull-ticket" => Ok("pull-ticket"),
        "codebase-research" => Ok("codebase-research"),
        "doc-manager" => Ok("doc-manager"),
        other => anyhow::bail!("Unsupported conversation job prompt: {}", other),
    }
}

#[cfg(test)]
mod tests {
    use super::{concurrency_allows_claim, parse_runner_concurrency, terminal_status_from_event};

    #[test]
    fn absent_runner_concurrency_is_unlimited() {
        assert_eq!(parse_runner_concurrency(None).unwrap(), None);
        assert!(concurrency_allows_claim(None, usize::MAX));
    }

    #[test]
    fn empty_runner_concurrency_is_unlimited() {
        assert_eq!(parse_runner_concurrency(Some("  ")).unwrap(), None);
    }

    #[test]
    fn configured_runner_concurrency_limits_claims() {
        let concurrency = parse_runner_concurrency(Some("4")).unwrap();

        assert!(concurrency_allows_claim(concurrency, 3));
        assert!(!concurrency_allows_claim(concurrency, 4));
    }

    #[test]
    fn invalid_runner_concurrency_fails() {
        assert!(parse_runner_concurrency(Some("0")).is_err());
        assert!(parse_runner_concurrency(Some("abc")).is_err());
    }

    #[test]
    fn terminal_event_status_maps_startup_errors_to_failed() {
        let event_data = r#"{"type":"error","error":{"type":"failed","message":"Startup context preflight failed"}}"#;

        assert_eq!(
            terminal_status_from_event("error", event_data),
            Some("failed")
        );
    }

    #[test]
    fn terminal_event_status_maps_cancelled_stop_reason() {
        let event_data = r#"{"type":"message_delta","delta":{"stop_reason":"cancelled"}}"#;

        assert_eq!(
            terminal_status_from_event("message_delta", event_data),
            Some("cancelled")
        );
    }
}
