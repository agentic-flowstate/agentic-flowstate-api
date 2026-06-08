use axum::{
    extract::{Path, State},
    http::StatusCode,
    Extension, Json,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{env, fmt::Display, sync::Arc};
use tower_cookies::Cookies;
use uuid::Uuid;
use webauthn_rs::prelude::{
    Passkey, PasskeyAuthentication, PasskeyRegistration, PublicKeyCredential,
    RegisterPublicKeyCredential, Url, Webauthn, WebauthnBuilder,
};

use ticketing_system::{passkeys, SqlitePool};

const REGISTRATION_CEREMONY: &str = "registration";
const AUTHENTICATION_CEREMONY: &str = "authentication";

#[derive(Debug, Deserialize)]
pub struct RegisterFinishRequest {
    pub challenge_id: String,
    pub credential: Value,
    pub nickname: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AuthenticateStartRequest {
    pub user_id: String,
}

#[derive(Debug, Deserialize)]
pub struct AuthenticateFinishRequest {
    pub challenge_id: String,
    pub credential: Value,
}

struct WebauthnConfig {
    rp_id: String,
    rp_name: String,
    origins: Vec<String>,
}

pub async fn passkey_config() -> Json<Value> {
    Json(passkey_config_status_from_values(
        env::var("AGENTIC_WEBAUTHN_RP_ID").ok(),
        env::var("AGENTIC_WEBAUTHN_RP_NAME").ok(),
        env::var("AGENTIC_WEBAUTHN_ORIGIN").ok(),
        env::var("AGENTIC_WEBAUTHN_EXTRA_ORIGINS").ok(),
    ))
}

pub async fn list_passkeys(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<crate::auth_middleware::AuthenticatedUser>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let credentials = passkeys::list_user_passkeys(&pool, &user.user_id)
        .await
        .map_err(internal_error("Failed to list passkeys"))?;

    Ok(Json(json!({
        "passkeys": credentials.into_iter().map(|credential| {
            json!({
                "credential_id": credential.credential_id,
                "nickname": credential.nickname,
                "created_at": credential.created_at,
                "last_used_at": credential.last_used_at,
            })
        }).collect::<Vec<_>>()
    })))
}

pub async fn start_passkey_registration(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<crate::auth_middleware::AuthenticatedUser>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let webauthn = webauthn().map_err(config_error)?;
    let account = ticketing_system::users::get_user(&pool, &user.user_id)
        .await
        .map_err(internal_error("Failed to load user"))?
        .ok_or_else(|| {
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "Authenticated user no longer exists"})),
            )
        })?;
    let existing = passkeys::list_user_passkeys(&pool, &user.user_id)
        .await
        .map_err(internal_error("Failed to load existing passkeys"))?;
    let exclude = existing
        .iter()
        .map(|credential| {
            decode_credential_id(&credential.credential_id)
                .map_err(internal_error("Invalid stored passkey credential id"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let user_uuid = stable_user_uuid(&account.user_id);
    let (options, state) = webauthn
        .start_passkey_registration(user_uuid, &account.user_id, &account.name, Some(exclude))
        .map_err(webauthn_error("Failed to start passkey registration"))?;
    let state_json =
        serde_json::to_string(&state).map_err(internal_error("Failed to serialize state"))?;
    let challenge = passkeys::store_challenge(
        &pool,
        Some(&user.user_id),
        REGISTRATION_CEREMONY,
        &state_json,
    )
    .await
    .map_err(internal_error("Failed to store passkey challenge"))?;

    Ok(Json(json!({
        "challenge_id": challenge.challenge_id,
        "expires_at": challenge.expires_at,
        "options": public_key_options(options)?,
    })))
}

pub async fn finish_passkey_registration(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<crate::auth_middleware::AuthenticatedUser>,
    Json(req): Json<RegisterFinishRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let webauthn = webauthn().map_err(config_error)?;
    let challenge = passkeys::take_challenge(&pool, &req.challenge_id, REGISTRATION_CEREMONY)
        .await
        .map_err(internal_error("Failed to load passkey challenge"))?
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                Json(
                    json!({"error": "Passkey registration challenge expired or was already used"}),
                ),
            )
        })?;
    if challenge.user_id.as_deref() != Some(&user.user_id) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(json!({"error": "Passkey challenge does not belong to this account"})),
        ));
    }

    let response = serde_json::from_value::<RegisterPublicKeyCredential>(req.credential)
        .map_err(bad_request("Invalid passkey registration credential"))?;
    let state = serde_json::from_str::<PasskeyRegistration>(&challenge.state_json)
        .map_err(internal_error("Invalid stored registration state"))?;
    let passkey = webauthn
        .finish_passkey_registration(&response, &state)
        .map_err(webauthn_error("Passkey registration failed"))?;
    let credential_id = encode_credential_id(passkey.cred_id());
    let credential_json =
        serde_json::to_string(&passkey).map_err(internal_error("Failed to serialize passkey"))?;
    let record = passkeys::upsert_passkey(
        &pool,
        &credential_id,
        &user.user_id,
        req.nickname.as_deref(),
        &credential_json,
    )
    .await
    .map_err(internal_error("Failed to save passkey"))?;

    Ok(Json(json!({
        "credential_id": record.credential_id,
        "nickname": record.nickname,
        "created_at": record.created_at,
        "last_used_at": record.last_used_at,
    })))
}

