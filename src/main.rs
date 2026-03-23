mod handlers;
mod models;
mod mcp_wrapper;
mod agents;
mod email_fetcher;
mod auth_middleware;
mod request_logger;
pub mod system_log_helper;
pub mod apns;

use axum::{
    routing::{delete, get, patch, post},
    Router,
    extract::{DefaultBodyLimit, FromRef},
};
use std::sync::Arc;
use tower_http::cors::{CorsLayer, AllowOrigin};
use http::{header, Method};
use tower_cookies::CookieManagerLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use tokio::signal;
use tokio_util::sync::CancellationToken;

use handlers::chat_client_manager::ChatClientManager;

/// Shared application state.
/// Implements `FromRef` so handlers can extract individual components.
#[derive(Clone)]
struct AppState {
    db: Arc<sqlx::SqlitePool>,
    chat_manager: Arc<ChatClientManager>,
    apns: Option<Arc<apns::ApnsService>>,
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

    // Initialize MCP handler
    mcp_wrapper::init_mcp_handler().await?;
    tracing::info!("MCP handler initialized");

    // Initialize SQLite database pool
    let db_pool = Arc::new(ticketing_system::init_db().await?);
    tracing::info!("SQLite database pool initialized");

    // Mark any interrupted agent checkpoints from previous run
    match ticketing_system::checkpoints::mark_all_running_as_interrupted(&db_pool).await {
        Ok(count) if count > 0 => {
            tracing::warn!("Marked {} interrupted agent checkpoint(s) from previous run", count);
        }
        Ok(_) => {
            tracing::debug!("No interrupted agent checkpoints to clean up");
        }
        Err(e) => {
            tracing::error!("Failed to clean up interrupted checkpoints: {}", e);
        }
    }

    // Mark any interrupted agent runs from previous run (killed by server restart)
    match ticketing_system::agent_runs::mark_all_running_as_interrupted(&db_pool).await {
        Ok(count) if count > 0 => {
            tracing::warn!("Marked {} interrupted agent run(s) as failed from previous run", count);
        }
        Ok(_) => {
            tracing::debug!("No interrupted agent runs to clean up");
        }
        Err(e) => {
            tracing::error!("Failed to clean up interrupted agent runs: {}", e);
        }
    }

    // Clear any stale restart queue entries from before the restart
    match ticketing_system::restart_queue::clear_all_pending(&db_pool).await {
        Ok(count) if count > 0 => {
            tracing::info!("Cleared {} stale pending restart(s) from previous run", count);
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!("Failed to clear stale restart queue: {}", e);
        }
    }

    // Create shutdown token for coordinated cancellation of all background tasks.
    // When cancelled, all tasks using a child token will break out of their loops.
    let shutdown_token = CancellationToken::new();

    // Start email fetcher background task (queries email_accounts table each cycle)
    tracing::info!("Starting email fetcher (hot-reload from database)");
    email_fetcher::start_email_fetcher(db_pool.clone(), shutdown_token.child_token());

    // Create chat client manager for persistent ClaudeSDKClient instances
    let chat_manager = Arc::new(ChatClientManager::new());
    tracing::info!("Chat client manager initialized");

    // Initialize APNs push notification service (optional — no-op if .p8 key missing)
    let apns = apns::ApnsService::init();
    if apns.is_some() {
        tracing::info!("APNs push notification service initialized");
    }

    let app_state = AppState {
        db: db_pool.clone(),
        chat_manager,
        apns,
    };

