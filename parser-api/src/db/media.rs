//! Index of original-media files stored on the system disk under the
//! hardcoded `media/` folder, per docs/offline-catalog.md.
//!
//! The DB deliberately holds **no media bytes** — `media_entries` only maps a
//! post to the relative path of its locally-stored original file plus size and
//! an LRU key (`mtime`). Serving reads the file off disk (see `media_store.rs`
//! and `routes/media.rs`).
//!
//! All access is opt-in: nothing is written here unless a caller explicitly
//! inserts an entry (mode-B prefetcher or the proxy's on-demand save).

use rusqlite::{OptionalExtension, params};
use std::collections::HashSet;

use super::{open_db, with_write_tx};

/// One locally-stored original file.
pub struct MediaEntry {
    pub post_id: i64,
    pub rel_path: String,
    pub bytes: i64,
    pub mtime: i64,
}

/// Insert or refresh the media index entry for a post.
pub fn upsert_media_entry(
    post_id: i64,
    rel_path: &str,
    bytes: i64,
    mtime: i64,
    url_digest: &str,
) -> Result<(), String> {
    with_write_tx(|tx| {
        tx.execute(
            "INSERT INTO media_entries (post_id, rel_path, bytes, mtime, url_md5)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(post_id) DO UPDATE SET
               rel_path = excluded.rel_path,
               bytes    = excluded.bytes,
               mtime    = excluded.mtime,
               url_md5  = excluded.url_md5",
            params![post_id, rel_path, bytes, mtime, url_digest],
        )
        .map_err(|e| format!("upsert_media_entry: {e}"))?;
        Ok(())
    })
}

/// Look up the stored file for a post. Returns `(rel_path, mtime)`.
pub fn get_media_entry(post_id: i64) -> Result<Option<(String, i64)>, String> {
    let conn = open_db().map_err(|e| format!("get_media_entry open: {e}"))?;
    conn.query_row(
        "SELECT rel_path, mtime FROM media_entries WHERE post_id = ?1",
        params![post_id],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
    )
    .optional()
    .map_err(|e| format!("get_media_entry: {e}"))
}

/// Bump the LRU timestamp when a stored original is served (so frequently
/// viewed posts are evicted last).
pub fn touch_media_entry(post_id: i64, mtime: i64) -> Result<(), String> {
    with_write_tx(|tx| {
        tx.execute(
            "UPDATE media_entries SET mtime = ?2 WHERE post_id = ?1",
            params![post_id, mtime],
        )
        .map_err(|e| format!("touch_media_entry: {e}"))?;
        Ok(())
    })
}

/// Total bytes of all stored originals.
pub fn count_media_bytes() -> Result<i64, String> {
    let conn = open_db().map_err(|e| format!("count_media_bytes open: {e}"))?;
    conn.query_row(
        "SELECT COALESCE(SUM(bytes), 0) FROM media_entries",
        [],
        |r| r.get(0),
    )
    .map_err(|e| format!("count_media_bytes: {e}"))
}

/// SQL for the `pending_saved` leg of [`queue_stats`].
///
/// A named constant rather than an inline literal so the `EXPLAIN QUERY PLAN`
/// regression test below asserts on the statement that actually runs.
const QUEUE_STATS_PENDING_SQL: &str = "SELECT COUNT(*) FROM posts p
     WHERE p.file_url IS NOT NULL AND p.is_deleted = 0
       AND EXISTS (SELECT 1 FROM accounts_post a WHERE a.post_id = p.id)
       AND NOT EXISTS (SELECT 1 FROM media_entries m WHERE m.post_id = p.id)";

