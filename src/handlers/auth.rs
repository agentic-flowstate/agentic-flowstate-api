//! Authentication handlers - register, login, logout, session check

use std::sync::Arc;
use axum::{extract::State, http::StatusCode, Extension, Json};
use serde_json::{json, Value};
use tower_cookies::{Cookie, Cookies};

use ticketing_system::{LoginRequest, RegisterUserRequest, SqlitePool};

const SESSION_COOKIE: &str = "session";
const MAX_AGE_SECS: i64 = 30 * 24 * 60 * 60; // 30 days

fn make_session_cookie(session_id: &str) -> Cookie<'static> {
    let mut cookie = Cookie::new(SESSION_COOKIE, session_id.to_string());
    cookie.set_path("/");
    cookie.set_http_only(true);
    cookie.set_same_site(tower_cookies::cookie::SameSite::Lax);
    cookie.set_secure(false); // Internal HTTP on Tailscale
    cookie.set_max_age(tower_cookies::cookie::time::Duration::seconds(MAX_AGE_SECS));
    cookie
}

fn removal_cookie() -> Cookie<'static> {
    let mut cookie = Cookie::new(SESSION_COOKIE, "");
    cookie.set_path("/");
    cookie.set_http_only(true);
    cookie.set_max_age(tower_cookies::cookie::time::Duration::ZERO);
    cookie
}

/// POST /api/auth/register
pub async fn register(
    State(pool): State<Arc<SqlitePool>>,
    cookies: Cookies,
    Json(req): Json<RegisterUserRequest>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    if req.user_id.trim().is_empty() || req.password.trim().is_empty() || req.name.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, Json(json!({"error": "user_id, name, and password are required"}))));
    }

    let user = ticketing_system::auth::register_user(
        &pool,
        &req.user_id,
        &req.name,
        &req.password,
        req.email.as_deref(),
    )
    .await
    .map_err(|e| {
        let msg = e.to_string();
        if msg.contains("already has an account") {
            (StatusCode::CONFLICT, Json(json!({"error": msg})))
        } else {
            tracing::error!("Registration error: {:?}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "Registration failed"})))
        }
    })?;

    let session_id = ticketing_system::auth::create_session(&pool, &user.user_id)
        .await
        .map_err(|e| {
            tracing::error!("Session creation error: {:?}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "Failed to create session"})))
        })?;

    cookies.add(make_session_cookie(&session_id));

    crate::system_log_helper::log_event(
        &pool, "info", "auth",
        &format!("New user registered: {}", user.user_id),
        None, Some(&user.user_id), Some(&session_id),
    ).await;

    Ok((StatusCode::CREATED, Json(json!({
        "user_id": user.user_id,
        "name": user.name,
        "email": user.email,
    }))))
}

/// POST /api/auth/login
pub async fn login(
    State(pool): State<Arc<SqlitePool>>,
    cookies: Cookies,
    Json(req): Json<LoginRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    tracing::info!("Login attempt for user: {}", req.user_id);
    let user = ticketing_system::auth::authenticate(&pool, &req.user_id, &req.password)
        .await
        .map_err(|e| {
            tracing::error!("Authentication error: {:?}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "Authentication failed"})))
        })?;

    let Some(user) = user else {
        tracing::warn!("Login rejected (401) for user: {}", req.user_id);
        crate::system_log_helper::log_event(
            &pool, "warn", "auth",
            &format!("Failed login attempt for user: {}", req.user_id),
            None, Some(&req.user_id), None,
        ).await;
        return Err((StatusCode::UNAUTHORIZED, Json(json!({"error": "Invalid user_id or password"}))));
    };

    let session_id = ticketing_system::auth::create_session(&pool, &user.user_id)
        .await
        .map_err(|e| {
            tracing::error!("Session creation error: {:?}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "Failed to create session"})))
        })?;

    cookies.add(make_session_cookie(&session_id));

    crate::system_log_helper::log_event(
        &pool, "info", "auth",
        &format!("User logged in: {}", user.user_id),
        None, Some(&user.user_id), Some(&session_id),
    ).await;

    Ok(Json(json!({
        "user_id": user.user_id,
        "name": user.name,
        "email": user.email,
    })))
}

/// POST /api/auth/logout
pub async fn logout(
    State(pool): State<Arc<SqlitePool>>,
    cookies: Cookies,
) -> StatusCode {
    if let Some(cookie) = cookies.get(SESSION_COOKIE) {
        let session_id = cookie.value().to_string();
        let _ = ticketing_system::auth::delete_session(&pool, &session_id).await;
    }
    cookies.add(removal_cookie());
    StatusCode::NO_CONTENT
}

/// GET /api/auth/users/public
///
/// Returns a public list of every user in the system. Used by the iOS login
/// screen to populate a dropdown so users pick their account instead of
/// typing it. Intentionally unauthenticated — this is an internal tool on a
/// Tailscale network, and user_id + display name are not sensitive.
pub async fn list_public_users(
    State(pool): State<Arc<SqlitePool>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let users = ticketing_system::users::list_users(&pool)
        .await
        .map_err(|e| {
            tracing::error!("Failed to list users for public dropdown: {:?}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "Failed to list users"})))
        })?;

    let payload: Vec<Value> = users
        .into_iter()
        .map(|u| json!({"user_id": u.user_id, "name": u.name}))
        .collect();

    Ok(Json(json!({"users": payload})))
}

/// POST /api/auth/setup-code/redeem
///
/// Unauthenticated. Body: `{"code": "123456"}`. On success:
///   - Writes a fresh random password hash to the user row
///   - Creates a session (session cookie is set on the response)
///   - Returns `{user_id, name, password}` — the plaintext password is for
///     the iOS client to stash in the Keychain behind Face ID so the
///     auto-rotate-on-every-login flow can proceed.
pub async fn redeem_setup_code(
    State(pool): State<Arc<SqlitePool>>,
    cookies: Cookies,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let code = body
        .get("code")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "code is required"})),
            )
        })?;

    let redeemed = ticketing_system::setup_codes::redeem_code(&pool, &code)
        .await
        .map_err(|e| {
            let msg = e.to_string();
            tracing::warn!("Setup code redemption failed: {}", msg);
            if msg.contains("Invalid")
                || msg.contains("expired")
                || msg.contains("already been used")
                || msg.contains("no longer exists")
            {
                (StatusCode::UNAUTHORIZED, Json(json!({"error": msg})))
            } else {
                tracing::error!("Setup code redemption error: {:?}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "Redemption failed"})),
                )
            }
        })?;

    cookies.add(make_session_cookie(&redeemed.session_id));

    crate::system_log_helper::log_event(
        &pool,
        "info",
        "auth",
        &format!("Setup code redeemed for user: {}", redeemed.user_id),
        None,
        Some(&redeemed.user_id),
        Some(&redeemed.session_id),
    )
    .await;

    Ok(Json(json!({
        "user_id": redeemed.user_id,
        "name": redeemed.name,
        "password": redeemed.password,
    })))
}

