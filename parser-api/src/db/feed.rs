use chrono::Utc;
use rusqlite::{Connection, params};
use std::collections::{HashMap, HashSet};

use crate::models::{FeedInteractionRequest, FeedInteractionType};

use super::open_db;

pub fn record_feed_interaction(
    owner_token: &str,
    interaction: &FeedInteractionRequest,
) -> Result<(), String> {
    super::with_write_tx(|tx| {
        let linked: bool = tx
            .query_row(
                "
                SELECT EXISTS(
                    SELECT 1 FROM account_device_links
                    WHERE owner_token = ?1 AND account_id = ?2
                )
                ",
                params![owner_token, interaction.account_id],
                |row| row.get(0),
            )
            .map_err(|e| format!("Failed to validate feed interaction owner link: {e}"))?;

        if !linked {
            return Err("Account is not linked to this device token".to_string());
        }

        // Resolve the bucket the same way `get_recommendations` does so the
        // logged arm matches the one the user actually saw.
        let explicit: Option<String> = tx
            .query_row(
                "SELECT experiment_bucket FROM accounts WHERE id = ?1",
                params![interaction.account_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .unwrap_or(None);
        let bucket: Option<String> = crate::models::cfg()
            .pick_bucket(interaction.account_id, explicit.as_deref())
            .0;

        let inserted = tx
            .execute(
                "
                INSERT OR IGNORE INTO feed_interactions (
                    account_id, post_id, event_type, position, session_id, created_at, experiment_bucket
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                ",
                params![
                    interaction.account_id,
                    interaction.post_id,
                    interaction.event_type.to_string(),
                    interaction.position,
                    interaction.session_id,
                    Utc::now().to_rfc3339(),
                    bucket,
                ],
            )
            .map_err(|e| format!("Failed to record feed interaction: {e}"))?;

        if inserted > 0 {
            let (impression_delta, positive_delta, negative_delta) = match interaction.event_type {
                FeedInteractionType::QualifiedImpression => (1, 0, 0),
                FeedInteractionType::Open | FeedInteractionType::Like => (0, 1, 0),
                FeedInteractionType::StrongLike => (0, 3, 0),
                FeedInteractionType::Hide => (0, 0, 1),
                FeedInteractionType::Unknown => (0, 0, 0),
            };

            let now_iso = Utc::now().to_rfc3339();
            tx.execute(
                "
                INSERT INTO account_tag_feedback (
                    account_id, tag_name, group_type,
                    impression_count, positive_count, negative_count,
                    last_interaction_at, last_decayed_at
                )
                SELECT
                    ?1,
                    t.name,
                    t.group_type,
                    ?2,
                    ?3,
                    ?4,
                    ?5,
                    ?5
                FROM tags t
                INNER JOIN tags_posts tp ON tp.tag_id = t.id
                WHERE tp.post_id = ?6
                ON CONFLICT(account_id, tag_name, group_type) DO UPDATE SET
                    impression_count = account_tag_feedback.impression_count + excluded.impression_count,
                    positive_count = account_tag_feedback.positive_count + excluded.positive_count,
                    negative_count = account_tag_feedback.negative_count + excluded.negative_count,
                    last_interaction_at = excluded.last_interaction_at,
                    last_decayed_at = excluded.last_decayed_at
                ",
                params![
                    interaction.account_id,
                    impression_delta,
                    positive_delta,
                    negative_delta,
                    now_iso,
                    interaction.post_id,
                ],
            )
            .map_err(|e| format!("Failed to update tag feedback aggregates: {e}"))?;
        }

        Ok(())
    })
}

/// Remove one explicit feedback event and reverse its tag-feedback aggregate.
/// The session is part of the identity, so Undo cannot affect another device.
pub fn remove_feed_interaction(
    owner_token: &str,
    interaction: &FeedInteractionRequest,
) -> Result<bool, String> {
    super::with_write_tx(|tx| {
        let linked: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM account_device_links WHERE owner_token = ?1 AND account_id = ?2)",
            params![owner_token, interaction.account_id],
            |row| row.get(0),
        ).map_err(|e| format!("Failed to validate interaction owner link: {e}"))?;
        if !linked {
            return Err("Account is not linked to this device token".to_string());
        }
        let deleted = tx.execute(
            "DELETE FROM feed_interactions WHERE account_id = ?1 AND post_id = ?2 AND event_type = ?3 AND session_id = ?4",
            params![interaction.account_id, interaction.post_id, interaction.event_type.to_string(), interaction.session_id],
        ).map_err(|e| format!("Failed to remove feed interaction: {e}"))?;
        if deleted == 0 {
            return Ok(false);
        }
        let (impressions, positives, negatives) = match interaction.event_type {
            FeedInteractionType::QualifiedImpression => (1, 0, 0),
            FeedInteractionType::Open | FeedInteractionType::Like => (0, 1, 0),
            FeedInteractionType::StrongLike => (0, 3, 0),
            FeedInteractionType::Hide => (0, 0, 1),
            FeedInteractionType::Unknown => (0, 0, 0),
        };
        tx.execute(
            "UPDATE account_tag_feedback SET
                impression_count = MAX(0, impression_count - ?1),
                positive_count = MAX(0, positive_count - ?2),
                negative_count = MAX(0, negative_count - ?3),
                last_interaction_at = ?4
             WHERE account_id = ?5 AND tag_name IN (
                SELECT t.name FROM tags t INNER JOIN tags_posts tp ON tp.tag_id = t.id WHERE tp.post_id = ?6
             )",
            params![impressions, positives, negatives, Utc::now().to_rfc3339(), interaction.account_id, interaction.post_id],
        ).map_err(|e| format!("Failed to reverse tag feedback: {e}"))?;
        Ok(true)
    })
}

/// Batch version of `record_feed_interaction`. Processes up to 100
/// interactions in a single write transaction. Ownership is verified
/// per distinct `account_id` (cheaper than per-interaction).
/// Fetch the most recent interaction rows for an account, newest first,
/// optionally filtered to one event type. Used by the interaction-history
/// page. Does not verify ownership — callers must gate on `owner_token`.
pub fn get_account_interaction_history(
    account_id: i32,
    event_filter: Option<FeedInteractionType>,
    limit: i64,
) -> Result<Vec<crate::models::InteractionHistoryItem>, String> {
    let conn = open_db()?;
    let limit = limit.clamp(1, 500);
    let et_str = event_filter.as_ref().map(ToString::to_string);
    let sql = match &et_str {
        Some(_) => "SELECT post_id, event_type, position, created_at \
             FROM feed_interactions \
             WHERE account_id = ?1 AND event_type = ?2 \
             ORDER BY id DESC LIMIT ?3"
            .to_string(),
        None => "SELECT post_id, event_type, position, created_at \
             FROM feed_interactions \
             WHERE account_id = ?1 \
             ORDER BY id DESC LIMIT ?2"
            .to_string(),
    };
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("Failed to prepare interaction history query: {e}"))?;
    let rows = match &et_str {
        Some(et) => stmt.query_map(rusqlite::params![account_id, et, limit], history_row_mapper),
        None => stmt.query_map(rusqlite::params![account_id, limit], history_row_mapper),
    };
    rows.map_err(|e| format!("Failed to query interaction history: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect interaction history: {e}"))
}

