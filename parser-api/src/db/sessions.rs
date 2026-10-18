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
///
/// The dedup set is scoped by `account_id` via `feed_sessions`, so a caller
/// can only write into a session that is actually linked to the account they
/// own. A session belonging to another account is refused.
pub fn record_session_shown_posts(
    session_id: &str,
    account_id: i32,
    posts: &[(i64, i32)], // (post_id, position)
) -> Result<(), String> {
    if posts.is_empty() {
        return Ok(());
    }
    let now = Utc::now().to_rfc3339();
    with_write_tx(|tx| {
        // Confirm this session belongs to the given account before writing
        // into its dedup namespace.
        let owned: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM feed_sessions \
                 WHERE session_id = ?1 AND account_id = ?2)",
                params![session_id, account_id],
                |r| r.get(0),
            )
            .map_err(|e| format!("check session ownership: {e}"))?;
        if !owned {
            return Err("session does not belong to this account".to_string());
        }
        let mut stmt = tx
            .prepare_cached(
                "INSERT OR IGNORE INTO feed_session_posts \
                 (account_id, session_id, post_id, position, shown_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .map_err(|e| format!("Failed to prepare session post insert: {e}"))?;
        for (post_id, position) in posts {
            stmt.execute(params![account_id, session_id, post_id, position, now])
                .map_err(|e| format!("Failed to record session post: {e}"))?;
        }
        Ok(())
    })
}

/// Get all `post_ids` already shown in this session (dedup set).
/// Scoped by `account_id` in the key itself (V37), not by a join on
/// `feed_sessions`: a session id can be shared by two accounts when the user
/// switches accounts in one tab, and a join alone would hand account B the
/// posts account A was shown.
/// Read the shown-post set for one `(account_id, session_id)` pair.
///
/// A constant so the tests below can run the exact statement the production
/// path runs, against an in-memory database instead of the shared file.
const SHOWN_POSTS_SQL: &str = "SELECT post_id FROM feed_session_posts \
     WHERE account_id = ?1 AND session_id = ?2";

pub fn get_session_shown_post_ids(
    session_id: &str,
    account_id: i32,
) -> Result<HashSet<i64>, String> {
    let conn = open_db()?;
    let mut stmt = conn
        .prepare(SHOWN_POSTS_SQL)
        .map_err(|e| format!("Failed to prepare session shown posts query: {e}"))?;
    let rows = stmt
        .query_map(params![account_id, session_id], |r| r.get::<_, i64>(0))
        .map_err(|e| format!("Failed to query session shown posts: {e}"))?;
    rows.collect::<Result<HashSet<_>, _>>()
        .map_err(|e| format!("Failed to collect session shown posts: {e}"))
}

/// SQL for the shown-post cleanup leg of [`prune_expired_sessions`].
///
/// Deletes every row no live session covers — expired *and* orphaned. Rows
/// whose session never existed are possible because `feed_session_posts` had
/// no foreign key between V22 and V37, so the sweep cannot assume the join
/// finds a row; `NOT EXISTS` handles both cases in one statement, and the
/// `feed_sessions` side is a primary-key probe.
///
/// Named constants so the `EXPLAIN QUERY PLAN` regression test below asserts on
/// the statements that actually run.
const PRUNE_SESSION_POSTS_SQL: &str = "DELETE FROM feed_session_posts \
     WHERE NOT EXISTS (SELECT 1 FROM feed_sessions fs \
                       WHERE fs.session_id = feed_session_posts.session_id \
                         AND fs.account_id = feed_session_posts.account_id \
                         AND fs.last_accessed_at >= ?1)";

/// SQL for the session-deletion leg of [`prune_expired_sessions`].
const PRUNE_SESSIONS_SQL: &str = "DELETE FROM feed_sessions WHERE last_accessed_at < ?1";

