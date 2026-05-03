//! Mirror of the backend `ApiErrorBody` shape so the frontend can
//! pull the `error` field out of a 4xx/5xx response and render it
//! verbatim, instead of embedding the raw body or showing a JSON
//! parse error to the user (M-1, S-3 in the audit).
//!
//! Kept in this small module rather than next to each call site so
//! the parser logic doesn't drift across components.

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ApiErrorBody {
    pub error: String,
    /// Mirrored from the backend envelope — currently unused on the
    /// rendering side, kept so devtools / future "show error code"
    /// affordances don't have to change the schema.
    #[allow(dead_code)]
    #[serde(default)]
    pub code: u16,
}

/// Try to extract a human-readable error message from a non-2xx
/// response body. Falls back to the raw body when the server returned
/// something other than the expected JSON envelope (e.g. a stray
/// nginx/Cloudflare HTML page slipping through). Trims and clips so
/// `alert` blobs don't span half the screen.
pub fn humanize_error_body(status: u16, body: &str) -> String {
    if let Ok(parsed) = serde_json::from_str::<ApiErrorBody>(body) {
        let msg = parsed.error.trim();
        if !msg.is_empty() {
            return msg.to_string();
        }
    }
    let trimmed = body.trim();
    let preview: String = trimmed.chars().take(200).collect();
    if preview.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {preview}")
    }
}
