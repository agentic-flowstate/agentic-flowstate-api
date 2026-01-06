use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
    response::{IntoResponse, Response},
};
use aws_sdk_dynamodb::types::AttributeValue;
use std::sync::Arc;
use tracing::error;

use crate::{
    db::DynamoDbPool,
    models::Slice,
};

pub async fn list_slices(
    State(pool): State<Arc<DynamoDbPool>>,
    Path(epic_id): Path<String>,
) -> Response {
    let client = pool.client();
    let table_name = pool.table_name();

    // Query for all slices in this epic
    let pk = format!("EPIC#{}", epic_id);
    let result = client
        .query()
        .table_name(table_name)
        .key_condition_expression("PK = :pk AND begins_with(SK, :sk_prefix)")
        .expression_attribute_values(":pk", AttributeValue::S(pk))
        .expression_attribute_values(":sk_prefix", AttributeValue::S("SLICE#".to_string()))
        .send()
        .await;

    match result {
        Ok(output) => {
            let items = output.items.unwrap_or_default();
            let mut slices = Vec::new();

            for item in items {
                // Manual extraction from DynamoDB item
                let slice_id = item.get("slice_id")
                        .and_then(|v| v.as_s().ok())
                        .unwrap_or(&String::new())
                        .clone();

                let title = item.get("title")
                    .and_then(|v| v.as_s().ok())
                    .unwrap_or(&String::new())
                    .clone();

                if !slice_id.is_empty() && !title.is_empty() {
                    slices.push(Slice {
                        slice_id,
                        epic_id: epic_id.clone(),
                        title,
                        notes: item.get("notes").and_then(|v| v.as_s().ok()).cloned(),
                        assignees: item.get("assignees")
                            .and_then(|v| v.as_l().ok())
                            .map(|list| {
                                list.iter()
                                    .filter_map(|v| v.as_s().ok().cloned())
                                    .collect()
                            }),
                        created_at_iso: item.get("created_at_iso")
                            .and_then(|v| v.as_s().ok())
                            .unwrap_or(&String::new())
                            .clone(),
                        updated_at_iso: item.get("updated_at_iso")
                            .and_then(|v| v.as_s().ok())
                            .unwrap_or(&String::new())
                            .clone(),
                        ticket_count: item.get("ticket_count")
                            .and_then(|v| v.as_n().ok())
                            .and_then(|n| n.parse().ok()),
                    });
                }
            }

            Json(slices).into_response()
        }
        Err(e) => {
            error!("Failed to query slices: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Failed to list slices" }))
            ).into_response()
        }
    }
}