use axum::{http::StatusCode, Json};
use serde::Deserialize;

use ticketing_system::TranscriptionResponse;

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
    Json(req): Json<VoiceTranscribeRequest>,
) -> Result<Json<TranscriptionResponse>, (StatusCode, String)> {
    use base64::Engine;
    let audio_bytes = base64::engine::general_purpose::STANDARD
        .decode(&req.audio_data)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("Invalid base64: {}", e)))?;

    let api_key = std::env::var("OPENAI_KEY").map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "OPENAI_KEY not set".to_string(),
        )
    })?;

    let file_name = format!("audio.{}", req.format);
    let mime_type = match req.format.as_str() {
        "webm" => "audio/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "ogg" => "audio/ogg",
        _ => "audio/mp4",
    };

    let part = reqwest::multipart::Part::bytes(audio_bytes)
        .file_name(file_name)
        .mime_str(mime_type)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut form = reqwest::multipart::Form::new()
        .part("file", part)
        .text("model", "whisper-1");

    if let Some(lang) = &req.language {
        form = form.text("language", lang.clone());
    }

    let client = reqwest::Client::new();
    let response = client
        .post("https://api.openai.com/v1/audio/transcriptions")
        .header("Authorization", format!("Bearer {}", api_key))
        .multipart(form)
        .send()
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Whisper API request failed: {}", e),
            )
        })?;

    if !response.status().is_success() {
        let error_text = response.text().await.unwrap_or_default();
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("OpenAI Whisper error: {}", error_text),
        ));
    }

    #[derive(Deserialize)]
    struct WhisperResponse {
        text: String,
    }

    let whisper: WhisperResponse = response.json().await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to parse Whisper response: {}", e),
        )
    })?;

    Ok(Json(TranscriptionResponse {
        text: whisper.text,
        duration_seconds: None,
    }))
}
