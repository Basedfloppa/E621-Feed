//! Uniform error type for HTTP responses.
//!
//! Every API route returns `Result<T, ApiError>` so failures land as
//! `application/json` with a real 4xx/5xx status — not the Rocket default
//! that turns `Err(String)` into a `200 OK` plain-text body.
//!
//! `From<String>` upgrades existing `format!("Failed to …: {e}")`
//! call-sites transparently; a small heuristic promotes "no row" misses
//! to 404.

use std::io::Cursor;

use rocket::http::{ContentType, Status};
use rocket::request::Request;
use rocket::response::{self, Responder, Response};
use rocket::serde::Serialize;
use rocket::serde::json::serde_json;

use rocket_okapi::r#gen::OpenApiGenerator;
use rocket_okapi::okapi::openapi3::Responses;
use rocket_okapi::response::OpenApiResponderInner;
use rocket_okapi::util::ensure_status_code_exists;
use schemars::JsonSchema;

#[derive(Debug, Serialize, JsonSchema)]
#[serde(crate = "rocket::serde")]
pub struct ApiErrorBody {
    pub error: String,
    pub code: u16,
}

/// Marker embedded in e621-layer error strings to flag a failure that is the
/// *upstream's* fault (e621/Cloudflare unreachable or non-2xx) rather than our
/// own. `ApiError::from_string` strips it and maps such strings to
/// `ApiError::Upstream` (HTTP 503) so the frontend can tell the user that
/// e621 itself is unavailable instead of showing a generic 500.
pub const UPSTREAM_ERR_MARKER: &str = "e621-upstream-unavailable: ";

#[derive(Debug)]
pub enum ApiError {
    BadRequest(String),
    Unauthorized(String),
    NotFound(String),
    Forbidden(String),
    TooManyRequests(String),
    /// The e621 upstream itself failed (503/429/network/timeout) — surfaced
    /// as HTTP 503 so clients can show "upstream is down" rather than a
    /// generic server error.
    Upstream(String),
    Internal(String),
}

impl ApiError {
    fn status(&self) -> Status {
        match self {
            ApiError::BadRequest(_) => Status::BadRequest,
            ApiError::Unauthorized(_) => Status::Unauthorized,
            ApiError::NotFound(_) => Status::NotFound,
            ApiError::Forbidden(_) => Status::Forbidden,
            ApiError::TooManyRequests(_) => Status::TooManyRequests,
            ApiError::Upstream(_) => Status::ServiceUnavailable,
            ApiError::Internal(_) => Status::InternalServerError,
        }
    }

    fn message(&self) -> &str {
        match self {
            ApiError::BadRequest(m)
            | ApiError::Unauthorized(m)
            | ApiError::NotFound(m)
            | ApiError::Forbidden(m)
            | ApiError::TooManyRequests(m)
            | ApiError::Upstream(m)
            | ApiError::Internal(m) => m,
        }
    }

    /// Classify an e621-layer error string: if it carries the upstream marker
    /// (position-independent, so contextual prefixes are preserved), map it to
    /// `Upstream` (503); otherwise it is our own internal error (500).
    pub fn from_string(s: String) -> Self {
        if let Some(idx) = s.find(UPSTREAM_ERR_MARKER) {
            let rest = s[idx + UPSTREAM_ERR_MARKER.len()..].trim();
            ApiError::Upstream(rest.to_string())
        } else {
            ApiError::Internal(s)
        }
    }
}

/// Map `rusqlite::Error` directly to the appropriate `ApiError` variant.
///
/// * `QueryReturnedNoRows` → `NotFound`
/// * Everything else → `Internal` (including `MultipleRowsReturned`)
impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        match e {
            rusqlite::Error::QueryReturnedNoRows => {
                ApiError::NotFound("No matching row found".into())
            }
            other => ApiError::Internal(format!("Database error: {other}")),
        }
    }
}

impl From<String> for ApiError {
    /// All string errors become `Internal` (500), except those carrying the
    /// upstream marker, which map to `Upstream` (503). Code that needs a typed
    /// 404 should use `ApiError::NotFound(…)` directly or propagate a
    /// `rusqlite::Error::QueryReturnedNoRows` through `From<rusqlite::Error>`.
    fn from(s: String) -> Self {
        ApiError::from_string(s)
    }
}

