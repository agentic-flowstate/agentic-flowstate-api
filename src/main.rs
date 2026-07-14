mod agents;
pub mod apns;
mod auth_middleware;
mod dailies_scheduler;
mod daily_actions;
mod disk_pressure;
mod email_attachment_safety;
mod email_classifier;
mod email_delivery;
mod email_fetcher;
mod email_intake_scheduler;
mod email_notification_dispatcher;
mod email_threading;
mod email_tracking_consumer;
mod fable_coordinator;
mod handlers;
mod health_monitor;
mod inbound_outreach;
mod log_incident_triage;
mod mcp_wrapper;
mod models;
mod observability;
mod outreach_compliance;
mod package_updates;
mod rate_limiting;
mod request_logger;
mod retention;
pub mod safety;
mod secret_cipher;
mod ses_event_quarantine;
pub mod system_log_helper;

mod runner_commands {
    pub use agentic_api::runner_commands::*;
}

use axum::{
    extract::{DefaultBodyLimit, FromRef},
    routing::{delete, get, patch, post},
    Router,
};
use std::sync::Arc;
use tokio::signal;
use tokio_util::sync::CancellationToken;
use tower_cookies::CookieManagerLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use handlers::chat_client_manager::ChatClientManager;

const RUNNER_HEARTBEAT_STALE_SECONDS: i64 = 90;
const RUNNER_HEARTBEAT_INTERVAL_SECONDS: u64 = 15;
const RUNNER_RECONCILE_INTERVAL_SECONDS: u64 = 60;

/// Shared application state.
/// Implements `FromRef` so handlers can extract individual components.
///
/// NOTE: the legacy alert-style `apns::ApnsService` is deliberately NOT
/// held here. Its `init()` call populates a process-wide `OnceCell`
/// (`APNS_INSTANCE`) that all downstream callers access via
/// `ApnsService::global()`, so a separate AppState field would just be
/// a dead Arc clone. The silent-push sender below IS kept on AppState
/// because handlers extract it via `FromRef`.
#[derive(Clone)]
struct AppState {
    db: Arc<sqlx::SqlitePool>,
    chat_manager: Arc<ChatClientManager>,
    /// Silent-push sender (apns-h2 based) used by the durable chat
    /// streaming pipeline. Always present in app state; internally holds
    /// an `OnceCell` that is only populated when APNS_SILENT_ENABLED=true
    /// at startup. Handlers must call `send_silent_push` and handle
    /// `ApnsSilentError::NotInitialized` when silent push is disabled.
    apns_silent: Arc<apns::ApnsClient>,
    /// Per-(user, conversation) SSE rate limiter (T-C410DD96).
    /// Holds every active [`rate_limiting::StreamPermit`] on its backing
    /// DashMap. Enforces the hard concurrency cap and the reconnect-
    /// per-window sliding cap for both [`handlers::resume_stream`] and
    /// [`handlers::chat_stream`] opens. See [`rate_limiting`] for
    /// algorithm details and the cleanup task.
    rate_limiter: Arc<rate_limiting::StreamRateLimiter>,
}

impl FromRef<AppState> for Arc<sqlx::SqlitePool> {
    fn from_ref(state: &AppState) -> Self {
        state.db.clone()
    }
}

impl FromRef<AppState> for Arc<ChatClientManager> {
    fn from_ref(state: &AppState) -> Self {
        state.chat_manager.clone()
    }
}

impl FromRef<AppState> for Arc<apns::ApnsClient> {
    fn from_ref(state: &AppState) -> Self {
        state.apns_silent.clone()
    }
}

impl FromRef<AppState> for Arc<rate_limiting::StreamRateLimiter> {
    fn from_ref(state: &AppState) -> Self {
        state.rate_limiter.clone()
    }
}

fn env_flag_enabled(name: &str) -> bool {
    std::env::var(name)
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes"))
        .unwrap_or(false)
}

fn launchd_restart_command(uid: &str, label: &str) -> String {
    format!(
        "echo \"[$(date -u +%Y-%m-%dT%H:%M:%SZ)] restart_watcher: kickstart {label}\"\n\
         launchctl kickstart -k gui/{uid}/{label} 2>&1\n\
         rc=$?\n\
         if [ \"$rc\" -ne 0 ]; then\n\
             echo \"[$(date -u +%Y-%m-%dT%H:%M:%SZ)] restart_watcher: kickstart failed for {label} rc=$rc\"\n\
             exit \"$rc\"\n\
         fi\n\
         sleep 1\n\
         launchctl print gui/{uid}/{label} >/dev/null 2>&1\n\
         rc=$?\n\
         if [ \"$rc\" -ne 0 ]; then\n\
             echo \"[$(date -u +%Y-%m-%dT%H:%M:%SZ)] restart_watcher: {label} not loaded after kickstart rc=$rc\"\n\
             exit \"$rc\"\n\
         fi",
        uid = uid,
        label = label
    )
}

fn launchd_runner_handoff_command(uid: &str) -> String {
    format!(
        r#"echo "[$(date -u +%Y-%m-%dT%H:%M:%SZ)] restart_watcher: handoff bootstrap com.agentic.runner"
runner_plist="${{HOME:-/Users/jarvisgpt}}/Library/LaunchAgents/com.agentic.runner.plist"
if [ ! -f "$runner_plist" ]; then
    echo "[$(date -u +%Y-%m-%dT%H:%M:%SZ)] restart_watcher: runner plist missing at $runner_plist"
    exit 1
fi
handoff_label="com.agentic.runner.handoff.$(date +%s).$$"
handoff_plist="/tmp/${{handoff_label}}.plist"
cp "$runner_plist" "$handoff_plist"
plutil -replace Label -string "$handoff_label" "$handoff_plist"
plutil -replace KeepAlive -bool NO "$handoff_plist"
plutil -replace RunAtLoad -bool true "$handoff_plist"
launchctl bootstrap gui/{uid} "$handoff_plist" 2>&1
rc=$?
if [ "$rc" -ne 0 ]; then
    echo "[$(date -u +%Y-%m-%dT%H:%M:%SZ)] restart_watcher: runner handoff bootstrap failed rc=$rc label=$handoff_label"
    exit "$rc"
fi
echo "[$(date -u +%Y-%m-%dT%H:%M:%SZ)] restart_watcher: runner handoff bootstrap started label=$handoff_label plist=$handoff_plist""#,
        uid = uid
    )
}

fn spawn_direct_restart_or_setup(service: &str, action: &str) {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/jarvisgpt".to_string());
    let log = "/tmp/agentic-restart-watcher.log";
    let script = if action == "setup" {
        format!(
            r#"exec > '{log}' 2>&1
echo "[$(date -u +%Y-%m-%dT%H:%M:%SZ)] restart_watcher: setup started"
exec bash -l '{home}/projects/agentic-flowstate/agentic-flowstate-setup/setup.sh'
"#,
            home = home,
            log = log
        )
    } else {
        let uid = std::process::Command::new("id")
            .arg("-u")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|_| "501".to_string());
        let mut commands = Vec::new();
        if matches!(service, "api-server" | "all") {
            commands.push(launchd_restart_command(&uid, "com.agentic.api"));
        }
        if matches!(service, "agent-runner" | "all") {
            commands.push(launchd_runner_handoff_command(&uid));
        }
        if matches!(service, "mcp-server" | "all") {
            commands.push(launchd_restart_command(&uid, "com.agentic.mcp"));
        }

        if commands.is_empty() {
            tracing::error!(
                target: "agentic_api::restart_watcher",
                event = "restart_watcher.unknown_service",
                service,
                action,
                "refusing direct restart for unknown service"
            );
            return;
        }

        format!(
            r#"exec > '{log}' 2>&1
echo "[$(date -u +%Y-%m-%dT%H:%M:%SZ)] restart_watcher: direct restart started for {service}"
{commands}
echo "[$(date -u +%Y-%m-%dT%H:%M:%SZ)] restart_watcher: direct restart complete"
"#,
            log = log,
            service = service,
            commands = commands.join("\n")
        )
    };

    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new("bash");
    cmd.args(["-c", &script])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    if let Err(e) = cmd.spawn() {
        tracing::error!(
            target: "agentic_api::restart_watcher",
            event = "restart_watcher.spawn_failed",
            service,
            action,
            error = %e,
            "failed to spawn direct restart"
        );
    }
}

