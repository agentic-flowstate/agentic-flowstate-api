use agentic_api::agents::AgentType;
use agentic_api::apns;
use agentic_api::handlers::chat_client_manager::ChatClientManager;
use agentic_api::handlers::chat_stream::{self, ChatAttachmentData, ChatConfig, ChatRuntime};
use agentic_api::handlers::conversation_worker::{ConversationWorker, WorkerMessage};
use agentic_api::observability::agent_lifecycle;
use agentic_api::observability::runtime::{self, RuntimeFailurePhase, RuntimeLatencyPhase};
use agentic_api::runner_commands;
use agentic_api::system_log_helper;
use anyhow::{Context, Result};
use chrono::Utc;
use futures::FutureExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use ticketing_system::{
    agent_runners, checkpoints, conversation_turn_jobs, conversations, runner_capacity,
};
use tokio::signal;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

const RUNNER_KIND: &str = runner_capacity::RUNNER_KIND;
const RUNNER_HEARTBEAT_STALE_SECONDS: i64 = runner_capacity::RUNNER_HEARTBEAT_STALE_SECONDS;
const RUNNER_HEARTBEAT_INTERVAL_SECONDS: u64 = 15;
const RUNNER_POLL_INTERVAL_MS: u64 = 750;
const RUNNER_POLICY_REFRESH_INTERVAL_SECONDS: u64 = 5;
const RUNNER_RECONCILE_INTERVAL_SECONDS: u64 = 60;
const RUNNER_BACKPRESSURE_LOG_INTERVAL_SECONDS: u64 = 15;
const RUNNER_COMMAND_SERVER_RETRY_SECONDS: u64 = 10;

struct RunnerPolicyCache {
    policy: runner_capacity::RunnerCapacityPolicy,
    loaded_at: Instant,
}

impl RunnerPolicyCache {
    async fn load(db: &ticketing_system::SqlitePool) -> Result<Self> {
        Ok(Self {
            policy: runner_capacity::load_policy(db, RUNNER_KIND).await?,
            loaded_at: Instant::now(),
        })
    }

    async fn refresh_if_stale(&mut self, db: &ticketing_system::SqlitePool) -> Result<()> {
        if self.loaded_at.elapsed() >= Duration::from_secs(RUNNER_POLICY_REFRESH_INTERVAL_SECONDS) {
            self.policy = runner_capacity::load_policy(db, RUNNER_KIND).await?;
            self.loaded_at = Instant::now();
        }
        Ok(())
    }

