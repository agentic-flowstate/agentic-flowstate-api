use axum::{
    extract::{Extension, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use ticketing_system::SqlitePool;

use crate::{agents::prompts::load_prompt, auth_middleware::AuthenticatedUser, system_log_helper};

const CLIENT_REQUEST_ID_HEADER: &str = "X-Client-Request-ID";
const OPENAI_AUDIO_TIMEOUT_SECONDS: u64 = 90;
const ALEX_VOICE_MODEL: &str = "gpt-4o-audio-preview";
const OUTPUT_FORMAT: &str = "mp3";
const DEFAULT_VOICE: &str = "alloy";

#[derive(Debug, Deserialize)]
pub struct AlexVoiceTurnRequest {
    pub audio_data: String,
    pub format: String,
    pub voice: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AlexVoiceTurnResponse {
    pub audio_data: String,
    pub format: String,
    pub transcript: Option<String>,
    pub audio_bytes: usize,
    pub model: String,
}

#[derive(Debug, Deserialize)]
struct OpenAiAudioResponse {
    choices: Vec<OpenAiAudioChoice>,
}

#[derive(Debug, Deserialize)]
struct OpenAiAudioChoice {
    message: OpenAiAudioMessage,
}

#[derive(Debug, Deserialize)]
struct OpenAiAudioMessage {
    audio: Option<OpenAiAudioPayload>,
}

#[derive(Debug, Deserialize)]
struct OpenAiAudioPayload {
    data: String,
    transcript: Option<String>,
}

pub async fn alex_voice_turn(
    State(pool): State<Arc<SqlitePool>>,
    Extension(user): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Json(req): Json<AlexVoiceTurnRequest>,
) -> Result<Json<AlexVoiceTurnResponse>, (StatusCode, String)> {
    let request_id = client_request_id(&headers);
    let started_at = Instant::now();

    let audio_bytes = base64::engine::general_purpose::STANDARD
        .decode(&req.audio_data)
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("Invalid base64 audio_data: {}", e),
            )
        })?;
    if audio_bytes.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "audio_data cannot be empty".to_string(),
        ));
    }

    let input_format = req.format.trim().to_ascii_lowercase();
    if !matches!(input_format.as_str(), "wav" | "mp3") {
        log_alex_voice_event(
            &pool,
            "warn",
            "alex_voice_unsupported_format",
            format!("request_id={} format={}", request_id, req.format),
            &user.user_id,
        )
        .await;
        return Err((
            StatusCode::BAD_REQUEST,
            "Alex voice input format must be wav or mp3".to_string(),
        ));
    }

    let api_key = std::env::var("OPENAI_KEY").map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "OPENAI_KEY not set".to_string(),
        )
    })?;
    let voice = req
        .voice
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_VOICE.to_string());
    let system_prompt = load_prompt("alex-voice-controller", HashMap::new()).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to load alex voice prompt: {}", e),
        )
    })?;

    log_alex_voice_event(
        &pool,
        "info",
        "alex_voice_turn_received",
        format!(
            "request_id={} input_format={} audio_bytes={} model={}",
            request_id,
            input_format,
            audio_bytes.len(),
            ALEX_VOICE_MODEL
        ),
        &user.user_id,
    )
    .await;

    let body = serde_json::json!({
        "model": ALEX_VOICE_MODEL,
        "modalities": ["text", "audio"],
        "audio": {
            "voice": voice,
            "format": OUTPUT_FORMAT
        },
        "messages": [
            {
                "role": "system",
                "content": system_prompt
            },
            {
                "role": "user",
                "content": [
                    {
                        "type": "text",
                        "text": "Respond as a concise spoken Alex voice controller turn."
                    },
                    {
                        "type": "input_audio",
                        "input_audio": {
                            "data": req.audio_data,
                            "format": input_format
                        }
                    }
                ]
            }
        ]
    });

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(OPENAI_AUDIO_TIMEOUT_SECONDS))
        .build()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let upstream_started_at = Instant::now();
    let response = client
        .post("https://api.openai.com/v1/chat/completions")
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("OpenAI audio request failed: {}", e),
            )
        })?;
    let upstream_duration_ms = upstream_started_at.elapsed().as_millis();

    if !response.status().is_success() {
        let upstream_status = response.status();
        let error_text = response.text().await.unwrap_or_default();
        log_alex_voice_event(
            &pool,
            "error",
            "alex_voice_upstream_error",
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
            StatusCode::BAD_GATEWAY,
            format!("OpenAI audio error: {}", error_text),
        ));
    }

    let parsed: OpenAiAudioResponse = response.json().await.map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            format!("Failed to parse OpenAI audio response: {}", e),
        )
    })?;
    let payload = parsed
        .choices
        .into_iter()
        .find_map(|choice| choice.message.audio)
        .ok_or_else(|| {
            (
                StatusCode::BAD_GATEWAY,
                "OpenAI response did not include audio".to_string(),
            )
        })?;
    let response_audio_bytes = base64::engine::general_purpose::STANDARD
        .decode(&payload.data)
        .map(|bytes| bytes.len())
        .unwrap_or(0);

    log_alex_voice_event(
        &pool,
        "info",
        "alex_voice_turn_succeeded",
        format!(
            "request_id={} duration_ms={} upstream_duration_ms={} response_audio_bytes={} transcript_chars={}",
            request_id,
            started_at.elapsed().as_millis(),
            upstream_duration_ms,
            response_audio_bytes,
            payload.transcript.as_deref().map(str::len).unwrap_or(0)
        ),
        &user.user_id,
    )
    .await;

    Ok(Json(AlexVoiceTurnResponse {
        audio_data: payload.data,
        format: OUTPUT_FORMAT.to_string(),
        transcript: payload.transcript,
        audio_bytes: response_audio_bytes,
        model: ALEX_VOICE_MODEL.to_string(),
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

async fn log_alex_voice_event(
    pool: &Arc<SqlitePool>,
    level: &str,
    message: &str,
    detail: String,
    user_id: &str,
) {
    system_log_helper::log_event(
        pool,
        level,
        "alex_voice",
        message,
        Some(&detail),
        Some(user_id),
        None,
    )
    .await;
}
