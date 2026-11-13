//! The one Anthropic Messages API client every AI feature in this module
//! goes through: model, output cap, auth headers, error handling and reply
//! parsing live here, so a change to any of them is a one-file change.

/// Model used by every AI feature in this module.
pub(crate) const MODEL: &str = "claude-haiku-5-5";

/// Output cap per call. Haiku 5.5 thinks by default and thinking counts
/// toward `max_tokens`, so this leaves room for it plus the short reply.
const MAX_TOKENS: u32 = 2048;

const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";

/// How hard the model thinks before answering (`output_config.effort`).
/// Each feature picks its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Effort {
    /// Short rewrites and narrations of data already in the prompt.
    Low,
    /// Answers that must pick the relevant facts out of retrieved context.
    Medium,
}

impl Effort {
    fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
        }
    }
}

/// Send one user prompt and return the model's reply text. Errors if
/// `ANTHROPIC_API_KEY` is unset, the API fails or refuses, or the reply has
/// no text.
pub(crate) async fn complete(prompt: String, effort: Effort) -> Result<String, String> {
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .map_err(|_| "ANTHROPIC_API_KEY environment variable not set".to_string())?;

    let mut request = crate::logistics::http::client()
        .post(MESSAGES_URL)
        .header("x-api-key", &api_key)
        .header("anthropic-version", "2023-06-01");

    // Identity-linked API keys (issued against a user rather than a single
    // workspace) must name the target workspace explicitly, or the API rejects
    // the call with `workspace_id_required`. Workspace-scoped keys don't need
    // this, so the header is only sent when `ANTHROPIC_WORKSPACE_ID` is set.
    if let Ok(workspace_id) = std::env::var("ANTHROPIC_WORKSPACE_ID")
        && !workspace_id.trim().is_empty()
    {
        request = request.header("anthropic-workspace-id", workspace_id);
    }

    let resp = request
        .json(&request_body(&prompt, effort))
        .send()
        .await
        .map_err(|e| format!("Failed to reach Anthropic API: {}", e))?;

    if !resp.status().is_success() {
        let err = resp.text().await.unwrap_or_default();
        return Err(format!("Anthropic API error: {}", err));
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse API response: {}", e))?;

    response_text(&json).map(|s| s.to_string())
}

fn request_body(prompt: &str, effort: Effort) -> serde_json::Value {
    serde_json::json!({
        "model": MODEL,
        "max_tokens": MAX_TOKENS,
        "output_config": { "effort": effort.as_str() },
        "messages": [{ "role": "user", "content": prompt }]
    })
}

/// The reply text of a Messages API response. Reads the first `text` block
/// by type, because Haiku 5.5 can put `thinking` blocks before it, and turns
/// a safety refusal or an empty cut-off reply into an error.
fn response_text(json: &serde_json::Value) -> Result<&str, String> {
    if json["stop_reason"] == "refusal" {
        let category = json["stop_details"]["category"]
            .as_str()
            .unwrap_or("unspecified");
        return Err(format!(
            "Anthropic API declined the request (category: {category})"
        ));
    }
    json["content"]
        .as_array()
        .and_then(|blocks| blocks.iter().find(|b| b["type"] == "text"))
        .and_then(|b| b["text"].as_str())
        .ok_or_else(|| "Unexpected response format from Anthropic API".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_body_uses_the_shared_model_and_the_features_effort() {
        let body = request_body("hi", Effort::Medium);
        assert_eq!(body["model"], MODEL);
        assert_eq!(body["max_tokens"], MAX_TOKENS);
        assert_eq!(body["output_config"]["effort"], "medium");
        assert_eq!(body["messages"][0]["content"], "hi");
        assert_eq!(
            request_body("hi", Effort::Low)["output_config"]["effort"],
            "low"
        );
    }

    #[test]
    fn response_text_skips_leading_thinking_blocks() {
        let resp = json!({"stop_reason": "end_turn", "content": [
            {"type": "thinking", "thinking": "", "signature": "x"},
            {"type": "text", "text": "Truck MH12 is on time."}
        ]});
        assert_eq!(response_text(&resp).unwrap(), "Truck MH12 is on time.");
    }

    #[test]
    fn response_text_errors_on_refusal_and_on_thinking_only() {
        let refused =
            json!({"stop_reason": "refusal", "stop_details": {"category": "cyber"}, "content": []});
        assert!(response_text(&refused).unwrap_err().contains("cyber"));
        let cut_off =
            json!({"stop_reason": "max_tokens", "content": [{"type": "thinking", "thinking": ""}]});
        assert!(response_text(&cut_off).is_err());
    }
}