async fn log_ops_error(
    pool: &Arc<sqlx::SqlitePool>,
    component: &str,
    event: &'static str,
    message: &str,
    error: impl std::fmt::Display,
) {
    let detail = error.to_string();
    tracing::error!(
        target: "agentic_api::ops",
        event,
        component,
        error = %detail,
        "{}", message
    );
    system_log_helper::log_error(pool, component, message, Some(&detail)).await;
}

async fn log_ops_warn(
    pool: &Arc<sqlx::SqlitePool>,
    component: &str,
    event: &'static str,
    message: &str,
    error: impl std::fmt::Display,
) {
    let detail = error.to_string();
    tracing::warn!(
        target: "agentic_api::ops",
        event,
        component,
        error = %detail,
        "{}", message
    );
    system_log_helper::log_warn(pool, component, message, Some(&detail)).await;
}

async fn mark_matching_pending_restarts_executed(
    pool: &sqlx::SqlitePool,
    service: &str,
    action: &str,
) -> anyhow::Result<u64> {
    let now = chrono::Utc::now().timestamp();
    let result = sqlx::query(
        r#"
        UPDATE restart_queue
        SET status = 'executed', executed_at = ?
        WHERE status = 'pending'
          AND service = ?
          AND action = ?
          AND requested_at <= ?
        "#,
    )
    .bind(now)
    .bind(service)
    .bind(action)
    .bind(now)
    .execute(pool)
    .await?;

    Ok(result.rows_affected())
}