    // Clone db_pool for shutdown handler before building router (which moves app_state)
    let shutdown_db = db_pool.clone();

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
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(24 * 60 * 60));
            loop {
                tokio::select! {
                    _ = token.cancelled() => break,
                    _ = interval.tick() => {}
                }
                let cutoff = chrono::Utc::now().timestamp() - 30 * 24 * 60 * 60;
                match ticketing_system::system_logs::delete_logs_before(&cleanup_pool, cutoff).await {
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
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(24 * 60 * 60));
            loop {
                tokio::select! {
                    _ = token.cancelled() => break,
                    _ = interval.tick() => {}
                }
                let cutoff = chrono::Utc::now().timestamp() - 90 * 24 * 60 * 60;
                match ticketing_system::client_events::delete_client_events_before(&cleanup_pool, cutoff).await {
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

    // Periodic cleanup of old conversation events (every hour, keep 1 hour after completion)
    {
        let cleanup_pool = db_pool.clone();
        let token = shutdown_token.child_token();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(3600));
            loop {
                tokio::select! {
                    _ = token.cancelled() => break,
                    _ = interval.tick() => {}
                }
                match ticketing_system::conversations::cleanup_old_events(&cleanup_pool, 3600).await {
                    Ok(deleted) if deleted > 0 => {
                        tracing::info!("[CLEANUP] Deleted {} old conversation events", deleted);
                    }
                    Err(e) => {
                        tracing::warn!("[CLEANUP] Failed to cleanup events: {}", e);
                    }
                    _ => {}
                }
            }
        });
    }

    // Deferred restart watcher: polls every 10 seconds for queued restarts.
    // When a restart is pending AND no active work remains, executes the restart.
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
                let pending = match ticketing_system::restart_queue::get_pending_restart(&restart_pool).await {
                    Ok(Some(entry)) => entry,
                    Ok(None) => continue,
                    Err(e) => {
                        tracing::warn!("[RESTART_WATCHER] Failed to check restart queue: {}", e);
                        continue;
                    }
                };

                // Pending restart found — check if all active work is done
                let active = match ticketing_system::restart_queue::count_active_work(&restart_pool).await {
                    Ok(a) => a,
                    Err(e) => {
                        tracing::warn!("[RESTART_WATCHER] Failed to count active work: {}", e);
                        continue;
                    }
                };

                if active.total > 0 {
                    tracing::debug!(
                        "[RESTART_WATCHER] Restart queued for '{}' but {} agent(s) + {} conversation(s) still active, waiting...",
                        pending.service, active.agent_run_count, active.checkpoint_count
                    );
                    continue;
                }

                // All clear — execute the deferred restart
                tracing::info!(
                    "[RESTART_WATCHER] Executing deferred {} for service '{}' (requested by {:?})",
                    pending.action, pending.service, pending.requested_by
                );

                // Mark as executed before we restart (in case restart kills us mid-write)
                let _ = ticketing_system::restart_queue::mark_executed(&restart_pool, pending.id).await;

                // Log to system_logs
                system_log_helper::log_info(
                    &restart_pool,
                    "restart_watcher",
                    &format!("Executing deferred {} for '{}'", pending.action, pending.service),
                    None,
                ).await;

                if pending.action == "setup" {
                    // For setup, we need to run the setup script which will rebuild and restart
                    let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/jarvisgpt".to_string());
                    let setup_script = format!("{}/projects/agentic-flowstate-setup/setup.sh", home);
                    match std::process::Command::new("bash")
                        .arg(&setup_script)
                        .current_dir(format!("{}/projects/agentic-flowstate-setup", home))
                        .spawn()
                    {
                        Ok(_child) => {
                            tracing::info!("[RESTART_WATCHER] Setup script spawned, it will restart the API server");
                        }
                        Err(e) => {
                            tracing::error!("[RESTART_WATCHER] Failed to spawn setup script: {}", e);
                        }
                    }
                } else {
                    // For restart, SIGTERM ourselves — launchd KeepAlive will restart with new binary
                    let pid = std::process::id();
                    tracing::info!("[RESTART_WATCHER] Sending SIGTERM to self (PID {})", pid);
                    let _ = std::process::Command::new("kill")
                        .args(&["-TERM", &pid.to_string()])
                        .output();
                }

                break;
            }
        });
    }

    // Public routes (no auth required)
    let public_routes = Router::new()
        .route("/api/auth/register", post(handlers::auth::register))
        .route("/api/auth/login", post(handlers::auth::login))
        .route("/api/auth/logout", post(handlers::auth::logout))
        .route("/api/auth/me", get(handlers::auth::me))
        .route("/health", get(|| async { "OK" }))
        .route("/api/debug-log", post(handlers::debug_log::post_debug_log).get(handlers::debug_log::get_debug_log))
