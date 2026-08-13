//! Prometheus metrics for the e621-feed backend.
//!
//! All metrics are lazily registered on first access via the global default
//! registry. The `/api/metrics` endpoint renders them in Prometheus text
//! format for scraping.

use prometheus::{
    HistogramVec, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, register_histogram_vec,
    register_int_counter, register_int_counter_vec, register_int_gauge, register_int_gauge_vec,
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

    /// Feed recommendation views (global counter — deliberately NOT labelled
    /// by `account_id`: that label leaked which e621 accounts are active to any
    /// unauthenticated scraper of `/api/metrics`, and let a remote client grow
    /// the registry without bound. It was removed for both reasons.).
    pub feed_views_total: IntCounter,
    /// Digest views (global counter — same rationale as feed_views_total).
    pub digest_views_total: IntCounter,
    /// Browse views by source (trending / favorites).
    pub browse_views_total: IntCounterVec,

    /// /process pipeline runs (label: status = "success" | "failed").
    pub process_runs_total: IntCounterVec,

    /// Age in seconds of the currently-running /process job (0 when idle).
    /// Feeds the "/process wall time > 30 min" alert.
    pub process_running_seconds: IntGauge,

    /// Wall-clock seconds the most recently finished /process job took.
    pub process_last_duration_seconds: IntGauge,

    /// Times the catalog-prefetch circuit breaker opened (e621 failures
    /// exceeded `prefetch_breaker_threshold`).
    pub prefetch_breaker_trips_total: IntCounter,

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

    /// Whole-request wall-clock duration (from receipt to response) by method
    /// and route. Covers the full lifetime: outbound e621 calls + local
    /// scoring + serialization. Always-on; pair it with
    /// `e621_upstream_request_seconds` to separate e621 wait time from local
    /// processing. This is the metric that makes "the response takes too
    /// long" quantifiable per route.
    pub http_request_duration_seconds: HistogramVec,

    /// Outbound e621 request failures by class — `429`, `5xx`, `4xx`,
    /// `timeout`, `network`, `decode`. Feeds the "High % 429/5xx"
    /// Grafana alert.
    pub upstream_errors_total: IntCounterVec,

    /// Latency of each outbound e621 HTTP request attempt (wall-clock, from
    /// start of the attempt to response / error / hard-timeout). Labelled by
    /// outcome class: `success` / `429` / `5xx` / `4xx` / `timeout` /
    /// `network` / `decode`. Always-on (not perf_metrics-gated): feeds
    /// latency percentiles in Grafana and directly diagnoses "the response
    /// takes too long" complaints (time spent waiting on e621 vs locally).
    pub upstream_request_seconds: HistogramVec,

    /// Cache entries dropped by the cache-pruner worker, by category.
    /// Categories: api_ttl, api_idle, jobs, ratelimit, candidates, revoked,
    /// idf, relation.
    pub cache_pruned_total: IntCounterVec,
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

            feed_views_total: register_int_counter!(
                "e621_feed_views_total",
                "Feed recommendation views"
            )
            .unwrap(),

            digest_views_total: register_int_counter!(
                "e621_digest_views_total",
                "Digest views"
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

            process_running_seconds: register_int_gauge!(
                "e621_process_running_seconds",
                "Age in seconds of the currently running /process job (0 when idle)"
            )
            .unwrap(),

            process_last_duration_seconds: register_int_gauge!(
                "e621_process_last_duration_seconds",
                "Wall-clock seconds of the most recently finished /process job"
            )
            .unwrap(),

            prefetch_breaker_trips_total: register_int_counter!(
                "e621_prefetch_breaker_trips_total",
                "Times the catalog-prefetch circuit breaker opened"
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

            http_request_duration_seconds: register_histogram_vec!(
                "e621_http_request_duration_seconds",
                "Wall-clock seconds of a whole HTTP request (receipt to response) by method and route",
                &["method", "route"],
                vec![
                    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0,
                    10.0, 30.0, 60.0, 120.0, 300.0,
                ]
            )
            .unwrap(),

            upstream_errors_total: register_int_counter_vec!(
                "e621_upstream_errors_total",
                "Outbound e621 request failures by class (429 / 5xx / 4xx / timeout / network / decode)",
                &["class"]
            )
            .unwrap(),

            upstream_request_seconds: register_histogram_vec!(
                "e621_upstream_request_seconds",
                "Wall-clock seconds of an outbound e621 HTTP request attempt, by outcome class",
                &["class"],
                vec![
                    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0,
                    10.0, 30.0, 60.0, 120.0, 300.0,
                ]
            )
            .unwrap(),

            cache_pruned_total: register_int_counter_vec!(
                "e621_cache_pruned_total",
                "Cache entries dropped by the cache-pruner, by category",
                &["category"]
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Touching the new fields forces LazyLock init (which panics on a
    /// duplicate metric registration) and asserts the names are exported so
    /// dashboards / alerts keep working.
    #[test]
    fn registers_new_process_and_breaker_metrics() {
        let m = &METRICS;
        m.process_running_seconds.set(0);
        m.process_last_duration_seconds.set(0);
        m.prefetch_breaker_trips_total.inc();
        m.upstream_request_seconds
            .with_label_values(&["success"])
            .observe(0.001);
        m.http_request_duration_seconds
            .with_label_values(&["GET", "/api/health"])
            .observe(0.01);
        let out = render();
        assert!(out.contains("e621_process_running_seconds"));
        assert!(out.contains("e621_process_last_duration_seconds"));
        assert!(out.contains("e621_prefetch_breaker_trips_total"));
        assert!(out.contains("e621_upstream_request_seconds"));
        assert!(out.contains("e621_http_request_duration_seconds"));
    }
}
