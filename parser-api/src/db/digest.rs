//! Database queries for the Daily digest feature.
//!
//! Provides trending / popular / random post lookups that the
//! `/digest` endpoint combines into a lightweight page.
//!
//! All queries read from `catalog_posts` (the `posts` table) and
//! avoid touching the scoring pipeline, keeping the generic-fallback
//! path cheap even for accounts with millions of interactions.

use chrono::{DateTime, Utc};
use rusqlite::params;

use crate::models::ScoredPost;

use super::{hydrate_posts_by_ids, open_db};

fn ids_to_scored(ids: &[i64]) -> Result<Vec<ScoredPost>, String> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let posts = hydrate_posts_by_ids(ids)?;
    Ok(posts
        .into_iter()
        .map(|post| ScoredPost {
            post,
            score: 0.0,
            breakdown: None,
        })
        .collect())
}

/// Trending posts from the last `days` days, ordered by `score_total DESC`.
pub fn get_trending_posts(days: i64, limit: usize) -> Result<Vec<ScoredPost>, String> {
    let conn = open_db()?;
    let cutoff = (Utc::now() - chrono::Duration::days(days)).to_rfc3339();
    let mut stmt = conn
        .prepare("SELECT id FROM posts WHERE created_at >= ?1 ORDER BY score_total DESC LIMIT ?2")
        .map_err(|e| format!("get_trending_posts prep: {e}"))?;
    let ids: Vec<i64> = stmt
        .query_map(params![cutoff, limit as i64], |r| r.get::<_, i64>(0))
        .map_err(|e| format!("get_trending_posts query: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("get_trending_posts collect: {e}"))?;
    ids_to_scored(&ids)
}

/// Popular posts created since `since`, ordered by `score_total DESC`.
pub fn get_popular_posts_since(
    since: DateTime<Utc>,
    limit: usize,
) -> Result<Vec<ScoredPost>, String> {
    let conn = open_db()?;
    let since_str = since.to_rfc3339();
    let mut stmt = conn
        .prepare("SELECT id FROM posts WHERE created_at >= ?1 ORDER BY score_total DESC LIMIT ?2")
        .map_err(|e| format!("get_popular_posts_since prep: {e}"))?;
    let ids: Vec<i64> = stmt
        .query_map(params![since_str, limit as i64], |r| r.get::<_, i64>(0))
        .map_err(|e| format!("get_popular_posts_since query: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("get_popular_posts_since collect: {e}"))?;
    ids_to_scored(&ids)
}

/// Random posts from the catalog, using SQLite's built-in RANDOM().
pub fn get_random_posts(limit: usize) -> Result<Vec<ScoredPost>, String> {
    let conn = open_db()?;
    let mut stmt = conn
        .prepare("SELECT id FROM posts ORDER BY RANDOM() LIMIT ?1")
        .map_err(|e| format!("get_random_posts prep: {e}"))?;
    let ids: Vec<i64> = stmt
        .query_map(params![limit as i64], |r| r.get::<_, i64>(0))
        .map_err(|e| format!("get_random_posts query: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("get_random_posts collect: {e}"))?;
    ids_to_scored(&ids)
}

/// Random posts with preference for groups the user has engaged with.
/// Picks a random tag group from the user's history, then finds posts
/// matching tags from that group.
pub fn get_random_posts_by_group(account_id: i32, limit: usize) -> Result<Vec<ScoredPost>, String> {
    let conn = open_db()?;
    // Pick a random group using SQLite RANDOM().
    let group: Option<String> = conn
        .query_row(
            "SELECT DISTINCT group_type FROM account_tag_feedback \
             WHERE account_id = ?1 AND impression_count > 0 \
             ORDER BY RANDOM() LIMIT 1",
            params![account_id],
            |r| r.get::<_, String>(0),
        )
        .ok();

    let group = match group {
        Some(g) => g,
        None => return get_random_posts(limit),
    };

    // Get some tag IDs the user has interacted with in this group.
    let tag_ids: Vec<i64> = {
        let mut stmt = conn
            .prepare(
                "SELECT tag_id FROM account_tag_feedback \
                 WHERE account_id = ?1 AND group_type = ?2 AND impression_count > 0 \
                 ORDER BY RANDOM() LIMIT 3",
            )
            .map_err(|e| format!("get_random_posts_by_group tag_ids prep: {e}"))?;
        stmt.query_map(params![account_id, group], |r| r.get::<_, i64>(0))
            .map_err(|e| format!("get_random_posts_by_group tag_ids query: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("get_random_posts_by_group tag_ids collect: {e}"))?
    };

    if tag_ids.is_empty() {
        return get_random_posts(limit);
    }

    // Build dynamic SQL with placeholders for tag_ids.
    let placeholders: Vec<String> = tag_ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 2))
        .collect();
    let sql = format!(
        "SELECT DISTINCT tp.post_id FROM tags_posts tp \
         INNER JOIN posts p ON p.id = tp.post_id \
         WHERE tp.tag_id IN ({}) \
         ORDER BY RANDOM() LIMIT ?1",
        placeholders.join(",")
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("get_random_posts_by_group posts prep: {e}"))?;

    // Build dynamic params.
    let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> =
        Vec::with_capacity(tag_ids.len() + 1);
    params_vec.push(Box::new(limit as i64));
    for tid in &tag_ids {
        params_vec.push(Box::new(*tid));
    }
    let params_refs: Vec<&dyn rusqlite::types::ToSql> =
        params_vec.iter().map(|p| p.as_ref()).collect();

    let ids: Vec<i64> = stmt
        .query_map(params_refs.as_slice(), |r| r.get::<_, i64>(0))
        .map_err(|e| format!("get_random_posts_by_group posts query: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("get_random_posts_by_group posts collect: {e}"))?;

    if ids.is_empty() {
        return get_random_posts(limit);
    }
    ids_to_scored(&ids)
}
