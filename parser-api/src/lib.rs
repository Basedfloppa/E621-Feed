//! Library entry point. The `e621-account-parser-api` binary uses these
//! modules via the usual `mod` declarations in `main.rs`; auxiliary binaries
//! (currently `calibrate`) link against this crate as a library.
//!
//! Re-exports are deliberately permissive — the offline tools poke at the DB,
//! IDF, and scorer internals that the production server doesn't expose.

#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::too_many_lines,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::unnecessary_wraps,
    clippy::float_cmp,
    clippy::default_trait_access
)]

#[macro_use]
extern crate rocket;
// Route modules are shared by the binary and integration-test library surface;
// keep their existing crate-name imports valid in both compilation contexts.
extern crate self as e621_account_parser_api;

pub mod api;
pub mod audit;
pub mod auth;
pub mod cache_pruner;
pub mod crypto;
pub mod db;
pub mod errors;
pub mod jobs;
pub mod load_monitor;
pub mod media_hydrator;
pub mod metrics;
pub mod models;
pub mod pipeline;
pub mod prefetch;
pub mod prefetch_backfill;
pub mod ratelimit;
#[allow(dead_code)] // The binary mounts the full API; integration tests mount a subset.
pub mod routes;
pub mod sync;
pub mod utils;
pub mod validation;

/// Run a blocking rusqlite closure on `spawn_blocking` so the `SQLite`
/// call doesn't park the request's Tokio worker. Translates `JoinHandle`
/// panics to `Err`.
pub async fn db_blocking<F, T>(f: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    match rocket::tokio::task::spawn_blocking(f).await {
        Ok(res) => res,
        Err(e) => Err(format!("DB task panicked: {e}")),
    }
}
