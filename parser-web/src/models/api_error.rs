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

/// Render transport failures without exposing browser-/proxy-specific errors
/// such as `TypeError: Failed to fetch` to the user.
pub fn humanize_network_error(_detail: impl std::fmt::Display) -> String {
    "Could not reach the server. Check your connection and try again.".to_string()
}

/// Extract a human-readable error from a non-2xx body. Falls back to a
/// concise status-specific explanation rather than proxy HTML or a raw code.
pub fn humanize_error_body(status: u16, body: &str) -> String {
    let detail = serde_json::from_str::<ApiErrorBody>(body)
        .ok()
        .map(|parsed| parsed.error.trim().to_string())
        .filter(|message| !message.is_empty());

    match status {
        429 => "Too many requests. Please wait a moment, then try again.".to_string(),
        // 503 is reserved for upstream (e621) outages by the backend — tell
        // the user it's e621's problem, not ours, so they retry later instead
        // of blaming the app/server.
        503 => {
            "E621 is temporarily unavailable right now (upstream issue). Please try again shortly."
                .to_string()
        }
        500..=502 | 504..=599 => {
            "The server is temporarily unavailable. Please try again shortly.".to_string()
        }
        _ => detail.unwrap_or_else(|| {
            let preview: String = body.trim().chars().take(200).collect();
            if preview.is_empty() {
                format!("Request failed (HTTP {status}).")
            } else {
                format!("Request failed (HTTP {status}): {preview}")
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::humanize_error_body;

    #[test]
    fn rate_limit_message_is_actionable() {
        assert_eq!(
            humanize_error_body(429, r#"{\"error\":\"rate limit exceeded\"}"#),
            "Too many requests. Please wait a moment, then try again."
        );
    }

    #[test]
    fn server_failures_do_not_expose_proxy_body() {
        assert_eq!(
            humanize_error_body(502, "<html>gateway error</html>"),
            "The server is temporarily unavailable. Please try again shortly."
        );
    }

    #[test]
    fn upstream_503_is_labelled_as_upstream() {
        // A 503 from the backend signals an e621 upstream outage — surfaced
        // as such rather than a generic server message.
        assert_eq!(
            humanize_error_body(
                503,
                r#"{"error":"returned 503 Service Unavailable","code":503}"#
            ),
            "E621 is temporarily unavailable right now (upstream issue). Please try again shortly."
        );
    }
}