fn history_row_mapper(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<crate::models::InteractionHistoryItem> {
    let event_str: String = row.get(1)?;
    let event_type = match event_str.as_str() {
        "qualified_impression" => crate::models::FeedInteractionType::QualifiedImpression,
        "open" => crate::models::FeedInteractionType::Open,
        "like" => crate::models::FeedInteractionType::Like,
        "strong_like" => crate::models::FeedInteractionType::StrongLike,
        "hide" => crate::models::FeedInteractionType::Hide,
        _ => crate::models::FeedInteractionType::Unknown,
    };
    Ok(crate::models::InteractionHistoryItem {
        post_id: row.get(0)?,
        event_type,
        position: row.get(2)?,
        created_at: row.get(3)?,
    })
}

/// Clear the interaction-derived recommendation state for one account owned by
/// this device. Favorites, blacklist, profile, and account links are retained.
pub fn clear_feed_interactions(owner_token: &str, account_id: i32) -> Result<usize, String> {
    super::with_write_tx(|tx| {
        let linked: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM account_device_links WHERE owner_token = ?1 AND account_id = ?2)",
                params![owner_token, account_id],
                |row| row.get(0),
            )
            .map_err(|e| format!("Failed to validate interaction-clear owner link: {e}"))?;
        if !linked {
            return Err("Account is not linked to this device token".to_string());
        }

        let deleted = tx
            .execute(
                "DELETE FROM feed_interactions WHERE account_id = ?1",
                params![account_id],
            )
            .map_err(|e| format!("Failed to clear feed interactions: {e}"))?;
        tx.execute(
            "DELETE FROM account_tag_feedback WHERE account_id = ?1",
            params![account_id],
        )
        .map_err(|e| format!("Failed to clear tag feedback: {e}"))?;
        tx.execute(
            "DELETE FROM feed_session_posts WHERE session_id IN (SELECT session_id FROM feed_sessions WHERE account_id = ?1)",
            params![account_id],
        )
        .map_err(|e| format!("Failed to clear feed-session posts: {e}"))?;
        tx.execute(
            "DELETE FROM feed_sessions WHERE account_id = ?1",
            params![account_id],
        )
        .map_err(|e| format!("Failed to clear feed sessions: {e}"))?;
        Ok(deleted)
    })
}

