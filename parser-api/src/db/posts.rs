use chrono::Utc;
use rusqlite::params;
use std::collections::{HashMap, HashSet};

use crate::models::Post;

use super::{hydrate_cache, open_db, parse_db_datetime};

/// Tag-side effects of deleting posts, captured inside the deleting
/// transaction and applied to the in-memory IDF index only after it commits.
///
/// The delete cascades into `tags_posts`, so `tags.df` (recomputed in the same
/// transaction) and the in-memory index would otherwise disagree until the
/// next *full* rebuild — which is exactly what the prune workers used to force
/// on every pass by calling `mark_idf_dirty()`. That rebuild re-reads the whole
/// `tags` table plus `COUNT(*)` over `posts`, and its cost grows with the
/// catalog rather than with the deletion. Carrying the exact negative delta
/// here lets `bump_idf` correct the index in place.
struct PostDeleteEffects {
    removed: usize,
    df_delta: HashMap<String, i64>,
}

fn apply_post_delete_effects(effects: PostDeleteEffects) {
    if effects.removed == 0 {
        return;
    }
    crate::utils::bump_idf(effects.df_delta, -(effects.removed as i64));
}

/// Delete posts by id, in one transaction, and report what the deletion did to
/// tag document frequencies.
///
/// Both the postings that the cascade removes and the new `tags.df` are
/// computed here, not by a later rebuild: `tags_posts` is keyed
/// `(tag_id, post_id)`, and `idx_tp_post(post_id)` serves the per-post lookup,
/// so the per-tag count of a bounded id list stays an index-only `GROUP BY`.
/// `recompute_df_for_tags` then re-counts just those tags.
fn delete_posts_by_ids_in_tx(
    tx: &rusqlite::Transaction,
    ids: &[i64],
) -> Result<PostDeleteEffects, String> {
    if ids.is_empty() {
        return Ok(PostDeleteEffects {
            removed: 0,
            df_delta: HashMap::new(),
        });
    }

    let mut postings: HashMap<i64, i64> = HashMap::new();
    for chunk in ids.chunks(super::IN_CLAUSE_CHUNK) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let mut stmt = tx
            .prepare(&format!(
                "SELECT tag_id, COUNT(*) FROM tags_posts \
                 WHERE post_id IN ({placeholders}) GROUP BY tag_id"
            ))
            .map_err(|e| format!("prepare post tag deltas: {e}"))?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(chunk.iter()), |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })
            .map_err(|e| format!("query post tag deltas: {e}"))?;
        for row in rows {
            let (tag_id, count) = row.map_err(|e| format!("read post tag delta: {e}"))?;
            *postings.entry(tag_id).or_insert(0) += count;
        }
    }

    let tag_ids: Vec<i64> = postings.keys().copied().collect();
    let mut names: HashMap<i64, String> = HashMap::with_capacity(tag_ids.len());
    for chunk in tag_ids.chunks(super::IN_CLAUSE_CHUNK) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let mut stmt = tx
            .prepare(&format!(
                "SELECT id, name FROM tags WHERE id IN ({placeholders})"
            ))
            .map_err(|e| format!("prepare deleted-tag names: {e}"))?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(chunk.iter()), |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(|e| format!("query deleted-tag names: {e}"))?;
        for row in rows {
            let (id, name) = row.map_err(|e| format!("read deleted-tag name: {e}"))?;
            names.insert(id, name);
        }
    }

    let mut removed = 0usize;
    for chunk in ids.chunks(super::IN_CLAUSE_CHUNK) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        removed += tx
            .execute(
                &format!("DELETE FROM posts WHERE id IN ({placeholders})"),
                rusqlite::params_from_iter(chunk.iter()),
            )
            .map_err(|e| format!("delete posts by id: {e}"))?;
    }

    let touched: HashSet<i64> = postings.keys().copied().collect();
    super::tags::recompute_df_for_tags(tx, &touched)?;

    let df_delta = postings
        .iter()
        .filter_map(|(id, count)| names.get(id).map(|name| (name.clone(), -count)))
        .collect();
    Ok(PostDeleteEffects { removed, df_delta })
}