;

    // Org-scoped routes (require valid session + org membership)
    let org_scoped_routes = Router::new()
        // Epic routes
        .route("/api/epics", get(handlers::list_epics).post(handlers::create_epic))
        .route("/api/epics/:epic_id", get(handlers::get_epic).delete(handlers::delete_epic))

        // Slice routes
        .route("/api/epics/:epic_id/slices",
            get(handlers::list_slices)
            .post(handlers::create_slice))
        .route("/api/epics/:epic_id/slices/:slice_id",
            get(handlers::get_slice)
            .delete(handlers::delete_slice))

        // Ticket routes
        .route("/api/tickets", get(handlers::list_all_tickets))
        .route("/api/tickets/:ticket_id", get(handlers::get_ticket_by_id))
        .route("/api/tickets/:ticket_id/guidance", patch(handlers::update_ticket_guidance))
        .route("/api/tickets/:ticket_id/history", get(handlers::get_ticket_history_by_id))
        .route("/api/epics/:epic_id/tickets", get(handlers::list_tickets))
        .route("/api/epics/:epic_id/slices/:slice_id/tickets",
            get(handlers::list_slice_tickets)
            .post(handlers::create_ticket))
        .route("/api/epics/:epic_id/slices/:slice_id/tickets/:ticket_id",
            get(handlers::get_ticket_nested)
            .patch(handlers::update_ticket_nested)
            .delete(handlers::delete_ticket_nested))
        .route("/api/epics/:epic_id/slices/:slice_id/tickets/:ticket_id/relationships",
            post(handlers::add_relationship_nested)
            .delete(handlers::remove_relationship_nested))
        .route("/api/epics/:epic_id/slices/:slice_id/tickets/:ticket_id/history",
            get(handlers::get_ticket_history))

        // Agent run routes (org-scoped, nested under epics)
        .route("/api/epics/:epic_id/slices/:slice_id/tickets/:ticket_id/agent-runs",
            get(handlers::list_agent_runs)
            .post(handlers::run_agent))
        .route("/api/epics/:epic_id/slices/:slice_id/tickets/:ticket_id/agent-runs/stream",
            post(handlers::stream_agent_run))
        .route("/api/epics/:epic_id/slices/:slice_id/tickets/:ticket_id/agent-runs/active",
            get(handlers::get_active_agent_run))

        // Workspace Manager routes
        .route("/api/workspace-manager/chat",
            post(handlers::workspace_manager_chat))

        // Document routes (artifact-based)
        .route("/api/tickets/:ticket_id/docs",
            get(handlers::list_ticket_docs))
        .route("/api/tickets/:ticket_id/docs/content",
            get(handlers::serve_document_content))

        // Library routes (artifacts & documents browsing)
        .route("/api/library/artifacts",
            get(handlers::list_library_artifacts))
        .route("/api/library/artifacts/search",
            get(handlers::search_library_artifacts))
        .route("/api/library/artifacts/:artifact_id",
            get(handlers::get_library_artifact))
        .route("/api/library/documents",
            get(handlers::list_library_documents))
        .route("/api/library/documents/search",
            get(handlers::search_library_documents))
        .route("/api/library/documents/:document_id/download",
            get(handlers::download_library_document))

        // Data events SSE (live updates)
        .route("/api/data/subscribe", get(handlers::subscribe_data))

        .layer(axum::middleware::from_fn_with_state(app_state.db.clone(), auth_middleware::require_org_access))
        .layer(axum::middleware::from_fn_with_state(app_state.db.clone(), auth_middleware::require_auth));

    // User-scoped routes (require valid session only, no org membership check)
    let user_scoped_routes = Router::new()
        // Agent run routes (accessed by session_id, not org-scoped)
        .route("/api/agent-runs/:session_id",
            get(handlers::get_agent_run))
        .route("/api/agent-runs/:session_id/stream",
            get(handlers::reconnect_agent_stream))
        .route("/api/agent-runs/:session_id/message",
            post(handlers::send_message_to_agent))

        // Email account management routes
        .route("/api/email-accounts",
            get(handlers::list_email_accounts)
            .post(handlers::create_email_account))
        .route("/api/email-accounts/:email",
            delete(handlers::delete_email_account))
        .route("/api/email-accounts/:email/sync",
            post(handlers::sync_email_account))

        // Email SSE (live updates)
        .route("/api/emails/subscribe", get(handlers::subscribe_emails))

        // Email routes
        .route("/api/emails", get(handlers::list_emails))
        .route("/api/emails/send", post(handlers::send_email))
        .route("/api/emails/stats", get(handlers::get_email_stats))
        .route("/api/emails/search", get(handlers::search_emails))
        .route("/api/emails/archive", post(handlers::archive_emails))
        .route("/api/emails/unarchive", post(handlers::unarchive_emails))
        .route("/api/emails/threads", get(handlers::list_threads))
        .route("/api/emails/threads/:thread_id", get(handlers::get_thread))
        .route("/api/emails/attachments/:attachment_id", get(handlers::download_attachment))
        .route("/api/emails/:id",
            get(handlers::get_email)
            .patch(handlers::update_email)
            .delete(handlers::delete_email))
        .route("/api/emails/:id/attachments", get(handlers::list_attachments))

        // Draft routes
        .route("/api/drafts",
            get(handlers::list_drafts)
            .post(handlers::create_draft))
        .route("/api/drafts/:id",
            get(handlers::get_draft)
            .patch(handlers::update_draft)
            .delete(handlers::delete_draft))
        .route("/api/drafts/:id/status",
            post(handlers::update_draft_status))
        .route("/api/drafts/:id/send",
            post(handlers::send_draft))

        // Email thread-ticket linking routes
        .route("/api/email-threads/:thread_id/tickets",
            get(handlers::get_tickets_for_thread)
            .post(handlers::link_thread_to_ticket))
        .route("/api/email-threads/:thread_id/tickets/:ticket_id",
            delete(handlers::unlink_thread_from_ticket))
        .route("/api/tickets/:ticket_id/threads",
            get(handlers::get_threads_for_ticket))

        // Transcript routes
        .route("/api/transcripts",
            get(handlers::list_sessions)
            .post(handlers::create_session))
        .route("/api/transcripts/:session_id",
            get(handlers::get_session))
        .route("/api/transcripts/:session_id/end",
            post(handlers::end_session))
        .route("/api/transcripts/:session_id/entries",
            post(handlers::add_entry))
        .route("/api/transcripts/:session_id/stream",
            get(handlers::stream_session))

        // Home Planner routes
        .route("/api/home-planner/chat",
            post(handlers::home_planner_chat))

        // Full Access Chat routes
        .route("/api/full-access/chat",
            post(handlers::full_access_chat))

        // My Tickets SSE (live updates across all orgs)
        .route("/api/my-tickets/subscribe", get(handlers::subscribe_my_tickets))

        // Project Workload routes
        .route("/api/project-workload",
            get(handlers::list_project_workload))
        .route("/api/project-workload/pull",
            post(handlers::pull_project_ticket))
        .route("/api/project-workload/toggle",
            post(handlers::toggle_project_workload))
        .route("/api/project-workload/:id",
            delete(handlers::remove_project_workload))

        // Daily Plan routes
        .route("/api/daily-plan",
            get(handlers::get_daily_plan))
        .route("/api/daily-plan/subscribe",
            get(handlers::subscribe_daily_plan))
        .route("/api/daily-plan/toggle",
            post(handlers::toggle_daily_plan_item))
        .route("/api/daily-plan/items",
            get(handlers::list_daily_plan_items)
            .post(handlers::create_daily_plan_item))
        .route("/api/daily-plan/items/:item_id",
            patch(handlers::update_daily_plan_item)
            .delete(handlers::delete_daily_plan_item))
        .route("/api/daily-plan/date-items",
            post(handlers::create_daily_plan_date_item))

        // Token usage tracking
        .route("/api/usage", get(handlers::usage::get_usage))

        // Conversation routes (user-scoped, filtered by authenticated user_id)
        .route("/api/conversations",
            get(handlers::list_conversations)
            .post(handlers::create_conversation))
        .route("/api/conversations/subscribe",
            get(handlers::subscribe_conversations))
        .route("/api/conversations/:id",
            get(handlers::get_conversation)
            .patch(handlers::update_conversation)
            .delete(handlers::delete_conversation))
        .route("/api/conversations/:id/wait",
            post(handlers::wait_conversation))
        .route("/api/conversations/:id/activate",
            post(handlers::activate_conversation))
        .route("/api/conversations/:id/cancel",
            post(handlers::cancel_conversation))
        .route("/api/conversations/:id/checkpoint",
            get(handlers::get_conversation_checkpoint))
        .route("/api/conversations/:id/stream",
            get(handlers::reconnect_conversation_stream))
        .route("/api/conversations/:id/messages",
            get(handlers::list_messages)
            .post(handlers::add_message))
        .route("/api/conversations/:conv_id/messages/:message_id",
            patch(handlers::update_message))

        // Chat image serving route
        .route("/api/chat-images/:conversation_id/:filename",
            get(handlers::get_chat_image))

        // DM routes (user-scoped, 1:1 direct messages)
        .route("/api/dms",
            get(handlers::list_dms)
            .post(handlers::create_dm))
        .route("/api/dms/subscribe",
            get(handlers::subscribe_dms))
        .route("/api/dms/contacts",
            get(handlers::list_contacts))
        .route("/api/dms/:dm_id/messages",
            get(handlers::get_dm_messages)
            .post(handlers::send_dm_message))
        .route("/api/dms/:dm_id/messages/json",
            post(handlers::send_dm_message_json))
        .route("/api/dms/:dm_id/attachments/:attachment_id",
            get(handlers::download_dm_attachment))
        .route("/api/dms/:dm_id/read",
            post(handlers::mark_dm_read))

        // Meeting routes
        .route("/api/meetings",
            get(handlers::list_meetings)
            .post(handlers::create_meeting))
        .route("/api/meetings/subscribe",
            get(handlers::subscribe_meetings))
        .route("/api/meetings/signaling",
            get(handlers::signaling_websocket))
        .route("/api/meetings/:room_id",
            get(handlers::get_meeting)
            .patch(handlers::update_meeting)
            .delete(handlers::delete_meeting))
        .route("/api/meetings/:room_id/start",
            post(handlers::start_meeting))
        .route("/api/meetings/:room_id/end",
            post(handlers::end_meeting))
        .route("/api/meetings/:room_id/transcribe",
            post(handlers::transcribe_meeting))
        .route("/api/meetings/:room_id/audio",
            post(handlers::upload_meeting_audio))
        .route("/api/meetings/:room_id/finalize-transcript",
            post(handlers::finalize_meeting_transcript))
        .route("/api/meetings/:room_id/join",
            post(handlers::join_meeting_as_participant))
        .route("/api/meetings/:room_id/favorite",
            post(handlers::toggle_meeting_favorite))

        // Voice transcription (standalone Whisper)
        .route("/api/transcribe", post(handlers::voice_transcribe))

        // TTS route
        .route("/api/tts", post(handlers::text_to_speech))

        // Membership routes
        .route("/api/memberships",
            get(handlers::memberships::list_my_organizations)
            .post(handlers::memberships::add_member))
        .route("/api/memberships/:org",
            get(handlers::memberships::list_members))
        .route("/api/memberships/:org/:user_id",
            delete(handlers::memberships::remove_member))

        // Client telemetry ingestion
        .route("/api/telemetry/events",
            post(handlers::client_telemetry::ingest_events))

        // Device token routes (APNs push notifications)
        .route("/api/device-tokens",
            post(handlers::device_tokens::register_device_token)
            .delete(handlers::device_tokens::remove_device_token))

        .layer(axum::middleware::from_fn_with_state(app_state.db.clone(), auth_middleware::require_auth));

    // Admin routes (require valid session + admin role)
    let admin_routes = Router::new()
        .route("/api/admin/logs", get(handlers::admin_logs::list_logs))
        .route("/api/admin/check", get(handlers::admin_logs::check_admin))
        .route("/api/admin/reload", post(handlers::admin_reload::reload_services))
        .route("/api/admin/reload/log", get(handlers::admin_reload::reload_log))
        .route("/api/admin/ios-install", post(handlers::admin_reload::ios_install))
        .route("/api/admin/ios-install/log", get(handlers::admin_reload::ios_install_log))
        .route("/api/admin/client-events", get(handlers::client_telemetry::list_client_events))
        .route("/api/meeting-agent/chat", post(handlers::meeting_agent_chat))
        .layer(axum::middleware::from_fn_with_state(app_state.db.clone(), auth_middleware::require_admin))
        .layer(axum::middleware::from_fn_with_state(app_state.db.clone(), auth_middleware::require_auth));

    // Clone db_pool for request logger injection
    let logger_pool = db_pool.clone();

    let app = public_routes
        .merge(org_scoped_routes)
        .merge(user_scoped_routes)
        .merge(admin_routes)
        .with_state(app_state)
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024 * 1024)) // 2GB - never lose a session due to size limits
        .layer(axum::middleware::from_fn(request_logger::request_logger))
        .layer(axum::middleware::from_fn(move |mut req: axum::extract::Request, next: axum::middleware::Next| {
            let pool = logger_pool.clone();
            async move {
                req.extensions_mut().insert(pool);
                next.run(req).await
            }
        }))
        .layer(CookieManagerLayer::new())
        .layer(
            CorsLayer::new()
                .allow_origin(AllowOrigin::list([
                    "http://localhost:3000".parse().unwrap(),
                    "http://100.119.87.128:3000".parse().unwrap(),
                    "https://jarviss-mac-mini-1.tail3da916.ts.net".parse().unwrap(),
                ]))
                .allow_credentials(true)
                .allow_methods([
                    Method::GET,
                    Method::POST,
                    Method::PUT,
                    Method::PATCH,
                    Method::DELETE,
                    Method::OPTIONS,
                ])
                .allow_headers([
                    header::CONTENT_TYPE,
                    header::ACCEPT,
                    header::AUTHORIZATION,
                    header::COOKIE,
                    header::HeaderName::from_static("x-organization"),
                ])
                .expose_headers([
                    header::SET_COOKIE,
                    header::CONTENT_TYPE,
                ]),
        );

    // Start the server - bind to 0.0.0.0 to allow access from other devices (mobile via Tailscale)
    let addr = "0.0.0.0:8001";
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("Server running on http://{}", addr);

    // Log server startup to system_logs so it appears on the admin page
    system_log_helper::log_info(&db_pool, "api", "API server started on http://0.0.0.0:8001", None).await;

    // Spawn a force-exit watchdog. After shutdown_signal completes and cancels
    // the token, this gives 5 seconds for connections to drain, then force-exits.
    // Without this, axum waits indefinitely for SSE/WebSocket connections to close.
    {
        let exit_token = shutdown_token.child_token();
        tokio::spawn(async move {
            exit_token.cancelled().await;
            tracing::info!("Shutdown watchdog: waiting 5s for connections to drain...");
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            tracing::info!("Shutdown watchdog: force exiting");
            std::process::exit(0);
        });
    }

    // Run server with graceful shutdown
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown_db, shutdown_token))
        .await?;

    Ok(())
}

