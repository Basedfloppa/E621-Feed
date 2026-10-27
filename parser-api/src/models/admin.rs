//! Owner-console response types (`GET /admin/overview`, TODO §3.2).
//!
//! The admin page is a view over the *instance*, not a second authority: its
//! rows come from the same account scope `/accounts` uses, which for the
//! instance owner is every account on the instance. The endpoint itself requires
//! the instance claim (`auth::require_instance_owner`), because everything on it
//! — the media cache, the job registry — is instance state.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One account in scope, with the per-row state the console renders.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AdminAccount {
    pub id: i32,
    pub name: String,
    /// Whether an e621 API key is stored for this account (enables key-backed
    /// direct sync). The key itself is never read here — only its presence.
    pub has_key: bool,
    /// Rows in `accounts_post` for this account: its saved-post library.
    pub saved_posts: i64,
    /// Whether this browser holds a *direct* link to the account.
    ///
    /// The instance owner addresses accounts they hold no link to, and the
    /// console's unlink action removes links — so the action is offered from
    /// this flag, not from the row being visible.
    pub linked: bool,
    /// Whether this browser may *act* on the account at all.
    ///
    /// Every per-row action (sync, export, `/process`) lands on a route that
    /// re-checks `db::account_in_scope`, so the console offers them
    /// from this flag. It is wider than [`Self::linked`] for the instance owner
    /// and narrower than "the row is visible" for a mere operator.
    pub addressable: bool,
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

/// Instance-wide totals, counted over **every** account in scope rather than the
/// returned page.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct AdminTotals {
    /// Accounts this browser can address. For the instance owner that is every
    /// account on the instance.
    pub accounts: i64,
    /// Distinct posts in those accounts' libraries. Distinct, not summed: one
    /// post saved by two of your accounts is one post.
    pub saved_posts: i64,
    /// Accounts with an e621 API key stored (direct-sync capable).
    pub accounts_with_key: i64,
    /// `/process` jobs currently running for those accounts.
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
    /// Accounts on this instance. The denominator of what owning it covers.
    pub instance_accounts: i64,
    pub totals: AdminTotals,
    pub media: AdminMedia,
    /// One page of accounts in scope, newest-seen first (same order as
    /// `GET /accounts`).
    pub accounts: Vec<AdminAccount>,
}
