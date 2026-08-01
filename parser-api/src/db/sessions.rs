//! Persistent owner-token revocation list + feed session management.
//!
//! Revocation: load all (startup / post-prune), insert one (logout), prune old rows.
//! Sessions: create/update/touch/prune for feed continuation.

use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use std::collections::HashSet;

use crate::db::{open_db, with_write_tx};

/// Feed-session sliding TTL in minutes. Centralised so the touch path
/// (in `touch_or_create_feed_session`) and the prune path (in
/// `prune_expired_sessions`) can't drift apart silently.
pub const FEED_SESSION_TTL_MIN: i64 = 30;

/// Outcome of `touch_or_create_feed_session`. Drives whether the caller
/// should load the dedup set, record new shown posts, and whether to
/// signal `fresh_start` to the client (which prompts the frontend to
/// rotate the `session_id`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedSessionState {
    /// Session row didn't exist; we just created it. No dedup history yet,
    /// but the session is now live — record shown posts as usual.
    Fresh,
    /// Session existed and was still within TTL; we touched
    /// `last_accessed_at`. Load the dedup set and continue normally.
    Active,
    /// Session row existed but `last_accessed_at` was older than TTL. We
    /// did NOT touch — the client should rotate the session token. Caller
    /// should return `fresh_start=true` and skip recording.
    Expired,
}

// ── Revocation ──────────────────────────────────────────────────────

pub fn revoke_token_in_db(token: &str) -> Result<(), String> {
    let now = Utc::now().timestamp();
    with_write_tx(|tx| {
        tx.execute(
            "INSERT OR IGNORE INTO revoked_tokens (token, revoked_at) VALUES (?, ?)",
            rusqlite::params![token, now],
        )
        .map_err(|e| format!("Failed to insert revoked token: {e}"))?;
        Ok(())
    })
}

pub fn load_all_revoked_tokens() -> Result<Vec<String>, String> {
    let conn = open_db()?;
    let mut stmt = conn
        .prepare("SELECT token FROM revoked_tokens")
        .map_err(|e| format!("Failed to prepare revoked-tokens load: {e}"))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| format!("Failed to query revoked tokens: {e}"))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| format!("Failed to read revoked token row: {e}"))?);
    }
    Ok(out)
}

/// Drop entries older than `retention_secs`. Returns `(before, after)`.
pub fn prune_revoked_tokens(retention_secs: i64) -> Result<(usize, usize), String> {
    let cutoff = Utc::now().timestamp() - retention_secs;
    with_write_tx(|tx| {
        let before: i64 = tx
            .query_row("SELECT COUNT(*) FROM revoked_tokens", [], |r| r.get(0))
            .map_err(|e| format!("Failed to count revoked tokens: {e}"))?;
        let removed = tx
            .execute(
                "DELETE FROM revoked_tokens WHERE revoked_at < ?",
                rusqlite::params![cutoff],
            )
            .map_err(|e| format!("Failed to prune revoked tokens: {e}"))?;
        Ok((before as usize, before as usize - removed))
    })
}

// ── Feed Sessions ───────────────────────────────────────────────────