pub async fn delete_passkey(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<crate::auth_middleware::AuthenticatedUser>,
    Path(credential_id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<Value>)> {
    let deleted = passkeys::delete_user_passkey(&pool, &user.user_id, &credential_id)
        .await
        .map_err(internal_error("Failed to delete passkey"))?;
    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Passkey not found"})),
        ))
    }
}

pub async fn start_passkey_authentication(
    State(pool): State<Arc<SqlitePool>>,
    Json(req): Json<AuthenticateStartRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let user_id = req.user_id.trim();
    if user_id.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "user_id is required"})),
        ));
    }

    let webauthn = webauthn().map_err(config_error)?;
    let records = passkeys::list_user_passkeys(&pool, user_id)
        .await
        .map_err(internal_error("Failed to load passkeys"))?;
    if records.is_empty() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "No passkeys are registered for this account"})),
        ));
    }

    let credentials = records
        .iter()
        .map(|record| {
            serde_json::from_str::<Passkey>(&record.credential_json)
                .map_err(internal_error("Invalid stored passkey"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let (options, state) = webauthn
        .start_passkey_authentication(&credentials)
        .map_err(webauthn_error("Failed to start passkey authentication"))?;
    let state_json =
        serde_json::to_string(&state).map_err(internal_error("Failed to serialize state"))?;
    let challenge =
        passkeys::store_challenge(&pool, Some(user_id), AUTHENTICATION_CEREMONY, &state_json)
            .await
            .map_err(internal_error("Failed to store passkey challenge"))?;

    Ok(Json(json!({
        "challenge_id": challenge.challenge_id,
        "expires_at": challenge.expires_at,
        "options": public_key_options(options)?,
    })))
}

pub async fn finish_passkey_authentication(
    State(pool): State<Arc<SqlitePool>>,
    cookies: Cookies,
    Json(req): Json<AuthenticateFinishRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let webauthn = webauthn().map_err(config_error)?;
    let challenge = passkeys::take_challenge(&pool, &req.challenge_id, AUTHENTICATION_CEREMONY)
        .await
        .map_err(internal_error("Failed to load passkey challenge"))?
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Passkey authentication challenge expired or was already used"})),
            )
        })?;
    let user_id = challenge.user_id.ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Passkey challenge is missing an account"})),
        )
    })?;
    let response = serde_json::from_value::<PublicKeyCredential>(req.credential)
        .map_err(bad_request("Invalid passkey authentication credential"))?;
    let credential_id = response.id.clone();
    let record = passkeys::get_passkey(&pool, &credential_id)
        .await
        .map_err(internal_error("Failed to load passkey"))?
        .ok_or_else(|| {
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "Passkey is not registered"})),
            )
        })?;
    if record.user_id != user_id {
        return Err((
            StatusCode::FORBIDDEN,
            Json(json!({"error": "Passkey does not belong to this challenge account"})),
        ));
    }

    let mut passkey = serde_json::from_str::<Passkey>(&record.credential_json)
        .map_err(internal_error("Invalid stored passkey"))?;
    let state = serde_json::from_str::<PasskeyAuthentication>(&challenge.state_json)
        .map_err(internal_error("Invalid stored authentication state"))?;
    let result = webauthn
        .finish_passkey_authentication(&response, &state)
        .map_err(webauthn_error("Passkey authentication failed"))?;
    let changed = passkey.update_credential(&result).unwrap_or(false);
    let updated_json = if changed {
        Some(
            serde_json::to_string(&passkey)
                .map_err(internal_error("Failed to serialize updated passkey"))?,
        )
    } else {
        None
    };
    passkeys::touch_passkey(&pool, &credential_id, updated_json.as_deref())
        .await
        .map_err(internal_error("Failed to update passkey"))?;

    let user = ticketing_system::users::get_user(&pool, &record.user_id)
        .await
        .map_err(internal_error("Failed to load user"))?
        .ok_or_else(|| {
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "Passkey account no longer exists"})),
            )
        })?;
    let session_id = ticketing_system::auth::create_session(&pool, &user.user_id)
        .await
        .map_err(internal_error("Failed to create session"))?;
    cookies.add(crate::handlers::auth::make_session_cookie(&session_id));

    Ok(Json(
        crate::handlers::auth::auth_user_json(&pool, &user).await,
    ))
}

