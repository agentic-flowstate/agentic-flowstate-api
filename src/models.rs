use serde::Deserialize;

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

/// Thin HTTP-boundary shape for `POST /api/tickets`. Intentionally
/// narrower than `ticketing_system::models::CreateTicketRequest` — the
/// handler validates this body and enriches it into the domain shape
/// before persisting. Renamed from `CreateTicketRequest` to
/// disambiguate from the domain type (A-45F91AB6 P1-7).
#[derive(Debug, Deserialize)]
pub struct CreateTicketHttpBody {
    pub title: String,
    pub description: Option<String>,
    pub ticket_type: Option<String>,
    pub milestone_id: Option<String>,
    pub blocked_by: Option<Vec<String>>,
    pub assignee: Option<String>,
    pub agent: Option<String>,
    pub repository: Option<String>,
    pub due_date: Option<String>,
    pub anchored: Option<bool>,
    pub classification: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateTicketRequest {
    pub status: Option<String>,
    pub notes: Option<String>,
    pub due_date: Option<String>,
    pub anchored: Option<bool>,
    pub expected_updated_at: Option<i64>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub assignee: Option<String>,
    pub agent: Option<String>,
    pub repository: Option<String>,
    pub ticket_type: Option<String>,
    pub guidance: Option<String>,
}
