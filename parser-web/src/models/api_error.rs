//! Mirror of the backend `ApiErrorBody` so the frontend renders the
//! server's `error` field directly. Centralised so each call site uses
//! the same parser.

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ApiErrorBody {
    pub error: String,
    /// Mirrored from the backend envelope; currently unused on render side.
    #[allow(dead_code)]
    #[serde(default)]
    pub code: u16,
}

/// Extract a human-readable error from a non-2xx body. Falls back to
/// a clipped raw body for stray nginx/Cloudflare HTML pages.
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
