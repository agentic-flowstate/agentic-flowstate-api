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
const VOICE_JOB_ID_HEADER: &str = "X-Voice-Job-ID";
const VOICE_SESSION_ID_HEADER: &str = "X-Voice-Session-ID";
const OPENAI_TRANSCRIPTIONS_URL: &str = "https://api.openai.com/v1/audio/transcriptions";
const OPENAI_TRANSCRIPTIONS_PATH_ERROR: &str = "Invalid URL (POST /v1/audio/transcriptions)";
const WHISPER_TIMEOUT_SECONDS: u64 = 65;
const TRANSCRIPTION_MAX_ATTEMPTS: u8 = 2;
const TRANSCRIPTION_RETRY_DELAY_MS: u64 = 250;

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
    let trace_ids = voice_trace_ids(&headers);
    let trace_detail = trace_ids.detail_prefix();
    let started_at = Instant::now();

    let audio_bytes = match base64::engine::general_purpose::STANDARD.decode(&req.audio_data) {
        Ok(bytes) => bytes,
        Err(e) => {
            log_voice_event(
                &pool,
                "warn",
                "transcription_invalid_audio",
                format!("{} error={}", trace_detail, e),
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
            "{} format={} audio_bytes={} base64_chars={}",
            trace_detail,
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
                format!("{} missing=OPENAI_KEY", trace_detail),
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
    let mime_type = match mime_type_for_format(&req.format) {
        Some(mime_type) => mime_type,
        _ => {
            log_voice_event(
                &pool,
                "warn",
                "transcription_unsupported_format",
                format!("{} format={}", trace_detail, req.format),
                &user.user_id,
            )
            .await;
            return Err((
                StatusCode::BAD_REQUEST,
                format!("Unsupported audio format: {}", req.format),
            ));
        }
    };

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
                format!("{} error={}", trace_detail, e),
                &user.user_id,
            )
            .await;
            return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
        }
    };

    let mut attempt = 1;
    let (response, upstream_duration_ms, final_attempt) = loop {
        let form = match build_whisper_form(
            &audio_bytes,
            &file_name,
            mime_type,
            req.language.as_deref(),
        ) {
            Ok(form) => form,
            Err(e) => {
                log_voice_event(
                    &pool,
                    "error",
                    "transcription_multipart_failed",
                    format!("{} attempt={} error={}", trace_detail, attempt, e),
                    &user.user_id,
                )
                .await;
                return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
            }
        };

        let upstream_started_at = Instant::now();
        let response = match client
            .post(OPENAI_TRANSCRIPTIONS_URL)
            .header("Authorization", format!("Bearer {}", api_key))
            .multipart(form)
            .send()
            .await
        {
            Ok(response) => response,
            Err(e) => {
                let duration_ms = upstream_started_at.elapsed().as_millis();
                if attempt < TRANSCRIPTION_MAX_ATTEMPTS {
                    tracing::warn!(
                        target: "agentic_api::voice_transcribe",
                        request_id = %trace_ids.client_request_id,
                        voice_job_id = ?trace_ids.voice_job_id,
                        voice_session_id = ?trace_ids.voice_session_id,
                        attempt,
                        next_attempt = attempt + 1,
                        duration_ms,
                        timeout_seconds = WHISPER_TIMEOUT_SECONDS,
                        retry_delay_ms = TRANSCRIPTION_RETRY_DELAY_MS,
                        error = %e,
                        "retrying voice transcription after upstream request failure"
                    );
                    log_voice_event(
                        &pool,
                        "warn",
                        "transcription_upstream_retry",
                        format!(
                            "{} attempt={} next_attempt={} duration_ms={} timeout_seconds={} delay_ms={} reason=request_failed error={}",
                            trace_detail,
                            attempt,
                            attempt + 1,
                            duration_ms,
                            WHISPER_TIMEOUT_SECONDS,
                            TRANSCRIPTION_RETRY_DELAY_MS,
                            e
                        ),
                        &user.user_id,
                    )
                    .await;
                    tokio::time::sleep(Duration::from_millis(TRANSCRIPTION_RETRY_DELAY_MS)).await;
                    attempt += 1;
                    continue;
                }

                tracing::error!(
                    target: "agentic_api::voice_transcribe",
                    request_id = %trace_ids.client_request_id,
                    voice_job_id = ?trace_ids.voice_job_id,
                    voice_session_id = ?trace_ids.voice_session_id,
                    attempt,
                    duration_ms,
                    timeout_seconds = WHISPER_TIMEOUT_SECONDS,
                    error = %e,
                    "voice transcription upstream request failed"
                );
                log_voice_event(
                    &pool,
                    "error",
                    "transcription_upstream_request_failed",
                    format!(
                        "{} attempt={} duration_ms={} timeout_seconds={} error={}",
                        trace_detail, attempt, duration_ms, WHISPER_TIMEOUT_SECONDS, e
                    ),
                    &user.user_id,
                )
                .await;
                return Err((
                    StatusCode::BAD_GATEWAY,
                    format!("Whisper API request failed: {}", e),
                ));
            }
        };
        let upstream_duration_ms = upstream_started_at.elapsed().as_millis();

        if !response.status().is_success() {
            let upstream_status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            let truncated_error_text = truncate_detail(&error_text, 300);
            if attempt < TRANSCRIPTION_MAX_ATTEMPTS
                && should_retry_transcription_response(upstream_status, &error_text)
            {
                tracing::warn!(
                    target: "agentic_api::voice_transcribe",
                    request_id = %trace_ids.client_request_id,
                    voice_job_id = ?trace_ids.voice_job_id,
                    voice_session_id = ?trace_ids.voice_session_id,
                    attempt,
                    next_attempt = attempt + 1,
                    upstream_status = upstream_status.as_u16(),
                    upstream_duration_ms,
                    retry_delay_ms = TRANSCRIPTION_RETRY_DELAY_MS,
                    "retrying voice transcription after upstream response"
                );
                log_voice_event(
                    &pool,
                    "warn",
                    "transcription_upstream_retry",
                    format!(
                        "{} attempt={} next_attempt={} upstream_status={} duration_ms={} delay_ms={} reason=upstream_response body={}",
                        trace_detail,
                        attempt,
                        attempt + 1,
                        upstream_status.as_u16(),
                        upstream_duration_ms,
                        TRANSCRIPTION_RETRY_DELAY_MS,
                        truncated_error_text
                    ),
                    &user.user_id,
                )
                .await;
                tokio::time::sleep(Duration::from_millis(TRANSCRIPTION_RETRY_DELAY_MS)).await;
                attempt += 1;
                continue;
            }

            tracing::error!(
                target: "agentic_api::voice_transcribe",
                request_id = %trace_ids.client_request_id,
                voice_job_id = ?trace_ids.voice_job_id,
                voice_session_id = ?trace_ids.voice_session_id,
                attempt,
                upstream_status = upstream_status.as_u16(),
                upstream_duration_ms,
                "voice transcription upstream error"
            );
            log_voice_event(
                &pool,
                "error",
                "transcription_upstream_error",
                format!(
                    "{} attempt={} upstream_status={} duration_ms={} body={}",
                    trace_detail,
                    attempt,
                    upstream_status.as_u16(),
                    upstream_duration_ms,
                    truncated_error_text
                ),
                &user.user_id,
            )
            .await;
            return Err((
                StatusCode::BAD_GATEWAY,
                format!("OpenAI Whisper error: {}", error_text),
            ));
        }

        break (response, upstream_duration_ms, attempt);
    };

    #[derive(Deserialize)]
    struct WhisperResponse {
        text: String,
    }

    let whisper: WhisperResponse = match response.json().await {
        Ok(whisper) => whisper,
        Err(e) => {
            tracing::error!(
                target: "agentic_api::voice_transcribe",
                request_id = %trace_ids.client_request_id,
                voice_job_id = ?trace_ids.voice_job_id,
                voice_session_id = ?trace_ids.voice_session_id,
                attempt = final_attempt,
                upstream_duration_ms,
                error = %e,
                "failed to parse voice transcription response"
            );
            log_voice_event(
                &pool,
                "error",
                "transcription_upstream_parse_failed",
                format!(
                    "{} attempt={} duration_ms={} error={}",
                    trace_detail, final_attempt, upstream_duration_ms, e
                ),
                &user.user_id,
            )
            .await;
            return Err((
                StatusCode::BAD_GATEWAY,
                format!("Failed to parse Whisper response: {}", e),
            ));
        }
    };

    log_voice_event(
        &pool,
        "info",
        "transcription_request_succeeded",
        format!(
            "{} attempt={} duration_ms={} upstream_duration_ms={} audio_bytes={} text_chars={}",
            trace_detail,
            final_attempt,
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct VoiceTraceIds {
    client_request_id: String,
    voice_job_id: Option<String>,
    voice_session_id: Option<String>,
}

impl VoiceTraceIds {
    fn detail_prefix(&self) -> String {
        let mut parts = vec![format!("request_id={}", self.client_request_id)];
        if let Some(voice_job_id) = &self.voice_job_id {
            parts.push(format!("voice_job_id={}", voice_job_id));
        }
        if let Some(voice_session_id) = &self.voice_session_id {
            parts.push(format!("voice_session_id={}", voice_session_id));
        }
        parts.join(" ")
    }
}

fn voice_trace_ids(headers: &HeaderMap) -> VoiceTraceIds {
    VoiceTraceIds {
        client_request_id: header_value(headers, CLIENT_REQUEST_ID_HEADER)
            .unwrap_or_else(|| "missing".to_string()),
        voice_job_id: header_value(headers, VOICE_JOB_ID_HEADER),
        voice_session_id: header_value(headers, VOICE_SESSION_ID_HEADER),
    }
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn mime_type_for_format(format: &str) -> Option<&'static str> {
    match format {
        "webm" => Some("audio/webm"),
        "mp3" => Some("audio/mpeg"),
        "wav" => Some("audio/wav"),
        "m4a" => Some("audio/mp4"),
        "ogg" => Some("audio/ogg"),
        _ => None,
    }
}

fn build_whisper_form(
    audio_bytes: &[u8],
    file_name: &str,
    mime_type: &str,
    language: Option<&str>,
) -> Result<reqwest::multipart::Form, reqwest::Error> {
    let part = reqwest::multipart::Part::bytes(audio_bytes.to_vec())
        .file_name(file_name.to_string())
        .mime_str(mime_type)?;

    let mut form = reqwest::multipart::Form::new()
        .part("file", part)
        .text("model", "whisper-1");

    if let Some(lang) = language {
        form = form.text("language", lang.to_string());
    }

    Ok(form)
}

fn should_retry_transcription_response(status: StatusCode, body: &str) -> bool {
    if matches!(status.as_u16(), 408 | 409 | 425 | 429) || status.is_server_error() {
        return true;
    }

    status == StatusCode::NOT_FOUND && body.contains(OPENAI_TRANSCRIPTIONS_PATH_ERROR)
}

fn truncate_detail(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_observed_openai_audio_route_404() {
        let body = r#"{"error":{"message":"Invalid URL (POST /v1/audio/transcriptions)"}}"#;

        assert!(should_retry_transcription_response(
            StatusCode::NOT_FOUND,
            body
        ));
    }

    #[test]
    fn does_not_retry_unrelated_404() {
        assert!(!should_retry_transcription_response(
            StatusCode::NOT_FOUND,
            r#"{"error":{"message":"missing resource"}}"#
        ));
    }

    #[test]
    fn retries_transient_status_codes() {
        assert!(should_retry_transcription_response(
            StatusCode::REQUEST_TIMEOUT,
            ""
        ));
        assert!(should_retry_transcription_response(
            StatusCode::TOO_MANY_REQUESTS,
            ""
        ));
        assert!(should_retry_transcription_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            ""
        ));
    }

    #[test]
    fn does_not_retry_bad_request() {
        assert!(!should_retry_transcription_response(
            StatusCode::BAD_REQUEST,
            ""
        ));
    }

    #[test]
    fn voice_trace_ids_include_client_job_and_session_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(CLIENT_REQUEST_ID_HEADER, "request-1".parse().unwrap());
        headers.insert(VOICE_JOB_ID_HEADER, "job-1".parse().unwrap());
        headers.insert(VOICE_SESSION_ID_HEADER, "session-1".parse().unwrap());

        let trace_ids = voice_trace_ids(&headers);

        assert_eq!(
            trace_ids,
            VoiceTraceIds {
                client_request_id: "request-1".to_string(),
                voice_job_id: Some("job-1".to_string()),
                voice_session_id: Some("session-1".to_string()),
            }
        );
        assert_eq!(
            trace_ids.detail_prefix(),
            "request_id=request-1 voice_job_id=job-1 voice_session_id=session-1"
        );
    }

    #[test]
    fn missing_client_request_id_is_explicit_in_trace_detail() {
        let trace_ids = voice_trace_ids(&HeaderMap::new());

        assert_eq!(trace_ids.client_request_id, "missing");
        assert_eq!(trace_ids.detail_prefix(), "request_id=missing");
    }
}
