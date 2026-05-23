//! Persistent owner-token revocation list + feed session management.
//!
//! Revocation: load all (startup / post-prune), insert one (logout), prune old rows.
//! Sessions: create/update/touch/prune for feed continuation.

use chrono::Utc;
use rusqlite::params;
use std::collections::HashSet;

use crate::db::{open_db, with_write_tx};

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

/// Create or update a feed session (upsert on session_id).
/// Returns true if this was a new session.
pub fn upsert_feed_session(session_id: &str, account_id: i32) -> Result<bool, String> {
    let now = Utc::now().to_rfc3339();
    with_write_tx(|tx| {
        let existing: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM feed_sessions WHERE session_id = ?1)",
                params![session_id],
                |r| r.get(0),
            )
            .map_err(|e| format!("Failed to check session existence: {e}"))?;

        tx.execute(
            "INSERT INTO feed_sessions (session_id, account_id, created_at, last_accessed_at)
             VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(session_id) DO UPDATE SET
                last_accessed_at = excluded.last_accessed_at",
            params![session_id, account_id, now],
        )
        .map_err(|e| format!("Failed to upsert feed session: {e}"))?;

        Ok(!existing)
    })
}

/// Validate and touch a session. Returns Ok(()) if the session exists,
/// belongs to the account, and hasn't expired (TTL = 30 min).
pub fn validate_feed_session(session_id: &str, account_id: i32) -> Result<(), String> {
    let conn = open_db()?;
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT created_at, last_accessed_at FROM feed_sessions
             WHERE session_id = ?1 AND account_id = ?2",
            params![session_id, account_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();

    match row {
        None => Err("Session not found or does not belong to this account".to_string()),
        Some((_created, last_accessed)) => {
            let last = crate::db::parse_db_datetime(&last_accessed)
                .map_err(|e| format!("Failed to parse session timestamp: {e}"))?;
            let elapsed = (Utc::now() - last).num_minutes();
            if elapsed > 30 {
                return Err("Session expired (TTL 30 min)".to_string());
            }
            // Touch last_accessed_at.
            with_write_tx(|tx| {
                tx.execute(
                    "UPDATE feed_sessions SET last_accessed_at = ?1 WHERE session_id = ?2",
                    params![Utc::now().to_rfc3339(), session_id],
                )
                .map_err(|e| format!("Failed to touch feed session: {e}"))?;
                Ok(())
            })
        }
    }
}

/// Record a batch of shown post_ids for a session (for dedup).
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

/// Get all post_ids already shown in this session (dedup set).
pub fn get_session_shown_post_ids(session_id: &str) -> Result<HashSet<i64>, String> {
    let conn = open_db()?;
    let mut stmt = conn
        .prepare(
            "SELECT post_id FROM feed_session_posts WHERE session_id = ?1",
        )
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
    let cutoff = (Utc::now() - chrono::Duration::minutes(30)).to_rfc3339();
    with_write_tx(|tx| {
        // CASCADE will handle feed_session_posts rows.
        let n = tx
            .execute(
                "DELETE FROM feed_sessions WHERE last_accessed_at < ?1",
                params![cutoff],
            )
            .map_err(|e| format!("Failed to prune expired sessions: {e}"))?;
        Ok(n)
    })
}