/// Queue/status summary for the media worker control UI:
/// `(pending_saved, stored, bytes_total)`. `pending_saved` is the number of
/// saved posts still awaiting their original download.
pub fn queue_stats() -> Result<(i64, i64, i64), String> {
    let conn = open_db().map_err(|e| format!("queue_stats open: {e}"))?;
    let pending: i64 = conn
        .query_row(QUEUE_STATS_PENDING_SQL, [], |r| r.get(0))
        .map_err(|e| format!("queue_stats pending: {e}"))?;
    let stored: i64 = conn
        .query_row("SELECT COUNT(*) FROM media_entries", [], |r| r.get(0))
        .map_err(|e| format!("queue_stats stored: {e}"))?;
    let bytes: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(bytes), 0) FROM media_entries",
            [],
            |r| r.get(0),
        )
        .map_err(|e| format!("queue_stats bytes: {e}"))?;
    Ok((pending, stored, bytes))
}

/// Oldest `limit` entries by `mtime`, oldest first — candidates for LRU
/// eviction once `media_cache_max_bytes` is exceeded.
pub fn oldest_media_entries(limit: i64) -> Result<Vec<MediaEntry>, String> {
    let conn = open_db().map_err(|e| format!("oldest_media_entries open: {e}"))?;
    let mut stmt = conn
        .prepare(
            "SELECT post_id, rel_path, bytes, mtime
             FROM media_entries ORDER BY mtime ASC LIMIT ?1",
        )
        .map_err(|e| format!("oldest_media_entries prepare: {e}"))?;
    let rows = stmt
        .query_map(params![limit], |r| {
            Ok(MediaEntry {
                post_id: r.get(0)?,
                rel_path: r.get(1)?,
                bytes: r.get(2)?,
                mtime: r.get(3)?,
            })
        })
        .map_err(|e| format!("oldest_media_entries query: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("oldest_media_entries collect: {e}"))
}

/// The subset of `ids` that actually have an original stored on disk. Used to
/// rewrite media URLs at post-hydration time so locally-available posts are
/// served from this backend.
pub fn stored_url_map(ids: &[i64]) -> Result<HashSet<i64>, String> {
    if ids.is_empty() {
        return Ok(HashSet::new());
    }
    let conn = open_db().map_err(|e| format!("stored_url_map open: {e}"))?;
    let mut set = HashSet::with_capacity(ids.len());
    for chunk in ids.chunks(512) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("SELECT post_id FROM media_entries WHERE post_id IN ({placeholders})");
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| format!("stored_url_map prepare: {e}"))?;
        let params_vec: Vec<&dyn rusqlite::ToSql> =
            chunk.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params_vec), |r| {
                r.get::<_, i64>(0)
            })
            .map_err(|e| format!("stored_url_map query: {e}"))?;
        for r in rows {
            let id = r.map_err(|e| format!("stored_url_map row: {e}"))?;
            set.insert(id);
        }
    }
    Ok(set)
}

