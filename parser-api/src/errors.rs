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

#[derive(Debug)]
pub enum ApiError {
    BadRequest(String),
    Unauthorized(String),
    NotFound(String),
    Forbidden(String),
    TooManyRequests(String),
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
            | ApiError::Internal(m) => m,
        }
    }
}

impl From<String> for ApiError {
    /// Promote "no row found" rusqlite errors to 404; anything else stays 500.
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
        let bytes =
            serde_json::to_vec(&body).unwrap_or_else(|_| br#"{"error":"serialize failed","code":500}"#.to_vec());
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
        for code in [400u16, 401, 403, 404, 429, 500] {
            ensure_status_code_exists(&mut responses, code);
        }
        Ok(responses)
    }
}
