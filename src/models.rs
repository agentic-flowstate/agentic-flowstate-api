use serde::Deserialize;

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
    pub slice_id: String,
    pub title: String,
    pub notes: Option<String>,
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