/// Delete catalog posts that aren't favourited by anyone and haven't been
/// re-touched within `retention_secs`. Used by the cache-validator worker
/// to keep `/recommendations` browse-time inserts (called "candidates"
/// because they're shown but not chosen) from bloating the catalog and,
/// transitively, the IDF + global tag-relation graphs they feed.
///
/// Returns `(before, after)`: the count of orphan posts before/after the
/// prune. `before` is computed via a separate COUNT so the caller can log
/// "dropped N entries" without instrumenting the DELETE itself.
///
/// The in-memory IDF index is corrected incrementally per batch (see
/// [`delete_posts_by_ids_in_tx`]), so this path no longer forces a full
/// rebuild. The global relation graph still needs its dirty flag: its table is
/// not decremented by post deletes. Mirror of the longer-lived
/// `prune_stale_catalog_posts` — both are now unified in `cache_pruner.rs`.
pub fn prune_orphan_candidates(retention_secs: u64) -> Result<(usize, usize), String> {
    let conn = open_db().map_err(|e| format!("prune_orphan_candidates open: {e}"))?;
    let cutoff = (Utc::now() - chrono::Duration::seconds(retention_secs as i64)).to_rfc3339();

    let before: i64 = conn
        .query_row(
            "
            SELECT COUNT(*)
            FROM posts p
            WHERE p.last_seen_at < ?1
              AND NOT EXISTS (SELECT 1 FROM accounts_post ap WHERE ap.post_id = p.id)
            ",
            params![cutoff],
            |r| r.get(0),
        )
        .map_err(|e| format!("prune_orphan_candidates count: {e}"))?;

    if before == 0 {
        return Ok((0, 0));
    }
    drop(conn);

    // Batched delete: each `DELETE FROM posts` cascades into `tags_posts`
    // and `accounts_post` via the FK CASCADE, so even modest catalog
    // backlogs translate into large per-row work inside the tx. Cap
    // each chunk to keep the writer mutex available between batches.
    let deleted = prune_orphan_posts_batched(&cutoff, ORPHAN_PRUNE_BATCH)?;

    if deleted > 0 {
        crate::utils::mark_global_relation_dirty();
    }

    let after = (before as usize).saturating_sub(deleted);
    Ok((before as usize, after))
}

const ORPHAN_PRUNE_BATCH: i64 = 5_000;

