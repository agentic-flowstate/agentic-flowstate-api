mod handlers;
mod models;
mod mcp_wrapper;
mod agents;
mod email_fetcher;
pub mod pipeline_automation;
mod seed_templates;
mod auth_middleware;

use axum::{
    routing::{delete, get, patch, post},
    Router,
    extract::DefaultBodyLimit,
};
use std::sync::Arc;
use tower_http::cors::{CorsLayer, AllowOrigin};
use http::{header, Method};
use tower_cookies::CookieManagerLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use tokio::signal;

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

    // Reset any pipeline steps stuck in "running" state from previous run
    match ticketing_system::pipelines::reset_interrupted_pipeline_steps(&db_pool).await {
        Ok(count) if count > 0 => {
            tracing::warn!("Reset interrupted pipeline steps on {} ticket(s) from previous run", count);
        }
        Ok(_) => {
            tracing::debug!("No interrupted pipeline steps to reset");
        }
        Err(e) => {
            tracing::error!("Failed to reset interrupted pipeline steps: {}", e);
        }
    }

    // Seed default pipeline templates
    if let Err(e) = seed_templates::seed_default_templates(&db_pool).await {
        tracing::warn!("Failed to seed pipeline templates: {:?}", e);
    }

    // Start email fetcher background task
    match email_fetcher::load_email_accounts() {
        Ok(accounts) if !accounts.is_empty() => {
            tracing::info!("Starting email fetcher for {} account(s)", accounts.len());
            email_fetcher::start_email_fetcher(db_pool.clone(), accounts);
        }
        Ok(_) => {
            tracing::info!("No email accounts configured, email fetcher disabled");
        }
        Err(e) => {
            tracing::warn!("Failed to load email accounts: {:?}", e);
        }
    }

    // Clone db_pool for shutdown handler before building router (which moves db_pool)
    let shutdown_db = db_pool.clone();

    // Session cleanup background task (every 6 hours)
    {
        let cleanup_pool = db_pool.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(6 * 60 * 60));
            loop {
                interval.tick().await;
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

    // Public routes (no auth required)
    let public_routes = Router::new()
        .route("/api/auth/register", post(handlers::auth::register))
        .route("/api/auth/login", post(handlers::auth::login))
        .route("/api/auth/logout", post(handlers::auth::logout))
        .route("/api/auth/me", get(handlers::auth::me))
        .route("/health", get(|| async { "OK" }));

    // Protected routes (require valid session)
    let protected_routes = Router::new()
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
        // Nested ticket routes (with epic_id/slice_id/ticket_id)
        .route("/api/epics/:epic_id/slices/:slice_id/tickets/:ticket_id",
            get(handlers::get_ticket_nested)
            .patch(handlers::update_ticket_nested)
            .delete(handlers::delete_ticket_nested))
        .route("/api/epics/:epic_id/slices/:slice_id/tickets/:ticket_id/relationships",
            post(handlers::add_relationship_nested)
            .delete(handlers::remove_relationship_nested))
        .route("/api/epics/:epic_id/slices/:slice_id/tickets/:ticket_id/history",
            get(handlers::get_ticket_history))

        // Agent run routes
        .route("/api/epics/:epic_id/slices/:slice_id/tickets/:ticket_id/agent-runs",
            get(handlers::list_agent_runs)
            .post(handlers::run_agent))
        .route("/api/epics/:epic_id/slices/:slice_id/tickets/:ticket_id/agent-runs/stream",
            post(handlers::stream_agent_run))
        .route("/api/epics/:epic_id/slices/:slice_id/tickets/:ticket_id/agent-runs/active",
            get(handlers::get_active_agent_run))
        .route("/api/agent-runs/:session_id",
            get(handlers::get_agent_run))
        .route("/api/agent-runs/:session_id/stream",
            get(handlers::reconnect_agent_stream))
        .route("/api/agent-runs/:session_id/message",
            post(handlers::send_message_to_agent))

        // Email routes
        .route("/api/emails", get(handlers::list_emails))
        .route("/api/emails/send", post(handlers::send_email))
        .route("/api/emails/stats", get(handlers::get_email_stats))
        .route("/api/emails/:id",
            get(handlers::get_email)
            .patch(handlers::update_email)
            .delete(handlers::delete_email))

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

        // Workspace Manager routes
        .route("/api/workspace-manager/chat",
            post(handlers::workspace_manager_chat))
        .route("/api/workspace-manager/resume",
            post(handlers::workspace_manager_resume))

        // Life Planner routes
        .route("/api/life-planner/chat",
            post(handlers::life_planner_chat))
        .route("/api/life-planner/resume",
            post(handlers::life_planner_resume))

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

        // Conversation routes (for workspace manager persistence)
        .route("/api/conversations",
            get(handlers::list_conversations)
            .post(handlers::create_conversation))
        .route("/api/conversations/subscribe",
            get(handlers::subscribe_conversations))
        .route("/api/conversations/:id",
            get(handlers::get_conversation)
            .patch(handlers::update_conversation)
            .delete(handlers::delete_conversation))
        .route("/api/conversations/:id/messages",
            get(handlers::list_messages)
            .post(handlers::add_message))
        .route("/api/conversations/:conv_id/messages/:message_id",
            patch(handlers::update_message))

        // Pipeline template routes
        .route("/api/pipeline-templates",
            get(handlers::list_templates)
            .post(handlers::create_template))
        .route("/api/pipeline-templates/:template_id",
            get(handlers::get_template)
            .delete(handlers::delete_template))

        // Ticket pipeline routes
        .route("/api/tickets/:ticket_id/pipeline",
            get(handlers::get_ticket_pipeline)
            .post(handlers::set_ticket_pipeline)
            .delete(handlers::delete_ticket_pipeline))
        .route("/api/tickets/:ticket_id/pipeline/run",
            post(handlers::run_pipeline))

        // Pipeline step operations
        .route("/api/tickets/:ticket_id/pipeline/steps/:step_id/start",
            post(handlers::start_step))
        .route("/api/tickets/:ticket_id/pipeline/steps/:step_id/complete",
            post(handlers::complete_step))
        .route("/api/tickets/:ticket_id/pipeline/steps/:step_id/fail",
            post(handlers::fail_step))
        .route("/api/tickets/:ticket_id/pipeline/steps/:step_id/approve",
            post(handlers::approve_step))
        .route("/api/tickets/:ticket_id/pipeline/steps/:step_id/reject",
            post(handlers::reject_step))
        .route("/api/tickets/:ticket_id/pipeline/steps/:step_id/retry",
            post(handlers::retry_step))
        .route("/api/tickets/:ticket_id/pipeline/steps/:step_id/agent-run",
            get(handlers::get_step_agent_run))

        // Data events SSE (live updates)
        .route("/api/data/subscribe", get(handlers::subscribe_data))

        // Meeting routes
        .route("/api/meetings",
            get(handlers::list_meetings)
            .post(handlers::create_meeting))
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
        .route("/api/meetings/:room_id/favorite",
            post(handlers::toggle_meeting_favorite))

        .layer(axum::middleware::from_fn_with_state(db_pool.clone(), auth_middleware::require_auth));

    let app = public_routes
        .merge(protected_routes)
        .with_state(db_pool)
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024 * 1024)) // 2GB - never lose a session due to size limits
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

    // Run server with graceful shutdown
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown_db))
        .await?;

    Ok(())
}

/// Graceful shutdown signal handler
/// Waits for SIGTERM or Ctrl+C, then marks running checkpoints as interrupted
async fn shutdown_signal(db_pool: Arc<ticketing_system::SqlitePool>) {
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
    tracing::info!("Waiting 2 seconds for in-flight operations to complete...");
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

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

    // Reset any pipeline steps stuck in "running" state
    match ticketing_system::pipelines::reset_interrupted_pipeline_steps(&db_pool).await {
        Ok(count) if count > 0 => {
            tracing::warn!("Reset interrupted pipeline steps on {} ticket(s) during shutdown", count);
        }
        Ok(_) => {}
        Err(e) => {
            tracing::error!("Failed to reset pipeline steps: {}", e);
        }
    }

    tracing::info!("Graceful shutdown complete");
}
