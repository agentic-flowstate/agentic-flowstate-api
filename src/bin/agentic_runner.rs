use agentic_api::agents::AgentType;
use agentic_api::apns;
use agentic_api::handlers::chat_client_manager::ChatClientManager;
use agentic_api::handlers::chat_stream::{ChatConfig, ChatImageData, ChatRuntime};
use agentic_api::handlers::conversation_worker::{ConversationWorker, WorkerMessage};
use anyhow::{bail, Context, Result};
use futures::FutureExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use ticketing_system::{
    agent_runners, checkpoints, conversation_turn_jobs, conversations, restart_queue,
};
use tokio::signal;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

const RUNNER_KIND: &str = "agent-runner";
const RUNNER_HEARTBEAT_INTERVAL_SECONDS: u64 = 15;
const RUNNER_POLL_INTERVAL_MS: u64 = 750;

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

    let manager = Arc::new(ChatClientManager::with_runner_generation_id(
        generation_id.clone(),
    ));
    let shutdown = CancellationToken::new();
    spawn_heartbeat(db.clone(), generation_id.clone(), shutdown.child_token());
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

        if accepting && restart_pending_for_runner(&db).await.unwrap_or(false) {
            accepting = false;
            tracing::info!(
                "Runner generation {} entering drain mode because a runner restart is queued",
                generation_id
            );
            agent_runners::mark_generation_draining(&db, &generation_id).await?;
        }

        while accepting && concurrency_allows_claim(concurrency, joins.len()) {
            match conversation_turn_jobs::claim_next_job(&db, &generation_id).await? {
                Some(job) => {
                    let job_id = job.id.clone();
                    tracing::info!(
                        "Claimed conversation job {} for conversation {}",
                        job.id,
                        job.conversation_id
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

fn init_apns() -> Result<()> {
    if apns::ApnsService::init().is_some() {
        tracing::info!("APNs alert push service initialized for runner");
    }

    let apns_silent = Arc::new(apns::ApnsClient::new());
    let silent_enabled = std::env::var("APNS_SILENT_ENABLED")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes"))
        .unwrap_or(false);

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

async fn restart_pending_for_runner(db: &ticketing_system::SqlitePool) -> Result<bool> {
    Ok(restart_queue::get_pending_restart(db)
        .await?
        .map(|entry| matches!(entry.service.as_str(), "agent-runner" | "all"))
        .unwrap_or(false))
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
    verify_job_conversation_owner(&db, &job).await?;
    let worker_message = worker_message_from_job(&job)?;
    let (tx, rx) = mpsc::channel(1);
    tx.send(worker_message)
        .await
        .map_err(|_| anyhow::anyhow!("failed to send job to conversation worker"))?;
    drop(tx);

    let worker = ConversationWorker::new(db.clone(), job.conversation_id.clone(), manager, rx);
    worker.run().await;

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

    let agent_type: AgentType =
        serde_json::from_value(serde_json::Value::String(payload.agent_type.clone()))
            .with_context(|| {
                format!("Unsupported conversation job agent: {}", payload.agent_type)
            })?;
    let images: Option<Vec<ChatImageData>> = match payload.images_json.as_deref() {
        Some(json) => Some(
            serde_json::from_str(json).context("Failed to deserialize conversation job images")?,
        ),
        None => None,
    };

    Ok(WorkerMessage {
        user_id: payload.user_id.clone(),
        message: payload.message.clone(),
        config: ChatConfig {
            agent_type,
            runtime: ChatRuntime::CodexAppServer,
            prompt_name: prompt_name_static(&payload.prompt_name)?,
            working_dir: PathBuf::from(&payload.working_dir),
            prompt_vars: payload.prompt_vars.clone(),
        },
        images,
        completion_tx: None,
        client_id: payload.client_id.clone(),
    })
}

fn prompt_name_static(prompt_name: &str) -> Result<&'static str> {
    match prompt_name {
        "full-access" => Ok("full-access"),
        "workspace-manager" => Ok("workspace-manager"),
        "scoped-workspace" => Ok("scoped-workspace"),
        "meeting-agent" => Ok("meeting-agent"),
        "home-planner" => Ok("home-planner"),
        other => anyhow::bail!("Unsupported conversation job prompt: {}", other),
    }
}

#[cfg(test)]
mod tests {
    use super::{concurrency_allows_claim, parse_runner_concurrency};

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
}
