//! Library entry point. The `e621-account-parser-api` binary uses these
//! modules via the usual `mod` declarations in `main.rs`; auxiliary binaries
//! (currently `calibrate`) link against this crate as a library.
//!
//! Re-exports are deliberately permissive — the offline tools poke at the DB,
//! IDF, and scorer internals that the production server doesn't expose.

#[macro_use]
extern crate rocket;

pub mod api;
pub mod db;
pub mod jobs;
pub mod models;
pub mod prefetch;
pub mod utils;