fn prune_orphan_posts_batched(cutoff: &str, batch_size: i64) -> Result<usize, String> {
    let mut total: usize = 0;
    loop {
        let effects = super::with_write_tx(|tx| {
            let mut stmt = tx
                .prepare(
                    "SELECT p.id FROM posts p \
                     WHERE p.last_seen_at < ?1 \
                       AND NOT EXISTS ( \
                           SELECT 1 FROM accounts_post ap WHERE ap.post_id = p.id \
                       ) \
                     LIMIT ?2",
                )
                .map_err(|e| format!("prepare prune batch: {e}"))?;
            let ids = stmt
                .query_map(params![cutoff, batch_size], |r| r.get::<_, i64>(0))
                .map_err(|e| format!("query prune batch: {e}"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("collect prune batch: {e}"))?;
            delete_posts_by_ids_in_tx(tx, &ids)
        })?;
        let removed = effects.removed;
        apply_post_delete_effects(effects);
        if removed == 0 {
            break;
        }
        total += removed;
    }
    Ok(total)
}

/// Longer-lived catalog cleanup run on `cleanup_interval_secs` (default 6h).
/// Deletes posts that aren't favourited by anyone and haven't been re-touched
/// within `catalog_retention_days`. Unlike `prune_orphan_candidates` (which
/// churns aggressively on browse-time inserts), this is the belt-and-suspenders
/// bound that ensures the catalog can't grow without limit.
///
/// Returns the number of deleted posts. The IDF index is corrected per batch;
/// only the global relation graph is marked dirty, because its table is not
/// decremented by post deletes.
pub fn prune_stale_catalog_posts(retention_days: i64) -> Result<i64, String> {
    let cutoff = (Utc::now() - chrono::Duration::days(retention_days)).to_rfc3339();

    // Previously this ran the DELETE on a pool connection, which bypasses
    // `WRITE_CONN` and races every other writer at the SQLite level —
    // visible as SQLITE_BUSY after a 60s busy_timeout when the dedicated
    // writer holds the connection during a long /process or backfill
    // batch. Route through the shared writer mutex, in chunks, so the
    // long-running maintenance path can't starve out interactive writes.
    let deleted = prune_orphan_posts_batched(&cutoff, ORPHAN_PRUNE_BATCH)?;

    if deleted > 0 {
        crate::utils::mark_global_relation_dirty();
    }

    Ok(deleted as i64)
}

pub fn drop_account_posts(account_id: i32) -> Result<(), String> {
    let effects = super::with_write_tx(|tx| {
        // Order matters: the "exclusively owned" test is a correlated
        // subquery against `accounts_post`, so it has to run *before* this
        // account's ownership rows are cleared. Doing it in the other order
        // forced a `SELECT COUNT(*)` per post inside the write transaction
        // (a 1500-favourite account = 1500 extra statements while the writer
        // mutex is held), which is exactly the kind of work `with_write_tx`
        // exists to keep short.
        //
        // We delete from `posts` at all because the `accounts_post` FK is
        // ON DELETE CASCADE in the wrong direction: clearing `accounts_post`
        // does not remove the post, and stale posts leak into profile/digest
        // queries. The cascade below only reaches this account's own rows,
        // because a post with any other owner is not selected.
        let ids: Vec<i64> = {
            let mut stmt = tx
                .prepare(
                    "SELECT ap.post_id FROM accounts_post ap \
                     WHERE ap.account_id = ?1 \
                       AND NOT EXISTS (SELECT 1 FROM accounts_post AS other \
                                       WHERE other.post_id = ap.post_id AND other.account_id != ?1)",
                )
                .map_err(|e| format!("prepare exclusively-owned posts: {e}"))?;
            stmt.query_map(params![account_id], |r| r.get::<_, i64>(0))
                .map_err(|e| format!("query exclusively-owned posts: {e}"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("collect exclusively-owned posts: {e}"))?
        };
        let effects = delete_posts_by_ids_in_tx(tx, &ids)?;

        tx.execute(
            "DELETE FROM accounts_post WHERE account_id = ?1",
            params![account_id],
        )
        .map_err(|e| format!("Failed to clear accounts_post: {e}"))?;
        Ok(effects)
    })?;
    apply_post_delete_effects(effects);
    hydrate_cache::clear_owned_and_candidates(i64::from(account_id));
    Ok(())
}

/// Delete every `account_tag_cooccurrence` row of this account.
///
/// `account_tag_cooccurrence` can hold millions of rows for a single account
/// (one row per (tag1, tag2) pair, materialised across the user's
/// favourites), and wiping them used to be a chunked delete. That shape was
/// removed on purpose: the `rowid IN (SELECT … LIMIT ?)` pattern materialised
/// the subquery as a temp table once per batch and took 27 minutes on a
/// 1.7M-row account, while the table has an index on `account_id`, so one
/// unbounded `DELETE` is O(log n) for the scan. Chunking only pays when
/// picking rowids is cheaper than deleting them — which is why
/// `drop_account_feed_interactions_batched` still chunks, and this does not.
///
/// `on_deleted` is called exactly once, with the total, when rows were
/// removed; it exists so callers keep their progress logging. Returns the
/// number of rows deleted.
pub fn drop_account_cooccurrence(
    account_id: i32,
    on_deleted: impl FnOnce(usize),
) -> Result<usize, String> {
    let deleted = super::with_write_tx(|tx| {
        let n = tx
            .execute(
                "DELETE FROM account_tag_cooccurrence WHERE account_id = ?1",
                params![account_id],
            )
            .map_err(|e| format!("Failed to drop account cooccurrence: {e}"))?;
        Ok(n)
    })?;
    if deleted > 0 {
        on_deleted(deleted);
    }
    Ok(deleted)
}

/// Drop this account's feed interactions in chunks. Same shape as the
/// cooccurrence wipe — long-running accounts accumulate thousands of
/// interaction rows and a single DELETE during account teardown can
/// hold the writer mutex long enough to look like a hang.
pub fn drop_account_feed_interactions_batched(
    account_id: i32,
    batch_size: usize,
    on_batch: impl FnMut(usize, usize),
) -> Result<usize, String> {
    delete_by_account_in_batches("feed_interactions", account_id, batch_size, on_batch)
}

/// Shared implementation behind the per-account batched deletes. Picks
/// rowids in chunks of `batch_size`, deletes them inside their own
/// `with_write_tx`, and yields the writer mutex between batches so
/// concurrent writers (prefetch, profile refresh, status polling) keep
/// moving. Loops until a batch deletes zero rows.
fn delete_by_account_in_batches(
    table: &'static str,
    account_id: i32,
    batch_size: usize,
    mut on_batch: impl FnMut(usize, usize),
) -> Result<usize, String> {
    let batch_size = batch_size.max(1) as i64;
    // Whitelist the table name since we splice it directly into SQL.
    // Caller passes a static str but we still gate the values to be
    // explicit about the intent — the table list is closed.
    let sql = match table {
        "account_tag_cooccurrence" => {
            "DELETE FROM account_tag_cooccurrence \
             WHERE rowid IN (SELECT rowid FROM account_tag_cooccurrence \
                             WHERE account_id = ?1 LIMIT ?2)"
        }
        "feed_interactions" => {
            "DELETE FROM feed_interactions \
             WHERE rowid IN (SELECT rowid FROM feed_interactions \
                             WHERE account_id = ?1 LIMIT ?2)"
        }
        other => {
            return Err(format!(
                "delete_by_account_in_batches: unknown table {other}"
            ));
        }
    };
    let mut total: usize = 0;
    loop {
        let deleted = super::with_write_tx(|tx| {
            let n = tx
                .execute(sql, params![account_id, batch_size])
                .map_err(|e| format!("Failed to batch delete from {table}: {e}"))?;
            Ok(n)
        })?;
        if deleted == 0 {
            break;
        }
        total += deleted;
        on_batch(deleted, total);
    }
    Ok(total)
}

pub fn save_posts(posts: &[Post], account_id: i32) -> Result<(), String> {
    let result = super::with_write_tx(|tx| save_posts_in_tx(tx, posts, account_id));
    hydrate_cache::clear_owned_and_candidates(i64::from(account_id));
    result
}

/// Body of [`save_posts`], taking an existing transaction so callers can put
/// the tag postings in the same commit
/// (see [`save_posts_with_tags`](super::save_posts_with_tags)).
pub(super) fn save_posts_in_tx(
    tx: &rusqlite::Connection,
    posts: &[Post],
    account_id: i32,
) -> Result<(), String> {
    let mut insert_post = tx
        .prepare_cached(POST_UPSERT_SQL)
        .map_err(|e| format!("Failed to prepare transaction: {e}"))?;
    let mut insert_account = tx
        .prepare_cached(
            // `added_at` records when this row first entered our library — the
            // only favourite clock e621 does not give us (see
            // `V40__accounts_post_added_at.sql`). `OR IGNORE` keeps the first
            // sighting, so re-syncing an old favourite does not move it.
            "INSERT OR IGNORE INTO accounts_post (account_id, post_id, added_at) \
             VALUES (?1, ?2, ?3);",
        )
        .map_err(|e| format!("Failed to prepare transaction: {e}"))?;

    let added_at = Utc::now().to_rfc3339();
    for post in posts {
        insert_post
            .execute(rusqlite::params_from_iter(post_upsert_params(post)))
            .map_err(|e| format!("Failed to execute transaction: {e}"))?;

        insert_account
            .execute(params![account_id, post.id, added_at])
            .map_err(|e| format!("Failed to execute transaction: {e}"))?;
    }
    Ok(())
}

/// Single source of truth for the posts upsert. Both `save_posts` and
/// `upsert_catalog_posts` route through here so column lists stay in lockstep.
const POST_UPSERT_SQL: &str = "
    INSERT INTO posts (
        id, created_at, updated_at, score_total, fav_count, rating, last_seen_at,
        file_ext, file_width, file_height, file_size, is_animated, duration,
        comment_count, has_notes, is_deleted, has_children,
        preview_url, sample_url, file_url, score_up, score_down,
        sample_width, sample_height, preview_width, preview_height,
        uploader_id
    )
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
            ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27)
    ON CONFLICT(id) DO UPDATE SET
        updated_at = excluded.updated_at,
        score_total = excluded.score_total,
        fav_count = excluded.fav_count,
        rating = excluded.rating,
        last_seen_at = excluded.last_seen_at,
        file_ext = excluded.file_ext,
        file_width = excluded.file_width,
        file_height = excluded.file_height,
        file_size = excluded.file_size,
        is_animated = excluded.is_animated,
        duration = excluded.duration,
        comment_count = excluded.comment_count,
        has_notes = excluded.has_notes,
        is_deleted = excluded.is_deleted,
        has_children = excluded.has_children,
        preview_url = COALESCE(excluded.preview_url, posts.preview_url),
        sample_url = COALESCE(excluded.sample_url, posts.sample_url),
        file_url = COALESCE(excluded.file_url, posts.file_url),
        score_up = excluded.score_up,
        score_down = excluded.score_down,
        sample_width = COALESCE(excluded.sample_width, posts.sample_width),
        sample_height = COALESCE(excluded.sample_height, posts.sample_height),
        preview_width = COALESCE(excluded.preview_width, posts.preview_width),
        preview_height = COALESCE(excluded.preview_height, posts.preview_height),
        uploader_id = excluded.uploader_id
";

fn post_upsert_params(post: &Post) -> Vec<rusqlite::types::Value> {
    use rusqlite::types::Value;

    let file_ext = post.files.meta.ext.clone();
    let file_width = Some(post.files.original.width);
    let file_height = Some(post.files.original.height);
    let file_size = Some(post.files.meta.size);
    let preview_url = post
        .files
        .preview
        .url
        .clone()
        .or_else(|| post.files.preview.alt.clone())
        .or_else(|| post.files.preview.jpg.clone())
        .or_else(|| post.files.preview.webp.clone());
    let preview_width = Some(post.files.preview.width);
    let preview_height = Some(post.files.preview.height);
    let sample_url = post
        .files
        .sample
        .url
        .clone()
        .or_else(|| post.files.sample.alt.clone())
        .or_else(|| post.files.sample.jpg.clone())
        .or_else(|| post.files.sample.webp.clone());
    let sample_width = sample_url.as_ref().map(|_| post.files.sample.width);
    let sample_height = sample_url.as_ref().map(|_| post.files.sample.height);
    let file_url = post.files.original.url.clone();

    fn opt_int(v: Option<i64>) -> Value {
        v.map_or(Value::Null, Value::Integer)
    }
    fn opt_text(v: Option<String>) -> Value {
        v.map_or(Value::Null, Value::Text)
    }
    fn opt_real(v: Option<f64>) -> Value {
        v.map_or(Value::Null, Value::Real)
    }

    vec![
        Value::Integer(post.id),
        Value::Text(post.created_at.to_rfc3339()),
        Value::Text(post.updated_at.to_rfc3339()),
        Value::Integer(post.stats.score.total),
        Value::Integer(post.stats.fav_count),
        Value::Text(post.rating.to_string()),
        Value::Text(Utc::now().to_rfc3339()),
        opt_text(file_ext),
        opt_int(file_width),
        opt_int(file_height),
        opt_int(file_size),
        Value::Integer(i64::from(post.is_animated())),
        opt_real(post.files.meta.duration),
        Value::Integer(post.stats.comment_count),
        Value::Integer(i64::from(post.has.notes)),
        Value::Integer(i64::from(post.flags.deleted)),
        Value::Integer(i64::from(post.has.children)),
        opt_text(preview_url),
        opt_text(sample_url),
        opt_text(file_url),
        Value::Integer(post.stats.score.up),
        Value::Integer(post.stats.score.down),
        opt_int(sample_width),
        opt_int(sample_height),
        opt_int(preview_width),
        opt_int(preview_height),
        Value::Integer(post.uploader_id),
    ]
}

/// Return catalog posts with incomplete upstream metadata. These can belong
/// to no account (e.g. historic recommendation candidates), so the catalog
/// maintenance worker repairs them independently of `/process`.
pub fn collect_post_ids_needing_hydration(limit: usize) -> Result<Vec<i64>, String> {
    let conn = open_db()?;
    let mut stmt = conn
        .prepare(
            "SELECT id FROM posts
         WHERE is_deleted = 0
           AND (
                (COALESCE(file_url, '') NOT LIKE '%e621.net/%'
                 AND COALESCE(preview_url, '') NOT LIKE '%e621.net/%'
                 AND COALESCE(sample_url, '') NOT LIKE '%e621.net/%')
                OR NOT EXISTS (SELECT 1 FROM tags_posts tp WHERE tp.post_id = posts.id)
                OR COALESCE(uploader_id, 0) = 0
           )
         ORDER BY id DESC LIMIT ?1",
        )
        .map_err(|e| format!("prepare missing-media scan: {e}"))?;
    stmt.query_map([limit.min(100) as i64], |row| row.get(0))
        .map_err(|e| format!("query missing-media scan: {e}"))?
        .collect::<Result<Vec<i64>, _>>()
        .map_err(|e| format!("read missing-media IDs: {e}"))
}

/// Remove catalog posts which e621 has already marked deleted. Cascading FKs
/// remove their tag, account-link, session, and interaction references.
///
/// Posts that are still in somebody's library are **not** purged. The media
/// worker sees `is_deleted = 1` after e621 hides a post, and deleting the row
/// would cascade through `accounts_post` — the post would silently vanish from
/// the owner's catalog and taste profile (the file itself stays on disk). A
/// deleted-but-favourited post is therefore kept; it is excluded from catalog
/// views by `is_deleted` and by the hydration scan, which only selects live
/// rows. The alternative (delete the row *and* its media file) would satisfy
/// the same invariant, but it needs a file deletion in a path that only knows
/// about ids; keeping the row is the smaller change and loses nothing.
pub fn purge_deleted_catalog_posts(limit: usize) -> Result<usize, String> {
    let effects = super::with_write_tx(|tx| {
        let ids: Vec<i64> = {
            let mut stmt = tx
                .prepare(
                    "SELECT p.id FROM posts p \
                     WHERE p.is_deleted = 1 \
                       AND NOT EXISTS (SELECT 1 FROM accounts_post ap WHERE ap.post_id = p.id) \
                     LIMIT ?1",
                )
                .map_err(|e| format!("prepare deleted-post purge: {e}"))?;
            stmt.query_map([limit.min(500) as i64], |r| r.get::<_, i64>(0))
                .map_err(|e| format!("query deleted-post purge: {e}"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("collect deleted-post purge: {e}"))?
        };
        delete_posts_by_ids_in_tx(tx, &ids)
    })?;
    let removed = effects.removed;
    apply_post_delete_effects(effects);
    Ok(removed)
}

/// Remove IDs that the upstream source has authoritatively reported as gone:
/// e621 omits deleted posts from normal searches, and a 404 on a post's
/// original means the bytes no longer exist. Retaining such a row would make
/// every future media repair pass retry an unrecoverable post forever.
///
/// Unlike [`purge_deleted_catalog_posts`] this is a hard delete, cascade and
/// all: the caller has a positive "gone" signal, not just a stale flag. The
/// `is_deleted` flag path (purge) deliberately keeps library rows, because a
/// hidden post is not the same as a deleted file.
pub fn delete_catalog_posts_by_ids(ids: &[i64]) -> Result<usize, String> {
    if ids.is_empty() {
        return Ok(0);
    }
    let effects = super::with_write_tx(|tx| delete_posts_by_ids_in_tx(tx, ids))?;
    let removed = effects.removed;
    apply_post_delete_effects(effects);
    Ok(removed)
}

pub fn upsert_catalog_posts(posts: &[Post]) -> Result<(), String> {
    super::with_write_tx(|tx| upsert_catalog_posts_in_tx(tx, posts))
}

/// Body of [`upsert_catalog_posts`], taking an existing transaction so callers
/// can put the tag postings in the same commit (see
/// [`upsert_catalog_posts_with_tags`](super::upsert_catalog_posts_with_tags)).
pub(super) fn upsert_catalog_posts_in_tx(
    tx: &rusqlite::Connection,
    posts: &[Post],
) -> Result<(), String> {
    let mut insert_post = tx
        .prepare_cached(POST_UPSERT_SQL)
        .map_err(|e| format!("Failed to prepare catalog upsert: {e}"))?;

    for post in posts {
        insert_post
            .execute(rusqlite::params_from_iter(post_upsert_params(post)))
            .map_err(|e| format!("Failed to upsert catalog post: {e}"))?;
    }
    Ok(())
}

/// Fallible counterpart of [`post_count`], for callers that must not panic:
/// the startup gauge seed and the background pruner, where a `panic!` would
/// take down the thread instead of failing one request.
pub fn try_post_count() -> Result<i64, String> {
    let conn = open_db()?;
    conn.query_row("SELECT COUNT(*) FROM posts", [], |row| row.get::<_, i64>(0))
        .map_err(|e| format!("Failed to count catalog posts: {e}"))
}

#[must_use]
pub fn post_count() -> i64 {
    try_post_count().expect("COUNT(*) query failed")
}

/// Reconstructs `Post` structs from the local catalog. Fields not stored
/// locally (sources, pools, alternates) get default values; the scorer and
/// feed UI don't read them.
pub fn hydrate_posts_by_ids(ids: &[i64]) -> Result<Vec<Post>, String> {
    use crate::models::{
        FileMeta, FileOriginal, FilePreview, FileSample, Files, Flags, Has, Rating, Relationships,
        Score, Stats, Tags,
    };

    if ids.is_empty() {
        return Ok(Vec::new());
    }

    let conn = open_db()?;

    // SQLITE_MAX_VARIABLE_NUMBER defaults to 999.
    const CHUNK: usize = 800;
    let mut posts: HashMap<i64, Post> = HashMap::with_capacity(ids.len());

    for chunk in ids.chunks(CHUNK) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "SELECT id, created_at, updated_at, score_total, score_up, score_down, fav_count,
                    rating, file_ext, file_width, file_height, file_size, file_url,
                    preview_url, preview_width, preview_height,
                    sample_url, sample_width, sample_height,
                    is_animated, duration, comment_count, has_notes, is_deleted, has_children, uploader_id
             FROM posts WHERE id IN ({placeholders})"
        );
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| format!("prep hydrate_posts_by_ids: {e}"))?;
        let params_vec: Vec<&dyn rusqlite::ToSql> =
            chunk.iter().map(|v| v as &dyn rusqlite::ToSql).collect();

        let rows = stmt
            .query_map(rusqlite::params_from_iter(params_vec), |row| {
                let id: i64 = row.get(0)?;
                let created_at_raw: String = row.get(1)?;
                let updated_at_default = created_at_raw.clone();
                let updated_at_raw: Option<String> = row.get(2)?;
                let score_total: i64 = row.get(3)?;
                let score_up: i64 = row.get(4)?;
                let score_down: i64 = row.get(5)?;
                let fav_count: i64 = row.get(6)?;
                let rating_raw: String = row.get(7)?;
                let file_ext: Option<String> = row.get(8)?;
                let file_width: Option<i64> = row.get(9)?;
                let file_height: Option<i64> = row.get(10)?;
                let file_size: Option<i64> = row.get(11)?;
                let file_url: Option<String> = row.get(12)?;
                let preview_url: Option<String> = row.get(13)?;
                let preview_width: Option<i64> = row.get(14)?;
                let preview_height: Option<i64> = row.get(15)?;
                let sample_url: Option<String> = row.get(16)?;
                let sample_width: Option<i64> = row.get(17)?;
                let sample_height: Option<i64> = row.get(18)?;
                let _is_animated: i64 = row.get(19)?;
                let duration: Option<f64> = row.get(20)?;
                let comment_count: i64 = row.get(21)?;
                let has_notes: i64 = row.get(22)?;
                let is_deleted: i64 = row.get(23)?;
                let has_children: i64 = row.get(24)?;
                let uploader_id: Option<i64> = row.get(25)?;
                Ok((
                    id,
                    created_at_raw,
                    updated_at_raw.unwrap_or(updated_at_default),
                    score_total,
                    score_up,
                    score_down,
                    fav_count,
                    rating_raw,
                    file_ext,
                    file_width,
                    file_height,
                    file_size,
                    file_url,
                    preview_url,
                    preview_width,
                    preview_height,
                    sample_url,
                    sample_width,
                    sample_height,
                    duration,
                    comment_count,
                    has_notes,
                    is_deleted,
                    has_children,
                    uploader_id,
                ))
            })
            .map_err(|e| format!("query hydrate_posts_by_ids: {e}"))?;

        for r in rows {
            let row = r.map_err(|e| format!("read hydrate row: {e}"))?;
            let (
                id,
                created_at_raw,
                updated_at_raw,
                score_total,
                score_up,
                score_down,
                fav_count,
                rating_raw,
                file_ext,
                file_w,
                file_h,
                file_size,
                file_url,
                preview_url,
                prev_w,
                prev_h,
                sample_url,
                sample_w,
                sample_h,
                duration,
                comment_count,
                has_notes,
                is_deleted,
                has_children,
                uploader_id,
            ) = row;

            let created_at = parse_db_datetime(&created_at_raw)
                .map_err(|e| format!("hydrate post {id} created_at: {e}"))?;
            let updated_at = parse_db_datetime(&updated_at_raw).unwrap_or(created_at);
            let rating = match rating_raw.as_str() {
                "s" => Rating::S,
                "q" => Rating::Q,
                "e" => Rating::E,
                other => return Err(format!("hydrate post {id}: bad rating {other}")),
            };

            posts.insert(
                id,
                Post {
                    id,
                    created_at,
                    updated_at,
                    change_seq: 0.0,
                    files: Files {
                        meta: FileMeta {
                            md5: None,
                            ext: file_ext,
                            size: file_size.unwrap_or(0),
                            duration,
                            has_sample: sample_url.is_some(),
                        },
                        original: FileOriginal {
                            width: file_w.unwrap_or(0),
                            height: file_h.unwrap_or(0),
                            url: file_url,
                        },
                        preview: FilePreview {
                            url: preview_url.clone(),
                            alt: None,
                            width: prev_w.unwrap_or(0),
                            height: prev_h.unwrap_or(0),
                            jpg: preview_url.clone(),
                            webp: None,
                        },
                        sample: FileSample {
                            url: sample_url.clone(),
                            alt: None,
                            width: sample_w.unwrap_or(0),
                            height: sample_h.unwrap_or(0),
                            jpg: sample_url.clone(),
                            webp: None,
                        },
                    },
                    uploader_id: uploader_id.unwrap_or(0),
                    uploader_name: None,
                    approver_id: None,
                    stats: Stats {
                        score: Score {
                            up: score_up,
                            down: score_down,
                            total: score_total,
                        },
                        fav_count,
                        is_favorited: false,
                        vote: 0,
                        comment_count,
                    },
                    flags: Flags {
                        pending: false,
                        flagged: false,
                        note_locked: false,
                        status_locked: false,
                        rating_locked: false,
                        deleted: is_deleted != 0,
                    },
                    has: Has {
                        parent: false,
                        children: has_children != 0,
                        active_children: false,
                        notes: has_notes != 0,
                        sample: sample_url.is_some(),
                    },
                    relationships: Relationships {
                        parent_id: None,
                        children: Vec::new(),
                    },
                    pools: Vec::new(),
                    rating,
                    locked_tags: Vec::new(),
                    sources: Vec::new(),
                    description: None,
                    tags: Tags::default(),
                },
            );
        }
    }

    if posts.is_empty() {
        return Ok(Vec::new());
    }

    // Single tag query for all posts at once — much cheaper than N round-trips.
    let post_ids: Vec<i64> = posts.keys().copied().collect();
    for chunk in post_ids.chunks(CHUNK) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "SELECT tp.post_id, t.name, t.group_type
             FROM tags_posts tp
             INNER JOIN tags t ON t.id = tp.tag_id
             WHERE tp.post_id IN ({placeholders})"
        );
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| format!("prep hydrate tags: {e}"))?;
        let params_vec: Vec<&dyn rusqlite::ToSql> =
            chunk.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params_vec), |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(|e| format!("query hydrate tags: {e}"))?;
        for r in rows {
            let (pid, name, group) = r.map_err(|e| format!("read hydrate tag row: {e}"))?;
            let Some(post) = posts.get_mut(&pid) else {
                continue;
            };
            match group.as_str() {
                "artist" => post.tags.artist.push(name),
                "character" => post.tags.character.push(name),
                "copyright" => post.tags.copyright.push(name),
                "general" => post.tags.general.push(name),
                "lore" => post.tags.lore.push(name),
                "meta" => post.tags.meta.push(name),
                "species" => post.tags.species.push(name),
                _ => {}
            }
        }
    }

    // Deterministic order: return posts in the same order as the input `ids`.
    // Callers pass an already-ordered list (e.g. `post_id DESC` for the
    // catalog, score order for browse); `HashMap::into_values()` iteration is
    // nondeterministic and would shuffle the page between requests.
    let mut out: Vec<Post> = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(p) = posts.remove(id) {
            out.push(p);
        }
    }
    // Rewrite media URLs to the local proxy for posts with a stored original
    // (docs/offline-catalog.md).
    crate::media_store::rewrite_local_media_urls(&mut out);
    Ok(out)
}

