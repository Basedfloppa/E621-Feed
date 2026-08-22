//! HTTP route handlers for the API. Bound to Rocket via the
//! `openapi_get_routes_spec!` macro in `main.rs`.

pub(crate) mod account;
pub(crate) mod browse;
pub(crate) mod digest;
pub(crate) mod feed;
pub(crate) mod post;
pub(crate) mod tag_relations;
pub(crate) mod taste_profile;

use rocket::get;

/// Prometheus metrics endpoint — returns all registered metrics in text format.
#[get("/metrics")]
pub(crate) fn get_metrics() -> String {
    // Force-init the lazy metrics so the default registry is populated.
    let _ = &*e621_account_parser_api::metrics::METRICS;
    e621_account_parser_api::metrics::render()
}

/// Minimal authenticated read-route set for integration tests.
#[doc(hidden)]
#[allow(dead_code)] // Mounted from the integration-test binary, not production main.
#[must_use]
pub fn integration_test_routes() -> Vec<rocket::Route> {
    rocket::routes![
        account::list_accounts,
        account::get_account_tag_counts,
        account::get_account_profile,
        account::get_session_devices,
        account::revoke_device_session,
        account::set_account_key,
        account::test_account_key,
        account::delete_account_key,
        account::get_account_key_state,
        account::create_account,
        account::sync_account_route,
        account::get_sync_status,
        account::export_account,
        account::import_account,
        feed::get_recommendations,
        feed::get_account_interactions,
        digest::get_daily_digest,
        browse::get_trending,
        browse::get_trending_scored,
        browse::get_favorites,
    ]
}