pub fn record_feed_interactions_batch(
    owner_token: &str,
    interactions: &[FeedInteractionRequest],
) -> Result<(), String> {
    if interactions.is_empty() {
        return Ok(());
    }
    super::with_write_tx(|tx| {
        // Pre-verify ownership for each distinct account_id in the batch.
        let distinct_accounts: HashSet<i32> = interactions.iter().map(|i| i.account_id).collect();
        // Build a single query per account.
        for aid in &distinct_accounts {
            let linked: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM account_device_links WHERE owner_token = ?1 AND account_id = ?2)",
                    params![owner_token, aid],
                    |row| row.get(0),
                )
                .map_err(|e| format!("Failed to validate feed interaction owner link: {e}"))?;
            if !linked {
                return Err(format!("Account {aid} is not linked to this device token"));
            }
        }

        // Pre-resolve experiment buckets for each account.
        let mut buckets: HashMap<i32, Option<String>> =
            HashMap::with_capacity(distinct_accounts.len());
        for aid in &distinct_accounts {
            let explicit: Option<String> = tx
                .query_row(
                    "SELECT experiment_bucket FROM accounts WHERE id = ?1",
                    params![aid],
                    |r| r.get::<_, Option<String>>(0),
                )
                .unwrap_or(None);
            let bucket = crate::models::cfg()
                .pick_bucket(*aid, explicit.as_deref())
                .0;
            buckets.insert(*aid, bucket);
        }

        let now_iso = Utc::now().to_rfc3339();

        // Prepare static statements.
        let mut insert_interaction = tx
            .prepare_cached(
                "INSERT OR IGNORE INTO feed_interactions (
                    account_id, post_id, event_type, position, session_id, created_at, experiment_bucket
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )
            .map_err(|e| format!("Failed to prepare batch interaction insert: {e}"))?;

        let mut update_feedback = tx
            .prepare_cached(
                "INSERT INTO account_tag_feedback (
                    account_id, tag_name, group_type,
                    impression_count, positive_count, negative_count,
                    last_interaction_at, last_decayed_at
                )
                SELECT
                    ?1,
                    t.name,
                    t.group_type,
                    ?2,
                    ?3,
                    ?4,
                    ?5,
                    ?5
                FROM tags t
                INNER JOIN tags_posts tp ON tp.tag_id = t.id
                WHERE tp.post_id = ?6
                ON CONFLICT(account_id, tag_name, group_type) DO UPDATE SET
                    impression_count = account_tag_feedback.impression_count + excluded.impression_count,
                    positive_count = account_tag_feedback.positive_count + excluded.positive_count,
                    negative_count = account_tag_feedback.negative_count + excluded.negative_count,
                    last_interaction_at = excluded.last_interaction_at,
                    last_decayed_at = excluded.last_decayed_at",
            )
            .map_err(|e| format!("Failed to prepare batch feedback update: {e}"))?;

        for interaction in interactions {
            let bucket = buckets
                .get(&interaction.account_id)
                .and_then(std::clone::Clone::clone);

            let inserted = insert_interaction
                .execute(params![
                    interaction.account_id,
                    interaction.post_id,
                    interaction.event_type.to_string(),
                    interaction.position,
                    interaction.session_id,
                    now_iso,
                    bucket,
                ])
                .map_err(|e| format!("Failed to record batch feed interaction: {e}"))?;

            if inserted > 0 {
                let (impression_delta, positive_delta, negative_delta) =
                    match interaction.event_type {
                        FeedInteractionType::QualifiedImpression => (1, 0, 0),
                        FeedInteractionType::Open | FeedInteractionType::Like => (0, 1, 0),
                        FeedInteractionType::StrongLike => (0, 3, 0),
                        FeedInteractionType::Hide => (0, 0, 1),
                        FeedInteractionType::Unknown => (0, 0, 0),
                    };

                update_feedback
                    .execute(params![
                        interaction.account_id,
                        impression_delta,
                        positive_delta,
                        negative_delta,
                        now_iso,
                        interaction.post_id,
                    ])
                    .map_err(|e| format!("Failed to update batch tag feedback: {e}"))?;
            }
        }

        Ok(())
    })
}

