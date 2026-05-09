//! Persistent owner-token revocation list.
//!
//! DB companion to the in-memory hot set in `auth.rs`. Operations:
//! load all (startup / post-prune), insert one (logout), prune old rows.
//! Request handlers read from the hot set, never from here.

use chrono::Utc;

use crate::db::{open_db, with_write_tx};

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
