//! Owner-console models (mirrors `GET /api/admin/overview`, TODO §3.2).
//!
//! The console is a view over the *instance*: the endpoint requires the instance
//! claim, and every row comes from the same account scope `/accounts` uses —
//! which for the instance owner is every account on the instance.

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

/// One account in scope, as the console's table row.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct AdminAccount {
    pub id: i32,
    pub name: String,
    pub has_key: bool,
    pub saved_posts: i64,
    /// Whether this browser holds a *direct* link to the account. The instance
    /// owner addresses accounts they hold no link to, and unlinking removes a
    /// link — so the row's action is offered from this flag, not from the row
    /// being visible.
    #[serde(default)]
    pub linked: bool,
    /// Whether this browser may *act* on the account: sync it, export it, start
    /// a `/process` run. Wider than [`Self::linked`] for the
    /// instance owner, which addresses accounts it holds no link to — and
    /// narrower than "the row is visible", because an operator's console covers
    /// the instance without handing over the libraries on it.
    #[serde(default)]
    pub addressable: bool,
    #[serde(default)]
    pub job: Option<AdminJob>,
}

/// Totals over **all** accounts in scope, not just the returned page.
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
    /// Accounts on this instance — what holding the claim covers.
    pub instance_accounts: i64,
    pub totals: AdminTotals,
    pub media: AdminMedia,
    pub accounts: Vec<AdminAccount>,
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
