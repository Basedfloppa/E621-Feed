//! Uniform error type for HTTP responses.
//!
//! Every API route returns `Result<T, ApiError>` so failures land as
//! `application/json` with a real 4xx/5xx status — not the Rocket default
//! that silently turns `Err(String)` into a `200 OK` plain-text body. The
//! frontend can therefore branch on `resp.ok()` and still parse JSON in
//! both arms instead of the SyntaxError path described in the audit
//! (M-1, S-3).
//!
//! `From<String>` is wired so existing call-sites that produce
//! `format!("Failed to …: {e}")` upgrade transparently when the route
//! signature changes — and a small heuristic over the message text
//! promotes "no row" misses to 404 so the frontend can distinguish
//! "doesn't exist" from "the server fell over".

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

#[derive(Debug)]
pub enum ApiError {
    BadRequest(String),
    NotFound(String),
    Forbidden(String),
    Internal(String),
}

impl ApiError {
    fn status(&self) -> Status {
        match self {
            ApiError::BadRequest(_) => Status::BadRequest,
            ApiError::NotFound(_) => Status::NotFound,
            ApiError::Forbidden(_) => Status::Forbidden,
            ApiError::Internal(_) => Status::InternalServerError,
        }
    }

    fn message(&self) -> &str {
        match self {
            ApiError::BadRequest(m)
            | ApiError::NotFound(m)
            | ApiError::Forbidden(m)
            | ApiError::Internal(m) => m,
        }
    }
}

impl From<String> for ApiError {
    /// Heuristic: existing call-sites bubble up rusqlite/db errors as
    /// strings, and "no row found" is by far the most common false-500.
    /// Match the small set of phrases used inside `db::accounts` so those
    /// land as a real 404 — anything else stays an opaque 500 since we
    /// can't tell from a string what went wrong.
    fn from(s: String) -> Self {
        let l = s.to_ascii_lowercase();
        if l.contains("no account found") || l.contains("query returned no rows") {
            ApiError::NotFound(s)
        } else {
            ApiError::Internal(s)
        }
    }
}

impl From<&str> for ApiError {
    fn from(s: &str) -> Self {
        ApiError::from(s.to_string())
    }
}

impl<'r> Responder<'r, 'static> for ApiError {
    fn respond_to(self, _: &'r Request<'_>) -> response::Result<'static> {
        let status = self.status();
        let body = ApiErrorBody {
            error: self.message().to_string(),
            code: status.code,
        };
        // serde_json::to_vec on this tiny shape can only fail in OOM
        // territory, which is not a recoverable error path; if it ever
        // does, fall back to a static byte string so the client at least
        // sees a JSON-shaped response rather than a hang.
        let bytes =
            serde_json::to_vec(&body).unwrap_or_else(|_| br#"{"error":"serialize failed","code":500}"#.to_vec());
        Response::build()
            .status(status)
            .header(ContentType::JSON)
            .sized_body(bytes.len(), Cursor::new(bytes))
            .ok()
    }
}

/// Tell rocket_okapi which status codes a route returning `ApiError`
/// can produce, so the generated OpenAPI spec is honest about 4xx/5xx
/// shapes. We don't bother attaching a body schema to each — a single
/// `ApiErrorBody` shape covers all of them and is documented separately
/// if anyone ever needs it.
impl OpenApiResponderInner for ApiError {
    fn responses(_gen: &mut OpenApiGenerator) -> rocket_okapi::Result<Responses> {
        let mut responses = Responses::default();
        for code in [400u16, 403, 404, 500] {
            ensure_status_code_exists(&mut responses, code);
        }
        Ok(responses)
    }
}
