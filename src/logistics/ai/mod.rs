pub mod assistant;
pub mod chunk;
pub mod digest;
#[cfg(test)]
mod eval;
pub mod notification_copy;
pub mod pod_check;
pub mod query;
pub mod reorder;
pub mod report;
pub mod status;

/// Model used by every AI feature in this module.
pub(crate) const MODEL: &str = "claude-haiku-5-5";

/// Output cap per call. Haiku 5.5 thinks by default and thinking counts
/// toward `max_tokens`, so this leaves room for it plus the short reply.
pub(crate) const MAX_TOKENS: u32 = 2048;

/// The reply text of a Messages API response. Reads the first `text` block
/// by type, because Haiku 5.5 can put `thinking` blocks before it, and turns
/// a safety refusal or an empty cut-off reply into an error.
pub(crate) fn response_text(json: &serde_json::Value) -> Result<&str, String> {
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
    use super::response_text;
    use serde_json::json;

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
