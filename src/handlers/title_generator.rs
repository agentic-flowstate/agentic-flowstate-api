//! Generate conversation titles and auto-detect organization with OpenAI.

use sqlx::SqlitePool;
use ticketing_system::{conversations, organizations, UpdateConversationRequest};

use crate::agents::openai_text::{resolve_openai_model, run_openai_text};

/// Result of title + org generation
pub struct TitleAndOrg {
    pub title: String,
    pub organization: Option<String>,
}

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
        "No organizations available. Use \"general\" for all conversations.".to_string()
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
        parts.push("### general\nUse this ONLY when the conversation genuinely does not belong to any of the organizations above. If the topic is even tangentially related to a specific organization, pick that organization instead. Err on the side of specificity.".to_string());
        parts.join("\n\n")
    };

    let system_prompt = format!(
        "You are a conversation classifier. Given a user's first message, you must:\n\
         1. Generate a concise conversation title (3-7 words)\n\
         2. Determine which organization this conversation belongs to\n\n\
         ## Organizations\n\n\
         {org_context}\n\n\
         ## Rules\n\
         - Pick the MOST SPECIFIC organization that matches. Do not default to \"general\" unless the message truly has nothing to do with any listed organization.\n\
         - If the message mentions specific technologies, repos, or topics described in an organization, pick that organization.\n\
         - If the message could plausibly belong to multiple organizations, pick the one it most strongly relates to.\n\
         - \"general\" is for truly generic topics: casual chat, questions about the weather, personal errands, etc.\n\n\
         ## Output Format\n\
         Output EXACTLY two lines, nothing else:\n\
         Line 1: A concise 3-7 word title (no quotes, no trailing punctuation)\n\
         Line 2: The organization name (must be one of: {org_list}, general)\n\n\
         No labels, no explanation, no markdown, just the two lines.",
        org_context = org_context,
        org_list = org_names.join(", "),
    );

    let prompt =
        format!("Classify this conversation based on the user's message:\n\n{user_message}");

    let raw_output = match run_openai_text(
        resolve_openai_model("haiku"),
        "low",
        &system_prompt,
        &prompt,
    )
    .await
    {
        Ok(text) => text.trim().to_string(),
        Err(e) => {
            tracing::error!("[TITLE] OpenAI title generation failed: {}", e);
            return None;
        }
    };

    if raw_output.is_empty() {
        tracing::warn!("[TITLE] Empty response from title model");
        return None;
    }

    // Parse two-line response: title on line 1, org on line 2
    let lines: Vec<&str> = raw_output.lines().collect();
    let title = lines
        .first()
        .map(|l| l.trim().to_string())
        .unwrap_or_default();
    let detected_org = lines.get(1).map(|l| l.trim().to_lowercase());

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
        "[TITLE] Generated for {}: title={:?}, org={:?} (current={:?})",
        conversation_id,
        title,
        final_org,
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