fn webauthn() -> Result<Webauthn, String> {
    let config = webauthn_config()?;
    let primary = Url::parse(&config.origins[0])
        .map_err(|error| format!("AGENTIC_WEBAUTHN_ORIGIN is invalid: {error}"))?;
    let mut builder = WebauthnBuilder::new(&config.rp_id, &primary)
        .map_err(|error| format!("WebAuthn relying-party configuration is invalid: {error}"))?
        .rp_name(&config.rp_name);
    for origin in config.origins.iter().skip(1) {
        let origin = Url::parse(origin).map_err(|error| {
            format!("AGENTIC_WEBAUTHN_EXTRA_ORIGINS contains invalid URL: {error}")
        })?;
        builder = builder.append_allowed_origin(&origin);
    }
    builder
        .build()
        .map_err(|error| format!("Failed to build WebAuthn verifier: {error}"))
}

fn webauthn_config() -> Result<WebauthnConfig, String> {
    let rp_id = required_env_value(
        "AGENTIC_WEBAUTHN_RP_ID",
        env::var("AGENTIC_WEBAUTHN_RP_ID").ok(),
    )?;
    let rp_name = required_env_value(
        "AGENTIC_WEBAUTHN_RP_NAME",
        env::var("AGENTIC_WEBAUTHN_RP_NAME").ok(),
    )?;
    let origin = required_env_value(
        "AGENTIC_WEBAUTHN_ORIGIN",
        env::var("AGENTIC_WEBAUTHN_ORIGIN").ok(),
    )?;
    let mut origins = vec![origin];
    if let Ok(extra) = env::var("AGENTIC_WEBAUTHN_EXTRA_ORIGINS") {
        origins.extend(
            extra
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
        );
    }
    Ok(WebauthnConfig {
        rp_id,
        rp_name,
        origins,
    })
}

fn required_env_value(name: &str, value: Option<String>) -> Result<String, String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{name} is required for passkey/WebAuthn endpoints"))
}

