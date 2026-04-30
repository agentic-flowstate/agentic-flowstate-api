use axum::{
    extract::{Extension, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::Deserialize;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use ticketing_system::{SqlitePool, TranscriptionResponse};

use crate::{auth_middleware::AuthenticatedUser, system_log_helper};

const CLIENT_REQUEST_ID_HEADER: &str = "X-Client-Request-ID";
const WHISPER_TIMEOUT_SECONDS: u64 = 65;

// ============================================================================
// Standalone Voice Transcription (OpenAI Whisper)
// ============================================================================

/// Request body for voice transcription.
#[derive(Debug, Deserialize)]
pub struct VoiceTranscribeRequest {
    /// Base64-encoded audio data
    pub audio_data: String,
    /// Audio format (m4a, webm, mp3, wav, etc.)
    pub format: String,
    /// Optional language hint (e.g., "en")
    pub language: Option<String>,
}

/// POST /api/transcribe
///
/// Accepts base64-encoded audio data, sends it to OpenAI Whisper, returns text.
/// Uses the server-side OPENAI_KEY — no client-side API key required.
pub async fn voice_transcribe(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Json(req): Json<VoiceTranscribeRequest>,
) -> Result<Json<TranscriptionResponse>, (StatusCode, String)> {
    use base64::Engine;
    let request_id = client_request_id(&headers);
    let started_at = Instant::now();

    let audio_bytes = match base64::engine::general_purpose::STANDARD.decode(&req.audio_data) {
        Ok(bytes) => bytes,
        Err(e) => {
            log_voice_event(
                &pool,
                "warn",
                "transcription_invalid_audio",
                format!("request_id={} error={}", request_id, e),
                &user.user_id,
            )
            .await;
            return Err((StatusCode::BAD_REQUEST, format!("Invalid base64: {}", e)));
        }
    };
    let audio_byte_count = audio_bytes.len();

    log_voice_event(
        &pool,
        "info",
        "transcription_request_received",
        format!(
            "request_id={} format={} audio_bytes={} base64_chars={}",
            request_id,
            req.format,
            audio_byte_count,
            req.audio_data.len()
        ),
        &user.user_id,
    )
    .await;

    let api_key = match std::env::var("OPENAI_KEY") {
        Ok(key) => key,
        Err(_) => {
            log_voice_event(
                &pool,
                "error",
                "transcription_config_missing",
                format!("request_id={} missing=OPENAI_KEY", request_id),
                &user.user_id,
            )
            .await;
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "OPENAI_KEY not set".to_string(),
            ));
        }
    };

    let file_name = format!("audio.{}", req.format);
    let mime_type = match req.format.as_str() {
        "webm" => "audio/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "ogg" => "audio/ogg",
        _ => {
            log_voice_event(
                &pool,
                "warn",
                "transcription_unsupported_format",
                format!("request_id={} format={}", request_id, req.format),
                &user.user_id,
            )
            .await;
            return Err((
                StatusCode::BAD_REQUEST,
                format!("Unsupported audio format: {}", req.format),
            ));
        }
    };

    let part = match reqwest::multipart::Part::bytes(audio_bytes)
        .file_name(file_name)
        .mime_str(mime_type)
    {
        Ok(part) => part,
        Err(e) => {
            log_voice_event(
                &pool,
                "error",
                "transcription_multipart_failed",
                format!("request_id={} error={}", request_id, e),
                &user.user_id,
            )
            .await;
            return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
        }
    };

    let mut form = reqwest::multipart::Form::new()
        .part("file", part)
        .text("model", "whisper-1");

    if let Some(lang) = &req.language {
        form = form.text("language", lang.clone());
    }

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(WHISPER_TIMEOUT_SECONDS))
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            log_voice_event(
                &pool,
                "error",
                "transcription_http_client_failed",
                format!("request_id={} error={}", request_id, e),
                &user.user_id,
            )
            .await;
            return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
        }
    };

    let upstream_started_at = Instant::now();
    let response = match client
        .post("https://api.openai.com/v1/audio/transcriptions")
        .header("Authorization", format!("Bearer {}", api_key))
        .multipart(form)
        .send()
        .await
    {
        Ok(response) => response,
        Err(e) => {
            let duration_ms = upstream_started_at.elapsed().as_millis();
            log_voice_event(
                &pool,
                "error",
                "transcription_upstream_request_failed",
                format!(
                    "request_id={} duration_ms={} timeout_seconds={} error={}",
                    request_id, duration_ms, WHISPER_TIMEOUT_SECONDS, e
                ),
                &user.user_id,
            )
            .await;
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Whisper API request failed: {}", e),
            ));
        }
    };
    let upstream_duration_ms = upstream_started_at.elapsed().as_millis();

    if !response.status().is_success() {
        let upstream_status = response.status();
        let error_text = response.text().await.unwrap_or_default();
        log_voice_event(
            &pool,
            "error",
            "transcription_upstream_error",
            format!(
                "request_id={} upstream_status={} duration_ms={} body={}",
                request_id,
                upstream_status.as_u16(),
                upstream_duration_ms,
                error_text.chars().take(300).collect::<String>()
            ),
            &user.user_id,
        )
        .await;
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("OpenAI Whisper error: {}", error_text),
        ));
    }

    #[derive(Deserialize)]
    struct WhisperResponse {
        text: String,
    }

    let whisper: WhisperResponse = match response.json().await {
        Ok(whisper) => whisper,
        Err(e) => {
            log_voice_event(
                &pool,
                "error",
                "transcription_upstream_parse_failed",
                format!(
                    "request_id={} duration_ms={} error={}",
                    request_id, upstream_duration_ms, e
                ),
                &user.user_id,
            )
            .await;
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to parse Whisper response: {}", e),
            ));
        }
    };

    log_voice_event(
        &pool,
        "info",
        "transcription_request_succeeded",
        format!(
            "request_id={} duration_ms={} upstream_duration_ms={} audio_bytes={} text_chars={}",
            request_id,
            started_at.elapsed().as_millis(),
            upstream_duration_ms,
            audio_byte_count,
            whisper.text.len()
        ),
        &user.user_id,
    )
    .await;

    Ok(Json(TranscriptionResponse {
        text: whisper.text,
        duration_seconds: None,
    }))
}

fn client_request_id(headers: &HeaderMap) -> String {
    headers
        .get(CLIENT_REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("missing")
        .to_string()
}

async fn log_voice_event(
    pool: &Arc<SqlitePool>,
    level: &str,
    message: &str,
    detail: String,
    user_id: &str,
) {
    system_log_helper::log_event(
        pool,
        level,
        "voice",
        message,
        Some(&detail),
        Some(user_id),
        None,
    )
    .await;
}
