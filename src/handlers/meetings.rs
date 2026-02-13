use axum::{
    extract::{
        ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

use ticketing_system::{CreateMeetingRequest, Meeting};

// ============================================================================
// State for WebSocket signaling
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SignalingMessage {
    #[serde(rename = "join")]
    Join { room_id: String, user_id: String },
    #[serde(rename = "leave")]
    Leave { room_id: String, user_id: String },
    #[serde(rename = "offer")]
    Offer {
        room_id: String,
        from_user: String,
        to_user: String,
        sdp: String,
    },
    #[serde(rename = "answer")]
    Answer {
        room_id: String,
        from_user: String,
        to_user: String,
        sdp: String,
    },
    #[serde(rename = "ice_candidate")]
    IceCandidate {
        room_id: String,
        from_user: String,
        to_user: String,
        candidate: String,
    },
    #[serde(rename = "user_joined")]
    UserJoined { room_id: String, user_id: String },
    #[serde(rename = "user_left")]
    UserLeft { room_id: String, user_id: String },
    #[serde(rename = "room_users")]
    RoomUsers {
        room_id: String,
        users: Vec<String>,
    },
    #[serde(rename = "error")]
    Error { message: String },
}

#[derive(Debug, Default)]
pub struct Room {
    pub participants: Vec<String>,
}

pub struct SignalingState {
    pub rooms: RwLock<HashMap<String, Room>>,
    pub channels: RwLock<HashMap<String, broadcast::Sender<SignalingMessage>>>,
}

impl SignalingState {
    pub fn new() -> Self {
        Self {
            rooms: RwLock::new(HashMap::new()),
            channels: RwLock::new(HashMap::new()),
        }
    }

    pub async fn get_or_create_channel(
        &self,
        room_id: &str,
    ) -> broadcast::Sender<SignalingMessage> {
        let mut channels = self.channels.write().await;
        if let Some(tx) = channels.get(room_id) {
            tx.clone()
        } else {
            let (tx, _) = broadcast::channel(100);
            channels.insert(room_id.to_string(), tx.clone());
            tx
        }
    }

    pub async fn join_room(&self, room_id: &str, user_id: &str) -> Vec<String> {
        let mut rooms = self.rooms.write().await;
        let room = rooms.entry(room_id.to_string()).or_default();
        if !room.participants.contains(&user_id.to_string()) {
            room.participants.push(user_id.to_string());
        }
        room.participants.clone()
    }

    pub async fn leave_room(&self, room_id: &str, user_id: &str) {
        let mut rooms = self.rooms.write().await;
        if let Some(room) = rooms.get_mut(room_id) {
            room.participants.retain(|u| u != user_id);
            if room.participants.is_empty() {
                rooms.remove(room_id);
                let mut channels = self.channels.write().await;
                channels.remove(room_id);
            }
        }
    }

    #[allow(dead_code)]
    pub async fn get_participants(&self, room_id: &str) -> Vec<String> {
        let rooms = self.rooms.read().await;
        rooms
            .get(room_id)
            .map(|r| r.participants.clone())
            .unwrap_or_default()
    }
}

lazy_static::lazy_static! {
    pub static ref SIGNALING: SignalingState = SignalingState::new();
}

// ============================================================================
// HTTP Handlers
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct ListMeetingsQuery {
    pub active_only: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct MeetingsResponse {
    pub meetings: Vec<Meeting>,
}

/// GET /api/meetings
pub async fn list_meetings(
    State(db): State<Arc<SqlitePool>>,
    Query(query): Query<ListMeetingsQuery>,
) -> Result<Json<MeetingsResponse>, (StatusCode, String)> {
    let active_only = query.active_only.unwrap_or(false);
    let meetings = ticketing_system::meetings::list_meetings(&db, active_only)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(MeetingsResponse { meetings }))
}

/// POST /api/meetings
pub async fn create_meeting(
    State(db): State<Arc<SqlitePool>>,
    Json(req): Json<CreateMeetingRequest>,
) -> Result<Json<Meeting>, (StatusCode, String)> {
    let meeting = ticketing_system::meetings::create_meeting(&db, req)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(meeting))
}

/// GET /api/meetings/:room_id
pub async fn get_meeting(
    Path(room_id): Path<String>,
    State(db): State<Arc<SqlitePool>>,
) -> Result<Json<Meeting>, (StatusCode, String)> {
    let meeting = ticketing_system::meetings::get_meeting(&db, &room_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Meeting not found".to_string()))?;

    Ok(Json(meeting))
}

/// POST /api/meetings/:room_id/start
pub async fn start_meeting(
    Path(room_id): Path<String>,
    State(db): State<Arc<SqlitePool>>,
) -> Result<StatusCode, (StatusCode, String)> {
    ticketing_system::meetings::start_meeting(&db, &room_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/meetings/:room_id/end
pub async fn end_meeting(
    Path(room_id): Path<String>,
    State(db): State<Arc<SqlitePool>>,
) -> Result<StatusCode, (StatusCode, String)> {
    ticketing_system::meetings::end_meeting(&db, &room_id, None)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

/// DELETE /api/meetings/:room_id
pub async fn delete_meeting(
    Path(room_id): Path<String>,
    State(db): State<Arc<SqlitePool>>,
) -> Result<StatusCode, (StatusCode, String)> {
    ticketing_system::meetings::delete_meeting(&db, &room_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct UpdateMeetingRequest {
    pub title: Option<String>,
}

/// PATCH /api/meetings/:room_id
pub async fn update_meeting(
    Path(room_id): Path<String>,
    State(db): State<Arc<SqlitePool>>,
    Json(req): Json<UpdateMeetingRequest>,
) -> Result<Json<Meeting>, (StatusCode, String)> {
    if let Some(title) = &req.title {
        ticketing_system::meetings::update_meeting_title(&db, &room_id, title)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }

    let meeting = ticketing_system::meetings::get_meeting(&db, &room_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Meeting not found".to_string()))?;

    Ok(Json(meeting))
}

#[derive(Debug, Serialize)]
pub struct ToggleFavoriteResponse {
    pub is_favorited: bool,
}

/// POST /api/meetings/:room_id/favorite
pub async fn toggle_meeting_favorite(
    Path(room_id): Path<String>,
    State(db): State<Arc<SqlitePool>>,
) -> Result<Json<ToggleFavoriteResponse>, (StatusCode, String)> {
    let is_favorited = ticketing_system::meetings::toggle_favorite(&db, &room_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(ToggleFavoriteResponse { is_favorited }))
}

// ============================================================================
// WebSocket Signaling Handler
// ============================================================================

/// GET /api/meetings/signaling
pub async fn signaling_websocket(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(handle_signaling)
}

async fn handle_signaling(socket: WebSocket) {
    let (mut sender, mut receiver) = socket.split();

    let mut current_room: Option<String> = None;
    let mut current_user: Option<String> = None;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<SignalingMessage>(100);

    let send_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let Ok(json) = serde_json::to_string(&msg) {
                if sender.send(WsMessage::Text(json.into())).await.is_err() {
                    break;
                }
            }
        }
    });

    while let Some(result) = receiver.next().await {
        let msg = match result {
            Ok(WsMessage::Text(text)) => text,
            Ok(WsMessage::Close(_)) => break,
            Err(_) => break,
            _ => continue,
        };

        let signal: SignalingMessage = match serde_json::from_str(&msg) {
            Ok(s) => s,
            Err(e) => {
                let _ = tx
                    .send(SignalingMessage::Error {
                        message: format!("Invalid message: {}", e),
                    })
                    .await;
                continue;
            }
        };

        match signal {
            SignalingMessage::Join { room_id, user_id } => {
                if let (Some(old_room), Some(old_user)) = (&current_room, &current_user) {
                    SIGNALING.leave_room(old_room, old_user).await;
                    let channel = SIGNALING.get_or_create_channel(old_room).await;
                    let _ = channel.send(SignalingMessage::UserLeft {
                        room_id: old_room.clone(),
                        user_id: old_user.clone(),
                    });
                }

                let users = SIGNALING.join_room(&room_id, &user_id).await;
                current_room = Some(room_id.clone());
                current_user = Some(user_id.clone());

                let channel = SIGNALING.get_or_create_channel(&room_id).await;
                {
                    let mut rx = channel.subscribe();
                    let tx_clone = tx.clone();
                    let user_clone = user_id.clone();
                    tokio::spawn(async move {
                        while let Ok(msg) = rx.recv().await {
                            let should_forward = match &msg {
                                SignalingMessage::Offer { from_user, to_user, .. } |
                                SignalingMessage::Answer { from_user, to_user, .. } |
                                SignalingMessage::IceCandidate { from_user, to_user, .. } => {
                                    from_user != &user_clone && (to_user == &user_clone || to_user == "*")
                                }
                                SignalingMessage::UserJoined { user_id, .. } |
                                SignalingMessage::UserLeft { user_id, .. } => {
                                    user_id != &user_clone
                                }
                                _ => true,
                            };

                            if should_forward {
                                if tx_clone.send(msg).await.is_err() {
                                    break;
                                }
                            }
                        }
                    });
                }

                let _ = tx
                    .send(SignalingMessage::RoomUsers {
                        room_id: room_id.clone(),
                        users: users.clone(),
                    })
                    .await;

                let channel = SIGNALING.get_or_create_channel(&room_id).await;
                let _ = channel.send(SignalingMessage::UserJoined {
                    room_id: room_id.clone(),
                    user_id: user_id.clone(),
                });
            }

            SignalingMessage::Leave { room_id, user_id } => {
                SIGNALING.leave_room(&room_id, &user_id).await;
                let channel = SIGNALING.get_or_create_channel(&room_id).await;
                let _ = channel.send(SignalingMessage::UserLeft {
                    room_id: room_id.clone(),
                    user_id: user_id.clone(),
                });
                current_room = None;
                current_user = None;
            }

            SignalingMessage::Offer { ref room_id, .. }
            | SignalingMessage::Answer { ref room_id, .. }
            | SignalingMessage::IceCandidate { ref room_id, .. } => {
                let channel = SIGNALING.get_or_create_channel(room_id).await;
                let _ = channel.send(signal);
            }

            _ => {}
        }
    }

    // Cleanup on disconnect
    if let (Some(room), Some(user)) = (current_room, current_user) {
        SIGNALING.leave_room(&room, &user).await;
        let channel = SIGNALING.get_or_create_channel(&room).await;
        let _ = channel.send(SignalingMessage::UserLeft {
            room_id: room,
            user_id: user,
        });
    }

    send_task.abort();
}
