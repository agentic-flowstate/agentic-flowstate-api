use serde_json::Value;

const DEFAULT_OPENAI_TEXT_MODEL: &str = "gpt-5.4";

pub fn resolve_openai_model(model: &str) -> &str {
    match model {
        "" | "haiku" | "opus" | "claude-opus-4-7" => DEFAULT_OPENAI_TEXT_MODEL,
        legacy if legacy.starts_with("claude-") => DEFAULT_OPENAI_TEXT_MODEL,
        other => other,
    }
}

pub fn normalize_reasoning_effort(effort: &str) -> &str {
    match effort {
        "minimal" => "minimal",
        "low" => "low",
        "medium" => "medium",
        "high" => "high",
        "xhigh" | "max" => "high",
        _ => "medium",
    }
}

pub async fn run_openai_text(
    model: &str,
    reasoning_effort: &str,
    system_prompt: &str,
    prompt: &str,
) -> Result<String, String> {
    let api_key = std::env::var("OPENAI_KEY").map_err(|_| "OPENAI_KEY not set".to_string())?;

    let body = serde_json::json!({
        "model": model,
        "instructions": system_prompt,
        "input": prompt,
        "reasoning": {
            "effort": normalize_reasoning_effort(reasoning_effort),
        },
        "text": {
            "format": {
                "type": "text",
            }
        },
        "store": false,
    });

    let response = reqwest::Client::new()
        .post("https://api.openai.com/v1/responses")
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("OpenAI Responses API request failed: {e}"))?;

    let status = response.status();
    if !status.is_success() {
        let error_text = response.text().await.unwrap_or_default();
        return Err(format!(
            "OpenAI Responses API error ({status}): {error_text}"
        ));
    }

    let json: Value = response
        .json()
        .await
        .map_err(|e| format!("Failed to decode OpenAI response JSON: {e}"))?;

    extract_output_text(&json).ok_or_else(|| {
        let summary = serde_json::to_string(&json)
            .unwrap_or_else(|_| "<unserializable response>".to_string());
        format!("OpenAI response did not contain output_text: {summary}")
    })
}

fn extract_output_text(response: &Value) -> Option<String> {
    let output = response.get("output")?.as_array()?;
    let mut parts = Vec::new();

    for item in output {
        if item.get("type")?.as_str()? != "message" {
            continue;
        }

        let content = item.get("content")?.as_array()?;
        for part in content {
            if part.get("type")?.as_str()? == "output_text" {
                let text = part.get("text")?.as_str()?;
                parts.push(text.to_string());
            }
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join(""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resolves_legacy_claude_models_to_gpt_5_4() {
        assert_eq!(resolve_openai_model("claude-opus-4-7"), "gpt-5.4");
        assert_eq!(resolve_openai_model("haiku"), "gpt-5.4");
        assert_eq!(resolve_openai_model("gpt-5.4"), "gpt-5.4");
    }

    #[test]
    fn normalizes_legacy_reasoning_labels() {
        assert_eq!(normalize_reasoning_effort("xhigh"), "high");
        assert_eq!(normalize_reasoning_effort("max"), "high");
        assert_eq!(normalize_reasoning_effort("low"), "low");
        assert_eq!(normalize_reasoning_effort("unknown"), "medium");
    }

    #[test]
    fn extracts_output_text_from_response_messages() {
        let response = json!({
            "output": [
                {
                    "type": "message",
                    "content": [
                        {
                            "type": "output_text",
                            "text": "hello"
                        },
                        {
                            "type": "output_text",
                            "text": " world"
                        }
                    ]
                }
            ]
        });

        assert_eq!(
            extract_output_text(&response).as_deref(),
            Some("hello world")
        );
    }
}