/// Atomic check-or-create-or-touch for a feed session.
///
/// Replaces the previous `upsert_feed_session` + `validate_feed_session`
/// pair, which had two latent bugs:
///   1. Caller-side, `validate` always succeeded because the preceding
///      `upsert` had just set `last_accessed_at = now` — the expiry
///      branch was unreachable and `fresh_start` was dead code.
///   2. `validate` read on a pool connection and touched on a separate
///      writer connection, leaving a TOCTOU window where another
///      connection could prune the session between the two.
///
/// This function does the read, the expiry check, and the
/// create/touch in a single write transaction so the three operations
/// either all happen or none do — and the caller learns the actual
/// state (`Fresh` / `Active` / `Expired`) rather than a binary OK.
pub fn touch_or_create_feed_session(
    session_id: &str,
    account_id: i32,
) -> Result<FeedSessionState, String> {
    with_write_tx(|tx| {
        let existing: Option<String> = tx
            .query_row(
                "SELECT last_accessed_at FROM feed_sessions \
                 WHERE session_id = ?1 AND account_id = ?2",
                params![session_id, account_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| format!("Failed to read feed session: {e}"))?;

        match existing {
            None => {
                let now = Utc::now().to_rfc3339();
                tx.execute(
                    "INSERT INTO feed_sessions \
                     (session_id, account_id, created_at, last_accessed_at) \
                     VALUES (?1, ?2, ?3, ?3) \
                     ON CONFLICT(session_id, account_id) DO NOTHING",
                    params![session_id, account_id, now],
                )
                .map_err(|e| format!("Failed to create feed session: {e}"))?;
                Ok(FeedSessionState::Fresh)
            }
            Some(last_raw) => {
                let last = crate::db::parse_db_datetime(&last_raw)
                    .map_err(|e| format!("Failed to parse session timestamp: {e}"))?;
                let elapsed_min = (Utc::now() - last).num_minutes();
                if elapsed_min > FEED_SESSION_TTL_MIN {
                    // Don't touch — let the prune sweep collect it.
                    return Ok(FeedSessionState::Expired);
                }
                let now = Utc::now().to_rfc3339();
                tx.execute(
                    "UPDATE feed_sessions SET last_accessed_at = ?1 \
                     WHERE session_id = ?2 AND account_id = ?3",
                    params![now, session_id, account_id],
                )
                .map_err(|e| format!("Failed to touch feed session: {e}"))?;
                Ok(FeedSessionState::Active)
            }
        }
    })
}

/// Record a batch of shown `post_ids` for a session (for dedup).
pub fn record_session_shown_posts(
    session_id: &str,
    posts: &[(i64, i32)], // (post_id, position)
) -> Result<(), String> {
    if posts.is_empty() {
        return Ok(());
    }
    let now = Utc::now().to_rfc3339();
    with_write_tx(|tx| {
        let mut stmt = tx
            .prepare_cached(
                "INSERT OR IGNORE INTO feed_session_posts (session_id, post_id, position, shown_at)
                 VALUES (?1, ?2, ?3, ?4)",
            )
            .map_err(|e| format!("Failed to prepare session post insert: {e}"))?;
        for (post_id, position) in posts {
            stmt.execute(params![session_id, post_id, position, now])
                .map_err(|e| format!("Failed to record session post: {e}"))?;
        }
        Ok(())
    })
}

/// Get all `post_ids` already shown in this session (dedup set).
pub fn get_session_shown_post_ids(session_id: &str) -> Result<HashSet<i64>, String> {
    let conn = open_db()?;
    let mut stmt = conn
        .prepare("SELECT post_id FROM feed_session_posts WHERE session_id = ?1")
        .map_err(|e| format!("Failed to prepare session shown posts query: {e}"))?;
    let rows = stmt
        .query_map(params![session_id], |r| r.get::<_, i64>(0))
        .map_err(|e| format!("Failed to query session shown posts: {e}"))?;
    rows.collect::<Result<HashSet<_>, _>>()
        .map_err(|e| format!("Failed to collect session shown posts: {e}"))
}

/// Prune expired sessions and their shown posts.
/// Returns the number of pruned sessions.
pub fn prune_expired_sessions() -> Result<usize, String> {
    let cutoff = (Utc::now() - chrono::Duration::minutes(FEED_SESSION_TTL_MIN)).to_rfc3339();
    with_write_tx(|tx| {
        // Delete orphaned feed_session_posts first (the FK was removed
        // when session_id became non-unique in V22). Then delete expired
        // session rows.
        let _ = tx.execute(
            "DELETE FROM feed_session_posts \
             WHERE session_id IN (SELECT session_id FROM feed_sessions \
                                  WHERE last_accessed_at < ?1)",
            params![cutoff],
        );
        let n = tx
            .execute(
                "DELETE FROM feed_sessions WHERE last_accessed_at < ?1",
                params![cutoff],
            )
            .map_err(|e| format!("Failed to prune expired sessions: {e}"))?;
        Ok(n)
    })
}