/// Prune expired sessions and their shown posts.
/// Returns the number of pruned sessions.
pub fn prune_expired_sessions() -> Result<usize, String> {
    let cutoff = (Utc::now() - chrono::Duration::minutes(FEED_SESSION_TTL_MIN)).to_rfc3339();
    with_write_tx(|tx| {
        // Sweep the shown posts first. V37 added the composite foreign key
        // with `ON DELETE CASCADE`, so deleting the session rows below also
        // removes their posts; this statement is the belt to that suspenders,
        // and it is the only thing that can collect rows orphaned while the
        // child table had no foreign key at all (V22–V37). It must NOT be
        // ignored: if it failed and the session rows were still deleted, any
        // leftover post rows would be unreachable — nothing reads them without
        // a live session.
        tx.execute(PRUNE_SESSION_POSTS_SQL, params![cutoff])
            .map_err(|e| format!("Failed to prune expired session posts: {e}"))?;
        let n = tx
            .execute(PRUNE_SESSIONS_SQL, params![cutoff])
            .map_err(|e| format!("Failed to prune expired sessions: {e}"))?;
        Ok(n)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A migrated, empty in-memory database — enough for the planner, and
    /// hermetic: the shared integration DB is mutated by dozens of other tests.
    fn migrated_memory_db() -> rusqlite::Connection {
        let mut conn = rusqlite::Connection::open_in_memory().expect("open in-memory db");
        crate::db::embedded::migrations::runner()
            .run(&mut conn)
            .expect("apply migrations");
        conn
    }

    /// Detail lines of `EXPLAIN QUERY PLAN` for `sql`, joined for readability.
    fn plan(conn: &rusqlite::Connection, sql: &str) -> String {
        let mut stmt = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .expect("prepare EXPLAIN QUERY PLAN");
        let rows = stmt
            .query_map(rusqlite::params!["2026-01-01T00:00:00+00:00"], |r| {
                r.get::<_, String>(3)
            })
            .expect("run EXPLAIN QUERY PLAN");
        rows.collect::<Result<Vec<_>, _>>()
            .expect("collect plan rows")
            .join("\n")
    }

    /// Run the production read statement against the in-memory database, so
    /// the account-scoping tests exercise the same SQL the route does without
    /// touching the shared file.
    fn shown(conn: &rusqlite::Connection, session_id: &str, account_id: i32) -> HashSet<i64> {
        let mut stmt = conn.prepare(SHOWN_POSTS_SQL).expect("prepare shown query");
        stmt.query_map(rusqlite::params![account_id, session_id], |r| {
            r.get::<_, i64>(0)
        })
        .expect("run shown query")
        .collect::<Result<HashSet<_>, _>>()
        .expect("collect shown ids")
    }

    /// Session pruning runs on every feed cycle. The expensive-looking part is
    /// the child sweep, and it is bounded by design: `feed_session_posts` only
    /// ever holds what one scroll showed (≈ a page or two per open tab), so a
    /// single pass over it is cheaper than the alternative — trusting the
    /// foreign key alone and leaving orphans behind if the pragma is ever off.
    ///
    /// What must stay indexed is the parent: `idx_feed_sessions_accessed`
    /// turns the TTL filter into a range seek, and the child sweep must probe
    /// `feed_sessions` by primary key instead of scanning it per row.
    #[test]
    fn prune_expired_sessions_plan_uses_indexes() {
        let conn = migrated_memory_db();

        let posts_plan = plan(&conn, PRUNE_SESSION_POSTS_SQL);
        let fs_line = posts_plan
            .lines()
            .find(|l| l.contains("feed_sessions") && l.contains("SEARCH"))
            .unwrap_or_else(|| {
                panic!("session-post cleanup no longer probes feed_sessions by key:\n{posts_plan}")
            });
        assert!(
            fs_line.contains("PRIMARY KEY") || fs_line.contains("INDEX"),
            "session-post cleanup probes feed_sessions without an index:\n{posts_plan}"
        );
        assert!(
            !posts_plan.contains("SCAN feed_sessions"),
            "session-post cleanup scans feed_sessions:\n{posts_plan}"
        );

        let sessions_plan = plan(&conn, PRUNE_SESSIONS_SQL);
        assert!(
            sessions_plan.contains("idx_feed_sessions_accessed"),
            "session deletion no longer uses idx_feed_sessions_accessed:\n{sessions_plan}"
        );
        assert!(
            !sessions_plan.contains("SCAN feed_sessions"),
            "session deletion now scans feed_sessions:\n{sessions_plan}"
        );
    }

    /// The whole point of V37: one session id may be shared by two accounts
    /// (the browser does not mint a new one when the account select changes),
    /// and what one account was shown must not filter the other's feed.
    #[test]
    fn shown_posts_are_scoped_by_account_not_by_session_id() {
        let conn = migrated_memory_db();
        conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
        let now = "2026-01-01T00:00:00+00:00";
        for account_id in [7, 8] {
            conn.execute(
                "INSERT INTO accounts (id, name) VALUES (?1, ?2)",
                rusqlite::params![account_id, format!("user{account_id}")],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO feed_sessions (session_id, account_id, created_at, last_accessed_at) \
                 VALUES ('shared-tab', ?1, ?2, ?2)",
                rusqlite::params![account_id, now],
            )
            .unwrap();
        }
        for (account_id, post_id) in [(7, 101), (8, 202)] {
            conn.execute(
                "INSERT INTO feed_session_posts (account_id, session_id, post_id, position, shown_at) \
                 VALUES (?1, 'shared-tab', ?2, 1, ?3)",
                rusqlite::params![account_id, post_id, now],
            )
            .unwrap();
        }

        let a = shown(&conn, "shared-tab", 7);
        let b = shown(&conn, "shared-tab", 8);
        assert_eq!(
            a,
            [101].into_iter().collect(),
            "account 7 saw only post 101"
        );
        assert_eq!(
            b,
            [202].into_iter().collect(),
            "account 8 saw only post 202"
        );
    }

    /// V37 made `(session_id, account_id)` a real foreign key with a cascade,
    /// so "forget this session" can no longer reach another account's rows —
    /// and no row can outlive its session.
    #[test]
    fn pruning_a_session_removes_only_that_accounts_rows() {
        let conn = migrated_memory_db();
        conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
        let now = "2026-01-01T00:00:00+00:00";
        for account_id in [7, 8] {
            conn.execute(
                "INSERT INTO accounts (id, name) VALUES (?1, ?2)",
                rusqlite::params![account_id, format!("user{account_id}")],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO feed_sessions (session_id, account_id, created_at, last_accessed_at) \
                 VALUES ('shared-tab', ?1, ?2, ?2)",
                rusqlite::params![account_id, now],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO feed_session_posts (account_id, session_id, post_id, position, shown_at) \
                 VALUES (?1, 'shared-tab', ?2, 1, ?3)",
                rusqlite::params![account_id, 100 + account_id, now],
            )
            .unwrap();
        }

        conn.execute(
            "DELETE FROM feed_sessions WHERE session_id = 'shared-tab' AND account_id = 7",
            [],
        )
        .unwrap();
        assert_eq!(
            shown(&conn, "shared-tab", 7),
            HashSet::new(),
            "deleting account 7's session must take its shown posts"
        );
        assert_eq!(
            shown(&conn, "shared-tab", 8),
            [108].into_iter().collect(),
            "account 8's scroll set must survive account 7's cleanup"
        );
    }

    /// V32 dropped the prefix-duplicate index and V37 did not bring it back:
    /// both read paths filter on `(account_id, session_id)`, which is exactly
    /// the leading part of the primary key.
    #[test]
    fn session_posts_has_no_prefix_duplicate_index() {
        let conn = migrated_memory_db();
        let left: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'index' AND name = 'idx_feed_session_posts_session'",
                [],
                |r| r.get(0),
            )
            .expect("count indexes");
        assert_eq!(left, 0, "idx_feed_session_posts_session came back");
    }
}
