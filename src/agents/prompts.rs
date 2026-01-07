use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use anyhow::{Result, Context};

/// Load a prompt template from the _prompts directory and substitute variables.
///
/// Variables in the template use the format `{{VARIABLE_NAME}}`.
pub fn load_prompt(agent_type: &str, vars: HashMap<String, String>) -> Result<String> {
    let prompts_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("_prompts");
    let prompt_file = prompts_dir.join(format!("{}.txt", agent_type));

    let template = fs::read_to_string(&prompt_file)
        .with_context(|| format!("Failed to load prompt template: {:?}", prompt_file))?;

    let mut result = template;
    for (key, value) in &vars {
        let placeholder = format!("{{{{{}}}}}", key.to_uppercase());
        result = result.replace(&placeholder, value);
    }

    // Handle conditional blocks: {{#if VAR}}content{{/if}}
    // Simple implementation - just removes blocks where the var is empty/missing
    result = process_conditionals(&result, &vars);

    Ok(result)
}

fn process_conditionals(template: &str, vars: &HashMap<String, String>) -> String {
    let mut result = template.to_string();

    // Find all {{#if VAR}}...{{/if}} blocks
    let if_pattern = regex::Regex::new(r"\{\{#if\s+(\w+)\}\}([\s\S]*?)\{\{/if\}\}").ok();

    if let Some(re) = if_pattern {
        result = re.replace_all(&result, |caps: &regex::Captures| {
            let var_name = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let content = caps.get(2).map(|m| m.as_str()).unwrap_or("");

            // Check if variable exists and is non-empty
            if vars.get(&var_name.to_uppercase()).map(|v| !v.is_empty()).unwrap_or(false) {
                content.to_string()
            } else {
                String::new()
            }
        }).to_string();
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_variable_substitution() {
        let template = "Hello {{NAME}}, your ticket is {{TICKET_ID}}";
        let mut vars = HashMap::new();
        vars.insert("name".to_string(), "Alice".to_string());
        vars.insert("ticket_id".to_string(), "T-123".to_string());

        // Note: load_prompt reads from file, so this test would need the file to exist
        // This is a basic test of the substitution logic
    }
}