fn restart_watcher_should_exit_after_spawn(service: &str, action: &str) -> bool {
    action == "setup" || matches!(service, "api-server" | "all")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_command_uses_kickstart_without_deregistering_service() {
        let command = launchd_restart_command("501", "com.agentic.api");

        assert!(command.contains("launchctl kickstart -k gui/501/com.agentic.api"));
        assert!(command.contains("launchctl print gui/501/com.agentic.api"));
        assert!(!command.contains("launchctl bootout"));
        assert!(!command.contains("launchctl bootstrap"));
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "agentic_api=debug,tower_http=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    tracing::info!("Starting Agentic API Server...");

    // Install the Prometheus exporter BEFORE any subsystem emits a
    // metric. T-56987678: every call to the `metrics` facade prior to
    // install is silently dropped, so we run this first. Double-install
    // is an error — we log and continue rather than crash, in case a
    // downstream integration test already installed a recorder.
    match observability::install_prometheus_exporter() {
        Ok(()) => tracing::info!("Prometheus exporter installed"),
        Err(e) => {
            tracing::warn!(error = %e, "Prometheus exporter install failed — /metrics may be stale")
        }
    }

    // Initialize MCP handler
    mcp_wrapper::init_mcp_handler().await?;
    tracing::info!("MCP handler initialized");

    // Initialize SQLite database pool
    let db_pool = Arc::new(ticketing_system::init_db().await?);
    fable_coordinator::ensure_schema(&db_pool).await?;
    tracing::info!("SQLite database pool initialized");

    match ticketing_system::daily_action_executions::reconcile_daily_action_executions(&db_pool)
        .await
    {
        Ok(report) if report.execution_transitions > 0 || report.job_repairs > 0 => {
            tracing::warn!(
                component = "dailies_api",
                operation = "daily_action.startup_reconciled",
                inspected = report.inspected,
                execution_transitions = report.execution_transitions,
                job_repairs = report.job_repairs,
                checkpoint_repairs = report.checkpoint_repairs,
                unrecoverable_link_mismatches = report.unrecoverable_link_mismatches,
                "reconciled durable Daily state at API startup"
            );
        }
        Ok(_) => tracing::debug!("No durable Daily state required startup reconciliation"),
        Err(error) => {
            log_ops_error(
                &db_pool,
                "dailies",
                "daily_action.startup_reconcile_failed",
                "Failed to reconcile durable Daily state at startup",
                &error,
            )
            .await;
        }
    }
    if let Err(error) =
        ticketing_system::daily_action_executions::trace_daily_action_diagnostics(&db_pool).await
    {
        log_ops_error(
            &db_pool,
            "dailies",
            "daily_action.startup_diagnostics_failed",
            "Failed to aggregate durable Daily diagnostics at startup",
            &error,
        )
        .await;
    }

    match ticketing_system::agent_runners::reconcile_stale_runner_generations(
        &db_pool,
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
        Ok(_) => {
            tracing::debug!("No stale agent runner generation metadata to reconcile");
        }
        Err(e) => {
            log_ops_error(
                &db_pool,
                "agent_runner",
                "startup.runner_generation_reconcile_failed",
                "Failed to reconcile stale runner generation metadata",
                &e,
            )
            .await;
        }
    }

    // Mark active checkpoints as interrupted only when no fresh runner
    // generation still owns them. This preserves the future split-runner
    // contract: API restart must not automatically kill turns owned by a
    // still-heartbeating runner generation.
    match ticketing_system::agent_runners::mark_unowned_active_checkpoints_interrupted(
        &db_pool,
        RUNNER_HEARTBEAT_STALE_SECONDS,
    )
    .await
    {
        Ok(count) if count > 0 => {
            tracing::warn!(
                "Marked {} unowned agent checkpoint(s) interrupted from previous run",
                count
            );
        }
        Ok(_) => {
            tracing::debug!("No unowned interrupted agent checkpoints to clean up");
        }
        Err(e) => {
            log_ops_error(
                &db_pool,
                "agent_runner",
                "startup.checkpoint_cleanup_failed",
                "Failed to clean up interrupted checkpoints",
                &e,
            )
            .await;
        }
    }

    // Mark any interrupted agent runs from previous run (killed by server restart)
    match ticketing_system::agent_runs::mark_all_running_as_interrupted(&db_pool).await {
        Ok(count) if count > 0 => {
            tracing::warn!(
                "Marked {} interrupted agent run(s) as failed from previous run",
                count
            );
        }
        Ok(_) => {
            tracing::debug!("No interrupted agent runs to clean up");
        }
        Err(e) => {
            log_ops_error(
                &db_pool,
                "agent",
                "startup.agent_run_cleanup_failed",
                "Failed to clean up interrupted agent runs",
                &e,
            )
            .await;
        }
    }

    // Mark any interrupted nightly runs from previous run
    match ticketing_system::nightly_runs::mark_running_as_failed(&db_pool).await {
        Ok(count) if count > 0 => {
            tracing::warn!(
                "Marked {} interrupted nightly run(s) as failed from previous run",
                count
            );
        }
        Ok(_) => {
            tracing::debug!("No interrupted nightly runs to clean up");
        }
        Err(e) => {
            log_ops_error(
                &db_pool,
                "nightly",
                "startup.nightly_run_cleanup_failed",
                "Failed to clean up interrupted nightly runs",
                &e,
            )
            .await;
        }
    }

    match ticketing_system::dailies::mark_running_runs_failed(&db_pool).await {
        Ok(count) if count > 0 => {
            tracing::warn!(
                "Marked {} interrupted daily run(s) as failed from previous run",
                count
            );
        }
        Ok(_) => {
            tracing::debug!("No interrupted daily runs to clean up");
        }
        Err(e) => {
            log_ops_error(
                &db_pool,
                "dailies",
                "startup.daily_run_cleanup_failed",
                "Failed to clean up interrupted daily runs",
                &e,
            )
            .await;
        }
    }

    // Pending restart entries are durable on purpose. If the API restarts while
    // a runner restart is queued, the watcher below must resume that queue
    // instead of deleting it and losing the drain request.
    match ticketing_system::restart_queue::get_pending_restart(&db_pool).await {
        Ok(Some(entry)) => {
            tracing::info!(
                "Preserving pending restart {} for service '{}' requested at {}",
                entry.id,
                entry.service,
                entry.requested_at
            );
        }
        Ok(None) => {}
        Err(e) => {
            log_ops_warn(
                &db_pool,
                "restart_watcher",
                "startup.pending_restart_inspect_failed",
                "Failed to inspect pending restart queue",
                &e,
            )
            .await;
        }
    }

    // Create shutdown token for coordinated cancellation of all background tasks.
    // When cancelled, all tasks using a child token will break out of their loops.
    let shutdown_token = CancellationToken::new();

    // Register the shutdown token with the SSE keepalive subsystem so active
    // SSE streams drain cleanly on SIGTERM / Ctrl+C with a
    // `message:end reason:server_shutdown` frame (T-CFFAF032). Handlers pull
    // this token via `handlers::sse_keepalive::shutdown_token()` without
    // needing to plumb AppState through — the keepalive wrapper is invoked
    // from many call sites (chat_stream, resume_stream, full_access_chat,
    // home_planner, workspace_manager, ...) and a global install keeps the
    // signature stable.
    if handlers::sse_keepalive::install_shutdown_token(shutdown_token.clone()).is_err() {
        tracing::warn!(
            target: "agentic_api::sse_keepalive",
            event = "sse_keepalive.shutdown_token_already_installed",
            "shutdown token already installed"
        );
    } else {
        tracing::info!(
            target: "agentic_api::sse_keepalive",
            event = "sse_keepalive.shutdown_token_registered",
            "shutdown token registered"
        );
    }

    spawn_runner_generation_reconciler(db_pool.clone(), shutdown_token.child_token());

    // Start email fetcher background task (queries email_accounts table each cycle)
    tracing::info!("Starting email fetcher (hot-reload from database)");
    email_fetcher::start_email_fetcher(db_pool.clone(), shutdown_token.child_token());

    tracing::info!("Starting SES outreach event consumer");
    email_tracking_consumer::spawn(db_pool.clone(), shutdown_token.child_token());

    tracing::info!("Starting email intake scheduler");
    email_intake_scheduler::spawn_email_intake_scheduler(
        db_pool.clone(),
        shutdown_token.child_token(),
    );

    tracing::info!("Starting email classifier scheduler");
    email_classifier::spawn_email_classifier_scheduler(
        db_pool.clone(),
        shutdown_token.child_token(),
    );

    // Nightly scheduler DISABLED — do not re-enable until conversation integration is validated.
    tracing::info!("Nightly scheduler is DISABLED");

    tracing::info!("Starting Dailies scheduler");
    dailies_scheduler::spawn_dailies_scheduler(db_pool.clone(), shutdown_token.child_token());

    tracing::info!("Starting system log incident triage scheduler");
    log_incident_triage::spawn_system_log_incident_triage(
        db_pool.clone(),
        shutdown_token.child_token(),
    );

    // Conversation-events retention prune (T-65DA4D32). Fires once per
    // day at RETENTION_RUN_HOUR_UTC (default 03:00 UTC). Set
    // RETENTION_RUN_ON_STARTUP=1 to also run immediately on boot. Tunables
    // (age_days, min_keep, batch_size) are read from env with safe
    // defaults — see `retention::RetentionConfig::from_env`.
    {
        let retention_config = retention::RetentionConfig::from_env();
        tracing::info!(
            "Retention scheduler starting (age_days={}, min_keep={}, batch_size={})",
            retention_config.age_days,
            retention_config.min_keep_per_conversation,
            retention_config.batch_size
        );
        retention::scheduler::spawn_retention_loop(
            db_pool.clone(),
            retention_config,
            shutdown_token.child_token(),
        );
    }

    // Create chat client manager for live Codex app-server turns.
    let chat_manager = Arc::new(ChatClientManager::new());
    let runner_generation_id = chat_manager.runner_generation_id().to_string();
    ticketing_system::agent_runners::register_generation(
        &db_pool,
        &runner_generation_id,
        "api-embedded",
        env!("CARGO_PKG_VERSION"),
        std::process::id() as i64,
    )
    .await?;
    tracing::info!(
        "Chat client manager initialized with runner generation {}",
        runner_generation_id
    );

    {
        let runner_pool = db_pool.clone();
        let runner_shutdown = shutdown_token.child_token();
        let heartbeat_generation_id = runner_generation_id.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(
                RUNNER_HEARTBEAT_INTERVAL_SECONDS,
            ));
            loop {
                tokio::select! {
                    _ = runner_shutdown.cancelled() => break,
                    _ = interval.tick() => {}
                }
                if let Err(e) = ticketing_system::agent_runners::heartbeat_generation(
                    &runner_pool,
                    &heartbeat_generation_id,
                )
                .await
                {
                    tracing::warn!(
                        target: "agentic_api::runner",
                        event = "runner.heartbeat_failed",
                        generation_id = %heartbeat_generation_id,
                        error = %e,
                        "failed to heartbeat runner generation"
                    );
                }
            }
        });
    }

    // Initialize user-visible APNs alert pushes for completed conversations.
    // This is gated separately from silent background pushes because alert
    // delivery is a user-facing product path: when enabled, every APNS_* value
    // is required and startup fails loudly on misconfiguration.
    let alert_enabled = env_flag_enabled("APNS_ALERT_ENABLED");
    if alert_enabled {
        if let Err(e) = apns::ApnsService::init_from_env() {
            log_ops_error(
                &db_pool,
                "apns",
                "startup.apns_alert_init_failed",
                "APNs alert push init failed",
                &e,
            )
            .await;
            return Err(anyhow::anyhow!("APNs alert push init failed: {}", e));
        }
        tracing::info!(
            target: "agentic_api::apns",
            event = "apns_alert.initialized",
            "APNs alert push initialized"
        );
    } else {
        tracing::info!(
            target: "agentic_api::apns",
            event = "apns_alert.disabled",
            "APNs alert push disabled"
        );
    }

    tracing::info!(
        component = "disk_pressure",
        operation = "disk_pressure.monitor_starting",
        "starting automatic disk pressure monitor"
    );
    disk_pressure::spawn_disk_pressure_monitor(db_pool.clone(), shutdown_token.child_token())
        .await?;

    tracing::info!("Starting email notification dispatcher");
    email_notification_dispatcher::spawn_email_notification_dispatcher(
        db_pool.clone(),
        shutdown_token.child_token(),
    );

    // Initialize silent-push sender (durable chat streaming wake signals).
    // Gated on APNS_SILENT_ENABLED to avoid forcing every dev box to have a
    // provisioned .p8 key. When enabled, ALL five env vars are required —
    // init() returns an error that we propagate via `?`, crashing startup.
    let apns_silent = Arc::new(apns::ApnsClient::new());
    let silent_enabled = env_flag_enabled("APNS_SILENT_ENABLED");
    if silent_enabled {
        let cfg = match apns_silent.init_from_env() {
            Ok(cfg) => cfg,
            Err(e) => {
                log_ops_error(
                    &db_pool,
                    "apns",
                    "startup.apns_silent_init_failed",
                    "APNs silent push init failed",
                    &e,
                )
                .await;
                return Err(anyhow::anyhow!("APNs silent push init failed: {}", e));
            }
        };
        tracing::info!(
            target: "agentic_api::apns",
            event = "apns_silent.initialized",
            bundle_id = %cfg.bundle_id,
            sandbox = cfg.use_sandbox,
            team_id = %cfg.team_id,
            key_id = %cfg.key_id,
            "APNs silent push initialized"
        );
    } else {
        tracing::info!(
            target: "agentic_api::apns",
            event = "apns_silent.disabled",
            "APNs silent push disabled"
        );
    }

    // Register the silent-push sender in the process-wide registry so the
    // ConversationWorker fan-out hook (T-90C7FAC4) can access it without
    // plumbing AppState through WORKER_MANAGER. The global captures both
    // the sender Arc and the enabled flag — when disabled the fan-out
    // still emits `push_attempts_total{result="skipped_disabled"}` for
    // every registered device token so dashboards stay accurate.
    match apns::silent_fanout::init_global(apns::silent_fanout::SilentPushConfig {
        enabled: silent_enabled,
        sender: apns_silent.clone(),
    }) {
        Ok(()) => tracing::info!(
            target: "agentic_api::apns",
            event = "apns_silent.fanout_registry_installed",
            enabled = silent_enabled,
            "APNs silent fan-out registry installed"
        ),
        Err(e) => tracing::warn!(
            target: "agentic_api::apns",
            event = "apns_silent.fanout_registry_already_initialized",
            error = %e,
            "APNs silent fan-out registry already initialized"
        ),
    }

    // Build the per-(user, conversation) SSE rate limiter (T-C410DD96).
    // Config is read from env with fail-loud parsing — an invalid
    // RATE_LIMIT_* env var panics here so the operator sees the error
    // immediately. Defaults: 1 concurrent stream, 10 reconnects / 60s.
    let rate_limit_config = rate_limiting::StreamRateLimitConfig::from_env();
    tracing::info!(
        target: "agentic_api::rate_limit",
        event = "rate_limit.stream_config_loaded",
        max_concurrent_streams = rate_limit_config.max_concurrent_streams,
        max_reconnects_per_window = rate_limit_config.max_reconnects_per_window,
        window_seconds = rate_limit_config.window_duration.as_secs(),
        "stream rate limiter config loaded"
    );
    let rate_limiter = Arc::new(rate_limiting::StreamRateLimiter::new(rate_limit_config));
    // Global install so chat_stream::chat (which does not yet receive
    // AppState via parameter) can resolve the limiter. Handlers that DO
    // already have State<...> in scope still pull from AppState —
    // the global just covers the chat-handler call path.
    if rate_limiting::install_global(rate_limiter.clone()).is_err() {
        tracing::warn!(
            target: "agentic_api::rate_limit",
            event = "rate_limit.global_limiter_already_installed",
            "global limiter already installed"
        );
    }
    // Cleanup task: prune idle buckets every 5 minutes.
    rate_limiting::spawn_cleanup_task(rate_limiter.clone(), shutdown_token.child_token());

    let app_state = AppState {
        db: db_pool.clone(),
        chat_manager,
        apns_silent,
        rate_limiter,
    };

    // Clone db_pool for shutdown handler before building router (which moves app_state)
    let shutdown_db = db_pool.clone();
    let shutdown_runner_generation_id = runner_generation_id.clone();

    // Session cleanup background task (every 6 hours)
    {
        let cleanup_pool = db_pool.clone();
        let token = shutdown_token.child_token();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(6 * 60 * 60));
            loop {
                tokio::select! {
                    _ = token.cancelled() => break,
                    _ = interval.tick() => {}
                }
                match ticketing_system::auth::cleanup_expired_sessions(&cleanup_pool).await {
                    Ok(count) if count > 0 => {
                        tracing::info!("Cleaned up {} expired session(s)", count);
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::error!("Session cleanup error: {:?}", e);
                    }
                }
            }
        });
    }

    // System log cleanup background task (every 24 hours, keep 30 days)
    {
        let cleanup_pool = db_pool.clone();
        let token = shutdown_token.child_token();
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(tokio::time::Duration::from_secs(24 * 60 * 60));
            loop {
                tokio::select! {
                    _ = token.cancelled() => break,
                    _ = interval.tick() => {}
                }
                let cutoff = chrono::Utc::now().timestamp() - 30 * 24 * 60 * 60;
                match ticketing_system::system_logs::delete_logs_before(&cleanup_pool, cutoff).await
                {
                    Ok(count) if count > 0 => {
                        tracing::info!("Cleaned up {} old system log(s)", count);
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::error!("System log cleanup error: {:?}", e);
                    }
                }
            }
        });
    }

    // Client events cleanup background task (every 24 hours, keep 90 days)
    {
        let cleanup_pool = db_pool.clone();
        let token = shutdown_token.child_token();
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(tokio::time::Duration::from_secs(24 * 60 * 60));
            loop {
                tokio::select! {
                    _ = token.cancelled() => break,
                    _ = interval.tick() => {}
                }
                let cutoff = chrono::Utc::now().timestamp() - 90 * 24 * 60 * 60;
                match ticketing_system::client_events::delete_client_events_before(
                    &cleanup_pool,
                    cutoff,
                )
                .await
                {
                    Ok(count) if count > 0 => {
                        tracing::info!("Cleaned up {} old client event(s)", count);
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::error!("Client event cleanup error: {:?}", e);
                    }
                }
            }
        });
    }

    // Deferred restart watcher: polls every 10 seconds for queued restarts.
    // When a restart is pending AND no active runner-owned work remains,
    // executes the restart. ChatLab checkpoints and Codex app-server turns
    // block restarts just like agent_runs.
    {
        let restart_pool = db_pool.clone();
        let restart_shutdown = shutdown_token.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(10));
            loop {
                tokio::select! {
                    _ = restart_shutdown.cancelled() => break,
                    _ = interval.tick() => {}
                }

                // Check for pending restarts
                let pending =
                    match ticketing_system::restart_queue::get_pending_restart(&restart_pool).await
                    {
                        Ok(Some(entry)) => entry,
                        Ok(None) => continue,
                        Err(e) => {
                            log_ops_warn(
                                &restart_pool,
                                "restart_watcher",
                                "restart_watcher.queue_check_failed",
                                "Failed to check restart queue",
                                &e,
                            )
                            .await;
                            continue;
                        }
                    };

                // Pending restart found — check only work this service restart would disrupt.
                // Runner-owned turns are preserved by the runner handoff bootstrap path.
                let active = match ticketing_system::restart_queue::count_restart_blocking_work(
                    &restart_pool,
                    &pending.service,
                )
                .await
                {
                    Ok(a) => a,
                    Err(e) => {
                        log_ops_warn(
                            &restart_pool,
                            "restart_watcher",
                            "restart_watcher.active_work_count_failed",
                            "Failed to count restart-blocking active work",
                            &e,
                        )
                        .await;
                        continue;
                    }
                };

                if active.total > 0 {
                    tracing::debug!(
                        target: "agentic_api::restart_watcher",
                        event = "restart_watcher.waiting_for_active_work",
                        service = %pending.service,
                        action = %pending.action,
                        active_total = active.total,
                        agent_run_count = active.agent_run_count,
                        runner_turn_count = active.runner_turn_count,
                        checkpoint_count = active.checkpoint_count,
                        "restart queued but active work remains"
                    );
                    continue;
                }

                tracing::info!(
                    target: "agentic_api::restart_watcher",
                    event = "restart_watcher.executing_deferred_restart",
                    service = %pending.service,
                    action = %pending.action,
                    requested_by = ?pending.requested_by,
                    "executing deferred restart"
                );

                match mark_matching_pending_restarts_executed(
                    &restart_pool,
                    &pending.service,
                    &pending.action,
                )
                .await
                {
                    Ok(count) if count > 1 => {
                        tracing::info!(
                            target: "agentic_api::restart_watcher",
                            event = "restart_watcher.coalesced_pending_requests",
                            coalesced_count = count,
                            service = %pending.service,
                            action = %pending.action,
                            "coalesced pending restart requests"
                        );
                    }
                    Ok(_) => {}
                    Err(e) => {
                        log_ops_warn(
                            &restart_pool,
                            "restart_watcher",
                            "restart_watcher.coalesce_failed",
                            "Failed to coalesce restart queue entries",
                            format!("entry_id={}; error={}", pending.id, e),
                        )
                        .await;
                        let _ = ticketing_system::restart_queue::mark_executed(
                            &restart_pool,
                            pending.id,
                        )
                        .await;
                    }
                }

                system_log_helper::log_info(
                    &restart_pool,
                    "restart_watcher",
                    &format!(
                        "Agents finished; executing {} for '{}' directly",
                        pending.action, pending.service
                    ),
                    None,
                )
                .await;

                spawn_direct_restart_or_setup(&pending.service, &pending.action);

                if restart_watcher_should_exit_after_spawn(&pending.service, &pending.action) {
                    break;
                }
            }
        });
    }

    // Uptime health monitor background task (every 6 hours)
    // Checks configured endpoints across all orgs, creates bug tickets on failure.
    {
        let monitor_pool = db_pool.clone();
        let token = shutdown_token.child_token();
        tokio::spawn(async move {
            // Wait 30 seconds after startup before first check (let services settle)
            tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(6 * 60 * 60));
            loop {
                tokio::select! {
                    _ = token.cancelled() => break,
                    _ = interval.tick() => {}
                }
                health_monitor::run_checks(&monitor_pool).await;
            }
        });
    }

    // Mark service as ready now that all initialization is complete
    handlers::health::set_ready();

    // Public routes (no auth required)
    let public_routes = Router::new()
        .route("/api/auth/register", post(handlers::auth::register))
        .route("/api/auth/login", post(handlers::auth::login))
        .route(
            "/api/auth/passkeys/config",
            get(handlers::passkeys::passkey_config),
        )
        .route(
            "/api/auth/passkeys/authenticate/start",
            post(handlers::passkeys::start_passkey_authentication),
        )
        .route(
            "/api/auth/passkeys/authenticate/finish",
            post(handlers::passkeys::finish_passkey_authentication),
        )
        .route("/api/auth/logout", post(handlers::auth::logout))
        .route("/api/auth/me", get(handlers::auth::me))
        .route(
            "/api/auth/users/public",
            get(handlers::auth::list_public_users),
        )
        .route(
            "/api/auth/setup-code/redeem",
            post(handlers::auth::redeem_setup_code),
        )
        .route("/health", get(|| async { "OK" }))
        .route("/health/ready", get(handlers::health::ready))
        // Prometheus scrape endpoint (T-56987678). Public because the
        // metrics contain no PII — only counters/gauges keyed by enum
        // labels. Grafana / Prometheus / VictoriaMetrics all ingest the
        // default text/plain exposition format.
        .route("/metrics", get(observability::metrics_handler))
        .route(
            "/api/debug-log",
            post(handlers::debug_log::post_debug_log).get(handlers::debug_log::get_debug_log),
        )
        // Client telemetry ingestion (public — TelemetryService uses its own URLSession without auth cookies)
        .route(
            "/api/telemetry/events",
            post(handlers::client_telemetry::ingest_events),
        )
        .route(
            "/api/leitner/sync",
            post(handlers::leitner_sync::ingest_sync),
        )
        .route(
            "/api/leitner/progress",
            get(handlers::leitner_sync::latest_progress),
        )
        .route(
            "/u/:token",
            get(handlers::outreach::human_get).post(handlers::outreach::one_click_post),
        )
        .route("/u/:token/confirm", post(handlers::outreach::human_confirm));

    // Org-scoped routes (require valid session + org membership)
    let org_scoped_routes = Router::new()
        // Epic routes
        .route(
            "/api/epics",
            get(handlers::list_epics).post(handlers::create_epic),
        )
        .route(
            "/api/epics/:epic_id",
            get(handlers::get_epic).delete(handlers::delete_epic),
        )
        // Slice routes
        .route(
            "/api/epics/:epic_id/slices",
            get(handlers::list_slices).post(handlers::create_slice),
        )
        .route(
            "/api/epics/:epic_id/slices/:slice_id",
            get(handlers::get_slice).delete(handlers::delete_slice),
        )
        // Ticket routes
        .route("/api/tickets", get(handlers::list_all_tickets))
        .route(
            "/api/tickets/ensure-work-ticket",
            post(handlers::ensure_work_ticket),
        )
        .route("/api/tickets/:ticket_id", get(handlers::get_ticket_by_id))
        .route(
            "/api/tickets/:ticket_id/guidance",
            patch(handlers::update_ticket_guidance),
        )
        .route(
            "/api/tickets/:ticket_id/history",
            get(handlers::get_ticket_history_by_id),
        )
        .route("/api/epics/:epic_id/tickets", get(handlers::list_tickets))
        .route(
            "/api/epics/:epic_id/slices/:slice_id/tickets",
            get(handlers::list_slice_tickets).post(handlers::create_ticket),
        )
        .route(
            "/api/epics/:epic_id/slices/:slice_id/tickets/:ticket_id",
            get(handlers::get_ticket_nested)
                .patch(handlers::update_ticket_nested)
                .delete(handlers::delete_ticket_nested),
        )
        .route(
            "/api/epics/:epic_id/slices/:slice_id/tickets/:ticket_id/relationships",
            post(handlers::add_relationship_nested).delete(handlers::remove_relationship_nested),
        )
        .route(
            "/api/epics/:epic_id/slices/:slice_id/tickets/:ticket_id/history",
            get(handlers::get_ticket_history),
        )
        .route(
            "/api/ticket-schedule-offsets/preview",
            post(handlers::preview_schedule_offset),
        )
        .route(
            "/api/ticket-schedule-offsets/:operation_id",
            get(handlers::get_schedule_offset_preview),
        )
        .route(
            "/api/ticket-schedule-offsets/:operation_id/apply",
            post(handlers::apply_schedule_offset),
        )
        // Agent run routes (org-scoped, nested under epics)
        .route(
            "/api/epics/:epic_id/slices/:slice_id/tickets/:ticket_id/agent-runs",
            get(handlers::list_agent_runs).post(handlers::run_agent),
        )
        .route(
            "/api/epics/:epic_id/slices/:slice_id/tickets/:ticket_id/agent-runs/stream",
            post(handlers::stream_agent_run),
        )
        .route(
            "/api/epics/:epic_id/slices/:slice_id/tickets/:ticket_id/agent-runs/active",
            get(handlers::get_active_agent_run),
        )
        // Workspace Manager routes
        .route(
            "/api/workspace-manager/chat",
            post(handlers::workspace_manager_chat),
        )
        .route(
            "/api/workspace-manager/chat/submit",
            post(handlers::workspace_manager_chat_submit),
        )
        // Artifact-memory context packet routes
        .route("/api/context/search", post(handlers::search_context))
        .route("/api/context/gather", post(handlers::gather_context_packet))
        .route("/api/context-packets", get(handlers::list_context_packets))
        .route(
            "/api/context-packets/:packet_id",
            get(handlers::get_context_packet),
        )
        .route(
            "/api/retrievals/:retrieval_id/explain",
            get(handlers::explain_retrieval),
        )
        .route(
            "/api/retrieval-feedback",
            post(handlers::record_retrieval_feedback),
        )
        // Document routes (artifact-based)
        .route(
            "/api/tickets/:ticket_id/docs",
            get(handlers::list_ticket_docs),
        )
        .route(
            "/api/tickets/:ticket_id/docs/content",
            get(handlers::serve_document_content),
        )
        // Library routes (artifacts & documents browsing)
        .route(
            "/api/library/artifacts",
            get(handlers::list_library_artifacts),
        )
        .route(
            "/api/library/artifacts/search",
            get(handlers::search_library_artifacts),
        )
        .route(
            "/api/library/artifacts/:artifact_id",
            get(handlers::get_library_artifact),
        )
        .route(
            "/api/library/documents",
            get(handlers::list_library_documents),
        )
        .route(
            "/api/library/documents/search",
            get(handlers::search_library_documents),
        )
        .route(
            "/api/library/documents/:document_id/download",
            get(handlers::download_library_document),
        )
        // Data events SSE (live updates)
        .route("/api/data/subscribe", get(handlers::subscribe_data))
        .layer(axum::middleware::from_fn_with_state(
            app_state.db.clone(),
            auth_middleware::require_org_access,
        ))
        .layer(axum::middleware::from_fn_with_state(
            app_state.db.clone(),
            auth_middleware::require_auth,
        ));

    // User-scoped routes (require valid session only, no org membership check)
    let user_scoped_routes = Router::new()
        // Dailies is an account-global surface. Each Daily retains its source
        // organization, but authorization is resolved from that stored
        // identity instead of the app's selected X-Organization value.
        .route(
            "/api/dailies",
            get(handlers::list_dailies).post(handlers::create_daily),
        )
        .route("/api/dailies/window", get(handlers::get_daily_window))
        .route(
            "/api/dailies/events",
            get(handlers::stream_daily_execution_events),
        )
        .route(
            "/api/dailies/:daily_id/occurrences/:occurrence_id/actions/:action_key/executions",
            post(handlers::create_daily_execution),
        )
        .route(
            "/api/daily-executions/by-idempotency-key/:key",
            get(handlers::get_daily_execution_by_idempotency_key),
        )
        .route(
            "/api/daily-executions/:execution_id",
            get(handlers::get_daily_execution),
        )
        .route(
            "/api/dailies/:daily_id",
            get(handlers::get_daily).patch(handlers::update_daily),
        )
        .route("/api/dailies/:daily_id/pause", post(handlers::pause_daily))
        .route(
            "/api/dailies/:daily_id/resume",
            post(handlers::resume_daily),
        )
        .route(
            "/api/dailies/:daily_id/run-now",
            post(handlers::run_daily_now),
        )
        .route(
            "/api/dailies/:daily_id/runs/:run_id/read",
            post(handlers::mark_daily_run_read),
        )
        .route(
            "/api/dailies/:daily_id/runs/:run_id/package-update-review",
            get(handlers::get_package_update_review),
        )
        .route(
            "/api/dailies/:daily_id/runs/:run_id/package-update-review/approve",
            post(handlers::approve_package_update_review),
        )
        .route(
            "/api/dailies/:daily_id/runs/:run_id/package-update-review/deny",
            post(handlers::deny_package_update_review),
        )
        // Password rotation — called after every Face ID sign-in so the
        // server-side secret never stays stable.
        .route(
            "/api/auth/password/rotate",
            post(handlers::auth::rotate_password),
        )
        .route("/api/auth/passkeys", get(handlers::passkeys::list_passkeys))
        .route(
            "/api/auth/passkeys/:credential_id",
            delete(handlers::passkeys::delete_passkey),
        )
        .route(
            "/api/auth/passkeys/register/start",
            post(handlers::passkeys::start_passkey_registration),
        )
        .route(
            "/api/auth/passkeys/register/finish",
            post(handlers::passkeys::finish_passkey_registration),
        )
        // Agent run routes (accessed by session_id, not org-scoped)
        .route("/api/agent-runs/:session_id", get(handlers::get_agent_run))
        .route(
            "/api/agent-runs/:session_id/stream",
            get(handlers::reconnect_agent_stream),
        )
        .route(
            "/api/agent-runs/:session_id/message",
            post(handlers::send_message_to_agent),
        )
        // Email account management routes
        .route(
            "/api/email-accounts",
            get(handlers::list_email_accounts).post(handlers::create_email_account),
        )
        .route(
            "/api/email-accounts/:email",
            delete(handlers::delete_email_account),
        )
        .route(
            "/api/email-accounts/:email/sync",
            post(handlers::sync_email_account),
        )
        .route(
            "/api/email-identities",
            get(handlers::list_email_identities),
        )
        // Email SSE (live updates)
        .route("/api/emails/subscribe", get(handlers::subscribe_emails))
        // Email routes
        .route("/api/emails", get(handlers::list_emails))
        .route("/api/emails/send", post(handlers::send_email))
        .route(
            "/api/outreach/messages/:outreach_message_id/send",
            post(handlers::outreach::send_commercial_message),
        )
        .route("/api/emails/stats", get(handlers::get_email_stats))
        .route("/api/emails/search", get(handlers::search_emails))
        .route("/api/emails/archive", post(handlers::archive_emails))
        .route("/api/emails/unarchive", post(handlers::unarchive_emails))
        .route("/api/emails/threads", get(handlers::list_threads))
        .route("/api/emails/threads/:thread_id", get(handlers::get_thread))
        .route("/api/email-intake/run", post(handlers::run_email_intake))
        .route(
            "/api/email-intake/attention",
            get(handlers::list_email_attention_items),
        )
        .route(
            "/api/email-intake/attention/:id/resolve",
            post(handlers::resolve_email_attention_item),
        )
        .route(
            "/api/email-intake/notification-intents",
            get(handlers::list_email_notification_intents),
        )
        .route(
            "/api/email-intake/security-scans",
            get(handlers::list_email_security_scans),
        )
        .route(
            "/api/email-intake/contexts",
            get(handlers::list_email_contexts).post(handlers::create_email_context),
        )
        .route(
            "/api/email-intake/contexts/:context_id",
            get(handlers::get_email_context),
        )
        .route(
            "/api/email-intake/contexts/:context_id/threads",
            post(handlers::link_email_thread_to_context),
        )
        .route(
            "/api/email-intake/expected-responses",
            get(handlers::list_expected_email_responses)
                .post(handlers::create_expected_email_response),
        )
        .route(
            "/api/email-intake/expected-responses/refresh",
            post(handlers::refresh_expected_email_responses),
        )
        .route(
            "/api/emails/attachments/:attachment_id",
            get(handlers::download_attachment),
        )
        .route(
            "/api/emails/:id",
            get(handlers::get_email)
                .patch(handlers::update_email)
                .delete(handlers::delete_email),
        )
        .route(
            "/api/emails/:id/attachments",
            get(handlers::list_attachments),
        )
        // Draft routes
        .route(
            "/api/drafts",
            get(handlers::list_drafts).post(handlers::create_draft),
        )
        .route(
            "/api/drafts/:id",
            get(handlers::get_draft)
                .patch(handlers::update_draft)
                .delete(handlers::delete_draft),
        )
        .route(
            "/api/drafts/:id/status",
            post(handlers::update_draft_status),
        )
        .route("/api/drafts/:id/send", post(handlers::send_draft))
        // Email thread-ticket linking routes
        .route(
            "/api/email-threads/:thread_id/tickets",
            get(handlers::get_tickets_for_thread).post(handlers::link_thread_to_ticket),
        )
        .route(
            "/api/email-threads/:thread_id/tickets/:ticket_id",
            delete(handlers::unlink_thread_from_ticket),
        )
        .route(
            "/api/tickets/:ticket_id/threads",
            get(handlers::get_threads_for_ticket),
        )
        // Transcript routes
        .route(
            "/api/transcripts",
            get(handlers::list_sessions).post(handlers::create_session),
        )
        .route("/api/transcripts/:session_id", get(handlers::get_session))
        .route(
            "/api/transcripts/:session_id/end",
            post(handlers::end_session),
        )
        .route(
            "/api/transcripts/:session_id/entries",
            post(handlers::add_entry),
        )
        .route(
            "/api/transcripts/:session_id/stream",
            get(handlers::stream_session),
        )
        // Home Planner routes
        .route("/api/home-planner/chat", post(handlers::home_planner_chat))
        .route(
            "/api/home-planner/chat/submit",
            post(handlers::home_planner_chat_submit),
        )
        .route("/api/chat/codex-options", get(handlers::codex_chat_options))
        // Scoped Workspace Chat routes (restricted agent for external collaborators)
        .route(
            "/api/scoped-workspace/chat",
            post(handlers::scoped_workspace_chat),
        )
        .route(
            "/api/scoped-workspace/chat/submit",
            post(handlers::scoped_workspace_chat_submit),
        )
        .route(
            "/api/conversation-evaluator/chat",
            post(handlers::conversation_evaluator_chat),
        )
        .route(
            "/api/conversation-evaluator/chat/submit",
            post(handlers::conversation_evaluator_chat_submit),
        )
        .route("/api/feedback/chat", post(handlers::feedback_chat))
        .route(
            "/api/feedback/chat/submit",
            post(handlers::feedback_chat_submit),
        )
        // Unified SSE (single multiplexed connection for all topics)
        .route(
            "/api/events/subscribe",
            get(handlers::subscribe_unified_events),
        )
        // My Tickets SSE (live updates across all orgs)
        .route(
            "/api/my-tickets/subscribe",
            get(handlers::subscribe_my_tickets),
        )
        // Focus routes
        .route("/api/focus", get(handlers::list_focus))
        .route("/api/focus/pull", post(handlers::pull_focus_ticket))
        .route("/api/focus/toggle", post(handlers::toggle_focus))
        .route("/api/focus/:id", delete(handlers::remove_focus))
        // Daily Plan routes
        .route("/api/daily-plan", get(handlers::get_daily_plan))
        .route(
            "/api/daily-plan/subscribe",
            get(handlers::subscribe_daily_plan),
        )
        .route(
            "/api/daily-plan/toggle",
            post(handlers::toggle_daily_plan_item),
        )
        .route(
            "/api/daily-plan/items",
            get(handlers::list_daily_plan_items).post(handlers::create_daily_plan_item),
        )
        .route(
            "/api/daily-plan/items/:item_id",
            patch(handlers::update_daily_plan_item).delete(handlers::delete_daily_plan_item),
        )
        .route(
            "/api/daily-plan/date-items",
            post(handlers::create_daily_plan_date_item),
        )
        // Quick commands (dynamic presets above keyboard)
        .route(
            "/api/quick-commands",
            get(handlers::quick_commands::list_quick_commands),
        )
        // Chat quick-reference drawer routes. These are user-scoped because
        // conversation references are authorized by conversation ownership,
        // while organization references are membership-gated in the handler.
        .route(
            "/api/quick-reference",
            get(handlers::list_quick_reference).post(handlers::upsert_quick_reference),
        )
        .route(
            "/api/quick-reference/reorder",
            post(handlers::reorder_quick_reference),
        )
        .route(
            "/api/quick-reference/:id",
            patch(handlers::patch_quick_reference).delete(handlers::delete_quick_reference),
        )
        .route(
            "/api/secret-references",
            get(handlers::list_secret_references).post(handlers::upsert_secret_reference),
        )
        .route(
            "/api/secret-references/:id",
            axum::routing::delete(handlers::delete_secret_reference),
        )
        // Token usage tracking
        .route("/api/usage", get(handlers::usage::get_usage))
        // Alex tab: one permanent subscription-authenticated Fable coordinator.
        .route(
            "/api/alex/coordinator",
            get(handlers::get_fable_coordinator),
        )
        .route(
            "/api/alex/coordinator/health",
            get(handlers::get_fable_coordinator_health),
        )
        .route(
            "/api/fable-coordinator/chat",
            post(handlers::fable_coordinator_chat),
        )
        .route(
            "/api/fable-coordinator/chat/submit",
            post(handlers::fable_coordinator_chat_submit),
        )
        .route(
            "/api/alex/coordinator/session/repair",
            post(handlers::repair_fable_coordinator_session),
        )
        // Conversation routes (user-scoped, filtered by authenticated user_id)
        .route(
            "/api/conversations",
            get(handlers::list_conversations).post(handlers::create_conversation),
        )
        .route(
            "/api/conversations/multi-agent",
            post(handlers::create_multi_agent_conversation),
        )
        .route(
            "/api/conversations/subscribe",
            get(handlers::subscribe_conversations),
        )
        .route(
            "/api/conversations/read-states",
            post(handlers::sync_conversation_read_states),
        )
        .route(
            "/api/conversations/scale-cockpit",
            get(handlers::get_conversation_scale_cockpit),
        )
        .route(
            "/api/conversations/:id/children",
            get(handlers::list_child_conversations).post(handlers::create_child_conversations),
        )
        .route(
            "/api/conversations/:id/child-agent",
            post(handlers::launch_conversation_child_agent),
        )
        .route(
            "/api/conversations/:id/branch",
            post(handlers::branch_conversation),
        )
        .route(
            "/api/conversations/:id",
            get(handlers::get_conversation)
                .patch(handlers::update_conversation)
                .delete(handlers::delete_conversation),
        )
        .route(
            "/api/conversations/:id/wait",
            post(handlers::wait_conversation),
        )
        .route(
            "/api/conversations/:id/activate",
            post(handlers::activate_conversation),
        )
        .route(
            "/api/conversations/:id/read",
            post(handlers::mark_conversation_read),
        )
        .route(
            "/api/conversations/:id/cancel",
            post(handlers::cancel_conversation),
        )
        .route(
            "/api/conversations/:id/queued-messages",
            get(handlers::list_queued_messages),
        )
        .route(
            "/api/conversations/:id/queued-messages/:job_id",
            delete(handlers::cancel_queued_message),
        )
        .route(
            "/api/conversations/:id/checkpoint",
            get(handlers::get_conversation_checkpoint),
        )
        .route(
            "/api/conversations/:id/next-actions",
            get(handlers::list_conversation_next_actions),
        )
        .route(
            "/api/conversations/:id/agent-status",
            get(handlers::get_conversation_run_status),
        )
        .route(
            "/api/conversations/:id/agent-status/stream",
            get(handlers::stream_conversation_run_status),
        )
        .route(
            "/api/agent-operations/status",
            get(handlers::agent_operations::get_agent_operations_status),
        )
        .route(
            "/api/agent-operations/trends",
            get(handlers::agent_operations::get_agent_operations_trends),
        )
        .route(
            "/api/conversations/:id/stream",
            get(handlers::reconnect_conversation_stream),
        )
        .route(
            "/api/v1/conversations/:id/events",
            get(handlers::resume_stream::resume_conversation_stream),
        )
        .route(
            "/api/v1/conversations/:id/events/page",
            get(handlers::list_conversation_events_page),
        )
        .route(
            "/api/conversations/:id/messages",
            get(handlers::list_messages).post(handlers::add_message),
        )
        .route(
            "/api/conversations/:conv_id/messages/:message_id",
            patch(handlers::update_message),
        )
        // Chat attachment serving route
        .route(
            "/api/chat-attachments/:conversation_id/:filename",
            get(handlers::get_chat_attachment),
        )
        // DM routes (user-scoped, 1:1 direct messages)
        .route(
            "/api/dms",
            get(handlers::list_dms).post(handlers::create_dm),
        )
        .route("/api/dms/subscribe", get(handlers::subscribe_dms))
        .route("/api/dms/contacts", get(handlers::list_contacts))
        .route(
            "/api/dms/:dm_id/messages",
            get(handlers::get_dm_messages).post(handlers::send_dm_message),
        )
        .route(
            "/api/dms/:dm_id/messages/json",
            post(handlers::send_dm_message_json),
        )
        .route(
            "/api/dms/:dm_id/attachments/:attachment_id",
            get(handlers::download_dm_attachment),
        )
        .route("/api/dms/:dm_id/read", post(handlers::mark_dm_read))
        // Meeting routes
        .route(
            "/api/meetings",
            get(handlers::list_meetings).post(handlers::create_meeting),
        )
        .route("/api/meetings/subscribe", get(handlers::subscribe_meetings))
        .route(
            "/api/meetings/signaling",
            get(handlers::signaling_websocket),
        )
        .route(
            "/api/meetings/:room_id",
            get(handlers::get_meeting)
                .patch(handlers::update_meeting)
                .delete(handlers::delete_meeting),
        )
        .route(
            "/api/meetings/:room_id/start",
            post(handlers::start_meeting),
        )
        .route("/api/meetings/:room_id/end", post(handlers::end_meeting))
        .route(
            "/api/meetings/:room_id/transcribe",
            post(handlers::transcribe_meeting),
        )
        .route(
            "/api/meetings/:room_id/audio",
            post(handlers::upload_meeting_audio),
        )
        .route(
            "/api/meetings/:room_id/finalize-transcript",
            post(handlers::finalize_meeting_transcript),
        )
        .route(
            "/api/meetings/:room_id/join",
            post(handlers::join_meeting_as_participant),
        )
        .route(
            "/api/meetings/:room_id/favorite",
            post(handlers::toggle_meeting_favorite),
        )
        // Voice transcription (standalone Whisper)
        .route("/api/transcribe", post(handlers::voice_transcribe))
        // Alex voice controller prototype
        .route("/api/alex/voice-turn", post(handlers::alex_voice_turn))
        // TTS route
        .route("/api/tts", post(handlers::text_to_speech))
        // Membership routes
        .route(
            "/api/memberships",
            get(handlers::memberships::list_my_organizations)
                .post(handlers::memberships::add_member),
        )
        .route(
            "/api/memberships/:org",
            get(handlers::memberships::list_members),
        )
        .route(
            "/api/memberships/:org/:user_id",
            delete(handlers::memberships::remove_member),
        )
        // Device token routes (APNs push notifications)
        .route(
            "/api/device-tokens",
            post(handlers::device_tokens::register_device_token)
                .delete(handlers::device_tokens::remove_device_token),
        )
        // v1 device registration endpoints — rich contract used by the
        // durable-streaming iOS client. Upsert on POST, list active on GET.
        // Soft-delete is driven server-side by APNs 410 responses, not via
        // a DELETE endpoint.
        .route(
            "/v1/devices",
            post(handlers::device_tokens::register_device)
                .get(handlers::device_tokens::list_devices),
        )
        // CAD file routes (LaminarForge 3D models)
        .route("/api/cad/files", get(handlers::cad::list_cad_files))
        .route(
            "/api/cad/files/:filename",
            get(handlers::cad::download_cad_file),
        )
        .route(
            "/api/cad/files/:filename/thumbnail",
            get(handlers::cad::get_cad_thumbnail),
        )
        .layer(axum::middleware::from_fn_with_state(
            app_state.db.clone(),
            auth_middleware::require_auth,
        ));

    // Admin routes (require valid session + admin role)
    let admin_routes = Router::new()
        .route("/api/admin/logs", get(handlers::admin_logs::list_logs))
        .route(
            "/api/admin/log-incidents",
            get(handlers::log_incidents::list_log_incidents),
        )
        .route(
            "/api/admin/log-incidents/triage",
            post(handlers::log_incidents::run_log_incident_triage),
        )
        .route(
            "/api/admin/log-incidents/triage/status",
            get(handlers::log_incidents::get_log_incident_triage_status),
        )
        .route("/api/admin/check", get(handlers::admin_logs::check_admin))
        .route(
            "/api/admin/ops/timeline",
            get(handlers::ops_timeline::list_ops_timeline),
        )
        // Full Access Chat routes are admin-only; external collaborators use scoped-workspace.
        .route("/api/full-access/chat", post(handlers::full_access_chat))
        .route(
            "/api/full-access/chat/submit",
            post(handlers::full_access_chat_submit),
        )
        .route(
            "/api/admin/reload",
            post(handlers::admin_reload::reload_services),
        )
        .route(
            "/api/admin/reload/log",
            get(handlers::admin_reload::reload_log),
        )
        .route(
            "/api/admin/ios-install",
            post(handlers::admin_reload::ios_install),
        )
        .route(
            "/api/admin/ios-install/log",
            get(handlers::admin_reload::ios_install_log),
        )
        .route(
            "/api/admin/pending-restart",
            get(handlers::admin_reload::get_pending_restart)
                .delete(handlers::admin_reload::clear_pending_restart),
        )
        .route(
            "/api/admin/restart",
            post(handlers::admin_reload::restart_api),
        )
        .route(
            "/api/admin/storage/scan",
            get(handlers::storage::scan_storage),
        )
        .route(
            "/api/admin/storage/scan/start",
            post(handlers::storage::start_storage_scan),
        )
        .route(
            "/api/admin/storage/actions/:action_id/start",
            post(handlers::storage::start_storage_action),
        )
        .route(
            "/api/admin/storage/jobs/:job_id",
            get(handlers::storage::get_storage_job),
        )
        .route(
            "/api/admin/storage/investigation/start",
            post(handlers::storage::start_storage_investigation_conversation),
        )
        .route(
            "/api/admin/client-events",
            get(handlers::client_telemetry::list_client_events),
        )
        .route(
            "/api/admin/app-diagnostics/ios-degradation",
            get(handlers::app_diagnostics::ios_degradation_diagnostics),
        )
        .route(
            "/api/meeting-agent/chat",
            post(handlers::meeting_agent_chat),
        )
        .route(
            "/api/meeting-agent/chat/submit",
            post(handlers::meeting_agent_chat_submit),
        )
        .layer(axum::middleware::from_fn_with_state(
            app_state.db.clone(),
            auth_middleware::require_admin,
        ))
        .layer(axum::middleware::from_fn_with_state(
            app_state.db.clone(),
            auth_middleware::require_auth,
        ));

    // Clone db_pool for request logger injection
    let logger_pool = db_pool.clone();

    let app = public_routes
        .merge(org_scoped_routes)
        .merge(user_scoped_routes)
        .merge(admin_routes)
        .with_state(app_state)
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024 * 1024)) // 2GB - never lose a session due to size limits
        .layer(axum::middleware::from_fn(request_logger::request_logger))
        .layer(axum::middleware::from_fn(
            move |mut req: axum::extract::Request, next: axum::middleware::Next| {
                let pool = logger_pool.clone();
                async move {
                    req.extensions_mut().insert(pool);
                    next.run(req).await
                }
            },
        ))
        .layer(CookieManagerLayer::new());

    // Bind to 0.0.0.0 so native clients can reach the API over WireGuard.
    let addr = "0.0.0.0:8001";
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("Server running on http://{}", addr);

    // Log server startup to system_logs so it appears on the admin page
    system_log_helper::log_info(
        &db_pool,
        "api",
        "API server started on http://0.0.0.0:8001",
        None,
    )
    .await;

    // Spawn a force-exit watchdog. `shutdown_signal` cancels the token before
    // its 10-second cleanup/drain window, so this must outlast that window or
    // cleanup never runs.
    {
        let exit_token = shutdown_token.child_token();
        tokio::spawn(async move {
            exit_token.cancelled().await;
            tracing::info!(
                "Shutdown watchdog: waiting 20s for cleanup and connections to drain..."
            );
            tokio::time::sleep(tokio::time::Duration::from_secs(20)).await;
            tracing::info!("Shutdown watchdog: force exiting");
            std::process::exit(0);
        });
    }

    // Run server with graceful shutdown
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(
            shutdown_db,
            shutdown_token,
            shutdown_runner_generation_id,
        ))
        .await?;

    Ok(())
}