/// Posts the user has already interacted with in any way (qualified
/// impression, explicit hide, or open-through to e621) within the last
/// `days`. Used to drop them from the candidate pool before scoring so
/// the same post doesn't surface across sessions.
///
/// `hide` is the load-bearing addition: without it, a hidden post would
/// reappear on the very next page request because the dedup query
/// previously only matched `qualified_impression`. `open` is included
/// because the user has already seen the post in full and likely
/// doesn't need it surfaced again so soon.
pub fn get_recently_seen_post_ids(account_id: i32, days: i64) -> Result<HashSet<i64>, String> {
    let conn = open_db()?;
    let cutoff = (Utc::now() - chrono::Duration::days(days.max(1))).to_rfc3339();
    let mut stmt = conn
        .prepare(
            "
            SELECT DISTINCT post_id FROM feed_interactions
            WHERE account_id = ?1
              AND event_type IN ('qualified_impression', 'hide', 'open')
              AND created_at >= ?2
            ",
        )
        .map_err(|e| format!("prep get_recently_seen_post_ids: {e}"))?;
    let rows = stmt
        .query_map(params![account_id, cutoff], |r| r.get::<_, i64>(0))
        .map_err(|e| format!("query get_recently_seen_post_ids: {e}"))?;
    rows.collect::<Result<HashSet<_>, _>>()
        .map_err(|e| format!("collect get_recently_seen_post_ids: {e}"))
}

/// Long-term seen post IDs (up to `long_days` back). Used in addition to
/// `get_recently_seen_post_ids` for OLD posts (local candidates) so that
/// even posts shown weeks ago are excluded from the "mix-in" pool.
/// Fresh live posts from e621 still use the short window.
pub fn get_long_term_seen_post_ids(
    account_id: i32,
    long_days: i64,
) -> Result<HashSet<i64>, String> {
    let conn = open_db()?;
    let cutoff = (Utc::now() - chrono::Duration::days(long_days.max(1))).to_rfc3339();
    let mut stmt = conn
        .prepare(
            "
            SELECT DISTINCT post_id FROM feed_interactions
            WHERE account_id = ?1
              AND event_type IN ('qualified_impression', 'hide', 'open')
              AND created_at >= ?2
            ",
        )
        .map_err(|e| format!("prep get_long_term_seen_post_ids: {e}"))?;
    let rows = stmt
        .query_map(params![account_id, cutoff], |r| r.get::<_, i64>(0))
        .map_err(|e| format!("query get_long_term_seen_post_ids: {e}"))?;
    rows.collect::<Result<HashSet<_>, _>>()
        .map_err(|e| format!("collect get_long_term_seen_post_ids: {e}"))
}

