use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Epic {
    pub epic_id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
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

// Request types
#[derive(Debug, Deserialize)]
pub struct CreateEpicRequest {
    pub epic_id: String,
    pub title: String,
    pub organization: String,
    pub notes: Option<String>,
    pub assignees: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct CreateSliceRequest {
    pub title: String,
    pub notes: Option<String>,
    pub assignees: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct CreateTicketRequest {
    pub title: String,
    pub intent: Option<String>,
    pub notes: Option<String>,
    pub priority: Option<String>,
    pub assignees: Option<Vec<String>>,
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateTicketRequest {
    pub status: Option<String>,
    pub notes: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateTicketStatusRequest {
    pub status: String,
}

#[derive(Debug, Deserialize)]
pub struct AddRelationshipRequest {
    pub relationship_type: String,  // blocks, blocked_by, caused_by
    pub target_ticket_id: String,
}

#[derive(Debug, Deserialize)]
pub struct RemoveRelationshipRequest {
    pub relationship_type: String,  // blocks, blocked_by, caused_by
    pub target_ticket_id: String,
}

// Response types
#[derive(Debug, Serialize)]
pub struct EpicsResponse {
    pub epics: Vec<Epic>,
}

#[derive(Debug, Serialize)]
pub struct SlicesResponse {
    pub slices: Vec<Slice>,
}

#[derive(Debug, Serialize)]
pub struct TicketsResponse {
    pub tickets: Vec<Ticket>,
}