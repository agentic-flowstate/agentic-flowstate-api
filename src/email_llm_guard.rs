use anyhow::{Context, Result};
use std::path::PathBuf;
use ticketing_system::{email_intake, emails, Email, SqlitePool};
use uuid::Uuid;

use crate::agents::codex_app_server::{
    resolve_codex_model, spawn_codex_app_server, CodexAppServerEvent, CodexAppServerOptions,
    CodexSandboxMode, CodexToolProfile,
};
use crate::agents::prompts::load_prompt;

const EMAIL_LLM_GUARD_CREATED_BY_SUFFIX: &str = ":email_llm_guard";
const EMAIL_LLM_GUARD_REASONING_EFFORT: &str = "xhigh";
const EMAIL_QUARANTINE_TOOLS: &[&str] = &[
    "inspect_email_for_quarantine",
    "submit_quarantine_verdict",
    "exa_search",
    "exa_get_contents",
];

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
    let evaluation_run_id = Uuid::new_v4().to_string();

    let system_prompt = load_prompt("email-llm-guard-system", Default::default())
        .context("Failed to load email LLM guard system prompt")?;
    let prompt = load_prompt("email-llm-guard", prompt_vars(email, &evaluation_run_id))
        .context("Failed to load email LLM guard prompt")?;
    let working_dir = email_llm_guard_working_dir()?;

    match run_quarantine_agent(
        &model,
        EMAIL_LLM_GUARD_REASONING_EFFORT,
        &system_prompt,
        &working_dir,
        &prompt,
        pool,
        email.id,
        &evaluation_run_id,
    )
    .await
    {
        Ok(evaluation) => {
            email_intake::record_email_llm_guard_output(
                pool,
                email.id,
                &model,
                prompt_version,
                &evaluation.verdict,
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

async fn run_quarantine_agent(
    model: &str,
    reasoning_effort: &str,
    system_prompt: &str,
    working_dir: &std::path::Path,
    prompt: &str,
    pool: &SqlitePool,
    email_id: i64,
    evaluation_run_id: &str,
) -> Result<email_intake::EmailQuarantineEvaluation, String> {
    let approved_mcp_tools = EMAIL_QUARANTINE_TOOLS
        .iter()
        .map(|tool| (*tool).to_string())
        .collect();
    let mut running = spawn_codex_app_server(CodexAppServerOptions {
        model,
        reasoning_effort,
        system_prompt,
        working_dir,
        prompt,
        sandbox: CodexSandboxMode::ReadOnly,
        bypass_approvals_and_sandbox: false,
        resume_session_id: None,
        ephemeral: true,
        tool_profile: CodexToolProfile::ConfiguredMcpOnly,
        scoped_user_id: None,
        approved_mcp_tools,
    })
    .await?;

    while let Some(event) = running.events.recv().await {
        match event {
            CodexAppServerEvent::ToolCallStarted { name, .. } => {
                tracing::debug!(email_id, evaluation_run_id, tool = %name, "email quarantine agent tool call started");
            }
            CodexAppServerEvent::ToolCallCompleted { is_error, .. } if is_error => {
                tracing::warn!(
                    email_id,
                    evaluation_run_id,
                    "email quarantine agent tool call failed"
                );
            }
            _ => {}
        }
    }

    let outcome = running.wait().await;
    let submitted =
        email_intake::get_email_quarantine_evaluation_by_run_id(pool, email_id, evaluation_run_id)
            .await
            .map_err(|e| format!("failed to load submitted quarantine verdict: {e}"))?;

    if let Some(evaluation) = submitted {
        return Ok(evaluation);
    }

    match outcome {
        Ok(outcome) if outcome.success() => Err(format!(
            "quarantine agent completed without calling submit_quarantine_verdict for email://message/{email_id}"
        )),
        Ok(outcome) => Err(outcome.failure_summary("email quarantine agent")),
        Err(error) => Err(error),
    }
}

fn prompt_vars(
    email: &Email,
    evaluation_run_id: &str,
) -> std::collections::HashMap<String, String> {
    let mut vars = std::collections::HashMap::new();
    vars.insert("email_id".to_string(), email.id.to_string());
    vars.insert(
        "email_uri".to_string(),
        format!("email://message/{}", email.id),
    );
    vars.insert(
        "evaluation_run_id".to_string(),
        evaluation_run_id.to_string(),
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
