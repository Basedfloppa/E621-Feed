use chrono::Utc;
use rusqlite::params;
use std::collections::HashMap;

use crate::models::Post;

use super::{open_db, open_db_for_prefetch, parse_db_datetime};

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
/// Marks `IDF` and `GLOBAL` caches dirty when anything was deleted, so the
/// in-memory graphs shrink on the next worker tick. Mirror of the longer-
/// lived `prune_stale_catalog_posts` — both are now unified in `cache_pruner.rs`.
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

    let deleted = super::with_write_tx(|tx| {
        let n = tx
            .execute(
                "
                DELETE FROM posts
                WHERE last_seen_at < ?1
                  AND NOT EXISTS (SELECT 1 FROM accounts_post ap WHERE ap.post_id = posts.id)
                ",
                params![cutoff],
            )
            .map_err(|e| format!("prune_orphan_candidates delete: {e}"))?;
        Ok(n)
    })?;

    if deleted > 0 {
        crate::utils::mark_idf_dirty();
        crate::utils::mark_global_relation_dirty();
    }

    let after = (before as usize).saturating_sub(deleted);
    Ok((before as usize, after))
}

/// Longer-lived catalog cleanup run on `cleanup_interval_secs` (default 6h).
/// Deletes posts that aren't favourited by anyone and haven't been re-touched
/// within `catalog_retention_days`. Unlike `prune_orphan_candidates` (which
/// churns aggressively on browse-time inserts), this is the belt-and-suspenders
/// bound that ensures the catalog can't grow without limit.
///
/// Returns the number of deleted posts. Marks IDF + global-relation dirty when
/// anything was pruned so in-memory caches shrink on the next rebuild.
pub fn prune_stale_catalog_posts(retention_days: i64) -> Result<i64, String> {
    let conn = open_db_for_prefetch()?;
    let cutoff = (Utc::now() - chrono::Duration::days(retention_days)).to_rfc3339();

    let deleted = conn
        .execute(
            "
            DELETE FROM posts
            WHERE last_seen_at < ?1
              AND NOT EXISTS (SELECT 1 FROM accounts_post ap WHERE ap.post_id = posts.id)
            ",
            params![cutoff],
        )
        .map_err(|e| format!("delete stale catalog posts: {e}"))?;

    if deleted > 0 {
        crate::utils::mark_idf_dirty();
        crate::utils::mark_global_relation_dirty();
    }

    Ok(deleted as i64)
}

pub fn drop_account_posts(account_id: i32) -> Result<(), String> {
    super::with_write_tx(|tx| {
        tx.execute(
            "DELETE FROM accounts_post WHERE account_id = ?1",
            params![account_id],
        )
        .map_err(|e| format!("Failed to clear accounts_post: {e}"))?;
        tx.execute(
            "DELETE FROM account_tag_cooccurrence WHERE account_id = ?1",
            params![account_id],
        )
        .map_err(|e| format!("Failed to clear account_tag_cooccurrence: {e}"))?;
        Ok(())
    })
}

pub fn save_posts(posts: &[Post], account_id: i32) -> Result<(), String> {
    super::with_write_tx(|tx| {
        let mut insert_post = tx
            .prepare_cached(POST_UPSERT_SQL)
            .map_err(|e| format!("Failed to prepare transaction: {e}"))?;
        let mut insert_account = tx
            .prepare_cached(
                "INSERT OR IGNORE INTO accounts_post (account_id, post_id) VALUES (?1, ?2);",
            )
            .map_err(|e| format!("Failed to prepare transaction: {e}"))?;

        for post in posts {
            insert_post
                .execute(rusqlite::params_from_iter(post_upsert_params(post)))
                .map_err(|e| format!("Failed to execute transaction: {e}"))?;

            insert_account
                .execute(params![account_id, post.id])
                .map_err(|e| format!("Failed to execute transaction: {e}"))?;
        }
        Ok(())
    })
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

    let file_ext = post.file.as_ref().and_then(|f| f.ext.clone());
    let file_width = post.file.as_ref().map(|f| f.width);
    let file_height = post.file.as_ref().map(|f| f.height);
    let file_size = post.file.as_ref().map(|f| f.size);
    let preview_url = post.preview.as_ref().and_then(|p| p.url.clone());
    let preview_width = post.preview.as_ref().map(|p| p.width);
    let preview_height = post.preview.as_ref().map(|p| p.height);
    let sample_url = post.sample.as_ref().and_then(|s| s.url.clone());
    let sample_width = post.sample.as_ref().and_then(|s| s.width);
    let sample_height = post.sample.as_ref().and_then(|s| s.height);
    let file_url = post.file.as_ref().and_then(|f| f.url.clone());

    fn opt_int(v: Option<i64>) -> Value {
        v.map(Value::Integer).unwrap_or(Value::Null)
    }
    fn opt_text(v: Option<String>) -> Value {
        v.map(Value::Text).unwrap_or(Value::Null)
    }
    fn opt_real(v: Option<f64>) -> Value {
        v.map(Value::Real).unwrap_or(Value::Null)
    }

    vec![
        Value::Integer(post.id),
        Value::Text(post.created_at.to_rfc3339()),
        Value::Text(post.updated_at.to_rfc3339()),
        Value::Integer(post.score.total),
        Value::Integer(post.fav_count),
        Value::Text(post.rating.to_string()),
        Value::Text(Utc::now().to_rfc3339()),
        opt_text(file_ext),
        opt_int(file_width),
        opt_int(file_height),
        opt_int(file_size),
        Value::Integer(if post.is_animated() { 1 } else { 0 }),
        opt_real(post.duration),
        Value::Integer(post.comment_count),
        Value::Integer(if post.has_notes { 1 } else { 0 }),
        Value::Integer(if post.flags.deleted { 1 } else { 0 }),
        Value::Integer(if post.relationships.has_children {
            1
        } else {
            0
        }),
        opt_text(preview_url),
        opt_text(sample_url),
        opt_text(file_url),
        Value::Integer(post.score.up),
        Value::Integer(post.score.down),
        opt_int(sample_width),
        opt_int(sample_height),
        opt_int(preview_width),
        opt_int(preview_height),
        Value::Integer(post.uploader_id),
    ]
}

