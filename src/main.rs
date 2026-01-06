mod db;
mod handlers;
mod models;

use axum::{
    routing::{get},
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

    // Initialize DynamoDB connection pool
    let db_pool = Arc::new(db::DynamoDbPool::new().await?);
    tracing::info!("DynamoDB connection pool initialized");

    // Build the application
    let app = Router::new()
        // Epic routes
        .route("/api/epics", get(handlers::list_epics))

        // Slice routes
        .route("/api/epics/:epic_id/slices", get(handlers::list_slices))

        // Ticket routes - supports both with and without slice_id
        .route("/api/epics/:epic_id/tickets", get(handlers::list_tickets))
        .route("/api/epics/:epic_id/slices/:slice_id/tickets", get(handlers::list_slice_tickets))

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