/// Post ids that still need their original downloaded — limited to posts the
/// owner actually *saved* (`accounts_post`) rather than the whole `posts`
/// corpus (which can be hundreds of thousands of rows). This is what the
/// in-server background media worker drains, so a favourites sync or
/// `/process` that persists a saved post queues its original for download
/// automatically without downloading the entire corpus.
pub fn pending_saved_original_posts(limit: i64) -> Result<Vec<(i64, String, String)>, String> {
    let conn = open_db().map_err(|e| format!("pending_saved_original_posts open: {e}"))?;
    let mut stmt = conn
        .prepare(
            "SELECT p.id, p.file_url, COALESCE(p.file_ext, '')
             FROM posts p
             WHERE p.file_url IS NOT NULL
               AND p.is_deleted = 0
               AND EXISTS (SELECT 1 FROM accounts_post a WHERE a.post_id = p.id)
               AND NOT EXISTS (SELECT 1 FROM media_entries m WHERE m.post_id = p.id)
             ORDER BY p.last_seen_at DESC, p.id DESC LIMIT ?1",
        )
        .map_err(|e| format!("pending_saved_original_posts prepare: {e}"))?;
    let rows = stmt
        .query_map(params![limit], |r| {
            Ok((r.get(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        })
        .map_err(|e| format!("pending_saved_original_posts query: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("pending_saved_original_posts collect: {e}"))
}

/// Remove an entry (after the backing file has been deleted).
pub fn delete_media_entries(post_ids: &[i64]) -> Result<usize, String> {
    if post_ids.is_empty() {
        return Ok(0);
    }
    with_write_tx(move |tx| {
        // Chunked so an over-long eviction list cannot blow SQLite's
        // bound-parameter ceiling; one transaction keeps it atomic (see
        // `db::IN_CLAUSE_CHUNK`).
        let mut deleted = 0usize;
        for chunk in post_ids.chunks(super::IN_CLAUSE_CHUNK) {
            let placeholders = std::iter::repeat_n("?", chunk.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!("DELETE FROM media_entries WHERE post_id IN ({placeholders})");
            let mut stmt = tx
                .prepare(&sql)
                .map_err(|e| format!("delete_media_entries prepare: {e}"))?;
            for (i, id) in chunk.iter().enumerate() {
                stmt.raw_bind_parameter(i + 1, id)
                    .map_err(|e| format!("delete_media_entries bind: {e}"))?;
            }
            deleted += stmt
                .raw_execute()
                .map_err(|e| format!("delete_media_entries: {e}"))?;
        }
        Ok(deleted)
    })
}

/// Every stored file entry — used by a full cache clear (bulk teardown).
pub fn all_media_entries() -> Result<Vec<MediaEntry>, String> {
    let conn = open_db().map_err(|e| format!("all_media_entries open: {e}"))?;
    let mut stmt = conn
        .prepare("SELECT post_id, rel_path, bytes, mtime FROM media_entries")
        .map_err(|e| format!("all_media_entries prepare: {e}"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(MediaEntry {
                post_id: r.get(0)?,
                rel_path: r.get(1)?,
                bytes: r.get(2)?,
                mtime: r.get(3)?,
            })
        })
        .map_err(|e| format!("all_media_entries query: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("all_media_entries collect: {e}"))
}

/// Wipe every media-index row (full cache clear). Returns rows deleted.
pub fn clear_media_entries() -> Result<usize, String> {
    with_write_tx(|tx| {
        tx.execute("DELETE FROM media_entries", [])
            .map_err(|e| format!("clear_media_entries: {e}"))
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
            .query_map([], |r| r.get::<_, String>(3))
            .expect("run EXPLAIN QUERY PLAN");
        rows.collect::<Result<Vec<_>, _>>()
            .expect("collect plan rows")
            .join("\n")
    }

    /// `queue_stats` is polled continuously by the media worker. Its
    /// `pending_saved` leg is index-dependent: degrade the join and SQLite
    /// scans `accounts_post` once per saved post, while still returning the
    /// right number — so only a plan assertion catches the regression.
    #[test]
    fn queue_stats_pending_plan_uses_accounts_post_index() {
        let conn = migrated_memory_db();
        let plan = plan(&conn, QUEUE_STATS_PENDING_SQL);

        assert!(
            plan.contains("SEARCH a EXISTS USING COVERING INDEX"),
            "pending_saved no longer resolves the accounts_post EXISTS with a \
             covering-index search:\n{plan}"
        );
        assert!(
            plan.contains("SEARCH m USING INTEGER PRIMARY KEY"),
            "stored-media exclusion is no longer a primary-key probe:\n{plan}"
        );
        // Assert the *shape*, not an index name: the planner serves the EXISTS
        // through whichever index leads on `post_id`, which today is the
        // `PRIMARY KEY(post_id, account_id)` autoindex — the named
        // `idx_ap_post(post_id)` is redundant beside it. Naming it here would
        // turn "the redundant index was dropped" into a false failure.
        for alias in ["a", "m"] {
            assert!(
                !plan.contains(&format!("SCAN {alias}")),
                "pending_saved scans {alias} instead of using an index:\n{plan}"
            );
        }
    }
}