/// Graceful shutdown signal handler
/// Waits for SIGTERM or Ctrl+C, then marks running checkpoints as interrupted
async fn shutdown_signal(db_pool: Arc<ticketing_system::SqlitePool>, shutdown_token: CancellationToken) {
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

    // Give running agents a brief moment to finish current tool calls
    // (in practice, this won't wait for long-running tools, just allows
    // any in-flight checkpointing to complete)
    // Cancel all background tasks (email fetcher, cleanup loops, etc.)
    shutdown_token.cancel();

    tracing::info!("Waiting 10 seconds for in-flight operations to complete...");
    tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;

    // Mark any still-running checkpoints as interrupted
    match ticketing_system::checkpoints::mark_all_running_as_interrupted(&db_pool).await {
        Ok(count) if count > 0 => {
            tracing::warn!("Marked {} agent checkpoint(s) as interrupted during shutdown", count);
        }
        Ok(_) => {
            tracing::debug!("No running checkpoints to interrupt");
        }
        Err(e) => {
            tracing::error!("Failed to mark checkpoints as interrupted: {}", e);
        }
    }

    // Mark any running agent runs as failed
    match ticketing_system::agent_runs::mark_all_running_as_interrupted(&db_pool).await {
        Ok(count) if count > 0 => {
            tracing::warn!("Marked {} agent run(s) as failed during shutdown", count);
        }
        Ok(_) => {}
        Err(e) => {
            tracing::error!("Failed to mark agent runs as failed: {}", e);
        }
    }

    // Log shutdown to system_logs
    let _ = ticketing_system::system_logs::insert_log(
        &db_pool, "warn", "api", "API server shutting down", None, None, None,
    ).await;

    tracing::info!("Graceful shutdown complete");
}
