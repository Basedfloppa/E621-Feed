//! HTTP route handlers for the API. Bound to Rocket via the
//! `openapi_get_routes_spec!` macro in `main.rs`.

pub(crate) mod account;
pub(crate) mod browse;
pub(crate) mod digest;
pub(crate) mod feed;

use rocket::get;

/// Prometheus metrics endpoint — returns all registered metrics in text format.
#[get("/metrics")]
pub(crate) fn get_metrics() -> String {
    // Force-init the lazy metrics so the default registry is populated.
    let _ = &*e621_account_parser_api::metrics::METRICS;
    e621_account_parser_api::metrics::render()
}