/// Find post IDs similar to `post_id` via tag overlap. Returns candidates
/// that share at least `min_overlap` tags with the source post, ordered by
/// overlap count DESC, `score_total` DESC. Excludes owned and recently-seen
/// posts. Supports pagination via `page` / `limit`.
pub fn find_similar_post_ids(
    post_id: i64,
    account_id: i32,
    min_overlap: i32,
    limit: i64,
    page: i64,
) -> Result<Vec<i64>, String> {
    let conn = open_db()?;
    let offset = page.saturating_sub(1).max(0) * limit;
    let mut stmt = conn
        .prepare(
            "
            SELECT p.id
            FROM posts p
            INNER JOIN tags_posts tp ON tp.post_id = p.id
            WHERE tp.tag_id IN (
                SELECT tp2.tag_id FROM tags_posts tp2 WHERE tp2.post_id = ?1
            )
              AND p.id != ?1
              AND p.is_deleted = 0
              AND p.preview_url IS NOT NULL
              AND NOT EXISTS (SELECT 1 FROM accounts_post ap WHERE ap.account_id = ?2 AND ap.post_id = p.id)
              AND NOT EXISTS (
                  SELECT 1 FROM feed_interactions fi
                  WHERE fi.account_id = ?2 AND fi.post_id = p.id
                    AND fi.event_type IN ('qualified_impression', 'hide', 'open')
              )
            GROUP BY p.id
            HAVING COUNT(DISTINCT tp.tag_id) >= ?3
            ORDER BY COUNT(DISTINCT tp.tag_id) DESC, p.score_total DESC
            LIMIT ?4 OFFSET ?5
            ",
        )
        .map_err(|e| format!("Failed to prepare similar posts query: {e}"))?;

    let rows = stmt
        .query_map(
            params![post_id, account_id, min_overlap, limit, offset],
            |r| r.get::<_, i64>(0),
        )
        .map_err(|e| format!("Failed to query similar posts: {e}"))?;

    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect similar posts: {e}"))
}

/// Fetch a single post by ID from the local DB. Returns None if not found.
///
/// Reuses `hydrate_posts_by_ids` so the returned post has its tag groups
/// populated — the original hand-rolled version left `tags.*` as empty
/// vecs, which broke `post_pair_similarity` (every score collapsed to 0)
/// and made `/posts/<id>/similar` return `[]` for any locally-cached
/// post.
pub fn get_post_by_id(post_id: i64) -> Result<Option<Post>, String> {
    let mut posts = hydrate_posts_by_ids(&[post_id])?;
    Ok(posts.pop())
}
