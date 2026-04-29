use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use std::collections::HashMap;

use crate::agents::prompts::load_prompt;
use crate::agents::run_oneshot;

#[derive(Debug, Deserialize)]
pub struct TtsRequest {
    pub text: String,
    pub voice: Option<String>,
    pub response_format: Option<String>,
}

const MAX_INPUT_LENGTH: usize = 16000;
const VALID_VOICES: &[&str] = &["alloy", "echo", "fable", "onyx", "nova", "shimmer"];
const VALID_FORMATS: &[&str] = &["mp3", "opus", "aac", "flac", "wav", "pcm"];

/// Clean raw text into a speakable transcript using a one-shot LLM call.
async fn prepare_transcript(raw_text: &str) -> Result<String, (StatusCode, String)> {
    let vars = HashMap::new();

    let system_prompt = load_prompt("tts-transcript", vars).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to load tts-transcript prompt: {}", e),
        )
    })?;

    let working_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    // Pass the raw text as the user message so the LLM treats it as data
    let result = run_oneshot(
        None, // no tools needed, just text in → text out
        &system_prompt,
        &working_dir,
        raw_text,
    )
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Transcript preparation failed: {}", e),
        )
    })?;

    Ok(result.text)
}

/// POST /api/tts
pub async fn text_to_speech(Json(req): Json<TtsRequest>) -> Result<Response, (StatusCode, String)> {
    if req.text.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "Text cannot be empty".to_string()));
    }
    if req.text.len() > MAX_INPUT_LENGTH {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "Text exceeds maximum length of {} characters",
                MAX_INPUT_LENGTH
            ),
        ));
    }

    let api_key = std::env::var("OPENAI_KEY").map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "OPENAI_KEY not set".to_string(),
        )
    })?;

    let voice = req.voice.unwrap_or_else(|| "alloy".to_string());
    if !VALID_VOICES.contains(&voice.as_str()) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("Invalid voice. Must be one of: {}", VALID_VOICES.join(", ")),
        ));
    }

    let response_format = req.response_format.unwrap_or_else(|| "mp3".to_string());
    if !VALID_FORMATS.contains(&response_format.as_str()) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "Invalid format. Must be one of: {}",
                VALID_FORMATS.join(", ")
            ),
        ));
    }

    // Step 1: Clean raw text into a speakable transcript
    tracing::info!(
        "Preparing transcript from {} chars of raw text",
        req.text.len()
    );
    let clean_text = prepare_transcript(&req.text).await?;
    tracing::info!(
        "Transcript prepared: {} chars\n---BEGIN TRANSCRIPT---\n{}\n---END TRANSCRIPT---",
        clean_text.len(),
        clean_text
    );

    // Step 2: Send cleaned transcript to OpenAI TTS
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "model": "tts-1-hd",
        "input": clean_text,
        "voice": voice,
        "response_format": response_format,
    });

    let response = client
        .post("https://api.openai.com/v1/audio/speech")
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("OpenAI API request failed: {}", e),
            )
        })?;

    if !response.status().is_success() {
        let error_text = response.text().await.unwrap_or_default();
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("OpenAI TTS API error: {}", error_text),
        ));
    }

    let content_type = match response_format.as_str() {
        "mp3" => "audio/mpeg",
        "opus" => "audio/opus",
        "aac" => "audio/aac",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "pcm" => "audio/pcm",
        _ => "audio/mpeg",
    };

    let audio_bytes = response.bytes().await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to read audio response: {}", e),
        )
    })?;

    Ok((
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, content_type.to_string()),
            (
                axum::http::header::CONTENT_DISPOSITION,
                format!("inline; filename=\"speech.{}\"", response_format),
            ),
        ],
        audio_bytes.to_vec(),
    )
        .into_response())
}
