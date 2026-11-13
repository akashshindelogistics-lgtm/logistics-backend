//! Proof-of-delivery photo verification: before a dispatch is marked
//! DELIVERED, a vision-capable Claude call looks at the uploaded image (see
//! `docs/file-uploads.md`) and flags one that is plainly not a signature or a
//! package/delivery photo.
//!
//! Only uploads this backend stores (`/api/uploads/{id}`) are checked — an
//! arbitrary external URL or `data:` URI is not fetched. The check *fails
//! open*: with no `ANTHROPIC_API_KEY`, a network error, or an unclear reply,
//! the delivery proceeds. Only a clear "NO" from the model blocks it.

use base64::Engine;
use uuid::Uuid;

use crate::logistics::upload::upload::UploadedFile;

const PROMPT: &str = "This image was submitted as proof of delivery for a freight shipment. \
    Is it plausibly a handwritten/drawn signature, a photo of a delivered package or goods, \
    or a photo of a signed delivery document? Answer with exactly one word: YES if it \
    plausibly is, NO if it clearly is not (e.g. a blank image, a random screenshot, a meme, \
    an unrelated scene).";

/// Outcome of a check that completed.
#[derive(Debug, PartialEq, Eq)]
pub enum PodVerdict {
    Plausible,
    NotProof,
}

/// The upload id in a `/api/uploads/{id}` URL (a backend-relative path or an
/// absolute URL ending in it), or `None` for any other value.
pub fn upload_id_from_url(url: &str) -> Option<Uuid> {
    let (_, id) = url.trim().rsplit_once("/api/uploads/")?;
    Uuid::parse_str(id).ok()
}

/// Map the model's reply to a verdict. Anything but a clear NO is plausible.
pub fn parse_verdict(reply: &str) -> PodVerdict {
    let first_word = reply
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches(|c: char| !c.is_alphanumeric());
    if first_word.eq_ignore_ascii_case("no") {
        PodVerdict::NotProof
    } else {
        PodVerdict::Plausible
    }
}

/// Check one stored upload. `Err` means the check could not run.
pub async fn check_upload(file: &UploadedFile) -> Result<PodVerdict, String> {
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .map_err(|_| "ANTHROPIC_API_KEY environment variable not set".to_string())?;
    let bytes = file
        .read_bytes()
        .map_err(|e| format!("Failed to read uploaded file: {e}"))?;
    let data = base64::engine::general_purpose::STANDARD.encode(bytes);

    let mut request = crate::logistics::http::client()
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", &api_key)
        .header("anthropic-version", "2023-06-01");
    if let Ok(workspace_id) = std::env::var("ANTHROPIC_WORKSPACE_ID")
        && !workspace_id.trim().is_empty()
    {
        request = request.header("anthropic-workspace-id", workspace_id);
    }

    let resp = request
        .json(&serde_json::json!({
            "model": super::MODEL,
            "max_tokens": super::MAX_TOKENS,
            "output_config": { "effort": "low" },
            "messages": [{ "role": "user", "content": [
                { "type": "image", "source": {
                    "type": "base64", "media_type": file.content_type, "data": data } },
                { "type": "text", "text": PROMPT }
            ]}]
        }))
        .send()
        .await
        .map_err(|e| format!("Failed to reach Anthropic API: {e}"))?;
    if !resp.status().is_success() {
        let err = resp.text().await.unwrap_or_default();
        return Err(format!("Anthropic API error: {err}"));
    }
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse API response: {e}"))?;
    super::response_text(&json).map(parse_verdict)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_id_from_url_accepts_relative_and_absolute_upload_urls() {
        let id = Uuid::new_v4();
        assert_eq!(upload_id_from_url(&format!("/api/uploads/{id}")), Some(id));
        assert_eq!(
            upload_id_from_url(&format!("https://api.example.com/api/uploads/{id}")),
            Some(id)
        );
        assert_eq!(upload_id_from_url("https://example.com/sig.png"), None);
        assert_eq!(upload_id_from_url("data:image/png;base64,AAAA"), None);
        assert_eq!(upload_id_from_url("/api/uploads/not-a-uuid"), None);
    }

    #[test]
    fn only_a_clear_no_is_not_proof() {
        assert_eq!(parse_verdict("NO"), PodVerdict::NotProof);
        assert_eq!(parse_verdict(" no."), PodVerdict::NotProof);
        assert_eq!(parse_verdict("YES"), PodVerdict::Plausible);
        assert_eq!(parse_verdict("Not sure"), PodVerdict::Plausible);
        assert_eq!(parse_verdict("unclear"), PodVerdict::Plausible);
    }
}
