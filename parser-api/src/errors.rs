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
        for code in [400u16, 401, 403, 404, 429, 500] {
            ensure_status_code_exists(&mut responses, code);
        }
        Ok(responses)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_string_promotes_missing_rows_to_not_found() {
        assert!(matches!(
            ApiError::from("no account found for id 5".to_string()),
            ApiError::NotFound(_)
        ));
        // Heuristic is case-insensitive.
        assert!(matches!(
            ApiError::from("Query Returned No Rows".to_string()),
            ApiError::NotFound(_)
        ));
        // Everything else is a 500.
        assert!(matches!(
            ApiError::from("disk on fire".to_string()),
            ApiError::Internal(_)
        ));
    }

    #[test]
    fn from_str_matches_from_string() {
        assert!(matches!(
            ApiError::from("no account found"),
            ApiError::NotFound(_)
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
}