fn passkey_config_status_from_values(
    rp_id: Option<String>,
    rp_name: Option<String>,
    origin: Option<String>,
    extra_origins: Option<String>,
) -> Value {
    let rp_id = trim_optional(rp_id);
    let rp_name = trim_optional(rp_name);
    let origin = trim_optional(origin);
    let extra_origins = extra_origins
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();

    let mut missing = Vec::new();
    if rp_id.is_none() {
        missing.push("AGENTIC_WEBAUTHN_RP_ID");
    }
    if rp_name.is_none() {
        missing.push("AGENTIC_WEBAUTHN_RP_NAME");
    }
    if origin.is_none() {
        missing.push("AGENTIC_WEBAUTHN_ORIGIN");
    }

    let mut origins = Vec::new();
    if let Some(origin) = &origin {
        origins.push(origin.clone());
    }
    origins.extend(extra_origins);

    json!({
        "enabled": missing.is_empty(),
        "rp_id": rp_id,
        "rp_name": rp_name,
        "origin": origin,
        "origins": origins,
        "missing": missing,
        "requires_secure_origin": true,
    })
}

fn trim_optional(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn public_key_options<T: serde::Serialize>(options: T) -> Result<Value, (StatusCode, Json<Value>)> {
    serde_json::to_value(options)
        .map(|mut value| value.pointer_mut("/publicKey").cloned().unwrap_or(value))
        .map_err(internal_error("Failed to serialize WebAuthn options"))
}

fn stable_user_uuid(user_id: &str) -> Uuid {
    Uuid::new_v5(&Uuid::NAMESPACE_URL, user_id.as_bytes())
}

fn encode_credential_id(credential_id: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(credential_id)
}

fn decode_credential_id(credential_id: &str) -> Result<Vec<u8>, base64::DecodeError> {
    URL_SAFE_NO_PAD.decode(credential_id)
}

fn config_error(message: String) -> (StatusCode, Json<Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": message})),
    )
}

fn internal_error<E: Display>(
    public_message: &'static str,
) -> impl FnOnce(E) -> (StatusCode, Json<Value>) {
    move |error| {
        tracing::error!("{public_message}: {error}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": public_message})),
        )
    }
}

fn bad_request<E: Display>(
    public_message: &'static str,
) -> impl FnOnce(E) -> (StatusCode, Json<Value>) {
    move |error| {
        tracing::warn!("{public_message}: {error}");
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": public_message})),
        )
    }
}

fn webauthn_error<E: Display>(
    public_message: &'static str,
) -> impl FnOnce(E) -> (StatusCode, Json<Value>) {
    move |error| {
        tracing::warn!("{public_message}: {error}");
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": public_message})),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passkey_config_status_reports_missing_required_env() {
        let status = passkey_config_status_from_values(None, Some("Agentic".into()), None, None);

        assert_eq!(status["enabled"], false);
        assert_eq!(
            status["missing"].as_array().unwrap(),
            &vec![
                json!("AGENTIC_WEBAUTHN_RP_ID"),
                json!("AGENTIC_WEBAUTHN_ORIGIN"),
            ]
        );
        assert_eq!(status["requires_secure_origin"], true);
    }

    #[test]
    fn passkey_config_status_reports_public_relying_party_values() {
        let status = passkey_config_status_from_values(
            Some("flowstate.example.com".into()),
            Some(" Agentic Flowstate ".into()),
            Some("https://flowstate.example.com".into()),
            Some(" https://backup.example.com, ,https://mac.example.com ".into()),
        );

        assert_eq!(status["enabled"], true);
        assert_eq!(status["rp_id"], "flowstate.example.com");
        assert_eq!(status["rp_name"], "Agentic Flowstate");
        assert_eq!(status["origin"], "https://flowstate.example.com");
        assert_eq!(
            status["origins"].as_array().unwrap(),
            &vec![
                json!("https://flowstate.example.com"),
                json!("https://backup.example.com"),
                json!("https://mac.example.com"),
            ]
        );
        assert!(status["missing"].as_array().unwrap().is_empty());
    }

    #[test]
    fn required_env_value_rejects_blank_values() {
        assert_eq!(
            required_env_value("AGENTIC_WEBAUTHN_RP_ID", Some("   ".into())),
            Err("AGENTIC_WEBAUTHN_RP_ID is required for passkey/WebAuthn endpoints".into())
        );
    }
}
