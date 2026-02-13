use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use sqlx::SqlitePool;

use cc_sdk::{query, ClaudeCodeOptions, Message as CcMessage, ContentBlock, ToolsConfig};
use futures::StreamExt;
use ticketing_system::{
    CreateTranscriptEntryRequest, CreateTranscriptSessionRequest, TranscribeAudioRequest,
    TranscriptionResponse,
};

use crate::agents::prompts::load_prompt;
use crate::agents::AgentType;

// ============================================================================
// Transcription Handler (OpenAI Whisper)
// ============================================================================

/// POST /api/meetings/:room_id/transcribe
pub async fn transcribe_meeting(
    Path(room_id): Path<String>,
    State(db): State<Arc<SqlitePool>>,
    Json(req): Json<TranscribeAudioRequest>,
) -> Result<Json<TranscriptionResponse>, (StatusCode, String)> {
    use base64::Engine;
    let audio_bytes = base64::engine::general_purpose::STANDARD
        .decode(&req.audio_data)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("Invalid base64: {}", e)))?;

    let api_key = std::env::var("OPENAI_KEY")
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "OPENAI_KEY not set".to_string()))?;

    let file_name = format!("audio.{}", req.format);
    let mime_type = match req.format.as_str() {
        "webm" => "audio/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "ogg" => "audio/ogg",
        _ => "audio/webm",
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
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("API request failed: {}", e)))?;

    if !response.status().is_success() {
        let error_text = response.text().await.unwrap_or_default();
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("OpenAI API error: {}", error_text),
        ));
    }

    #[derive(Deserialize)]
    struct WhisperResponse {
        text: String,
    }

    let whisper_response: WhisperResponse = response
        .json()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to parse response: {}", e)))?;

    let session_id = format!("mtg-{}", room_id);

    if ticketing_system::transcripts::get_session(&db, &session_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .is_none()
    {
        let create_req = CreateTranscriptSessionRequest {
            session_id: session_id.clone(),
            guild_id: room_id.clone(),
            channel_name: Some("Meeting".to_string()),
        };
        ticketing_system::transcripts::create_session(&db, create_req)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }

    let entry_req = CreateTranscriptEntryRequest {
        session_id: session_id.clone(),
        user_id: "meeting".to_string(),
        username: "Meeting Transcript".to_string(),
        text: whisper_response.text.clone(),
        timestamp: chrono::Utc::now().to_rfc3339(),
    };
    ticketing_system::transcripts::add_entry(&db, entry_req)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    ticketing_system::transcripts::end_session(&db, &session_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    ticketing_system::meetings::end_meeting(&db, &room_id, Some(&session_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(TranscriptionResponse {
        text: whisper_response.text,
        duration_seconds: None,
    }))
}

// ============================================================================
// Per-User Audio Upload & Merge
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct UploadAudioRequest {
    pub audio_data: String,
    pub format: String,
    pub username: String,
    pub start_time: i64,
}

/// POST /api/meetings/:room_id/audio
pub async fn upload_meeting_audio(
    Path(room_id): Path<String>,
    Json(req): Json<UploadAudioRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
    use base64::Engine;

    let audio_bytes = base64::engine::general_purpose::STANDARD
        .decode(&req.audio_data)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("Invalid base64: {}", e)))?;

    let audio_dir = dirs::home_dir()
        .unwrap_or_default()
        .join(".agentic-flowstate")
        .join("meeting-audio")
        .join(&room_id);

    std::fs::create_dir_all(&audio_dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to create audio dir: {}", e)))?;

    let filename = format!("{}_{}.{}", req.username.replace(' ', "_"), req.start_time, req.format);
    let filepath = audio_dir.join(&filename);

    std::fs::write(&filepath, &audio_bytes)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to write audio: {}", e)))?;

    let metadata = serde_json::json!({
        "username": req.username,
        "start_time": req.start_time,
        "format": req.format,
        "filename": filename,
    });

    let meta_path = audio_dir.join(format!("{}_{}.json", req.username.replace(' ', "_"), req.start_time));
    std::fs::write(&meta_path, serde_json::to_string_pretty(&metadata).unwrap_or_default())
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to write metadata: {}", e)))?;

    tracing::info!("Uploaded audio for {} in meeting {}", req.username, room_id);

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
struct WhisperVerboseResponse {
    #[allow(dead_code)]
    text: String,
    segments: Vec<WhisperSegment>,
}

#[derive(Debug, Deserialize)]
struct WhisperSegment {
    start: f64,
    #[allow(dead_code)]
    end: f64,
    text: String,
}

/// POST /api/meetings/:room_id/finalize-transcript
pub async fn finalize_meeting_transcript(
    Path(room_id): Path<String>,
    State(db): State<Arc<SqlitePool>>,
) -> Result<Json<TranscriptionResponse>, (StatusCode, String)> {
    ticketing_system::meetings::update_processing_status(&db, &room_id, "transcribing")
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let api_key = std::env::var("OPENAI_KEY")
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "OPENAI_KEY not set".to_string()))?;

    let audio_dir = dirs::home_dir()
        .unwrap_or_default()
        .join(".agentic-flowstate")
        .join("meeting-audio")
        .join(&room_id);

    if !audio_dir.exists() {
        return Err((StatusCode::NOT_FOUND, "No audio segments found".to_string()));
    }

    // Collect all audio segments with metadata
    let mut segments: Vec<(String, i64, std::path::PathBuf)> = Vec::new();

    for entry in std::fs::read_dir(&audio_dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    {
        let entry = entry.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let path = entry.path();

        if path.extension().map(|e| e == "json").unwrap_or(false) {
            let meta: serde_json::Value = serde_json::from_str(
                &std::fs::read_to_string(&path)
                    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            ).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

            let username = meta["username"].as_str().unwrap_or("Unknown").to_string();
            let start_time = meta["start_time"].as_i64().unwrap_or(0);
            let filename = meta["filename"].as_str().unwrap_or("");
            let audio_path = audio_dir.join(filename);

            if audio_path.exists() {
                segments.push((username, start_time, audio_path));
            }
        }
    }

    if segments.is_empty() {
        return Err((StatusCode::NOT_FOUND, "No audio segments found".to_string()));
    }

    tracing::info!("Processing {} audio segments for meeting {}", segments.len(), room_id);

    // Transcribe each segment with timestamps
    let client = reqwest::Client::new();
    let mut all_entries: Vec<(i64, String, String)> = Vec::new();

    for (username, start_time_ms, audio_path) in segments {
        let audio_bytes = std::fs::read(&audio_path)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to read audio: {}", e)))?;

        let ext = audio_path.extension().and_then(|e| e.to_str()).unwrap_or("webm");
        let mime_type = match ext {
            "webm" => "audio/webm",
            "mp3" => "audio/mpeg",
            "wav" => "audio/wav",
            _ => "audio/webm",
        };

        let part = reqwest::multipart::Part::bytes(audio_bytes)
            .file_name(format!("audio.{}", ext))
            .mime_str(mime_type)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

        let form = reqwest::multipart::Form::new()
            .part("file", part)
            .text("model", "whisper-1")
            .text("response_format", "verbose_json")
            .text("timestamp_granularities[]", "segment")
            .text("language", "en");

        let response = client
            .post("https://api.openai.com/v1/audio/transcriptions")
            .header("Authorization", format!("Bearer {}", api_key))
            .multipart(form)
            .send()
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("API request failed: {}", e)))?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            tracing::error!("Whisper API error for {}: {}", username, error_text);
            continue;
        }

        let whisper_response: WhisperVerboseResponse = response
            .json()
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to parse response: {}", e)))?;

        for seg in whisper_response.segments {
            let absolute_start = start_time_ms + (seg.start * 1000.0) as i64;
            all_entries.push((absolute_start, username.clone(), seg.text.trim().to_string()));
        }
    }

    all_entries.sort_by_key(|(ts, _, _)| *ts);

    // Format the merged transcript
    let mut transcript_lines: Vec<String> = Vec::new();
    let mut current_speaker = String::new();

    for (_, username, text) in all_entries {
        if !text.is_empty() {
            if username != current_speaker {
                transcript_lines.push(format!("\n[{}]: {}", username, text));
                current_speaker = username;
            } else {
                if let Some(last) = transcript_lines.last_mut() {
                    last.push(' ');
                    last.push_str(&text);
                }
            }
        }
    }

    let final_transcript = transcript_lines.join("").trim().to_string();

    // Store the transcript
    let session_id = format!("mtg-{}", room_id);

    if ticketing_system::transcripts::get_session(&db, &session_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .is_none()
    {
        let create_req = CreateTranscriptSessionRequest {
            session_id: session_id.clone(),
            guild_id: room_id.clone(),
            channel_name: Some("Meeting".to_string()),
        };
        ticketing_system::transcripts::create_session(&db, create_req)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }

    let entry_req = CreateTranscriptEntryRequest {
        session_id: session_id.clone(),
        user_id: "meeting".to_string(),
        username: "Meeting Transcript".to_string(),
        text: final_transcript.clone(),
        timestamp: chrono::Utc::now().to_rfc3339(),
    };
    ticketing_system::transcripts::add_entry(&db, entry_req)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    ticketing_system::transcripts::end_session(&db, &session_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    ticketing_system::meetings::end_meeting(&db, &room_id, Some(&session_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Cleanup audio files
    let _ = std::fs::remove_dir_all(&audio_dir);

    tracing::info!("Finalized transcript for meeting {}", room_id);

    // Extract meeting notes using Claude
    ticketing_system::meetings::update_processing_status(&db, &room_id, "extracting_notes")
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    match extract_meeting_notes(&final_transcript).await {
        Ok(notes) => {
            let title = generate_meeting_title(&notes);
            if let Some(t) = &title {
                ticketing_system::meetings::update_meeting_title(&db, &room_id, t)
                    .await
                    .ok();
            }

            ticketing_system::meetings::update_meeting_notes(&db, &room_id, &notes, "completed")
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            tracing::info!("Extracted meeting notes for {}", room_id);
        }
        Err(e) => {
            tracing::error!("Failed to extract meeting notes: {}", e);
            ticketing_system::meetings::update_processing_status(&db, &room_id, "failed")
                .await
                .ok();
        }
    };

    Ok(Json(TranscriptionResponse {
        text: final_transcript,
        duration_seconds: None,
    }))
}

/// Extract structured meeting notes from a transcript using Claude
async fn extract_meeting_notes(transcript: &str) -> Result<String, String> {
    tracing::info!("Starting meeting notes extraction, transcript length: {} chars", transcript.len());

    let mut vars = HashMap::new();
    vars.insert("transcript".to_string(), transcript.to_string());

    let system_prompt = load_prompt("meeting-notes", vars)
        .map_err(|e| format!("Failed to load meeting-notes prompt: {}", e))?;

    let agent_config = AgentType::MeetingNotes;
    let working_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    let options = ClaudeCodeOptions::builder()
        .system_prompt(&system_prompt)
        .model(agent_config.model())
        .tools(ToolsConfig::none())
        .max_turns(1)
        .cwd(&working_dir)
        .build();

    let prompt = "Extract structured notes from the transcript provided in the system prompt.";
    let mut output_parts = Vec::new();

    match query(prompt, Some(options)).await {
        Ok(stream) => {
            let mut stream = Box::pin(stream);

            while let Some(message_result) = stream.next().await {
                match message_result {
                    Ok(message) => {
                        if let CcMessage::Assistant { message: assistant_msg } = &message {
                            for block in &assistant_msg.content {
                                if let ContentBlock::Text(text_content) = block {
                                    output_parts.push(text_content.text.clone());
                                }
                            }
                        }
                        if let CcMessage::Result { .. } = &message {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::error!("Error receiving message from meeting-notes agent: {}", e);
                        break;
                    }
                }
            }
        }
        Err(e) => {
            return Err(format!("Failed to run meeting-notes agent: {}", e));
        }
    }

    if output_parts.is_empty() {
        return Err("No output from meeting-notes agent".to_string());
    }

    Ok(output_parts.join("\n\n"))
}

/// Generate a meeting title from the extracted notes
fn generate_meeting_title(notes: &str) -> Option<String> {
    if let Some(start) = notes.find("**Issue 1:") {
        let after_prefix = &notes[start + 10..];
        if let Some(end) = after_prefix.find("**") {
            let title = after_prefix[..end].trim();
            if !title.is_empty() {
                return Some(title.to_string());
            }
        }
    }

    let first_line = notes.lines().next()?;
    let clean = first_line.trim().trim_start_matches(['*', '#', '-', ' ']);
    if !clean.is_empty() && clean.len() < 100 {
        return Some(clean.to_string());
    }

    None
}
