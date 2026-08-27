//! Thin wrappers around `reqwasm::http::Request` that always send
//! cookies. `RequestCredentials::Include` is required in `trunk serve`
//! (SPA :8000, API :8181 are different origins); redundant behind nginx
//! but keeping the wrapper prevents new endpoints from forgetting it.

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

pub fn api_put(url: &str) -> Request {
    Request::put(url).credentials(RequestCredentials::Include)
}
