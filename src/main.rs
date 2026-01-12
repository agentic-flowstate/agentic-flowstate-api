mod handlers;
mod models;
mod utils;
mod mcp_wrapper;
mod agents;
mod email_fetcher;
pub mod pipeline_automation;
mod seed_templates;

use axum::{
    routing::{delete, get, patch, post},
    Router,
};
use std::sync::Arc;
use tower_http::cors::{CorsLayer, Any, AllowOrigin};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

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

    // Build the application
    let app = Router::new()
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
        .route("/api/tickets/:ticket_id", get(handlers::get_ticket_by_id))
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
        .route("/api/tickets/:ticket_id/pipeline/steps/:step_id/agent-run",
            get(handlers::get_step_agent_run))

        // Health check
        .route("/health", get(|| async { "OK" }))

        // Add state and middleware
        .with_state(db_pool)
        .layer(
            CorsLayer::new()
                .allow_origin(AllowOrigin::any())
                .allow_methods(Any)
                .allow_headers(Any)
                .expose_headers(Any),
        );

    // Start the server - bind to 0.0.0.0 to allow access from other devices (mobile via Tailscale)
    let addr = "0.0.0.0:8001";
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("Server running on http://{}", addr);

    axum::serve(listener, app).await?;

    Ok(())
}
