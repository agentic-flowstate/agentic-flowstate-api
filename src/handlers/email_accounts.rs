use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use ticketing_system::{
    email_accounts, CreateEmailAccountRequest, EmailAccount, EmailIdentity, SqlitePool,
};

use crate::auth_middleware::AuthenticatedUser;

/// List email accounts for the authenticated user (GET /api/email-accounts)
pub async fn list_email_accounts(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<Json<Vec<EmailAccount>>, (StatusCode, String)> {
    let accounts = email_accounts::list_email_accounts_for_user(&pool, &user.user_id, true)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(accounts))
}

/// List send identities for the authenticated user (GET /api/email-identities)
pub async fn list_email_identities(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<Json<Vec<EmailIdentity>>, (StatusCode, String)> {
    let identities = email_accounts::list_email_identities_for_user(&pool, &user.user_id, true)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(identities))
}

/// Create an email account (POST /api/email-accounts)
/// After creation, triggers an immediate IMAP sync and signals the IDLE manager
/// to start watchers for the new account.
pub async fn create_email_account(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(mut req): Json<CreateEmailAccountRequest>,
) -> Result<(StatusCode, Json<EmailAccount>), (StatusCode, String)> {
    req.user_id = user.user_id;

    let account = email_accounts::create_email_account(&pool, &req)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    // Trigger immediate sync in background so emails arrive before IDLE starts
    let sync_pool = pool.clone();
    let sync_email = account.email.clone();
    tokio::spawn(async move {
        if let Ok(internal) = email_accounts::get_account_internal(&sync_pool, &sync_email).await {
            match crate::email_fetcher::fetch_emails_for_account(&sync_pool, &internal).await {
                Ok(()) => {
                    email_accounts::update_fetch_status(&sync_pool, &sync_email, "ok", None)
                        .await
                        .ok();
                    tracing::info!("Force sync completed for new account {}", sync_email);
                }
                Err(e) => {
                    let err_msg = format!("{:?}", e);
                    tracing::error!("Force sync failed for {}: {}", sync_email, err_msg);
                    email_accounts::update_fetch_status(
                        &sync_pool,
                        &sync_email,
                        "error",
                        Some(&err_msg),
                    )
                    .await
                    .ok();
                }
            }
        }
    });

    // Signal the IDLE account manager to pick up the new account immediately
    crate::email_fetcher::account_change_signal().notify_one();

    Ok((StatusCode::CREATED, Json(account)))
}

#[derive(Deserialize)]
pub struct EmailAccountPath {
    pub email: String,
}

/// Delete an email account (DELETE /api/email-accounts/:email)
/// Signals the IDLE manager to stop watchers for the removed account.
pub async fn delete_email_account(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(path): Path<EmailAccountPath>,
) -> Result<StatusCode, (StatusCode, String)> {
    // Verify ownership
    let account = email_accounts::get_email_account(&pool, &path.email)
        .await
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    if account.user_id != user.user_id {
        return Err((StatusCode::FORBIDDEN, "Not your email account".to_string()));
    }

    email_accounts::delete_email_account(&pool, &path.email)
        .await
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    // Signal the IDLE account manager to stop watchers for this account
    crate::email_fetcher::account_change_signal().notify_one();

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub struct SyncResponse {
    pub status: String,
    pub message: String,
}

/// Force sync emails for a specific account (POST /api/email-accounts/:email/sync)
pub async fn sync_email_account(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(path): Path<EmailAccountPath>,
) -> Result<Json<SyncResponse>, (StatusCode, String)> {
    // Verify access (owner or granted)
    let has_access = email_accounts::user_has_email_access(&pool, &path.email, &user.user_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !has_access {
        return Err((
            StatusCode::FORBIDDEN,
            "No access to this email account".to_string(),
        ));
    }

    let internal = email_accounts::get_account_internal(&pool, &path.email)
        .await
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    match crate::email_fetcher::fetch_emails_for_account(&pool, &internal).await {
        Ok(()) => {
            email_accounts::update_fetch_status(&pool, &path.email, "ok", None)
                .await
                .ok();
            Ok(Json(SyncResponse {
                status: "ok".to_string(),
                message: format!("Sync completed for {}", path.email),
            }))
        }
        Err(e) => {
            let err_msg = format!("{:?}", e);
            email_accounts::update_fetch_status(&pool, &path.email, "error", Some(&err_msg))
                .await
                .ok();
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Sync failed: {}", err_msg),
            ))
        }
    }
}
