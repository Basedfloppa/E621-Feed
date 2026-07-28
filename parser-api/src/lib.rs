//! Library entry point. The `e621-account-parser-api` binary uses these
//! modules via the usual `mod` declarations in `main.rs`; auxiliary binaries
//! (currently `calibrate`) link against this crate as a library.
//!
//! Re-exports are deliberately permissive — the offline tools poke at the DB,
//! IDF, and scorer internals that the production server doesn't expose.

#[macro_use]
extern crate rocket;

pub mod api;
pub mod audit;
pub mod auth;
pub mod cache_pruner;
pub mod db;
pub mod errors;
pub mod jobs;
pub mod media_hydrator;
pub mod metrics;
pub mod models;
pub mod pipeline;
pub mod prefetch;
pub mod ratelimit;
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