/// POST /api/auth/password/rotate
///
/// Authenticated. Body: `{"new_password": "..."}`. Rotates the current
/// user's server-side password hash to the supplied plaintext. The iOS
/// client calls this after every Face ID sign-in so the secret never
/// stays stable long enough to matter if it ever leaked.
pub async fn rotate_password(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<crate::auth_middleware::AuthenticatedUser>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let new_password = body
        .get("new_password")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|s| s.len() >= 16)
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "new_password must be at least 16 characters"})),
            )
        })?;

    ticketing_system::auth::rotate_password(&pool, &user.user_id, &new_password)
        .await
        .map_err(|e| {
            tracing::error!("Password rotation error: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Rotation failed"})),
            )
        })?;

    crate::system_log_helper::log_event(
        &pool,
        "info",
        "auth",
        &format!("Password rotated for user: {}", user.user_id),
        None,
        Some(&user.user_id),
        None,
    )
    .await;

    Ok(Json(json!({"ok": true})))
}

/// GET /api/auth/me
pub async fn me(
    State(pool): State<Arc<SqlitePool>>,
    cookies: Cookies,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let session_id = cookies
        .get(SESSION_COOKIE)
        .map(|c| c.value().to_string())
        .ok_or_else(|| (StatusCode::UNAUTHORIZED, Json(json!({"error": "Not authenticated"}))))?;

    let user = ticketing_system::auth::validate_session(&pool, &session_id)
        .await
        .map_err(|e| {
            tracing::error!("Session validation error: {:?}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "Session validation failed"})))
        })?;

    let Some(user) = user else {
        return Err((StatusCode::UNAUTHORIZED, Json(json!({"error": "Session expired or invalid"}))));
    };

    let organizations = ticketing_system::memberships::list_user_organizations(&pool, &user.user_id)
        .await
        .unwrap_or_default();

    let is_admin = ticketing_system::system_logs::is_admin(&pool, &user.user_id)
        .await
        .unwrap_or(false);

    Ok(Json(json!({
        "user_id": user.user_id,
        "name": user.name,
        "email": user.email,
        "organizations": organizations,
        "is_admin": is_admin,
    })))
}
