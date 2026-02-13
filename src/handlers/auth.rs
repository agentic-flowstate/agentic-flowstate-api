//! Authentication handlers - register, login, logout, session check

use std::sync::Arc;
use axum::{extract::State, http::StatusCode, Json};
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
    let user = ticketing_system::auth::authenticate(&pool, &req.user_id, &req.password)
        .await
        .map_err(|e| {
            tracing::error!("Authentication error: {:?}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "Authentication failed"})))
        })?;

    let Some(user) = user else {
        return Err((StatusCode::UNAUTHORIZED, Json(json!({"error": "Invalid user_id or password"}))));
    };

    let session_id = ticketing_system::auth::create_session(&pool, &user.user_id)
        .await
        .map_err(|e| {
            tracing::error!("Session creation error: {:?}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "Failed to create session"})))
        })?;

    cookies.add(make_session_cookie(&session_id));

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

    Ok(Json(json!({
        "user_id": user.user_id,
        "name": user.name,
        "email": user.email,
    })))
}
