//! Device-token registration endpoints (APNs).
//!
//! * `POST /v1/devices` — upsert a `{device_token, bundle_id, user_id, platform}`
//!   registration for the authenticated session. Re-registration of a
//!   previously soft-deleted row revives it in place.
//! * `GET /v1/devices` — list the caller's active (non-soft-deleted) device
//!   registrations.
//!
//! Soft-delete is performed server-side when the APNs silent-push sender
//! returns [`apns::ApnsSilentError::DeviceUnregistered`] — see
//! [`ticketing_system::device_tokens::soft_delete_device_token`] and the
//! contract in artifact `A-C710FBCA`.
//!
//! Both endpoints sit behind the existing session-cookie auth middleware.
//! Missing / invalid session → 401 (handled upstream by `require_auth`).

use axum::{
    extract::{Extension, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use ticketing_system::device_tokens::{
    list_active_for_user, soft_delete_device_token, upsert_device_token, DeviceToken,
};
use ticketing_system::SqlitePool;

use crate::auth_middleware::AuthenticatedUser;
use crate::observability::streaming::{record_session_start, ClientPlatform};

// ---------------------------------------------------------------------------
// POST /v1/devices
// ---------------------------------------------------------------------------

/// Request body for `POST /v1/devices`. The JSON body mirrors the shape the
/// iOS client produces after `application:didRegisterForRemoteNotificationsWithDeviceToken:`.
#[derive(Debug, Deserialize)]
pub struct RegisterDeviceRequest {
    /// APNs device token (hex-encoded, 64+ chars depending on APNs format).
    pub device_token: String,
    /// App bundle identifier the token was registered against (used by the
    /// silent-push sender as `apns-topic`).
    pub bundle_id: String,
    /// User id reported by the client. Must match the authenticated session's
    /// user — the server authoritatively stamps `user_id` from the session so
    /// a mismatch is rejected with 400 rather than silently trusting the body.
    pub user_id: String,
    /// Must be exactly `"ios"`. Non-ios platforms are a schema change, not a
    /// request-time decision, and will be rejected with 400.
    pub platform: String,
    /// Optional human-readable device name (e.g. "Alex's iPhone 15 Pro"). Not
    /// part of the minimum contract but useful for the `GET /v1/devices`
    /// debug listing.
    pub device_name: Option<String>,
    /// Optional client build version (e.g. "1.23.0 (45)"). Populated by the
    /// iOS app so the `clients_session_start_total` metric can segment
    /// by release for adoption tracking and crash-rate dashboards.
    pub client_version: Option<String>,
}

/// Wire shape for a device-token row. Mirrors [`DeviceToken`] one-for-one so
/// clients don't have to know about the library type.
#[derive(Debug, Serialize)]
pub struct DeviceTokenResponse {
    pub id: i64,
    pub user_id: String,
    pub device_token: String,
    pub bundle_id: String,
    pub platform: String,
    pub device_name: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_seen_at: i64,
}

impl From<DeviceToken> for DeviceTokenResponse {
    fn from(d: DeviceToken) -> Self {
        Self {
            id: d.id,
            user_id: d.user_id,
            device_token: d.device_token,
            bundle_id: d.bundle_id,
            platform: d.platform,
            device_name: d.device_name,
            created_at: d.created_at,
            updated_at: d.updated_at,
            last_seen_at: d.last_seen_at,
        }
    }
}

/// `POST /v1/devices` — upsert a device token for the authenticated user.
pub async fn register_device(
    State(db): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(req): Json<RegisterDeviceRequest>,
) -> Result<(StatusCode, Json<DeviceTokenResponse>), (StatusCode, Json<serde_json::Value>)> {
    if req.platform != "ios" {
        return Err(bad_request(&format!(
            "platform must be exactly \"ios\" (got {:?})",
            req.platform
        )));
    }
    if req.device_token.trim().is_empty() {
        return Err(bad_request("device_token must be non-empty"));
    }
    if req.bundle_id.trim().is_empty() {
        return Err(bad_request("bundle_id must be non-empty"));
    }
    if req.user_id != user.user_id {
        return Err(bad_request(
            "user_id in body must match the authenticated session",
        ));
    }

    let row = upsert_device_token(
        &db,
        &user.user_id,
        &req.device_token,
        &req.bundle_id,
        &req.platform,
        req.device_name.as_deref(),
    )
    .await
    .map_err(|e| internal(&format!("upsert failed: {}", e)))?;

    tracing::info!(
        "[DEVICE] upserted token for user {} (id={}, bundle={}, active)",
        user.user_id,
        row.id,
        row.bundle_id
    );

    // T-56987678: emit `clients_session_start_total` on every device
    // registration. This is the only path iOS clients call at launch before
    // streaming, so it's the canonical session-start signal.
    let platform = ClientPlatform::parse(&req.platform);
    let version = req.client_version.as_deref().unwrap_or("unknown");
    record_session_start(&user.user_id, platform, version);

    Ok((StatusCode::OK, Json(row.into())))
}

// ---------------------------------------------------------------------------
// GET /v1/devices
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct ListDevicesResponse {
    pub devices: Vec<DeviceTokenResponse>,
}

/// `GET /v1/devices` — return the caller's active (non-soft-deleted)
/// device-token registrations. Debug / operational visibility.
pub async fn list_devices(
    State(db): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<Json<ListDevicesResponse>, (StatusCode, Json<serde_json::Value>)> {
    let rows = list_active_for_user(&db, &user.user_id)
        .await
        .map_err(|e| internal(&format!("list failed: {}", e)))?;
    let devices = rows.into_iter().map(DeviceTokenResponse::from).collect();
    Ok(Json(ListDevicesResponse { devices }))
}

// ---------------------------------------------------------------------------
// Legacy /api/device-tokens endpoints — retained to keep the existing iOS
// alert-push registration flow working against the new schema. These use
// `user_id` from the session and the app's standard bundle id.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct RegisterTokenRequest {
    pub device_token: String,
    pub device_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RemoveTokenRequest {
    pub device_token: String,
}

/// `POST /api/device-tokens` — legacy alert-push registration entry point.
///
/// The legacy body only carries `device_token` + `device_name`, so platform
/// is fixed to `"ios"` and bundle id is taken from `APNS_BUNDLE_ID` if set,
/// else the app's standard bundle id constant.
pub async fn register_device_token(
    State(db): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(req): Json<RegisterTokenRequest>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    let bundle_id = legacy_bundle_id();
    upsert_device_token(
        &db,
        &user.user_id,
        &req.device_token,
        &bundle_id,
        "ios",
        req.device_name.as_deref(),
    )
    .await
    .map_err(|e| internal(&format!("upsert failed: {}", e)))?;

    tracing::info!(
        "[DEVICE-TOKEN] Registered legacy token for user {}",
        user.user_id
    );
    Ok(StatusCode::OK)
}

/// `DELETE /api/device-tokens` — soft-delete a token for the caller.
pub async fn remove_device_token(
    State(db): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(req): Json<RemoveTokenRequest>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    soft_delete_device_token(&db, &user.user_id, &req.device_token)
        .await
        .map_err(|e| internal(&format!("soft-delete failed: {}", e)))?;
    tracing::info!(
        "[DEVICE-TOKEN] Soft-deleted legacy token for user {}",
        user.user_id
    );
    Ok(StatusCode::OK)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn legacy_bundle_id() -> String {
    std::env::var("APNS_BUNDLE_ID")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "com.agenticflowstate.app".to_string())
}

fn bad_request(msg: &str) -> (StatusCode, Json<serde_json::Value>) {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": msg })))
}

fn internal(msg: &str) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": msg })),
    )
}