    fn policy(&self) -> &runner_capacity::RunnerCapacityPolicy {
        &self.policy
    }
}

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
    agentic_api::fable_coordinator::ensure_schema(&db).await?;
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
    let mut command_server =
        match start_or_defer_command_server(manager.clone(), shutdown.child_token()).await {
            Ok(server) => server,
            Err(e) => {
                let reason = format!("runner command server startup failed: {e}");
                let _ = agent_runners::mark_generation_failed(&db, &generation_id, &reason).await;
                return Err(e);
            }
        };
    spawn_heartbeat(db.clone(), generation_id.clone(), shutdown.child_token());
    spawn_runner_generation_reconciler(db.clone(), shutdown.child_token());
    spawn_shutdown_listener(shutdown.clone());

    let mut policy_cache = RunnerPolicyCache::load(&db).await?;
    log_policy(policy_cache.policy());
    let mut joins = JoinSet::<(String, Result<String>)>::new();
    let mut accepting = true;
    let mut last_claim_at: Option<Instant> = None;
    let mut last_command_server_attempt = Instant::now();
    let mut last_backpressure_log =
        Instant::now() - Duration::from_secs(RUNNER_BACKPRESSURE_LOG_INTERVAL_SECONDS);

    loop {
        if command_server.is_none()
            && last_command_server_attempt.elapsed()
                >= Duration::from_secs(RUNNER_COMMAND_SERVER_RETRY_SECONDS)
        {
            last_command_server_attempt = Instant::now();
            match start_or_defer_command_server(manager.clone(), shutdown.child_token()).await {
                Ok(server) => {
                    command_server = server;
                }
                Err(e) => {
                    let reason = format!("runner command server retry failed: {e}");
                    let _ =
                        agent_runners::mark_generation_failed(&db, &generation_id, &reason).await;
                    return Err(e);
                }
            }
        }

        if shutdown.is_cancelled() {
            accepting = false;
            let _ = agent_runners::mark_generation_draining(&db, &generation_id).await;
        }

        if accepting && is_generation_draining(&db, &generation_id).await? {
            accepting = false;
            tracing::info!(
                "Agentic runner generation {} observed external drain request; stopping new claims",
                generation_id
            );
        }

        let mut claims_this_poll = 0;
        while accepting {
            policy_cache.refresh_if_stale(&db).await?;
            let global_active_jobs = runner_capacity::active_runner_job_count(&db)
                .await?
                .max(joins.len() as i64);
            let active_jobs = usize::try_from(global_active_jobs).unwrap_or(usize::MAX);
            match runner_capacity::evaluate_claim(
                policy_cache.policy(),
                active_jobs,
                claims_this_poll,
                last_claim_at.map(|instant| instant.elapsed()),
                &db,
            )? {
                runner_capacity::RunnerClaimDecision::Allowed => {}
                runner_capacity::RunnerClaimDecision::Denied(denial) => {
                    if should_report_backpressure(&mut last_backpressure_log) {
                        log_backpressure(&denial);
                        if let Err(e) = runner_capacity::record_claim_denial(
                            &db,
                            RUNNER_KIND,
                            Some(&generation_id),
                            &denial,
                        )
                        .await
                        {
                            tracing::warn!(
                                "Failed to record runner claim denial for generation {}: {}",
                                generation_id,
                                e
                            );
                        }
                    }
                    break;
                }
            }

            match conversation_turn_jobs::claim_next_job(&db, &generation_id).await? {
                Some(job) => {
                    claims_this_poll += 1;
                    last_claim_at = Some(Instant::now());
                    let job_id = job.id.clone();
                    let claimed_at_ms = Utc::now().timestamp_millis();
                    let queue_wait_ms = claimed_at_ms.saturating_sub(job.created_at * 1000);
                    tracing::info!(
                        target: "agentic_runner::jobs",
                        event = "runner.job_claimed",
                        phase = %RuntimeLatencyPhase::RunnerClaimed,
                        job_id = %job.id,
                        conversation_id = %job.conversation_id,
                        client_id = job.payload.client_id.as_deref().unwrap_or("none"),
                        generation_id = %generation_id,
                        created_at = job.created_at,
                        started_at = job.started_at.unwrap_or(job.updated_at),
                        claimed_at_ms,
                        queue_wait_ms,
                        "claimed conversation job"
                    );
                    runtime::record_latency_marker(
                        &job.conversation_id,
                        job.payload.client_id.as_deref(),
                        RuntimeLatencyPhase::RunnerClaimed,
                        queue_wait_ms as u64,
                        claimed_at_ms,
                        None,
                        None,
                    );
                    agent_lifecycle::refresh_queue_metrics(&db).await;
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

    if let Some(command_server) = command_server {
        command_server.shutdown().await;
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

async fn start_or_defer_command_server(
    manager: Arc<ChatClientManager>,
    shutdown: CancellationToken,
) -> Result<Option<runner_commands::RunnerCommandServer>> {
    match runner_commands::start_runner_command_server(manager, shutdown).await {
        Ok(server) => {
            tracing::info!(
                "Agentic runner command socket ready at {}",
                server.path().display()
            );
            Ok(Some(server))
        }
        Err(e) if runner_commands::is_live_runner_command_socket_error(&e) => {
            tracing::warn!(
                "Agentic runner command socket is owned by another live runner; this generation will retry ownership later: {}",
                e
            );
            Ok(None)
        }
        Err(e) => Err(e.context("Failed to start runner command server")),
    }
}

async fn is_generation_draining(
    db: &ticketing_system::SqlitePool,
    generation_id: &str,
) -> Result<bool> {
    let status: Option<String> =
        sqlx::query_scalar("SELECT status FROM agent_runner_generations WHERE generation_id = ?")
            .bind(generation_id)
            .fetch_optional(db)
            .await
            .context("Failed to inspect agent runner generation status")?;

    Ok(status.as_deref() == Some("draining"))
}

fn log_policy(policy: &runner_capacity::RunnerCapacityPolicy) {
    if policy.mode == "fixed" {
        tracing::info!(
            "Agentic runner capacity policy fixed: max_jobs={} max_pending_jobs={} source={}",
            policy.max_jobs,
            policy.max_pending_jobs,
            policy.policy_source
        );
    } else {
        tracing::info!(
            "Agentic runner capacity policy adaptive: max_jobs={} max_pending_jobs={} claim_burst={} claim_interval_seconds={} db_idle_reserve={} max_load_per_core={:.3} source={}",
            policy.max_jobs,
            policy.max_pending_jobs,
            policy.claim_burst.unwrap_or_default(),
            policy.claim_interval_seconds.unwrap_or_default(),
            policy.db_idle_reserve.unwrap_or_default(),
            f64::from(policy.max_load_per_core_millis.unwrap_or_default()) / 1000.0,
            policy.policy_source
        );
    }
}

fn should_report_backpressure(last_backpressure_log: &mut Instant) -> bool {
    if last_backpressure_log.elapsed()
        < Duration::from_secs(RUNNER_BACKPRESSURE_LOG_INTERVAL_SECONDS)
    {
        return false;
    }
    *last_backpressure_log = Instant::now();
    true
}

fn log_backpressure(denial: &runner_capacity::RunnerClaimDenial) {
    match denial.reason.as_str() {
        "host_load" => {
            if let Some(load) = denial.host {
                tracing::warn!(
                    "Agentic runner host-load backpressure: active_jobs={} load1={:.2} max_load={:.2} cpu_count={}",
                    denial.active_jobs,
                    load.load_1m,
                    load.max_load,
                    load.cpu_count
                );
            }
        }
        "db_pool" => {
            if let Some(pool) = denial.db_pool {
                tracing::warn!(
                    "Agentic runner DB backpressure: active_jobs={} pool_size={} pool_max={} pool_idle={} idle_reserve={}",
                    denial.active_jobs,
                    pool.size,
                    pool.max_connections,
                    pool.idle,
                    pool.idle_reserve.unwrap_or_default()
                );
            }
        }
        "concurrency_cap" => tracing::debug!(
            "Agentic runner at concurrency cap: active_jobs={} max_jobs={}",
            denial.active_jobs,
            denial.max_jobs.unwrap_or_default()
        ),
        "claim_burst" => tracing::debug!(
            "Agentic runner claim burst reached: claims_this_poll={} claim_burst={}",
            denial.claims_this_poll.unwrap_or_default(),
            denial.claim_burst.unwrap_or_default()
        ),
        "claim_pacing" => tracing::debug!(
            "Agentic runner claim pacing active: active_jobs={} elapsed_ms={} claim_interval_ms={}",
            denial.active_jobs,
            denial.elapsed_ms.unwrap_or_default(),
            denial.claim_interval_ms.unwrap_or_default()
        ),
        other => tracing::debug!(
            "Agentic runner claim denied: reason={} active_jobs={}",
            other,
            denial.active_jobs
        ),
    }
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
            match runner_capacity::build_snapshot(&db).await {
                Ok(snapshot) => {
                    if let Err(e) = runner_capacity::record_capacity_sample(
                        &db,
                        Some(&generation_id),
                        &snapshot,
                    )
                    .await
                    {
                        tracing::warn!(
                            "Failed to record runner capacity sample for generation {}: {}",
                            generation_id,
                            e
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to build runner capacity sample for generation {}: {}",
                        generation_id,
                        e
                    );
                }
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
            agent_lifecycle::record_coordinator_wake_terminal(&db, &job, &status, None).await;
            Ok(status)
        }
        Err(e) => {
            let message = e.to_string();
            runtime::record_runtime_failure(
                &job.conversation_id,
                RuntimeFailurePhase::RunnerJobFailed,
                &message,
            );
            system_log_helper::log_error(
                &db,
                "agent_runner",
                "Conversation runner job failed",
                Some(&format!(
                    "job_id={}; conversation_id={}; error={}",
                    job.id, job.conversation_id, message
                )),
            )
            .await;
            let _ = checkpoints::mark_interrupted(&db, &job.conversation_id).await;
            conversation_turn_jobs::mark_job_terminal(&db, &job.id, "failed", Some(&message))
                .await?;
            agent_lifecycle::record_coordinator_wake_terminal(&db, &job, "failed", Some(&message))
                .await;
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
    let queue_to_worker_ms = worker_start_ms.saturating_sub(job.created_at * 1000);
    tracing::info!(
        target: "agentic_runner::jobs",
        event = "runner.worker_starting",
        phase = %RuntimeLatencyPhase::WorkerStarting,
        job_id = %job.id,
        conversation_id = %job.conversation_id,
        client_id = job.payload.client_id.as_deref().unwrap_or("none"),
        worker_start_ms,
        queue_to_worker_ms,
        "conversation worker starting"
    );
    runtime::record_latency_marker(
        &job.conversation_id,
        job.payload.client_id.as_deref(),
        RuntimeLatencyPhase::WorkerStarting,
        queue_to_worker_ms as u64,
        worker_start_ms,
        None,
        None,
    );
    let (tx, rx) = mpsc::channel(1);
    tx.send(worker_message)
        .await
        .map_err(|_| anyhow::anyhow!("failed to send job to conversation worker"))?;
    drop(tx);

    let worker = ConversationWorker::new(db.clone(), job.conversation_id.clone(), manager, rx);
    worker.run().await;
    let worker_finished_ms = Utc::now().timestamp_millis();
    let worker_duration_ms = worker_finished_ms.saturating_sub(worker_start_ms);
    tracing::info!(
        target: "agentic_runner::jobs",
        event = "runner.worker_finished",
        phase = %RuntimeLatencyPhase::WorkerFinished,
        job_id = %job.id,
        conversation_id = %job.conversation_id,
        client_id = job.payload.client_id.as_deref().unwrap_or("none"),
        finished_at_ms = worker_finished_ms,
        worker_duration_ms,
        "conversation worker finished"
    );
    runtime::record_latency_marker(
        &job.conversation_id,
        job.payload.client_id.as_deref(),
        RuntimeLatencyPhase::WorkerFinished,
        worker_duration_ms as u64,
        worker_finished_ms,
        None,
        None,
    );

    if let Some(terminal) = latest_terminal_event_status(&db, &job.conversation_id).await? {
        tracing::warn!(
            target: "agentic_runner::jobs",
            event = "runner.terminal_event_observed",
            job_id = %job.id,
            conversation_id = %job.conversation_id,
            client_id = job.payload.client_id.as_deref().unwrap_or("none"),
            event_index = terminal.event_index,
            event_type = %terminal.event_type,
            status = terminal.status,
            "conversation job observed terminal event after worker finish"
        );
        if terminal.status == "failed" {
            anyhow::bail!(
                "{}",
                terminal.message.unwrap_or_else(|| {
                    "conversation worker emitted failed terminal event".to_string()
                })
            );
        }
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
            terminal_event_details_from_event(&event_type, &event_data).map(|details| {
                TerminalEventStatus {
                    status: details.status,
                    event_index,
                    event_type,
                    message: details.message,
                }
            })
        }))
}

struct TerminalEventStatus {
    status: &'static str,
    event_index: i64,
    event_type: String,
    message: Option<String>,
}

#[cfg(test)]
fn terminal_status_from_event(event_type: &str, event_data: &str) -> Option<&'static str> {
    terminal_event_details_from_event(event_type, event_data).map(|details| details.status)
}

struct TerminalEventDetails {
    status: &'static str,
    message: Option<String>,
}

fn terminal_event_details_from_event(
    event_type: &str,
    event_data: &str,
) -> Option<TerminalEventDetails> {
    let value: serde_json::Value = serde_json::from_str(event_data).ok()?;
    if event_type == "error"
        || value.get("type").and_then(serde_json::Value::as_str) == Some("error")
    {
        return Some(TerminalEventDetails {
            status: "failed",
            message: terminal_message_from_event(&value),
        });
    }

    if let Some(status) = value.get("status").and_then(serde_json::Value::as_str) {
        return match status {
            "failed" | "timeout" => Some(TerminalEventDetails {
                status: "failed",
                message: terminal_message_from_event(&value),
            }),
            "cancelled" => Some(TerminalEventDetails {
                status: "cancelled",
                message: terminal_message_from_event(&value),
            }),
            _ => None,
        };
    }

    value
        .get("delta")
        .and_then(|delta| delta.get("stop_reason"))
        .and_then(serde_json::Value::as_str)
        .and_then(|stop_reason| match stop_reason {
            "cancelled" => Some(TerminalEventDetails {
                status: "cancelled",
                message: terminal_message_from_event(&value),
            }),
            _ => None,
        })
}

fn terminal_message_from_event(value: &serde_json::Value) -> Option<String> {
    value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| value.get("message").and_then(serde_json::Value::as_str))
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .map(ToOwned::to_owned)
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
    agentic_api::fable_coordinator::validate_runtime_assignment(
        &conversation,
        job.payload.agent_type == agentic_api::fable_coordinator::FABLE_AGENT,
    )?;
    Ok(())
}

fn worker_message_from_job(
    job: &conversation_turn_jobs::ConversationTurnJob,
) -> Result<WorkerMessage> {
    let payload = &job.payload;
    let runtime = match payload.runtime.as_str() {
        value if value == ChatRuntime::CodexAppServer.as_job_runtime() => {
            ChatRuntime::CodexAppServer
        }
        value if value == ChatRuntime::ClaudeCodeFable.as_job_runtime() => {
            ChatRuntime::ClaudeCodeFable
        }
        _ => anyhow::bail!("Unsupported conversation job runtime: {}", payload.runtime),
    };

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
            runtime,
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
        "research" => Ok("research"),
        "polymarket" => Ok("polymarket"),
        "daily-research" => Ok("daily-research"),
        "package-update-review" => Ok("package-update-review"),
        "research-synthesis" => Ok("research-synthesis"),
        "ticket-planner" => Ok("ticket-planner"),
        "ticket-creator" => Ok("ticket-creator"),
        "doc-drafter" => Ok("doc-drafter"),
        "pull-ticket" => Ok("pull-ticket"),
        "codebase-research" => Ok("codebase-research"),
        "doc-manager" => Ok("doc-manager"),
        "fable-coordinator" => Ok("fable-coordinator"),
        other => anyhow::bail!("Unsupported conversation job prompt: {}", other),
    }
}

#[cfg(test)]
mod tests {
    use super::{prompt_name_static, terminal_status_from_event};

    #[test]
    fn runner_accepts_canonical_research_prompt() {
        assert_eq!(
            prompt_name_static("research").expect("research prompt"),
            "research"
        );
    }

    #[test]
    fn runner_accepts_only_the_canonical_fable_prompt_name() {
        assert_eq!(
            prompt_name_static("fable-coordinator").expect("Fable coordinator prompt"),
            "fable-coordinator"
        );
        assert!(prompt_name_static("fable").is_err());
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
