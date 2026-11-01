//! Watchlist: per-account tag subscriptions with a seen high-water mark.
//!
//! "New" is `post_id > last_seen_post_id`, not a timestamp: e621 post ids are
//! monotonic, the feed already orders by them, and a late-imported old post
//! must not resurface as new. The mark is seeded at subscribe time with the
//! newest post that already carries the tag, so a fresh subscription starts
//! empty instead of reporting the whole catalog.

use chrono::Utc;
use rusqlite::params;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::errors::NOT_FOUND_ERR_MARKER;
use crate::models::ScoredPost;

use super::{hydrate_posts_by_ids, open_db};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Subscription {
    pub tag_id: i64,
    pub tag_name: String,
    pub group_type: String,
    pub created_at: String,
    pub last_seen_post_id: i64,
    /// Non-deleted posts carrying the tag with an id above the mark.
    pub new_count: i64,
}

const SELECT_SUBSCRIPTIONS: &str = "
    SELECT s.tag_id, t.name, t.group_type, s.created_at, s.last_seen_post_id,
           (SELECT COUNT(*)
              FROM tags_posts tp
              JOIN posts p ON p.id = tp.post_id
             WHERE tp.tag_id = s.tag_id
               AND tp.post_id > s.last_seen_post_id
               AND p.is_deleted = 0)
      FROM account_subscriptions s
      JOIN tags t ON t.id = s.tag_id
     WHERE s.account_id = ?1
     ORDER BY t.name
";

fn row_to_subscription(row: &rusqlite::Row<'_>) -> rusqlite::Result<Subscription> {
    Ok(Subscription {
        tag_id: row.get(0)?,
        tag_name: row.get(1)?,
        group_type: row.get(2)?,
        created_at: row.get(3)?,
        last_seen_post_id: row.get(4)?,
        new_count: row.get(5)?,
    })
}

/// Subscribe `account_id` to `tag`. Subscribing twice is a no-op that keeps the
/// existing high-water mark — re-adding a tag must not replay its history.
pub fn add_subscription(account_id: i32, tag: &str) -> Result<Subscription, String> {
    let name = tag.trim().to_ascii_lowercase();
    if name.is_empty() {
        return Err("Tag name is empty".to_string());
    }
    let conn = open_db()?;
    let (tag_id, group_type): (i64, String) = conn
        .query_row(
            "SELECT id, group_type FROM tags WHERE name = ?1",
            params![name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|e| match e {
            // Marker → 404: a tag this instance has never synced is a client
            // mistake, not a server failure.
            rusqlite::Error::QueryReturnedNoRows => format!(
                "{NOT_FOUND_ERR_MARKER}Unknown tag '{name}': it is not in the local catalog yet. Subscribe to a tag that appears in a post you have synced."
            ),
            other => format!("Lookup tag '{name}': {other}"),
        })?;
    let seed: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(post_id), 0) FROM tags_posts WHERE tag_id = ?1",
            params![tag_id],
            |r| r.get(0),
        )
        .map_err(|e| format!("Seed high-water mark: {e}"))?;
    conn.execute(
        "INSERT OR IGNORE INTO account_subscriptions
             (account_id, tag_id, created_at, last_seen_post_id)
         VALUES (?1, ?2, ?3, ?4)",
        params![account_id, tag_id, Utc::now().to_rfc3339(), seed],
    )
    .map_err(|e| format!("Insert subscription: {e}"))?;
    list_subscriptions(account_id)?
        .into_iter()
        .find(|s| s.tag_id == tag_id)
        .ok_or_else(|| format!("Subscription to '{name}' ({group_type}) disappeared after insert"))
}

pub fn list_subscriptions(account_id: i32) -> Result<Vec<Subscription>, String> {
    let conn = open_db()?;
    let mut stmt = conn
        .prepare(SELECT_SUBSCRIPTIONS)
        .map_err(|e| format!("list_subscriptions prep: {e}"))?;
    let rows = stmt
        .query_map(params![account_id], row_to_subscription)
        .map_err(|e| format!("list_subscriptions query: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("list_subscriptions collect: {e}"))
}

/// Returns `true` when a row was removed.
pub fn remove_subscription(account_id: i32, tag_id: i64) -> Result<bool, String> {
    let conn = open_db()?;
    let removed = conn
        .execute(
            "DELETE FROM account_subscriptions WHERE account_id = ?1 AND tag_id = ?2",
            params![account_id, tag_id],
        )
        .map_err(|e| format!("remove_subscription: {e}"))?;
    Ok(removed > 0)
}

/// Move the mark to the newest post carrying each tag. Passing `tag_id` marks
/// one subscription; `None` marks them all (the "I have looked" button).
/// Returns the number of rows touched.
pub fn mark_seen(account_id: i32, tag_id: Option<i64>) -> Result<usize, String> {
    let conn = open_db()?;
    conn.execute(
        "UPDATE account_subscriptions
            SET last_seen_post_id = COALESCE(
                    (SELECT MAX(tp.post_id) FROM tags_posts tp
                      WHERE tp.tag_id = account_subscriptions.tag_id),
                    last_seen_post_id)
          WHERE account_id = ?1 AND (?2 IS NULL OR tag_id = ?2)",
        params![account_id, tag_id],
    )
    .map_err(|e| format!("mark_seen: {e}"))
}

/// Newest posts across every subscription, newest first. Not scored: this is
/// "what arrived for the tags you follow", not a ranking of the feed.
pub fn new_subscription_posts(account_id: i32, limit: usize) -> Result<Vec<ScoredPost>, String> {
    let conn = open_db()?;
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT tp.post_id
               FROM account_subscriptions s
               JOIN tags_posts tp ON tp.tag_id = s.tag_id AND tp.post_id > s.last_seen_post_id
               JOIN posts p ON p.id = tp.post_id AND p.is_deleted = 0
              WHERE s.account_id = ?1
              ORDER BY tp.post_id DESC
              LIMIT ?2",
        )
        .map_err(|e| format!("new_subscription_posts prep: {e}"))?;
    let ids: Vec<i64> = stmt
        .query_map(params![account_id, limit as i64], |r| r.get(0))
        .map_err(|e| format!("new_subscription_posts query: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("new_subscription_posts collect: {e}"))?;
    drop(stmt);
    drop(conn);
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(hydrate_posts_by_ids(&ids)?
        .into_iter()
        .map(|post| ScoredPost {
            post,
            score: 0.0,
            breakdown: None,
            reasons: Vec::new(),
        })
        .collect())
}
