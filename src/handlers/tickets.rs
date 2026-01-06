use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
    response::{IntoResponse, Response},
};
use aws_sdk_dynamodb::types::AttributeValue;
use serde::Deserialize;
use std::sync::Arc;
use tracing::error;

use crate::{
    db::DynamoDbPool,
    models::Ticket,
};

#[derive(Debug, Deserialize)]
pub struct TicketQuery {
    pub slice_id: Option<String>,
}

// List tickets for an epic or a specific slice
pub async fn list_tickets(
    State(pool): State<Arc<DynamoDbPool>>,
    Path(epic_id): Path<String>,
    Query(params): Query<TicketQuery>,
) -> Response {
    let client = pool.client();
    let table_name = pool.table_name();

    let pk = format!("EPIC#{}", epic_id);

    // Build the query based on whether slice_id is provided
    let result = if let Some(slice_id) = params.slice_id {
        // Query for specific slice
        let sk_prefix = format!("SLICE#{}#TICKET#", slice_id);
        client
            .query()
            .table_name(table_name)
            .key_condition_expression("PK = :pk AND begins_with(SK, :sk_prefix)")
            .expression_attribute_values(":pk", AttributeValue::S(pk))
            .expression_attribute_values(":sk_prefix", AttributeValue::S(sk_prefix))
            .send()
            .await
    } else {
        // Query for all tickets in the epic
        client
            .query()
            .table_name(table_name)
            .key_condition_expression("PK = :pk AND begins_with(SK, :sk_prefix)")
            .expression_attribute_values(":pk", AttributeValue::S(pk))
            .expression_attribute_values(":sk_prefix", AttributeValue::S("SLICE#".to_string()))
            .filter_expression("contains(SK, :ticket)")
            .expression_attribute_values(":ticket", AttributeValue::S("#TICKET#".to_string()))
            .send()
            .await
    };

    match result {
        Ok(output) => {
            let items = output.items.unwrap_or_default();
            let mut tickets = Vec::new();

            for item in items {
                // Manual extraction from DynamoDB item
                let ticket_id = item.get("ticket_id")
                        .and_then(|v| v.as_s().ok())
                        .unwrap_or(&String::new())
                        .clone();

                let title = item.get("title")
                        .and_then(|v| v.as_s().ok())
                        .unwrap_or(&String::new())
                        .clone();

                let intent = item.get("intent")
                        .and_then(|v| v.as_s().ok())
                        .unwrap_or(&String::new())
                        .clone();

                let slice_id_extracted = item.get("slice_id")
                        .and_then(|v| v.as_s().ok())
                        .unwrap_or(&String::new())
                        .clone();

                if !ticket_id.is_empty() && !title.is_empty() {
                    tickets.push(Ticket {
                            ticket_id,
                            epic_id: epic_id.clone(),
                            slice_id: slice_id_extracted,
                            title,
                            intent,
                            description: item.get("description").and_then(|v| v.as_s().ok()).cloned(),
                            ticket_type: item.get("type")
                                .and_then(|v| v.as_s().ok())
                                .unwrap_or(&"task".to_string())
                                .clone(),
                            status: item.get("status")
                                .and_then(|v| v.as_s().ok())
                                .unwrap_or(&"open".to_string())
                                .clone(),
                            priority: item.get("priority").and_then(|v| v.as_s().ok()).cloned(),
                            assignee: item.get("assignee").and_then(|v| v.as_s().ok()).cloned(),
                            notes: item.get("notes").and_then(|v| v.as_s().ok()).cloned(),
                            blocks_tickets: item.get("blocks_tickets")
                                .and_then(|v| v.as_l().ok())
                                .map(|list| {
                                    list.iter()
                                        .filter_map(|v| v.as_s().ok().cloned())
                                        .collect()
                                }),
                            blocked_by_tickets: item.get("blocked_by_tickets")
                                .and_then(|v| v.as_l().ok())
                                .map(|list| {
                                    list.iter()
                                        .filter_map(|v| v.as_s().ok().cloned())
                                        .collect()
                                }),
                            caused_by_tickets: item.get("caused_by_tickets")
                                .and_then(|v| v.as_l().ok())
                                .map(|list| {
                                    list.iter()
                                        .filter_map(|v| v.as_s().ok().cloned())
                                        .collect()
                                }),
                            created_at: item.get("created_at")
                                .and_then(|v| v.as_n().ok())
                                .and_then(|n| n.parse().ok())
                                .unwrap_or(0),
                            updated_at: item.get("updated_at")
                                .and_then(|v| v.as_n().ok())
                                .and_then(|n| n.parse().ok())
                                .unwrap_or(0),
                            created_at_iso: item.get("created_at_iso")
                                .and_then(|v| v.as_s().ok())
                                .unwrap_or(&String::new())
                                .clone(),
                            updated_at_iso: item.get("updated_at_iso")
                                .and_then(|v| v.as_s().ok())
                                .unwrap_or(&String::new())
                                .clone(),
                    });
                }
            }

            Json(tickets).into_response()
        }
        Err(e) => {
            error!("Failed to query tickets: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Failed to list tickets" }))
            ).into_response()
        }
    }
}

