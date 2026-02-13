use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;
use sqlx::SqlitePool;
use std::sync::Arc;
use tracing::{error, info};

use ticketing_system::{
    models::{CreatePipelineTemplateRequest, PipelineTemplateStep},
    pipelines,
};

// ============================================================================
// Request/Response Types
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct ListTemplatesQuery {
    pub organization: Option<String>,
    pub epic_id: Option<String>,
    pub slice_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateTemplateRequest {
    pub template_id: String,
    pub name: String,
    pub description: Option<String>,
    pub organization: Option<String>,
    pub epic_id: Option<String>,
    pub slice_id: Option<String>,
    pub steps: Vec<PipelineTemplateStep>,
}

// ============================================================================
// Pipeline Template Handlers
// ============================================================================

/// GET /api/pipeline-templates
pub async fn list_templates(
    State(pool): State<Arc<SqlitePool>>,
    Query(params): Query<ListTemplatesQuery>,
) -> Response {
    match pipelines::list_templates(
        &pool,
        params.organization.as_deref(),
        params.epic_id.as_deref(),
        params.slice_id.as_deref(),
    )
    .await
    {
        Ok(templates) => (StatusCode::OK, Json(json!({ "templates": templates }))).into_response(),
        Err(e) => {
            error!("Failed to list pipeline templates: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to list templates: {}", e) })),
            )
                .into_response()
        }
    }
}

/// GET /api/pipeline-templates/:template_id
pub async fn get_template(
    State(pool): State<Arc<SqlitePool>>,
    Path(template_id): Path<String>,
) -> Response {
    match pipelines::get_template(&pool, &template_id).await {
        Ok(Some(template)) => (StatusCode::OK, Json(template)).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Template not found" })),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to get pipeline template: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to get template: {}", e) })),
            )
                .into_response()
        }
    }
}

/// POST /api/pipeline-templates
pub async fn create_template(
    State(pool): State<Arc<SqlitePool>>,
    Json(request): Json<CreateTemplateRequest>,
) -> Response {
    let req = CreatePipelineTemplateRequest {
        template_id: request.template_id,
        name: request.name,
        description: request.description,
        organization: request.organization,
        epic_id: request.epic_id,
        slice_id: request.slice_id,
        steps: request.steps,
    };

    match pipelines::create_template(&pool, req).await {
        Ok(template) => {
            info!("Created pipeline template: {}", template.template_id);
            (StatusCode::CREATED, Json(template)).into_response()
        }
        Err(e) => {
            error!("Failed to create pipeline template: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to create template: {}", e) })),
            )
                .into_response()
        }
    }
}

/// DELETE /api/pipeline-templates/:template_id
pub async fn delete_template(
    State(pool): State<Arc<SqlitePool>>,
    Path(template_id): Path<String>,
) -> Response {
    match pipelines::delete_template(&pool, &template_id).await {
        Ok(()) => {
            info!("Deleted pipeline template: {}", template_id);
            (StatusCode::OK, Json(json!({ "deleted": template_id }))).into_response()
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("not found") {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "Template not found" })),
                )
                    .into_response()
            } else {
                error!("Failed to delete pipeline template: {:?}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("Failed to delete template: {}", e) })),
                )
                    .into_response()
            }
        }
    }
}