impl From<&str> for ApiError {
    fn from(s: &str) -> Self {
        ApiError::from_string(s.to_string())
    }
}

impl<'r> Responder<'r, 'static> for ApiError {
    fn respond_to(self, _: &'r Request<'_>) -> response::Result<'static> {
        let status = self.status();
        let body = ApiErrorBody {
            error: self.message().to_string(),
            code: status.code,
        };
        let bytes = serde_json::to_vec(&body)
            .unwrap_or_else(|_| br#"{"error":"serialize failed","code":500}"#.to_vec());
        Response::build()
            .status(status)
            .header(ContentType::JSON)
            .sized_body(bytes.len(), Cursor::new(bytes))
            .ok()
    }
}

impl OpenApiResponderInner for ApiError {
    fn responses(_gen: &mut OpenApiGenerator) -> rocket_okapi::Result<Responses> {
        let mut responses = Responses::default();
        for code in [400u16, 401, 403, 404, 429, 500, 503] {
            ensure_status_code_exists(&mut responses, code);
        }
        Ok(responses)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_marker_maps_to_503() {
        use crate::errors::UPSTREAM_ERR_MARKER;
        // Marker as a bare prefix → Upstream.
        assert!(matches!(
            ApiError::from_string(format!(
                "{UPSTREAM_ERR_MARKER}returned 503 Service Unavailable: <html>"
            )),
            ApiError::Upstream(_)
        ));
        // Marker buried inside a contextual prefix is still detected.
        assert!(matches!(
            ApiError::from_string(format!(
                "Failed to fetch posts: {UPSTREAM_ERR_MARKER}returned 502 Bad Gateway"
            )),
            ApiError::Upstream(_)
        ));
        // Plain internal errors are untouched.
        assert!(matches!(
            ApiError::from_string("database error".to_string()),
            ApiError::Internal(_)
        ));
    }

    #[test]
    fn from_string_is_always_internal() {
        // After removing the text-search heuristic, all From<String>
        // errors become Internal (500). Typed 404s must use From<rusqlite::Error>.
        assert!(matches!(
            ApiError::from("no account found for id 5".to_string()),
            ApiError::Internal(_)
        ));
        assert!(matches!(
            ApiError::from("disk on fire".to_string()),
            ApiError::Internal(_)
        ));
    }

    #[test]
    fn from_str_matches_from_string() {
        // From<&str> delegates to From<String> → both are Internal now.
        assert!(matches!(
            ApiError::from("no account found"),
            ApiError::Internal(_)
        ));
        assert!(matches!(ApiError::from("boom"), ApiError::Internal(_)));
    }

    #[test]
    fn status_codes_map_to_http() {
        assert_eq!(ApiError::BadRequest(String::new()).status().code, 400);
        assert_eq!(ApiError::Unauthorized(String::new()).status().code, 401);
        assert_eq!(ApiError::Forbidden(String::new()).status().code, 403);
        assert_eq!(ApiError::NotFound(String::new()).status().code, 404);
        assert_eq!(ApiError::TooManyRequests(String::new()).status().code, 429);
        assert_eq!(ApiError::Internal(String::new()).status().code, 500);
    }

    #[test]
    fn message_returns_inner_text() {
        assert_eq!(ApiError::BadRequest("nope".to_string()).message(), "nope");
        assert_eq!(ApiError::Internal("kaboom".to_string()).message(), "kaboom");
    }

    #[test]
    fn from_rusqlite_maps_query_returned_no_rows_to_not_found() {
        assert!(matches!(
            ApiError::from(rusqlite::Error::QueryReturnedNoRows),
            ApiError::NotFound(_)
        ));
    }

    #[test]
    fn from_rusqlite_maps_other_to_internal() {
        let invalid = rusqlite::Error::SqliteSingleThreadedMode;
        assert!(matches!(ApiError::from(invalid), ApiError::Internal(_)));
    }
}
