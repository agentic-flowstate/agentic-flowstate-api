use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use ticketing_system::{email_intake, emails, Email, SqlitePool};

use crate::agents::codex_app_server::{resolve_codex_model, run_codex_text_no_tools};
use crate::agents::prompts::load_prompt;

const EMAIL_LLM_GUARD_CREATED_BY_SUFFIX: &str = ":email_llm_guard";

pub async fn process_email_intake_with_llm_guard(
    pool: &SqlitePool,
    email_id: i64,
    created_by: &str,
) -> Result<email_intake::EmailIntakeResult> {
    let email = emails::get_email_by_id(pool, email_id).await?;
    if email.folder != "Sent" {
        evaluate_email_llm_guard_for_email(pool, &email, created_by).await?;
    }

    email_intake::process_email_intake(pool, email_id, created_by).await
}

async fn evaluate_email_llm_guard_for_email(
    pool: &SqlitePool,
    email: &Email,
    created_by: &str,
) -> Result<email_intake::EmailLlmGuardEvaluation> {
    let model = resolve_codex_model("").to_string();
    let prompt_version = email_intake::EMAIL_LLM_GUARD_PROMPT_VERSION;
    let actor = format!("{created_by}{EMAIL_LLM_GUARD_CREATED_BY_SUFFIX}");

    let system_prompt = load_prompt("email-llm-guard-system", HashMap::new())
        .context("Failed to load email LLM guard system prompt")?;
    let prompt = load_prompt("email-llm-guard", prompt_vars(email))
        .context("Failed to load email LLM guard prompt")?;
    let working_dir = email_llm_guard_working_dir()?;

    match run_codex_text_no_tools(&model, "low", &system_prompt, &working_dir, &prompt).await {
        Ok(output) => {
            email_intake::record_email_llm_guard_output(
                pool,
                email.id,
                &model,
                prompt_version,
                &output,
                &actor,
            )
            .await
        }
        Err(error) => {
            let reason = format!("codex_no_tools_guard_failed: {}", compact_failure(&error));
            email_intake::record_email_llm_guard_failure(
                pool,
                email.id,
                &model,
                prompt_version,
                &reason,
                &actor,
            )
            .await
        }
    }
}

fn prompt_vars(email: &Email) -> HashMap<String, String> {
    let mut vars = HashMap::new();
    vars.insert("mailbox".to_string(), email.mailbox.clone());
    vars.insert("folder".to_string(), email.folder.clone());
    vars.insert("from_address".to_string(), email.from_address.clone());
    vars.insert(
        "from_name".to_string(),
        email.from_name.clone().unwrap_or_default(),
    );
    vars.insert(
        "subject".to_string(),
        email.subject.clone().unwrap_or_default(),
    );
    vars.insert(
        "body_text".to_string(),
        email.body_text.clone().unwrap_or_default(),
    );
    vars.insert(
        "body_html".to_string(),
        email.body_html.clone().unwrap_or_default(),
    );
    vars
}

fn email_llm_guard_working_dir() -> Result<PathBuf> {
    let dir = dirs::home_dir()
        .context("Failed to resolve home directory for email LLM guard working dir")?
        .join(".agentic-flowstate")
        .join("email-llm-guard-workspace");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create email LLM guard working dir at {dir:?}"))?;
    Ok(dir)
}

fn compact_failure(error: &str) -> String {
    let mut text = error.replace('\n', " ");
    text.truncate(240);
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_failure_is_single_line_and_bounded() {
        let compact = compact_failure(&format!("{}\n{}", "x".repeat(300), "tail"));

        assert!(!compact.contains('\n'));
        assert!(compact.len() <= 240);
    }
}