/// Posts already in the user's favourites — they don't belong in a "discover"
/// feed because the user has already curated them.
pub fn get_owned_post_ids(account_id: i32) -> Result<HashSet<i64>, String> {
    let conn = open_db()?;
    let mut stmt = conn
        .prepare("SELECT post_id FROM accounts_post WHERE account_id = ?1")
        .map_err(|e| format!("prep get_owned_post_ids: {e}"))?;
    let rows = stmt
        .query_map(params![account_id], |r| r.get::<_, i64>(0))
        .map_err(|e| format!("query get_owned_post_ids: {e}"))?;
    rows.collect::<Result<HashSet<_>, _>>()
        .map_err(|e| format!("collect get_owned_post_ids: {e}"))
}

/// Posts authored by the user's top-N favourite tags within `group_type`,
/// randomized instead of ranked — pulls from a wider pool and shuffles
/// so the same best posts don't dominate every feed request.
fn local_candidates_for_top_tags(
    conn: &Connection,
    account_id: i32,
    group_type: &str,
    n_tags: i64,
    limit: i64,
) -> Result<Vec<i64>, String> {
    let mut stmt = conn
        .prepare(
            "
            WITH top_tags AS (
                SELECT tag_name
                FROM account_tag_counts
                WHERE account_id = ?1 AND group_type = ?2
                ORDER BY count DESC
                LIMIT ?3
            )
            SELECT DISTINCT p.id
            FROM posts p
            INNER JOIN tags_posts tp ON tp.post_id = p.id
            INNER JOIN tags t ON t.id = tp.tag_id
            WHERE t.group_type = ?2
              AND t.name IN (SELECT tag_name FROM top_tags)
              AND p.is_deleted = 0
              AND p.preview_url IS NOT NULL
            ORDER BY RANDOM()
            LIMIT ?4
            ",
        )
        .map_err(|e| format!("prep local_candidates_for_top_tags: {e}"))?;
    let rows = stmt
        .query_map(params![account_id, group_type, n_tags, limit], |r| {
            r.get::<_, i64>(0)
        })
        .map_err(|e| format!("query local_candidates_for_top_tags: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("collect local_candidates_for_top_tags: {e}"))
}

/// Recent posts above the user's own popularity baseline (`avg_fav_count`).
/// Randomized instead of ranked — picks from a wider pool to reduce
/// repetition of the same popular posts across requests.
fn local_candidates_recent_popular(
    conn: &Connection,
    account_id: i32,
    recent_days: i64,
    limit: i64,
) -> Result<Vec<i64>, String> {
    let cutoff = (Utc::now() - chrono::Duration::days(recent_days.max(1))).to_rfc3339();
    let mut stmt = conn
        .prepare(
            "
            WITH baseline AS (
                SELECT MAX(1.0, COALESCE(avg_fav_count, 0.0) * 0.6) AS thresh
                FROM account_quality_profile
                WHERE account_id = ?1
            )
            SELECT p.id
            FROM posts p
            WHERE p.created_at >= ?2
              AND p.is_deleted = 0
              AND p.preview_url IS NOT NULL
              AND p.fav_count >= COALESCE((SELECT thresh FROM baseline), 1.0)
            ORDER BY RANDOM()
            LIMIT ?3
            ",
        )
        .map_err(|e| format!("prep local_candidates_recent_popular: {e}"))?;
    let rows = stmt
        .query_map(params![account_id, cutoff, limit], |r| r.get::<_, i64>(0))
        .map_err(|e| format!("query local_candidates_recent_popular: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("collect local_candidates_recent_popular: {e}"))
}

/// Three SQL streams (top-artist, top-character, recent-popular) unioned and
/// capped at `limit`. Caller dedups against seen/owned in memory.
///
/// ## Randomisation (v6+)
/// Each stream uses `ORDER BY RANDOM()` instead of `ORDER BY score_total DESC`
/// so the same best posts don't dominate every feed request.
/// - Artist/character: pulls from the user's top 20 tags (was 10/12).
/// - Recent-popular: looks back 60 days instead of 30.
/// - Pool size per stream: `(limit / 3 * 2).max(100)` (was `(limit / 3).max(50)`).
pub fn collect_local_candidate_ids(account_id: i32, limit: i64) -> Result<Vec<i64>, String> {
    let conn = open_db()?;
    // Use a larger multiplier per stream so each source contributes a diverse
    // set. The ORDER BY RANDOM() inside each query ensures different posts
    // appear every time. Combined with the HashSet dedup this gives us a
    // varied pool that still respects the user's taste.
    let per_stream = (limit / 3 * 2).max(100);
    let cap_per_stream = (limit as usize / 3 + 1).max(20);

    let mut out: HashSet<i64> = HashSet::with_capacity(limit as usize);
    for ids in [
        // Pull from more tags (20 instead of 10/12) and randomize the results
        local_candidates_for_top_tags(&conn, account_id, "artist", 20, per_stream)?,
        local_candidates_for_top_tags(&conn, account_id, "character", 20, per_stream)?,
        // Look back 60 days instead of 30 for a bigger pool of recent-popular posts
        local_candidates_recent_popular(&conn, account_id, 60, per_stream)?,
    ] {
        for id in ids {
            if out.len() >= cap_per_stream {
                break;
            }
            out.insert(id);
            if out.len() as i64 >= limit {
                return Ok(out.into_iter().collect());
            }
        }
    }
    Ok(out.into_iter().collect())
}

/// Given a list of candidate post IDs and blacklisted simple tag names,
/// return the subset of post IDs that have at least one blacklisted tag.
///
/// `blacklisted_tags` must be plain tag names (no e621 search syntax like
/// `-rating:s` or `young furry`). The caller is responsible for extracting
/// only simple tags from the account's blacklist text.
pub fn load_blacklisted_post_ids(
    post_ids: &[i64],
    blacklisted_tags: &[String],
) -> Result<HashSet<i64>, String> {
    if post_ids.is_empty() || blacklisted_tags.is_empty() {
        return Ok(HashSet::new());
    }

    let conn = open_db()?;

    // Build placeholders for both tag names and post IDs.
    let tag_ph = vec!["?"; blacklisted_tags.len()].join(",");
    let post_ph = vec!["?"; post_ids.len()].join(",");
    let sql = format!(
        "SELECT DISTINCT tp.post_id
         FROM tags_posts tp
         INNER JOIN tags t ON t.id = tp.tag_id
         WHERE t.name IN ({tag_ph})
           AND tp.post_id IN ({post_ph})"
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("prep load_blacklisted_post_ids: {e}"))?;

    // Mixed parameter types (String + i64) → use rusqlite::types::Value.
    let mut params: Vec<rusqlite::types::Value> =
        Vec::with_capacity(blacklisted_tags.len() + post_ids.len());
    for tag in blacklisted_tags {
        params.push(rusqlite::types::Value::Text(tag.clone()));
    }
    for id in post_ids {
        params.push(rusqlite::types::Value::Integer(*id));
    }

    let rows = stmt
        .query_map(rusqlite::params_from_iter(params), |r| r.get::<_, i64>(0))
        .map_err(|e| format!("query load_blacklisted_post_ids: {e}"))?;
    rows.collect::<Result<HashSet<_>, _>>()
        .map_err(|e| format!("collect load_blacklisted_post_ids: {e}"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn blacklist_filter_empty() {
        let ids = load_blacklisted_post_ids(&[], &[]).unwrap();
        assert!(ids.is_empty());

        let ids = load_blacklisted_post_ids(&[1, 2, 3], &[]).unwrap();
        assert!(ids.is_empty());

        let ids = load_blacklisted_post_ids(&[], &["gore".to_string()]).unwrap();
        assert!(ids.is_empty());
    }
}
