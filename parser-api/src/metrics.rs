//! Prometheus metrics for the e621-feed backend.
//!
//! All metrics are lazily registered on first access via the global default
//! registry. The `/api/metrics` endpoint renders them in Prometheus text
//! format for scraping.

use prometheus::{
    IntCounter, IntCounterVec, IntGauge, IntGaugeVec, register_int_counter,
    register_int_counter_vec, register_int_gauge, register_int_gauge_vec,
};
use std::sync::LazyLock;

/// Total accounts currently in the database.
pub static METRICS: LazyLock<AppMetrics> = LazyLock::new(AppMetrics::new);

pub struct AppMetrics {
    /// Current number of saved accounts (gauge — goes up/down).
    pub accounts_total: IntGauge,
    /// Total accounts created since server start.
    pub accounts_created_total: IntCounter,
    /// Total accounts deleted since server start.
    pub accounts_deleted_total: IntCounter,

    /// Feed recommendation views by `account_id`.
    pub feed_views_total: IntCounterVec,
    /// Digest views by `account_id`.
    pub digest_views_total: IntCounterVec,
    /// Browse views by source (trending / favorites).
    pub browse_views_total: IntCounterVec,

    /// /process pipeline runs (label: status = "success" | "failed").
    pub process_runs_total: IntCounterVec,

    /// Feed interactions (open / hide / impression), split by A/B bucket.
    pub feed_interactions_total: IntCounterVec,

    /// Feed interactions by event type (legacy, no bucket split).
    pub feed_interactions_by_type_total: IntCounterVec,

    /// A/B experiment arms: current account distribution (gauge).
    pub experiment_bucket_accounts: IntGaugeVec,

    /// A/B experiment arms: total feed interactions observed since start.
    pub experiment_bucket_interactions_total: IntCounterVec,

    /// Total posts in the local catalog (gauge).
    pub catalog_posts_total: IntGauge,

    /// Total HTTP requests by method and route.
    pub http_requests_total: IntCounterVec,

    /// Outbound e621 request failures by class — `429`, `5xx`, `4xx`,
    /// `timeout`, `network`, `decode`. Feeds the "High % 429/5xx"
    /// Grafana alert.
    pub upstream_errors_total: IntCounterVec,
}

impl AppMetrics {
    fn new() -> Self {
        Self {
            accounts_total: register_int_gauge!(
                "e621_accounts_total",
                "Current number of saved accounts"
            )
            .unwrap(),

            accounts_created_total: register_int_counter!(
                "e621_accounts_created_total",
                "Total accounts created since server start"
            )
            .unwrap(),

            accounts_deleted_total: register_int_counter!(
                "e621_accounts_deleted_total",
                "Total accounts deleted since server start"
            )
            .unwrap(),

            feed_views_total: register_int_counter_vec!(
                "e621_feed_views_total",
                "Feed recommendation views",
                &["account_id"]
            )
            .unwrap(),

            digest_views_total: register_int_counter_vec!(
                "e621_digest_views_total",
                "Digest views",
                &["account_id"]
            )
            .unwrap(),

            browse_views_total: register_int_counter_vec!(
                "e621_browse_views_total",
                "Browse views by source (trending / favorites)",
                &["source"]
            )
            .unwrap(),

            process_runs_total: register_int_counter_vec!(
                "e621_process_runs_total",
                "/process pipeline runs by status (success / failed)",
                &["status"]
            )
            .unwrap(),

            feed_interactions_total: register_int_counter_vec!(
                "e621_feed_interactions_total",
                "Feed interactions by bucket and type (qualified_impression / open / like / strong_like / hide)",
                &["bucket", "type"]
            )
            .unwrap(),

            feed_interactions_by_type_total: register_int_counter_vec!(
                "e621_feed_interactions_by_type_total",
                "Feed interactions by type (legacy metric, no bucket split)",
                &["type"]
            )
            .unwrap(),

            experiment_bucket_accounts: register_int_gauge_vec!(
                "e621_experiment_bucket_accounts",
                "Current number of accounts per A/B experiment bucket",
                &["bucket"]
            )
            .unwrap(),

            experiment_bucket_interactions_total: register_int_counter_vec!(
                "e621_experiment_bucket_interactions_total",
                "Total feed interactions observed since server start, by A/B bucket",
                &["bucket"]
            )
            .unwrap(),

            catalog_posts_total: register_int_gauge!(
                "e621_catalog_posts_total",
                "Total posts in local catalog"
            )
            .unwrap(),

            http_requests_total: register_int_counter_vec!(
                "e621_http_requests_total",
                "Total HTTP requests by method and route",
                &["method", "route"]
            )
            .unwrap(),

            upstream_errors_total: register_int_counter_vec!(
                "e621_upstream_errors_total",
                "Outbound e621 request failures by class (429 / 5xx / 4xx / timeout / network / decode)",
                &["class"]
            )
            .unwrap(),
        }
    }
}

/// Render all registered metrics as Prometheus text format.
#[must_use]
pub fn render() -> String {
    use prometheus::Encoder;
    let encoder = prometheus::TextEncoder::new();
    let mut buffer = Vec::new();
    encoder.encode(&prometheus::gather(), &mut buffer).unwrap();
    String::from_utf8(buffer).unwrap_or_default()
}
