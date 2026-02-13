use axum::{
    extract::{Query, State},
    response::sse::{Event, KeepAlive, Sse},
};
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::convert::Infallible;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;
use ticketing_system::{epics, slices, tickets, Epic, Slice, SqlitePool, Ticket};

#[derive(Debug, Deserialize)]
pub struct DataSubscribeQuery {
    pub organization: String,
}

/// SSE event types for data updates
#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum DataEvent {
    /// Full sync of epics
    #[serde(rename = "epics")]
    Epics { epics: Vec<Epic> },
    /// Full sync of slices for selected epics
    #[serde(rename = "slices")]
    Slices { slices: Vec<Slice> },
    /// Full sync of tickets for selected slices
    #[serde(rename = "tickets")]
    Tickets { tickets: Vec<Ticket> },
}

fn hash_epics(epics: &[Epic]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for e in epics {
        e.epic_id.hash(&mut hasher);
        e.updated_at_iso.hash(&mut hasher);
        e.title.hash(&mut hasher);
        e.slice_count.hash(&mut hasher);
        e.ticket_count.hash(&mut hasher);
    }
    epics.len().hash(&mut hasher);
    hasher.finish()
}

fn hash_slices(slices: &[Slice]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for s in slices {
        s.slice_id.hash(&mut hasher);
        s.updated_at_iso.hash(&mut hasher);
        s.title.hash(&mut hasher);
        s.ticket_count.hash(&mut hasher);
    }
    slices.len().hash(&mut hasher);
    hasher.finish()
}

fn hash_tickets(tickets: &[Ticket]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for t in tickets {
        t.ticket_id.hash(&mut hasher);
        t.updated_at_iso.hash(&mut hasher);
        t.status.hash(&mut hasher);
        t.title.hash(&mut hasher);
    }
    tickets.len().hash(&mut hasher);
    hasher.finish()
}

/// GET /api/data/subscribe?organization=X
/// SSE endpoint for real-time data updates (epics, slices, tickets)
pub async fn subscribe_data(
    State(pool): State<Arc<SqlitePool>>,
    Query(params): Query<DataSubscribeQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let org = params.organization;

    let stream = async_stream::stream! {
        let mut last_epics_hash: u64 = 0;
        let mut last_slices_hash: u64 = 0;
        let mut last_tickets_hash: u64 = 0;

        loop {
            // Check epics
            if let Ok(epic_list) = epics::list_epics(&pool, Some(&org)).await {
                let hash = hash_epics(&epic_list);
                if hash != last_epics_hash {
                    last_epics_hash = hash;
                    let event = DataEvent::Epics { epics: epic_list.clone() };
                    if let Ok(json) = serde_json::to_string(&event) {
                        yield Ok(Event::default().data(json));
                    }
                }

                // Check slices
                let mut all_slices = Vec::new();
                for epic in &epic_list {
                    if let Ok(slice_list) = slices::list_slices(&pool, &org, &epic.epic_id).await {
                        all_slices.extend(slice_list);
                    }
                }
                let slices_hash = hash_slices(&all_slices);
                if slices_hash != last_slices_hash {
                    last_slices_hash = slices_hash;
                    let event = DataEvent::Slices { slices: all_slices.clone() };
                    if let Ok(json) = serde_json::to_string(&event) {
                        yield Ok(Event::default().data(json));
                    }
                }

                // Check tickets
                let mut all_tickets = Vec::new();
                for slice in &all_slices {
                    if let Ok(ticket_list) = tickets::list_tickets(&pool, &org, &slice.epic_id, &slice.slice_id).await {
                        all_tickets.extend(ticket_list);
                    }
                }
                let tickets_hash = hash_tickets(&all_tickets);
                if tickets_hash != last_tickets_hash {
                    last_tickets_hash = tickets_hash;
                    let event = DataEvent::Tickets { tickets: all_tickets };
                    if let Ok(json) = serde_json::to_string(&event) {
                        yield Ok(Event::default().data(json));
                    }
                }
            }

            // Poll every 2 seconds
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    };

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    )
}