pub fn upsert_catalog_posts(posts: &[Post]) -> Result<(), String> {
    super::with_write_tx(|tx| {
        let mut insert_post = tx
            .prepare_cached(POST_UPSERT_SQL)
            .map_err(|e| format!("Failed to prepare catalog upsert: {e}"))?;

        for post in posts {
            insert_post
                .execute(rusqlite::params_from_iter(post_upsert_params(post)))
                .map_err(|e| format!("Failed to upsert catalog post: {e}"))?;
        }
        Ok(())
    })
}

pub fn post_count() -> i64 {
    let conn = open_db().expect("open_db failed");
    conn.query_row("SELECT COUNT(*) FROM posts", [], |row| row.get::<_, i64>(0))
        .expect("COUNT(*) query failed")
}

/// Reconstructs `Post` structs from the local catalog. Fields not stored
/// locally (sources, pools, alternates) get default values; the scorer and
/// feed UI don't read them.
pub fn hydrate_posts_by_ids(ids: &[i64]) -> Result<Vec<Post>, String> {
    use crate::models::{FileInfo, Flags, Preview, Rating, Relationships, Sample, Score, Tags};

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
                    is_animated, duration, comment_count, has_notes, is_deleted, has_children
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
                let updated_at_raw: String = row.get(2)?;
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
                Ok((
                    id,
                    created_at_raw,
                    updated_at_raw,
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

            let file = if file_url.is_some() || file_ext.is_some() {
                Some(FileInfo {
                    width: file_w.unwrap_or(0),
                    height: file_h.unwrap_or(0),
                    ext: file_ext,
                    size: file_size.unwrap_or(0),
                    md5: None,
                    url: file_url,
                })
            } else {
                None
            };
            let preview = preview_url.as_ref().map(|url| Preview {
                width: prev_w.unwrap_or(0),
                height: prev_h.unwrap_or(0),
                url: Some(url.clone()),
            });
            let sample = sample_url.as_ref().map(|url| Sample {
                has: Some(true),
                width: sample_w,
                height: sample_h,
                url: Some(url.clone()),
                alternates: None,
                variants: None,
                samples: None,
            });

            posts.insert(
                id,
                Post {
                    id,
                    created_at,
                    updated_at,
                    file,
                    preview,
                    sample,
                    score: Score {
                        up: score_up,
                        down: score_down,
                        total: score_total,
                    },
                    tags: Tags {
                        general: Vec::new(),
                        artist: Vec::new(),
                        copyright: Vec::new(),
                        character: Vec::new(),
                        species: Vec::new(),
                        invalid: Vec::new(),
                        meta: Vec::new(),
                        lore: Vec::new(),
                        contributor: Vec::new(),
                    },
                    locked_tags: None,
                    change_seq: 0.0,
                    flags: Flags {
                        pending: false,
                        flagged: false,
                        note_locked: false,
                        status_locked: false,
                        rating_locked: false,
                        deleted: is_deleted != 0,
                    },
                    rating,
                    fav_count,
                    sources: Vec::new(),
                    pools: Vec::new(),
                    relationships: Relationships {
                        parent_id: None,
                        has_children: has_children != 0,
                        has_active_children: false,
                        children: Vec::new(),
                    },
                    approver_id: None,
                    uploader_id: 0,
                    description: None,
                    comment_count,
                    is_favorited: false,
                    has_notes: has_notes != 0,
                    duration,
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

    Ok(posts.into_values().collect())
}
