//! Local pool membership (docs/offline-catalog.md).
//!
//! Seeded from e621 pool responses so the post-viewer's pool navigation works
//! from local data, and so pool membership is available for future scoring.
//! Opt-in via `catalog.pool_membership`; the tables stay empty otherwise.

use rusqlite::params;

use super::{open_db, with_write_tx};

/// Upsert a pool and replace its membership in one transaction.
///
/// `members` is `(post_id, position)` in pool order. Posts that left the pool
/// are removed; new ones are inserted. Returns the number of members stored.
pub fn save_pool(pool_id: i64, name: &str, members: &[(i64, i64)]) -> Result<usize, String> {
    let created = chrono::Utc::now().to_rfc3339();
    with_write_tx(|tx| {
        tx.execute(
            "INSERT INTO pools (id, name, created_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET name = excluded.name",
            params![pool_id, name, created],
        )
        .map_err(|e| format!("save_pool upsert pool: {e}"))?;
        tx.execute(
            "DELETE FROM pool_posts WHERE pool_id = ?1",
            params![pool_id],
        )
        .map_err(|e| format!("save_pool clear members: {e}"))?;
        {
            let mut stmt = tx
                .prepare(
                    "INSERT OR IGNORE INTO pool_posts (pool_id, post_id, position)
                     VALUES (?1, ?2, ?3)",
                )
                .map_err(|e| format!("save_pool prepare: {e}"))?;
            for (post_id, position) in members {
                stmt.execute(params![pool_id, post_id, position])
                    .map_err(|e| format!("save_pool insert member: {e}"))?;
            }
        }
        Ok(members.len())
    })
}

/// Pools a post belongs to (drives the viewer's pool navigation).
pub fn pools_for_post(post_id: i64) -> Result<Vec<i64>, String> {
    let conn = open_db().map_err(|e| format!("pools_for_post open: {e}"))?;
    let mut stmt = conn
        .prepare("SELECT pool_id FROM pool_posts WHERE post_id = ?1 ORDER BY position")
        .map_err(|e| format!("pools_for_post prepare: {e}"))?;
    let rows = stmt
        .query_map(params![post_id], |r| r.get::<_, i64>(0))
        .map_err(|e| format!("pools_for_post query: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("pools_for_post collect: {e}"))
}

/// All posts of a pool in order: `(post_id, position)`.
pub fn get_pool_members(pool_id: i64) -> Result<Vec<(i64, i64)>, String> {
    let conn = open_db().map_err(|e| format!("get_pool_members open: {e}"))?;
    let mut stmt = conn
        .prepare("SELECT post_id, position FROM pool_posts WHERE pool_id = ?1 ORDER BY position")
        .map_err(|e| format!("get_pool_members prepare: {e}"))?;
    let rows = stmt
        .query_map(params![pool_id], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(|e| format!("get_pool_members query: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("get_pool_members collect: {e}"))
}
