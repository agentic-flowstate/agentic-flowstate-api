use std::sync::Arc;
use std::time::Duration;

use ticketing_system::{email_intake, SqlitePool};
use tokio_util::sync::CancellationToken;

pub fn spawn_email_intake_scheduler(pool: Arc<SqlitePool>, token: CancellationToken) {
    tokio::spawn(async move {
        tracing::info!("[EMAIL_INTAKE] Expected-response scheduler starting");
        if let Err(e) =
            email_intake::refresh_expected_responses(&pool, "email_intake_scheduler").await
        {
            tracing::warn!(
                "[EMAIL_INTAKE] Startup expected-response refresh failed: {:?}",
                e
            );
        }

        let mut interval = tokio::time::interval(Duration::from_secs(60 * 60));
        loop {
            tokio::select! {
                _ = token.cancelled() => {
                    tracing::info!("[EMAIL_INTAKE] Expected-response scheduler stopping");
                    break;
                }
                _ = interval.tick() => {
                    match email_intake::refresh_expected_responses(&pool, "email_intake_scheduler").await {
                        Ok(items) if !items.is_empty() => {
                            tracing::info!(
                                "[EMAIL_INTAKE] Created {} overdue-response attention item(s)",
                                items.len()
                            );
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!("[EMAIL_INTAKE] Expected-response refresh failed: {:?}", e);
                        }
                    }
                }
            }
        }
    });
}
