use rusqlite::params;
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};

use crate::models::{Post, TagCount};

use super::open_db;

/// TTL cache for `get_tag_counts`. Tag counts only change during `/process`
/// (profile refresh), which calls `clear_tag_counts_cache()`. Between
/// refreshes they're static, so a short TTL (30s) saves redundant PK lookups
/// during infinite-scroll pagination.
struct CachedTagCounts {
    counts: Vec<TagCount>,
    inserted_at: std::time::Instant,
}

static TAG_COUNTS_CACHE: LazyLock<Mutex<HashMap<i32, CachedTagCounts>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Cache TTL for tag counts. 30 seconds is long enough to dedup
/// infinite-scroll pages for the same account, short enough that stale
/// data after a `/process` refresh resolves within half a minute.
const TAG_COUNTS_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(30);

/// Clear the tag counts cache for a specific account.
/// Called after `/process` updates an account's tag counts.
pub fn clear_tag_counts_cache(account_id: i32) {
    if let Ok(mut cache) = TAG_COUNTS_CACHE.lock() {
        cache.remove(&account_id);
    }
}

/// Clear the entire tag counts cache. Used when bulk operations may have
/// invalidated multiple accounts at once.
pub fn clear_all_tag_counts_caches() {
    if let Ok(mut cache) = TAG_COUNTS_CACHE.lock() {
        cache.clear();
    }
}

