//! Owner-console models (mirrors `GET /api/admin/overview`, TODO §3.2).
//!
//! The console is a view over the accounts this browser owns — the server
//! derives every row from the same `account_device_links` filter `/accounts`
//! uses — so there is nothing here that a linked account could not already
//! read. The one write is the A/B bucket pin.

use serde::Deserialize;

/// A `/process` job as this server last saw it. In-memory on the backend, so
/// it disappears after a restart — which is why the console treats a missing
/// job as "nothing has run here recently", not as a failure.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct AdminJob {
    pub phase: String,
    pub pages_total: i32,
    pub pages_done: i32,
    pub elapsed_secs: f64,
    #[serde(default)]
    pub error: Option<String>,
}

/// One owned account, as the console's table row.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct AdminAccount {
    pub id: i32,
    pub name: String,
    /// Bucket pinned in the database, if any.
    pub bucket_explicit: Option<String>,
    /// Bucket scoring actually uses. Differs from `bucket_explicit` when the
    /// pin names a bucket that is no longer configured — the case
    /// [`bucket_cell`] exists to make visible.
    pub bucket: Option<String>,
    pub has_key: bool,
    pub saved_posts: i64,
    #[serde(default)]
    pub job: Option<AdminJob>,
}

/// Totals over **all** owned accounts, not just the returned page.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct AdminTotals {
    pub accounts: i64,
    pub saved_posts: i64,
    pub accounts_with_key: i64,
    pub jobs_running: i64,
}

/// Media-cache counters. Instance-wide on the server (`queue_stats()` has no
/// owner filter), which the panel says out loud rather than implying they are
/// this browser's.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct AdminMedia {
    pub enabled: bool,
    pub paused: bool,
    pub pending: i64,
    pub stored: i64,
    pub bytes: i64,
}

/// `GET /api/admin/overview`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct AdminOverview {
    pub instance_locked: bool,
    pub adoptable_accounts: i64,
    pub totals: AdminTotals,
    /// Bucket names from `[buckets.*]`. Empty = A/B is off on this instance.
    pub buckets: Vec<String>,
    pub media: AdminMedia,
    pub accounts: Vec<AdminAccount>,
}

/// The bucket cell for one row.
///
/// Three states, and the middle one is the reason this is not a one-liner: a
/// pin that no longer names a configured bucket is *silently ignored* by
/// `pick_bucket`, so the account scores under the hash while the database
/// still says it is pinned. Showing only the pin would be a lie and showing
/// only the effective bucket would hide the stale row forever.
pub fn bucket_cell(account: &AdminAccount) -> String {
    match (
        account.bucket_explicit.as_deref(),
        account.bucket.as_deref(),
    ) {
        (Some(pinned), Some(effective)) if pinned == effective => format!("Pinned: {pinned}"),
        (Some(pinned), effective) => format!(
            "Stale pin: {pinned} → {}",
            effective.unwrap_or("no buckets configured")
        ),
        (None, Some(effective)) => format!("Hash: {effective}"),
        (None, None) => "No [buckets.*] configured".to_string(),
    }
}

/// Whether the pin shown in the cell is one the server is actually honouring.
pub fn bucket_pin_is_stale(account: &AdminAccount) -> bool {
    match (
        account.bucket_explicit.as_deref(),
        account.bucket.as_deref(),
    ) {
        (Some(pinned), effective) => effective != Some(pinned),
        (None, _) => false,
    }
}

/// Human label for a job phase, with its progress when it has any.
pub fn job_cell(job: &Option<AdminJob>) -> String {
    match job {
        None => "—".to_string(),
        Some(j) if j.phase == "running" => {
            if j.pages_total > 0 {
                format!("Running · {}/{} pages", j.pages_done, j.pages_total)
            } else {
                format!("Running · {:.0}s", j.elapsed_secs)
            }
        }
        Some(j) if j.phase == "failed" => match j.error.as_deref() {
            Some(e) => format!("Failed: {e}"),
            None => "Failed".to_string(),
        },
        Some(_) => "Done".to_string(),
    }
}

/// `/process` modes, in the order the console offers them.
pub const PROCESS_MODES: [(&str, &str); 3] = [
    ("auto", "Auto"),
    ("incremental", "Incremental"),
    ("full", "Full"),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn account(pinned: Option<&str>, effective: Option<&str>) -> AdminAccount {
        AdminAccount {
            id: 1,
            name: "a".into(),
            bucket_explicit: pinned.map(str::to_string),
            bucket: effective.map(str::to_string),
            has_key: false,
            saved_posts: 0,
            job: None,
        }
    }

    /// The three states must read differently, because they mean different
    /// things and only one of them needs the operator to act.
    #[test]
    fn the_bucket_cell_distinguishes_a_pin_from_a_stale_pin_from_a_hash() {
        assert_eq!(
            bucket_cell(&account(Some("control"), Some("control"))),
            "Pinned: control"
        );

        // Pinned to a bucket that is no longer configured: `pick_bucket` falls
        // back to the hash, so the account is NOT in the arm the DB names.
        let stale = account(Some("control"), Some("variant"));
        assert_eq!(bucket_cell(&stale), "Stale pin: control → variant");
        assert!(bucket_pin_is_stale(&stale));

        assert_eq!(
            bucket_cell(&account(None, Some("variant"))),
            "Hash: variant"
        );
        assert!(!bucket_pin_is_stale(&account(None, Some("variant"))));
        assert_eq!(
            bucket_cell(&account(None, None)),
            "No [buckets.*] configured"
        );
        assert!(!bucket_pin_is_stale(&account(
            Some("control"),
            Some("control")
        )));
    }

    #[test]
    fn the_job_cell_reports_progress_and_failure_reasons() {
        assert_eq!(job_cell(&None), "—");
        let running = AdminJob {
            phase: "running".into(),
            pages_total: 4,
            pages_done: 1,
            elapsed_secs: 12.0,
            error: None,
        };
        assert_eq!(job_cell(&Some(running)), "Running · 1/4 pages");

        let failed = AdminJob {
            phase: "failed".into(),
            pages_total: 0,
            pages_done: 0,
            elapsed_secs: 1.0,
            error: Some("e621 said 429".into()),
        };
        assert_eq!(job_cell(&Some(failed)), "Failed: e621 said 429");
    }
}
