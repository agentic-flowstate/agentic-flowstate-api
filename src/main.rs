mod handlers;
mod models;
mod utils;
mod mcp_wrapper;
mod agents;

use axum::{
    routing::{get, post},
    Router,
};
use std::sync::Arc;
use tower_http::cors::{CorsLayer, Any};
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

        // Health check
        .route("/health", get(|| async { "OK" }))

        // Add state and middleware
        .with_state(db_pool)
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        );

    // Start the server
    let addr = "127.0.0.1:8001";
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("Server running on http://{}", addr);

    axum::serve(listener, app).await?;

    Ok(())
}
