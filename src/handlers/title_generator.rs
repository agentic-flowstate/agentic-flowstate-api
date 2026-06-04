//! Generate conversation titles and auto-detect organization using Codex.

use std::collections::HashMap;

use sqlx::SqlitePool;
use ticketing_system::{conversations, organizations, UpdateConversationRequest};

use crate::agents::codex_app_server::{resolve_codex_model, run_codex_text};
use crate::agents::prompts::load_prompt;

/// Result of title + org generation
pub struct TitleAndOrg {
    pub title: String,
    pub organization: Option<String>,
}

const VALID_CONVERSATION_TYPES: &[&str] = &["bug", "build", "research", "support", "general"];

/// Generate a concise conversation title and auto-detect the organization.
/// Called as a fire-and-forget background task after the first user message.
pub async fn generate_title_and_org(
    db: SqlitePool,
    user_id: String,
    conversation_id: String,
    user_message: String,
    current_org: String,
) -> Option<TitleAndOrg> {
    // Get the user's organizations WITH descriptions for rich classification
    let user_orgs = organizations::get_user_orgs_with_descriptions(&db, &user_id)
        .await
        .unwrap_or_default();

    let orgs_with_descriptions: Vec<_> = user_orgs
        .iter()
        .filter(|o| !o.organization.starts_with("__"))
        .collect();

    let org_names: Vec<String> = orgs_with_descriptions
        .iter()
        .map(|o| o.organization.clone())
        .collect();

    // Build rich org context for the classifier
    let org_context = if orgs_with_descriptions.is_empty() {
        "### general\nGeneric conversations not tied to a listed organization.".to_string()
    } else {
        let mut parts = Vec::new();
        for org in &orgs_with_descriptions {
            if let Some(desc) = &org.description {
                parts.push(format!("### {}\n{}", org.organization, desc));
            } else {
                parts.push(format!(
                    "### {}\n(No description available)",
                    org.organization
                ));
            }
        }
        parts.push(
            "### general\nGeneric conversations not tied to a listed organization.".to_string(),
        );
        parts.join("\n\n")
    };

    let mut system_vars = HashMap::new();
    system_vars.insert("org_context".to_string(), org_context);
    let mut valid_org_names = org_names.clone();
    valid_org_names.push("general".to_string());
    system_vars.insert("org_list".to_string(), valid_org_names.join(", "));
    let system_prompt = match load_prompt("conversation-classifier-system", system_vars) {
        Ok(prompt) => prompt,
        Err(e) => {
            tracing::error!("[TITLE] Failed to load classifier system prompt: {}", e);
            return None;
        }
    };

    let mut user_vars = HashMap::new();
    user_vars.insert("user_message".to_string(), user_message);
    let prompt = match load_prompt("conversation-classifier-user", user_vars) {
        Ok(prompt) => prompt,
        Err(e) => {
            tracing::error!("[TITLE] Failed to load classifier user prompt: {}", e);
            return None;
        }
    };

    let raw_output = match run_codex_text(
        resolve_codex_model(""),
        "low",
        &system_prompt,
        std::path::Path::new("/tmp"),
        &prompt,
    )
    .await
    {
        Ok(text) => text.trim().to_string(),
        Err(e) => {
            tracing::error!("[TITLE] codex app-server failed: {}", e);
            return None;
        }
    };

    if raw_output.is_empty() {
        tracing::warn!("[TITLE] Empty response from Codex");
        return None;
    }

    // Parse three-line response: title, organization, conversation type.
    let lines: Vec<&str> = raw_output.lines().collect();
    let title = lines
        .first()
        .map(|l| l.trim().to_string())
        .unwrap_or_default();
    let detected_org = lines.get(1).map(|l| l.trim().to_lowercase());
    let conversation_type = match lines.get(2).and_then(|l| normalize_conversation_type(l)) {
        Some(value) => value,
        None => {
            tracing::warn!(
                "[TITLE] Invalid conversation type from response: {:?}",
                raw_output
            );
            return None;
        }
    };

    if title.is_empty() {
        tracing::warn!("[TITLE] Empty title from response: {:?}", raw_output);
        return None;
    }

    // Validate detected org — must be in the user's org list or "general"
    let valid_org = detected_org.and_then(|org| {
        if org == "general" {
            Some("general".to_string())
        } else if org_names.iter().any(|o| o.to_lowercase() == org) {
            // Return the original-cased name from the org list
            org_names.iter().find(|o| o.to_lowercase() == org).cloned()
        } else {
            tracing::warn!(
                "[TITLE] Detected org {:?} not in user's orgs, using current",
                org
            );
            None
        }
    });

    // Determine the final org: use detected if different from current, otherwise keep current
    let final_org = valid_org.filter(|o| o != &current_org);

    tracing::info!(
        "[TITLE] Generated for {}: title={:?}, org={:?}, type={:?} (current={:?})",
        conversation_id,
        title,
        final_org,
        conversation_type,
        current_org
    );

    // Update conversation in DB
    if let Err(e) = conversations::update_conversation(
        &db,
        &user_id,
        &conversation_id,
        UpdateConversationRequest {
            title: Some(title.clone()),
            session_id: None,
            organization: final_org.clone(),
            conversation_type: Some(conversation_type.clone()),
        },
    )
    .await
    {
        tracing::error!("[TITLE] Failed to update conversation: {}", e);
        return None;
    }

    Some(TitleAndOrg {
        title,
        organization: final_org,
    })
}

fn normalize_conversation_type(raw: &str) -> Option<String> {
    let value = raw.trim().to_lowercase();
    VALID_CONVERSATION_TYPES
        .contains(&value.as_str())
        .then_some(value)
}
