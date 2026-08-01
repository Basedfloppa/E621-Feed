//! Library entry point. The `e621-account-parser-api` binary uses these
//! modules via the usual `mod` declarations in `main.rs`; auxiliary binaries
//! (currently `calibrate`) link against this crate as a library.
//!
//! Re-exports are deliberately permissive — the offline tools poke at the DB,
//! IDF, and scorer internals that the production server doesn't expose.

#[macro_use]
extern crate rocket;
// Route modules are shared by the binary and integration-test library surface;
// keep their existing crate-name imports valid in both compilation contexts.
extern crate self as e621_account_parser_api;

pub mod api;
pub mod audit;
pub mod auth;
pub mod cache_pruner;
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
pub mod utils;
pub mod validation;

/// Run a blocking rusqlite closure on `spawn_blocking` so the SQLite
/// call doesn't park the request's Tokio worker. Translates JoinHandle
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
