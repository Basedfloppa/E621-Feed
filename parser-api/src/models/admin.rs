//! Owner-console response types (`GET /admin/overview`, TODO §3.2).
//!
//! The admin page is a *view over the accounts this browser already owns*, not
//! a second authority: every account row here comes from the same
//! `account_device_links` filter `/accounts` uses, so the endpoint can only
//! show what the owner token could already read. The genuinely new capability
//! is the bucket assignment — `PUT /account/<id>/experiment_bucket` — which is
//! what turns the previously read-only `[buckets.*]` machinery into something
//! an owner can actually run an experiment with.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One owned account, with the per-row state the console renders.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AdminAccount {
    pub id: i32,
    pub name: String,
    /// Bucket pinned to this account in the DB, if any. Kept even when it no
    /// longer names a configured bucket, so the row can show the stale pin
    /// instead of silently pretending the account is unassigned.
    pub bucket_explicit: Option<String>,
    /// Bucket scoring actually uses: the pinned one when it names a configured
    /// bucket, otherwise the deterministic hash of `account_id` (see
    /// `Config::pick_bucket`). `None` when `[buckets.*]` is empty.
    pub bucket: Option<String>,
    /// Whether an e621 API key is stored for this account (enables key-backed
    /// direct sync). The key itself is never read here — only its presence.
    pub has_key: bool,
    /// Rows in `accounts_post` for this account: its saved-post library.
    pub saved_posts: i64,
    /// The in-process `/process` job for this account, when this server knows
    /// one. Absent after a restart — job state is deliberately in-memory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<AdminJob>,
}

/// Compact snapshot of a `/process` job for the console's status cell.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AdminJob {
    /// `running`, `done` or `failed` (mirrors `ProcessJobPhase`).
    pub phase: String,
    pub pages_total: i32,
    pub pages_done: i32,
    pub elapsed_secs: f64,
    pub error: Option<String>,
}

/// Instance-wide totals, counted over **every** owned account rather than the
/// returned page.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct AdminTotals {
    /// Accounts linked to this owner token.
    pub accounts: i64,
    /// Distinct posts in those accounts' libraries. Distinct, not summed: one
    /// post saved by two of your accounts is one post.
    pub saved_posts: i64,
    /// Accounts with an e621 API key stored (direct-sync capable).
    pub accounts_with_key: i64,
    /// `/process` jobs currently running for this owner's accounts.
    pub jobs_running: i64,
}

/// Media-cache state for the console's metrics panel.
///
/// ⚠️ `pending`/`stored`/`bytes` are **instance-wide** (`queue_stats()` has no
/// owner filter — TODO §4.5, «Тенант-скоупинг каталога»), while `enabled`
/// comes from `[catalog]`. On the primary single-owner deployment those are the
/// same thing; on a multi-owner instance the panel says so explicitly.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AdminMedia {
    /// `[catalog] save_favourites || save_all`
    /// (`Config::media_cache_enabled`): whether the worker downloads originals.
    pub enabled: bool,
    pub paused: bool,
    pub pending: i64,
    pub stored: i64,
    pub bytes: i64,
}

/// Response of `GET /admin/overview`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AdminOverview {
    /// `[instance].owner_secret` is set, so claiming requires proof.
    pub instance_locked: bool,
    /// Accounts this claim would link, before the secret is proved.
    pub adoptable_accounts: i64,
    pub totals: AdminTotals,
    /// Bucket names configured in `[buckets.*]`, sorted. Empty = A/B is off,
    /// and no bucket can be pinned.
    pub buckets: Vec<String>,
    pub media: AdminMedia,
    /// One page of owned accounts, newest-seen first (same order as
    /// `GET /accounts`).
    pub accounts: Vec<AdminAccount>,
}

/// Body of `PUT /account/<id>/experiment_bucket`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct BucketAssignment {
    /// Configured bucket name to pin. Validation happens in the route, because
    /// an unknown name is stored happily and then ignored by `pick_bucket` —
    /// a silent no-op is worse than a 400.
    pub bucket: Option<String>,
}

/// Result of pinning or clearing a bucket: the account's bucket state after
/// the write, so the console can update the row without refetching the page.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BucketState {
    pub bucket_explicit: Option<String>,
    /// Effective bucket after the write (`pick_bucket`).
    pub bucket: Option<String>,
}
