use sqlx::SqlitePool;
use std::sync::Arc;

/// Log a system event to the database. Swallows errors (non-critical path).
pub async fn log_event(
    pool: &Arc<SqlitePool>,
    level: &str,
    component: &str,
    message: &str,
    detail: Option<&str>,
    user_id: Option<&str>,
    session_id: Option<&str>,
) {
    if let Err(e) = ticketing_system::system_logs::insert_log(
        pool, level, component, message, detail, user_id, session_id,
    )
    .await
    {
        tracing::warn!("Failed to write system log: {:?}", e);
    }
}

pub async fn log_error(
    pool: &Arc<SqlitePool>,
    component: &str,
    message: &str,
    detail: Option<&str>,
) {
    log_event(pool, "error", component, message, detail, None, None).await;
}

pub async fn log_warn(
    pool: &Arc<SqlitePool>,
    component: &str,
    message: &str,
    detail: Option<&str>,
) {
    log_event(pool, "warn", component, message, detail, None, None).await;
}

pub async fn log_info(
    pool: &Arc<SqlitePool>,
    component: &str,
    message: &str,
    detail: Option<&str>,
) {
    log_event(pool, "info", component, message, detail, None, None).await;
}