pub fn save_posts_tags_batch(
    posts: &[Post],
    blacklist: &HashSet<String>,
    track_cooccurrence: bool,
    account_id: Option<i32>,
) -> Result<(), String> {
    if posts.is_empty() {
        return Ok(());
    }

    let mut cooc_dirty = false;
    let mut touched_tag_ids: HashSet<i64> = HashSet::new();
    // tag_id → number of (tag_id, post_id) rows actually inserted in this
    // batch. `INSERT OR IGNORE` returns 0 on duplicate, so summing
    // rows_affected gives us "new postings only" — that's the per-tag df
    // delta we publish to the in-memory IDF index after commit.
    let mut df_delta_by_id: HashMap<i64, i64> = HashMap::new();
    // Per-batch cache: ~5–10K tag references but only a few hundred distinct
    // (name, group) pairs in a 320-post batch. Caching collapses repeats to
    // a HashMap hit and avoids the upsert+RETURNING round-trip.
    let mut tag_id_cache: HashMap<(String, &'static str), i64> = HashMap::new();
    // Reverse map: tag_id → (name, group) for incremental account cooc.
    // Built at the end of the post loop when account_id is Some.
    let mut tag_id_to_meta: HashMap<i64, (String, &'static str)> = HashMap::new();

    super::with_write_tx(|tx| {
        // The `DO UPDATE SET name = name` no-op forces RETURNING to fire on
        // the conflict path, so we always get a row back.
        let mut upsert_tag = tx
            .prepare_cached(
                "INSERT INTO tags (name, group_type) VALUES (?1, ?2)
                 ON CONFLICT(name, group_type) DO UPDATE SET name = name
                 RETURNING id",
            )
            .map_err(|e| format!("prep upsert tag: {e}"))?;
        let mut link = tx
            .prepare_cached("INSERT OR IGNORE INTO tags_posts(tag_id, post_id) VALUES (?1, ?2)")
            .map_err(|e| format!("prep link: {e}"))?;
        let mut post_has_tags = tx
            .prepare_cached("SELECT EXISTS(SELECT 1 FROM tags_posts WHERE post_id = ?1)")
            .map_err(|e| format!("prep post_has_tags: {e}"))?;

        for post in posts {
            let pid = post.id;
            let had_tags: bool = if track_cooccurrence {
                post_has_tags
                    .query_row(params![pid], |r| r.get(0))
                    .map_err(|e| format!("post_has_tags lookup: {e}"))?
            } else {
                true
            };

            let mut post_tag_ids: Vec<i64> = Vec::new();

            // Meta is stored so the interaction channel can read it; the
            // other scorers filter meta out via group_wts[Meta] = 0.
            for (group, tags) in [
                ("artist", &post.tags.artist),
                ("character", &post.tags.character),
                ("copyright", &post.tags.copyright),
                ("general", &post.tags.general),
                ("lore", &post.tags.lore),
                ("species", &post.tags.species),
                ("meta", &post.tags.meta),
            ] {
                for tag in tags {
                    if tag.is_empty() {
                        continue;
                    }
                    let tag_lc = if tag.bytes().any(|b| b.is_ascii_uppercase()) {
                        tag.to_ascii_lowercase()
                    } else {
                        tag.clone()
                    };
                    if blacklist.contains(&tag_lc) {
                        continue;
                    }

                    let cache_key = (tag_lc, group);
                    let tag_id = if let Some(&id) = tag_id_cache.get(&cache_key) {
                        id
                    } else {
                        let id: i64 = upsert_tag
                            .query_row(params![&cache_key.0, group], |r| r.get(0))
                            .map_err(|e| format!("upsert tag {}:{group}: {e}", cache_key.0))?;
                        tag_id_cache.insert(cache_key, id);
                        id
                    };

                    let inserted = link
                        .execute(params![tag_id, pid])
                        .map_err(|e| format!("link tag_id={tag_id} post_id={pid}: {e}"))?;

                    if inserted > 0 {
                        *df_delta_by_id.entry(tag_id).or_insert(0) += inserted as i64;
                    }
                    touched_tag_ids.insert(tag_id);
                    post_tag_ids.push(tag_id);
                }
            }

            let has_multi_tags = post_tag_ids.len() >= 2;
            // Sort+dedup once per post — unconditional so the account-cooc
            // branch below can never see duplicate tag_ids (which would
            // produce phantom self-pairs in `account_tag_cooccurrence`).
            // The previous "skip if cooc_dirty" optimisation conflated a
            // batch-level flag with a per-post invariant: once any earlier
            // post had triggered the global branch, this branch would skip
            // the sort+dedup on every subsequent post regardless of whether
            // those posts had been sorted.
            if has_multi_tags {
                post_tag_ids.sort_unstable();
                post_tag_ids.dedup();
            }

            if track_cooccurrence && !had_tags && has_multi_tags {
                super::cooccurrence::upsert_cooccurrence_pairs(tx, &post_tag_ids)?;
                cooc_dirty = true;
            }
            // Class G: incremental account-level cooccurrence update.
            // Since we cleared account_tag_cooccurrence in drop_account_posts,
            // every post's tag pairs are fresh; ON CONFLICT handles dedup.
            // Build the reverse map incrementally as we discover new tags.
            for &tid in &post_tag_ids {
                if !tag_id_to_meta.contains_key(&tid)
                    && let Some(((name, group), _)) = tag_id_cache.iter().find(|&(_, &v)| v == tid) {
                        tag_id_to_meta.insert(tid, (name.clone(), *group));
                    }
            }
            if account_id.is_some() && track_cooccurrence && has_multi_tags {
                // Build the per-post meta map from our global reverse map.
                let id_to_meta: std::collections::HashMap<i64, (String, String)> = post_tag_ids
                    .iter()
                    .filter_map(|&tid| {
                        tag_id_to_meta
                            .get(&tid)
                            .map(|(n, g)| (tid, (n.clone(), g.to_string())))
                    })
                    .collect();
                super::cooccurrence::upsert_account_cooccurrence_pairs(
                    tx,
                    account_id.unwrap(),
                    &post_tag_ids,
                    &id_to_meta,
                )?;
            }
        }

        if !touched_tag_ids.is_empty() {
            recompute_df_for_tags(tx, &touched_tag_ids)?;
        }
        Ok(())
    })?;

    // Build the (lowercased name → df_delta) map after commit. The
    // tag_id_cache and df_delta_by_id maps survive past the closure, so
    // we still have everything we need to bump the in-memory IDF index.
    let df_delta_by_name: HashMap<String, i64> = if df_delta_by_id.is_empty() {
        HashMap::new()
    } else {
        let mut id_to_name: HashMap<i64, String> = HashMap::with_capacity(tag_id_cache.len());
        for ((name, _group), id) in &tag_id_cache {
            id_to_name.entry(*id).or_insert_with(|| name.clone());
        }
        df_delta_by_id
            .iter()
            .filter_map(|(id, delta)| id_to_name.get(id).map(|n| (n.clone(), *delta)))
            .collect()
    };

    // n_posts_delta = 0: catalog growth shifts every IDF by a tiny fraction,
    // and the periodic full rebuild reconciles the absolute level.
    if !df_delta_by_name.is_empty() {
        crate::utils::bump_idf(df_delta_by_name, 0);
    }

    if cooc_dirty {
        crate::utils::mark_global_relation_dirty();
    }
    Ok(())
}

pub(super) fn recompute_df_for_tags(
    tx: &rusqlite::Transaction,
    tag_ids: &HashSet<i64>,
) -> Result<(), String> {
    if tag_ids.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = tag_ids.iter().copied().collect();
    for chunk in ids.chunks(500) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "UPDATE tags
                SET df = (SELECT COUNT(*) FROM tags_posts WHERE tag_id = tags.id)
              WHERE id IN ({placeholders})"
        );
        let mut stmt = tx.prepare(&sql).map_err(|e| format!("prep df: {e}"))?;
        let params_vec: Vec<&dyn rusqlite::ToSql> =
            chunk.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
        stmt.execute(rusqlite::params_from_iter(params_vec))
            .map_err(|e| format!("exec df: {e}"))?;
    }
    Ok(())
}

