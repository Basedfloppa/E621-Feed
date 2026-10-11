//! Gauges whose value is a count over a table, kept honest by re-reading.
//!
//! None of these can be maintained incrementally, and each has already been
//! observed wrong in production or is wrong by construction:
//!
//! * `e621_accounts_total` / `e621_experiment_bucket_accounts` — the orphan
//!   cleanup deletes rows from SQL without passing through the delete handler,
//!   and it cannot know which bucket a deleted row was in, so it can decrement
//!   neither. Fed only by the handlers, the total reports "net change since
//!   process start"; on the production database it read **99 while the table
//!   held 91**.
//! * `e621_active_accounts` — a count over `user_visit_tracker` inside a
//!   rolling window, which no write path can adjust.
//! * `e621_catalog_posts_total` — posts are UPSERTed, so no write path can tell
//!   a new row from a rewrite.
//!
//! Re-reading on a timer makes that drift self-correcting instead of permanent
//! until the next restart, and a bucket that disappears converges to zero
//! rather than staying at its last value forever.

use std::collections::HashMap;
use std::time::Duration;

use prometheus::core::Collector as _;

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

/// Buckets that are still exported but hold no account any more.
///
/// These have to be set to `0` explicitly. Writing only the fresh counts leaves
/// a vanished bucket's last value exported forever, so the distribution keeps
/// summing to more than `e621_accounts_total` — exactly the 99-vs-91 drift the
/// production instance showed, where orphan cleanup removed rows that no
/// incremental path could subtract.
fn buckets_to_zero(exported: &[String], fresh: &HashMap<String, u64>) -> Vec<String> {
    let mut stale: Vec<String> = exported
        .iter()
        .filter(|bucket| !fresh.contains_key(bucket.as_str()))
        .cloned()
        .collect();
    stale.sort();
    stale.dedup();
    stale
}

/// Re-read every table-derived gauge.
///
/// A failed read leaves the previous value in place and logs a warning: the
/// scrape endpoint must never be the thing that takes the process down.
pub fn refresh() {
    match db::count_accounts() {
        Ok(n) => METRICS.accounts_total.set(n),
        Err(e) => warn!("[table-gauges] account count failed: {e}"),
    }

    match db::count_accounts_by_bucket() {
        Ok(counts) => {
            // Zero every bucket that no longer holds an account. Writing only
            // the fresh counts would leave the last value of a vanished bucket
            // exported forever, which is how the distribution and the total
            // drifted apart on the production instance.
            let exported: Vec<String> = METRICS
                .experiment_bucket_accounts
                .collect()
                .iter()
                .flat_map(|family| family.get_metric())
                .flat_map(|metric| metric.get_label())
                .filter(|l| l.name() == "bucket")
                .map(|l| l.value().to_string())
                .collect();
            for bucket in buckets_to_zero(&exported, &counts) {
                METRICS
                    .experiment_bucket_accounts
                    .with_label_values(&[&bucket])
                    .set(0);
            }
            for (bucket, count) in counts {
                METRICS
                    .experiment_bucket_accounts
                    .with_label_values(&[&bucket])
                    .set(count as i64);
            }
        }
        Err(e) => warn!("[table-gauges] bucket distribution failed: {e}"),
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
    #[test]
    fn refresh_does_not_panic() {
        refresh();
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

    /// The drift fix, stated as a test: a bucket whose accounts are gone must be
    /// explicitly zeroed, and a bucket that still holds accounts must not be.
    ///
    /// This is the arithmetic behind `e621_accounts_total` reading 99 while the
    /// table held 91 — orphan cleanup deletes rows without being able to
    /// decrement the bucket they were counted in.
    #[test]
    fn buckets_that_disappeared_are_zeroed_not_left_stale() {
        let fresh = HashMap::from([("control".to_string(), 4u64), ("none".to_string(), 9)]);
        let exported = vec![
            "control".to_string(),
            "explore".to_string(),
            "none".to_string(),
        ];

        assert_eq!(
            buckets_to_zero(&exported, &fresh),
            vec!["explore".to_string()],
            "only the vanished bucket is zeroed"
        );

        assert!(
            buckets_to_zero(&["control".to_string(), "none".to_string()], &fresh).is_empty(),
            "nothing vanished, so nothing is touched"
        );

        assert_eq!(
            buckets_to_zero(&exported, &HashMap::new()).len(),
            3,
            "with no accounts left every bucket must fall to zero"
        );

        // A repeated label must not be written twice.
        assert_eq!(
            buckets_to_zero(&["explore".to_string(), "explore".to_string()], &fresh),
            vec!["explore".to_string()]
        );
    }
}
