use axum::{
    extract::State,
    http::StatusCode,
    Json,
    response::{IntoResponse, Response},
};
use aws_sdk_dynamodb::types::AttributeValue;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::error;

use crate::{
    db::DynamoDbPool,
    models::{Epic, EpicsResponse},
};

pub async fn list_epics(
    State(pool): State<Arc<DynamoDbPool>>,
) -> Response {
    let client = pool.client();
    let table_name = pool.table_name();

    // Scan for all epics (items where PK starts with "EPIC#" and SK = "META")
    let result = client
        .scan()
        .table_name(table_name)
        .filter_expression("begins_with(PK, :pk_prefix) AND SK = :sk")
        .expression_attribute_values(":pk_prefix", AttributeValue::S("EPIC#".to_string()))
        .expression_attribute_values(":sk", AttributeValue::S("META".to_string()))
        .send()
        .await;

    match result {
        Ok(output) => {
            let items = output.items.unwrap_or_default();
            let mut epics = Vec::new();

            for item in items {
                // Extract fields manually from DynamoDB item
                let epic_id = item.get("epic_id")
                        .and_then(|v| v.as_s().ok())
                        .unwrap_or(&String::new())
                        .clone();

                let title = item.get("title")
                    .and_then(|v| v.as_s().ok())
                    .unwrap_or(&String::new())
                    .clone();

                if !epic_id.is_empty() && !title.is_empty() {
                    epics.push(Epic {
                        epic_id,
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
                        slice_count: item.get("slice_count")
                            .and_then(|v| v.as_n().ok())
                            .and_then(|n| n.parse().ok()),
                        ticket_count: item.get("ticket_count")
                            .and_then(|v| v.as_n().ok())
                            .and_then(|n| n.parse().ok()),
                    });
                }
            }

            Json(EpicsResponse { epics }).into_response()
        }
        Err(e) => {
            error!("Failed to query epics: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Failed to list epics" }))
            ).into_response()
        }
    }
}