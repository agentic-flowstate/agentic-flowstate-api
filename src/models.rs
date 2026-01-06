use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Epic {
    pub epic_id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignees: Option<Vec<String>>,
    pub created_at_iso: String,
    pub updated_at_iso: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slice_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticket_count: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Slice {
    pub slice_id: String,
    pub epic_id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignees: Option<Vec<String>>,
    pub created_at_iso: String,
    pub updated_at_iso: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticket_count: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Ticket {
    pub ticket_id: String,
    pub epic_id: String,
    pub slice_id: String,
    pub title: String,
    pub intent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "type")]
    pub ticket_type: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocks_tickets: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_by_tickets: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caused_by_tickets: Option<Vec<String>>,
    pub created_at: i64,
    pub updated_at: i64,
    pub created_at_iso: String,
    pub updated_at_iso: String,
}

// Request/Response types
#[derive(Debug, Deserialize)]
pub struct CreateEpicRequest {
    pub epic_id: String,
    pub title: String,
    pub notes: Option<String>,
    pub assignees: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct CreateSliceRequest {
    pub slice_id: String,
    pub title: String,
    pub notes: Option<String>,
    pub assignees: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct CreateTicketRequest {
    pub title: String,
    pub intent: String,
    pub notes: Option<String>,
    #[serde(rename = "type")]
    pub ticket_type: Option<String>,
    pub assignee: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct EpicsResponse {
    pub epics: Vec<Epic>,
}