// List tickets for a specific slice (alternative endpoint)
pub async fn list_slice_tickets(
    State(pool): State<Arc<DynamoDbPool>>,
    Path((epic_id, slice_id)): Path<(String, String)>,
) -> Response {
    let client = pool.client();
    let table_name = pool.table_name();

    let pk = format!("EPIC#{}", epic_id);
    let sk_prefix = format!("SLICE#{}#TICKET#", slice_id);

    let result = client
        .query()
        .table_name(table_name)
        .key_condition_expression("PK = :pk AND begins_with(SK, :sk_prefix)")
        .expression_attribute_values(":pk", AttributeValue::S(pk))
        .expression_attribute_values(":sk_prefix", AttributeValue::S(sk_prefix))
        .send()
        .await;

    match result {
        Ok(output) => {
            let items = output.items.unwrap_or_default();
            let mut tickets = Vec::new();

            for item in items {
                // Manual extraction from DynamoDB item
                let ticket_id = item.get("ticket_id")
                    .and_then(|v| v.as_s().ok())
                    .unwrap_or(&String::new())
                    .clone();

                let title = item.get("title")
                    .and_then(|v| v.as_s().ok())
                    .unwrap_or(&String::new())
                    .clone();

                let intent = item.get("intent")
                    .and_then(|v| v.as_s().ok())
                    .unwrap_or(&String::new())
                    .clone();

                let slice_id_extracted = item.get("slice_id")
                    .and_then(|v| v.as_s().ok())
                    .unwrap_or(&String::new())
                    .clone();

                if !ticket_id.is_empty() && !title.is_empty() {
                    tickets.push(Ticket {
                        ticket_id,
                        epic_id: epic_id.clone(),
                        slice_id: slice_id_extracted,
                        title,
                        intent,
                        description: item.get("description").and_then(|v| v.as_s().ok()).cloned(),
                        ticket_type: item.get("type")
                            .and_then(|v| v.as_s().ok())
                            .unwrap_or(&"task".to_string())
                            .clone(),
                        status: item.get("status")
                            .and_then(|v| v.as_s().ok())
                            .unwrap_or(&"open".to_string())
                            .clone(),
                        priority: item.get("priority").and_then(|v| v.as_s().ok()).cloned(),
                        assignee: item.get("assignee").and_then(|v| v.as_s().ok()).cloned(),
                        notes: item.get("notes").and_then(|v| v.as_s().ok()).cloned(),
                        blocks_tickets: item.get("blocks_tickets")
                            .and_then(|v| v.as_l().ok())
                            .map(|list| {
                                list.iter()
                                    .filter_map(|v| v.as_s().ok().cloned())
                                    .collect()
                            }),
                        blocked_by_tickets: item.get("blocked_by_tickets")
                            .and_then(|v| v.as_l().ok())
                            .map(|list| {
                                list.iter()
                                    .filter_map(|v| v.as_s().ok().cloned())
                                    .collect()
                            }),
                        caused_by_tickets: item.get("caused_by_tickets")
                            .and_then(|v| v.as_l().ok())
                            .map(|list| {
                                list.iter()
                                    .filter_map(|v| v.as_s().ok().cloned())
                                    .collect()
                            }),
                        created_at: item.get("created_at")
                            .and_then(|v| v.as_n().ok())
                            .and_then(|n| n.parse().ok())
                            .unwrap_or(0),
                        updated_at: item.get("updated_at")
                            .and_then(|v| v.as_n().ok())
                            .and_then(|n| n.parse().ok())
                            .unwrap_or(0),
                        created_at_iso: item.get("created_at_iso")
                            .and_then(|v| v.as_s().ok())
                            .unwrap_or(&String::new())
                            .clone(),
                        updated_at_iso: item.get("updated_at_iso")
                            .and_then(|v| v.as_s().ok())
                            .unwrap_or(&String::new())
                            .clone(),
                    });
                }
            }

            Json(tickets).into_response()
        }
        Err(e) => {
            error!("Failed to query tickets for slice: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Failed to list tickets" }))
            ).into_response()
        }
    }
}