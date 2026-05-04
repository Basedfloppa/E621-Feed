//! Thin wrappers around `reqwasm::http::Request` so every API call
//! sends cookies (audit #3 — owner-token now lives in an HttpOnly
//! cookie set by the server).
//!
//! `RequestCredentials::Include` is required because in `trunk serve`
//! the SPA (port 8000) and the API (port 8080) are different origins,
//! so the browser would otherwise drop the cookie. In production both
//! sit behind the same nginx so it's redundant — but a single shared
//! call site means we can't accidentally forget it on a new endpoint.

use reqwasm::http::Request;
use web_sys::RequestCredentials;

pub fn api_get(url: &str) -> Request {
    Request::get(url).credentials(RequestCredentials::Include)
}

pub fn api_post(url: &str) -> Request {
    Request::post(url).credentials(RequestCredentials::Include)
}

pub fn api_patch(url: &str) -> Request {
    Request::patch(url).credentials(RequestCredentials::Include)
}

pub fn api_delete(url: &str) -> Request {
    Request::delete(url).credentials(RequestCredentials::Include)
}