fn spawn_runner_generation_reconciler(
    db_pool: Arc<ticketing_system::SqlitePool>,
    shutdown_token: CancellationToken,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(
            RUNNER_RECONCILE_INTERVAL_SECONDS,
        ));
        interval.reset();

        loop {
            tokio::select! {
                _ = shutdown_token.cancelled() => break,
                _ = interval.tick() => {}
            }

            match ticketing_system::agent_runners::reconcile_stale_runner_generations(
                &db_pool,
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
                    log_ops_error(
                        &db_pool,
                        "agent_runner",
                        "runner_generation_reconcile_failed",
                        "Failed to reconcile stale runner generation metadata",
                        &e,
                    )
                    .await;
                }
            }
        }
    });
}

/// Graceful shutdown signal handler
/// Waits for SIGTERM or Ctrl+C, then marks running checkpoints as interrupted
async fn shutdown_signal(
    db_pool: Arc<ticketing_system::SqlitePool>,
    shutdown_token: CancellationToken,
    runner_generation_id: String,
) {
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
        _ = ctrl_c => {
            tracing::info!("Received Ctrl+C, initiating graceful shutdown...");
        },
        _ = terminate => {
            tracing::info!("Received SIGTERM, initiating graceful shutdown...");
        },
    }

    // Mark service as not ready immediately — health probes return 503
    handlers::health::set_not_ready();

    if let Err(e) =
        ticketing_system::agent_runners::mark_generation_draining(&db_pool, &runner_generation_id)
            .await
    {
        tracing::warn!(
            target: "agentic_api::shutdown",
            event = "shutdown.runner_generation_drain_failed",
            generation_id = %runner_generation_id,
            error = %e,
            "failed to mark runner generation draining during shutdown"
        );
        system_log_helper::log_warn(
            &db_pool,
            "agent_runner",
            "Failed to mark runner generation draining during shutdown",
            Some(&format!(
                "generation_id={}; error={}",
                runner_generation_id, e
            )),
        )
        .await;
    }

    // Cancel all background tasks (email fetcher, cleanup loops, etc.)
    shutdown_token.cancel();

    tracing::info!("Waiting 10 seconds for in-flight operations to complete...");
    tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;

    // Mark any still-active checkpoints as interrupted if this embedded API
    // runner did not drain before launchd terminates the process.
    match ticketing_system::agent_runners::mark_unowned_active_checkpoints_interrupted(&db_pool, 0)
        .await
    {
        Ok(count) if count > 0 => {
            tracing::warn!(
                "Marked {} unowned agent checkpoint(s) as interrupted during shutdown",
                count
            );
        }
        Ok(_) => {
            tracing::debug!("No running checkpoints to interrupt");
        }
        Err(e) => {
            log_ops_error(
                &db_pool,
                "agent_runner",
                "shutdown.checkpoint_interrupt_failed",
                "Failed to mark checkpoints as interrupted during shutdown",
                &e,
            )
            .await;
        }
    }

    // Mark any running agent runs as failed
    match ticketing_system::agent_runs::mark_all_running_as_interrupted(&db_pool).await {
        Ok(count) if count > 0 => {
            tracing::warn!("Marked {} agent run(s) as failed during shutdown", count);
        }
        Ok(_) => {}
        Err(e) => {
            log_ops_error(
                &db_pool,
                "agent",
                "shutdown.agent_run_interrupt_failed",
                "Failed to mark agent runs as failed during shutdown",
                &e,
            )
            .await;
        }
    }

    match ticketing_system::dailies::mark_running_runs_failed(&db_pool).await {
        Ok(count) if count > 0 => {
            tracing::warn!("Marked {} daily run(s) as failed during shutdown", count);
        }
        Ok(_) => {}
        Err(e) => {
            log_ops_error(
                &db_pool,
                "dailies",
                "shutdown.daily_run_interrupt_failed",
                "Failed to mark daily runs as failed during shutdown",
                &e,
            )
            .await;
        }
    }

    match ticketing_system::agent_runners::get_active_turns(&db_pool).await {
        Ok(turns)
            if turns
                .iter()
                .any(|turn| turn.generation_id == runner_generation_id) =>
        {
            if let Err(e) = ticketing_system::agent_runners::mark_generation_failed(
                &db_pool,
                &runner_generation_id,
                "api shutdown before generation drained",
            )
            .await
            {
                tracing::warn!(
                    "Failed to mark runner generation {} failed during shutdown: {}",
                    runner_generation_id,
                    e
                );
            }
        }
        Ok(_) => {
            if let Err(e) = ticketing_system::agent_runners::mark_generation_exited(
                &db_pool,
                &runner_generation_id,
            )
            .await
            {
                tracing::warn!(
                    "Failed to mark runner generation {} exited during shutdown: {}",
                    runner_generation_id,
                    e
                );
            }
        }
        Err(e) => tracing::warn!(
            "Failed to inspect runner generation {} during shutdown: {}",
            runner_generation_id,
            e
        ),
    }

    // Log shutdown to system_logs
    let _ = ticketing_system::system_logs::insert_log(
        &db_pool,
        "warn",
        "api",
        "API server shutting down",
        None,
        None,
        None,
    )
    .await;

    tracing::info!("Graceful shutdown complete");
}
