//! Gauges whose value is a count over a table, kept honest by re-reading.
//!
//! None of these can be maintained incrementally, and each has already been
//! observed wrong in production or is wrong by construction:
//!
//! * `e621_accounts_total` — the orphan cleanup deletes rows from SQL without
//!   passing through the delete handler, so it can decrement nothing. Fed only
//!   by the handlers, the total reports "net change since process start"; on
//!   the production database it read **99 while the table held 91**.
//! * `e621_active_accounts` — a count over `user_visit_tracker` inside a
//!   rolling window, which no write path can adjust.
//! * `e621_catalog_posts_total` — posts are UPSERTed, so no write path can tell
//!   a new row from a rewrite.
//!
//! Re-reading on a timer makes that drift self-correcting instead of permanent
//! until the next restart.

use std::time::Duration;

use crate::db;
use crate::metrics::METRICS;

/// Windows reported by `e621_active_accounts`, as (label, days).
///
/// `1d` reads as "today or yesterday": `user_visit_tracker.last_visit_date` is
/// a bare date, so a sub-day window cannot be expressed from this table.
pub const ACTIVE_WINDOWS: [(&str, i64); 3] = [("1d", 1), ("7d", 7), ("30d", 30)];

/// How often [`refresh`] re-reads the tables.
///
/// Deliberately far shorter than the cache pruner's `cleanup_interval_secs`
/// (default 6 h, and that loop is the only place the gauges were refreshed
/// before): `e621_active_accounts{window="1d"}` is a one-day count, and a value
/// six hours stale shows no daily pattern at all. Every query is a `COUNT(*)`
/// over a small table, so the tighter interval is cheap.
const REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// Re-read every table-derived gauge.
///
/// A failed read leaves the previous value in place and logs a warning: the
/// scrape endpoint must never be the thing that takes the process down.
pub fn refresh() {
    match db::count_accounts() {
        Ok(n) => METRICS.accounts_total.set(n),
        Err(e) => warn!("[table-gauges] account count failed: {e}"),
    }

    for (window, days) in ACTIVE_WINDOWS {
        match db::count_accounts_active_within(days) {
            Ok(n) => METRICS.active_accounts.with_label_values(&[window]).set(n),
            Err(e) => warn!("[table-gauges] active accounts ({window}) failed: {e}"),
        }
    }

    match db::try_post_count() {
        Ok(n) => METRICS.catalog_posts_total.set(n),
        Err(e) => warn!("[table-gauges] catalog post count failed: {e}"),
    }
}

/// Re-read the gauges once a minute on a dedicated thread.
///
/// Runs [`refresh`] before the first sleep, so this doubles as the startup seed
/// and no caller needs a separate initial pass.
pub fn spawn_refresher() {
    std::thread::Builder::new()
        .name("table-gauges".to_string())
        .spawn(move || {
            loop {
                refresh();
                std::thread::sleep(REFRESH_INTERVAL);
            }
        })
        .expect("spawn table-gauges thread");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `refresh` must survive a database that cannot be opened, because it runs
    /// unconditionally on a thread: an early `?` here would kill the refresher
    /// and leave every gauge frozen at its startup value with no further
    /// signal. The assertion is that it returns instead of panicking.
    ///
    /// The config is swapped for one pointing at an empty temp database and
    /// restored on the way out. Without that, the call followed `db_path` from
    /// the loaded config: on a developer machine that is their catalog, and on
    /// a fresh checkout `cargo test` left a stray `data/database.db` behind.
    /// The gauges only read, but the file is still opened and created, and the
    /// side effect was invisible until someone wondered where it came from.
    #[test]
    fn refresh_does_not_panic() {
        use crate::models::{CONFIG, Config, cfg};
        use std::sync::Arc;

        struct Restore(Arc<Config>);
        impl Drop for Restore {
            fn drop(&mut self) {
                CONFIG.store(self.0.clone());
            }
        }

        let root = std::env::temp_dir().join(format!("e621-refresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let saved = cfg();
        let mut isolated = (*saved).clone();
        isolated.db_path = root.join("empty.db").to_string_lossy().into_owned();

        let _restore = Restore(saved);
        CONFIG.store(Arc::new(isolated));

        // No migrations ran on the empty file, so every count fails; `refresh`
        // has to log each failure and return rather than propagate it.
        refresh();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The windows the dashboard reads must exist under exactly these labels:
    /// `e621_active_accounts{window="1d"}` and so on. Renaming one silently
    /// empties a panel, which is the failure mode this change is fixing.
    #[test]
    fn active_windows_match_the_documented_labels() {
        assert_eq!(
            ACTIVE_WINDOWS,
            [("1d", 1), ("7d", 7), ("30d", 30)],
            "panel queries key off these labels"
        );
    }
}