pub fn set_tag_counts(account_id: i32) -> Result<(), String> {
    let connection = open_db()?;

    let counts: Vec<TagCount> = {
        let mut stmt = connection
            .prepare(
                r#"
        SELECT t.name, t.group_type, COUNT(*) as count
        FROM tags t
        INNER JOIN tags_posts tp ON t.id = tp.tag_id
        INNER JOIN accounts_post ap ON tp.post_id = ap.post_id
        WHERE ap.account_id = ?
        GROUP BY t.name, t.group_type
        ORDER BY count DESC
        "#,
            )
            .map_err(|e| format!("Failed to construct query: {e}"))?;

        stmt.query_map([account_id], |row| {
            Ok(TagCount {
                name: row.get(0)?,
                group_type: row.get(1)?,
                count: row.get(2)?,
            })
        })
        .map_err(|e| format!("Failed to get accounts: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to enumerate accounts: {e}"))?
    };

    drop(connection);

    super::with_write_tx(|tx| {
        tx.execute(
            "DELETE FROM account_tag_counts WHERE account_id = ?1",
            params![account_id],
        )
        .map_err(|e| format!("Failed to delete account_tag_counts: {e}"))?;

        let mut insert_calc = tx
            .prepare_cached(
                "
        INSERT INTO account_tag_counts (account_id, tag_name, group_type, count)
        VALUES (?1, ?2, ?3, ?4)
        ON CONFLICT(account_id, tag_name, group_type) DO UPDATE SET
        count = excluded.count;
        ",
            )
            .map_err(|e| format!("Failed to prepare transaction: {e}"))?;

        for entry in counts {
            insert_calc
                .execute(params![
                    account_id,
                    entry.name,
                    entry.group_type,
                    entry.count
                ])
                .map_err(|e| format!("Failed to execute transaction: {e}"))?;
        }
        Ok(())
    })
}

pub fn get_tag_counts(account_id: i32) -> Result<Vec<TagCount>, String> {
    // Check TTL cache first (dedups repeated lookups for the same account
    // within a 30-second window, e.g. infinite-scroll pagination).
    if let Ok(cache) = TAG_COUNTS_CACHE.lock()
        && let Some(entry) = cache.get(&account_id)
            && entry.inserted_at.elapsed() < TAG_COUNTS_CACHE_TTL {
                return Ok(entry.counts.clone());
            }

    let conn = open_db()?;

    let mut stmt = conn
        .prepare("SELECT * FROM account_tag_counts WHERE account_id = ?")
        .map_err(|e| format!("Failed to construct query: {e}"))?;

    let counts = stmt
        .query_map([account_id], |row| {
            Ok(TagCount {
                name: row.get(1)?,
                group_type: row.get(2)?,
                count: row.get(3)?,
            })
        })
        .map_err(|e| format!("Failed to get accounts: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to enumerate accounts: {e}"))?;

    // Store in cache before returning.
    if let Ok(mut cache) = TAG_COUNTS_CACHE.lock() {
        cache.insert(
            account_id,
            CachedTagCounts {
                counts: counts.clone(),
                inserted_at: std::time::Instant::now(),
            },
        );
    }

    Ok(counts)
}

pub fn get_tags_df() -> rusqlite::Result<HashMap<String, i64>> {
    let conn = open_db().unwrap();
    let mut stmt = conn.prepare("SELECT name, df FROM tags")?;

    let mut map = HashMap::new();
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;

    for pair in rows {
        let (name, df) = pair?;
        map.insert(name, df);
    }

    Ok(map)
}
