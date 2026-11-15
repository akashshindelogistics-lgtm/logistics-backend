//! Proof-of-delivery photo verification: before a dispatch is marked
//! DELIVERED, a vision-capable Claude call looks at the uploaded image (see
//! `docs/file-uploads.md`) and flags one that is plainly not a signature or a
//! package/delivery photo.
//!
//! Only uploads this backend stores (`/api/uploads/{id}`) are checked — an
//! arbitrary external URL or `data:` URI is not fetched. The check *fails
//! open*: with no `ANTHROPIC_API_KEY`, a network error, or an unclear reply,
//! the delivery proceeds. Only a clear "NO" from the model blocks it.
//!
//! The Messages API rejects an image whose base64 payload exceeds 5 MB, so an
//! upload over about 3.75 MiB raw is skipped up front (see
//! [`MAX_IMAGE_BASE64_BYTES`]) rather than sent; the caller logs the reason.

use base64::Engine;
use uuid::Uuid;

use crate::logistics::upload::upload::UploadedFile;

const PROMPT: &str = "This image was submitted as proof of delivery for a freight shipment. \
    Is it plausibly a handwritten/drawn signature, a photo of a delivered package or goods, \
    or a photo of a signed delivery document? Answer with exactly one word: YES if it \
    plausibly is, NO if it clearly is not (e.g. a blank image, a random screenshot, a meme, \
    an unrelated scene).";

/// The Messages API's per-image limit, measured on the base64 payload.
pub const MAX_IMAGE_BASE64_BYTES: u64 = 5 * 1024 * 1024;

/// Length of the standard (padded) base64 encoding of `raw_bytes` bytes.
pub fn base64_len(raw_bytes: u64) -> u64 {
    raw_bytes.div_ceil(3) * 4
}

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
    let encoded_len = base64_len(file.byte_size.max(0) as u64);
    if encoded_len > MAX_IMAGE_BASE64_BYTES {
        return Err(format!(
            "image too large for the vision check ({} bytes, {encoded_len} as base64; \
             the API limit is {MAX_IMAGE_BASE64_BYTES} bytes of base64)",
            file.byte_size
        ));
    }
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
    fn base64_len_matches_the_encoder() {
        for n in [0usize, 1, 2, 3, 4, 5, 6, 3_932_160, 3_932_161] {
            let encoded = base64::engine::general_purpose::STANDARD.encode(vec![0u8; n]);
            assert_eq!(base64_len(n as u64), encoded.len() as u64, "n = {n}");
        }
    }

    #[actix_web::test]
    async fn check_upload_skips_an_image_too_large_for_the_api() {
        // 3.75 MiB raw is exactly 5 MiB as base64; one byte more is over.
        let file = |byte_size: i64| UploadedFile {
            id: Uuid::new_v4(),
            org_id: Uuid::new_v4(),
            content_type: "image/jpeg".to_string(),
            byte_size,
            storage_path: "/nonexistent/pod.jpg".to_string(),
            uploaded_at: 0,
        };
        assert_eq!(base64_len(3_932_160), MAX_IMAGE_BASE64_BYTES);

        // Rejected on size alone: no env lookup, no file read, no API call.
        let err = check_upload(&file(3_932_161)).await.unwrap_err();
        assert!(err.contains("too large"), "unexpected: {err}");
        let err = check_upload(&file(4 * 1024 * 1024)).await.unwrap_err();
        assert!(err.contains("too large"), "unexpected: {err}");
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
