//! Direct account sync (TODO §8.3 #5, read-only).
//!
//! When an account has a per-user e621 API key configured, the owner can
//! trigger a sync that imports/refreshes their private e621 data using THEIR
//! key (not the shared admin key):
//!
//!   * **favorites**        — the account's favorite posts (which also drive
//!     the derived profile: rating/media/quality/recency and preferred tags),
//!   * **votes**            — e621 has no separate "votes" listing; an upvote
//!     IS a favorite, so votes are covered by the favorites import,
//!   * **blacklist**        — the owner's real (private) blacklist from
//!     `users/<id>.json`, written back to their device blacklist,
//!   * **profile tags**     — derived from imported favorites by `/process`
//!     (account_tag_feedback / preferred tags).
//!
//! All e621 reads are authenticated as the owner and rate-limited per-user
//! (`e621:user:{account_id}`, distinct from the shared `e621:admin-key`
//! bucket). This module performs NO write-back: it never POSTs/PUTs/DELETEs
//! anything to e621.

use std::collections::HashSet;

use crate::db_blocking;
use crate::models::{TruncatedAccount, cfg};

/// Result of a successful direct sync run.
#[derive(Debug, Clone)]
pub struct DirectSyncSummary {
    /// Number of favorite posts persisted in this pass (new + refreshed).
    pub favorites_persisted: usize,
    /// Whether the owner's private blacklist was imported.
    pub blacklist_imported: bool,
    /// RFC 3339 timestamp of the sync.
    pub synced_at: String,
}

/// Run a direct sync for `owner_token`'s account `account_id`.
///
/// Returns `Err(NoKeyConfigured)` when the account has no e621 key set; the
/// caller maps that to a clear 400 so the frontend can prompt the user to add
/// a key first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncError {
    NoKeyConfigured,
    Other(String),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncError::NoKeyConfigured => write!(f, "no e621 API key configured for this account"),
            SyncError::Other(e) => write!(f, "{e}"),
        }
    }
}

/// Capture the computation that needs the sync state without holding the DB
/// pool across an await.
async fn resolve_key_and_account(
    owner_token: &str,
    account_id: i32,
) -> Result<(Option<String>, TruncatedAccount), String> {
    let owner_for = owner_token.to_string();
    db_blocking(move || {
        let key = crate::db::get_account_e621_key(&owner_for, account_id)?;
        let acc = crate::db::get_account_by_id(&owner_for, account_id)?;
        Ok((key, acc))
    })
    .await
}

/// Persist a page of favorite posts (reusing the pipeline's DB writer, which
/// maintains cooccurrence + tag feedback).
fn persist_favorites(
    account_id: i32,
    account: &TruncatedAccount,
    posts: Vec<crate::models::Post>,
) -> Result<usize, String> {
    let blacklist: HashSet<String> = account
        .blacklist
        .split_whitespace()
        .map(std::string::ToString::to_string)
        .collect();
    crate::db::save_posts(&posts, account_id).map_err(|e| format!("Failed to save posts: {e}"))?;
    crate::db::save_posts_tags_batch(&posts, &blacklist, true, Some(account_id))
        .map_err(|e| format!("Failed to update tag feedback: {e}"))?;
    Ok(posts.len())
}

/// Sync the account's private e621 data, authenticating as the account owner.
///
/// The key is ACCOUNT-scoped (shared across linked devices). For the
/// `admin_user` account the shared admin_api is used directly (no stored
/// per-account key needed); every other account uses its stored e621 key.
pub async fn sync_account_direct(
    owner_token: &str,
    account_id: i32,
) -> Result<DirectSyncSummary, SyncError> {
    let (stored_key, account) = resolve_key_and_account(owner_token, account_id)
        .await
        .map_err(SyncError::Other)?;
    let key = if account.name.eq_ignore_ascii_case(&cfg().admin_user) {
        Some(cfg().admin_api.clone())
    } else {
        stored_key
    };
    let Some(key) = key else {
        return Err(SyncError::NoKeyConfigured);
    };

    // Fetch the owner profile: favorite_count + their real private blacklist.
    let user = crate::api::get_user_with_key(account_id, &account.name, &key)
        .await
        .map_err(SyncError::Other)?;

    // Import favorites. e621 pages favourites in stable reverse-chronological
    // order; sync pulls the first pages (most recent) which is what the
    // personalised feed actually needs. Favourites collection is the
    // `save_favourites` (or `save_all`) scope: with both off the sync still
    // imports the owner's blacklist below but persists no favourites.
    let mut favorites_persisted = 0usize;
    if cfg().catalog.persistence_enabled() {
        let max_pages = ((user.favorite_count as i32 / cfg().posts_limit) + 1).clamp(1, 200);
        for page in 1..=max_pages {
            let posts = crate::api::get_favorites_with_key(&account, &key, page)
                .await
                .map_err(|e| SyncError::Other(format!("favorites page {page}: {e}")))?;
            if posts.is_empty() {
                break;
            }
            let n = persist_favorites(account_id, &account, posts).map_err(SyncError::Other)?;
            favorites_persisted += n;
        }
    }

    // Import the owner's real blacklist (private, only visible with their key).
    // e621 exposes it as `blacklisted_tags`: a single `\n`-joined string of
    // filter lines (e.g. `gore\nscatplay\nyoung -rating:s`). Preserve the
    // lines as-is so multi-tag filters like `young -rating:s` stay intact.
    let blacklist_imported = match user.blacklisted_tags.as_deref() {
        Some(raw) if !raw.trim().is_empty() => {
            let joined = raw.trim().to_string();
            let owner_for = owner_token.to_string();
            db_blocking(move || {
                crate::db::update_device_blacklist(&owner_for, account_id, &joined)
            })
            .await
            .map_err(SyncError::Other)?;
            true
        }
        _ => false,
    };

    // Record the sync timestamp.
    let owner_for = owner_token.to_string();
    db_blocking(move || crate::db::mark_account_direct_synced(&owner_for, account_id))
        .await
        .map_err(SyncError::Other)?;
    crate::db::mark_e621_key_verified(owner_token, account_id).map_err(SyncError::Other)?;

    let synced_at = chrono::Utc::now().to_rfc3339();
    Ok(DirectSyncSummary {
        favorites_persisted,
        blacklist_imported,
        synced_at,
    })
}
