//! HTTP route handlers for the API. Bound to Rocket via the
//! `openapi_get_routes_spec!` macro in `main.rs`.

pub(crate) mod account;
pub(crate) mod digest;
pub(crate) mod feed;
