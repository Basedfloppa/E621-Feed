use chrono::Utc;
use rusqlite::{Connection, params};
use std::collections::{HashMap, HashSet};

use crate::models::{FeedInteractionRequest, FeedInteractionType, InteractionHistoryItem};
use crate::utils::IdfIndex;

use super::{hydrate_cache, open_db};

/// Fixed `session_id` used when restoring interactions from an export, so the
/// `UNIQUE(account_id, post_id, event_type, session_id)` constraint keeps a
/// re-import of the same export idempotent (no duplicate rows across restores).
const INTERACTION_RESTORE_SESSION: &str = "import-restore";

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
    })?;
    hydrate_cache::clear_seen(i64::from(interaction.account_id));
    Ok(())
}

/// Remove one explicit feedback event and reverse its tag-feedback aggregate.
/// The session is part of the identity, so Undo cannot affect another device.
pub fn remove_feed_interaction(
    owner_token: &str,
    interaction: &FeedInteractionRequest,
) -> Result<bool, String> {
    let removed = super::with_write_tx(|tx| {
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
    })?;
    if removed {
        hydrate_cache::clear_seen(i64::from(interaction.account_id));
    }
    Ok(removed)
}

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

/// Return the account's full interaction model (open/like/hide/… events),
/// newest first, up to `limit`. Used by account export/backup so the
/// interaction-derived part of the taste profile can be transferred.
///
/// Does not verify ownership — callers must gate on `owner_token`.
pub fn get_account_interactions_for_export(
    account_id: i32,
    limit: i64,
) -> Result<Vec<InteractionHistoryItem>, String> {
    let conn = open_db()?;
    let limit = limit.clamp(0, 100_000);
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut stmt = conn
        .prepare(
            "SELECT post_id, event_type, position, created_at \
             FROM feed_interactions \
             WHERE account_id = ?1 \
             ORDER BY id DESC LIMIT ?2",
        )
        .map_err(|e| format!("Failed to prepare interaction export query: {e}"))?;
    let rows = stmt
        .query_map(rusqlite::params![account_id, limit], history_row_mapper)
        .map_err(|e| format!("Failed to query interaction export: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect interaction export: {e}"))
}

/// Restore an account's interaction model from an export payload: replay each
/// event into `feed_interactions` (idempotent `INSERT OR IGNORE` under a fixed
/// import session), then rebuild the derived per-tag feedback aggregate from the
/// account's full interaction history so the interaction-derived part of the
/// taste profile reflects the restored signal.
///
/// Returns `(inserted, offered)`. Callers must gate on `owner_token`.
pub fn restore_feed_interactions(
    account_id: i32,
    items: &[InteractionHistoryItem],
) -> Result<(usize, usize), String> {
    if items.is_empty() {
        return Ok((0, 0));
    }
    let bucket: Option<String> = crate::models::cfg().pick_bucket(account_id, None).0;
    let mut inserted = 0usize;
    super::with_write_tx(|tx| {
        let mut stmt = tx
            .prepare_cached(
                "INSERT OR IGNORE INTO feed_interactions ( \
                    account_id, post_id, event_type, position, session_id, created_at, experiment_bucket \
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )
            .map_err(|e| format!("Failed to prepare interaction restore: {e}"))?;
        for item in items {
            let created = if item.created_at.is_empty() {
                Utc::now().to_rfc3339()
            } else {
                item.created_at.clone()
            };
            let n = stmt
                .execute(params![
                    account_id,
                    item.post_id,
                    item.event_type.to_string(),
                    item.position,
                    INTERACTION_RESTORE_SESSION,
                    created,
                    bucket,
                ])
                .map_err(|e| format!("Failed to restore interaction: {e}"))?;
            if n > 0 {
                inserted += 1;
            }
        }
        Ok(())
    })?;
    rebuild_account_tag_feedback(account_id)?;
    Ok((inserted, items.len()))
}

/// Recompute `account_tag_feedback` from the account's full `feed_interactions`
/// (same event→delta mapping as `record_feed_interaction`). Used after restoring
/// interactions so the profile's tag-feedback reflects the restored signal.
fn rebuild_account_tag_feedback(account_id: i32) -> Result<(), String> {
    super::with_write_tx(|tx| {
        tx.execute(
            "DELETE FROM account_tag_feedback WHERE account_id = ?1",
            params![account_id],
        )
        .map_err(|e| format!("Failed to clear tag feedback for rebuild: {e}"))?;
        tx.execute(
            r#"
            INSERT INTO account_tag_feedback (
                account_id, tag_name, group_type,
                impression_count, positive_count, negative_count,
                last_interaction_at, last_decayed_at
            )
            SELECT
                fi.account_id, t.name AS tag_name, t.group_type,
                SUM(CASE WHEN fi.event_type = 'qualified_impression' THEN 1 ELSE 0 END),
                SUM(CASE
                    WHEN fi.event_type IN ('open','like') THEN 1
                    WHEN fi.event_type = 'strong_like' THEN 3
                    ELSE 0 END),
                SUM(CASE WHEN fi.event_type = 'hide' THEN 1 ELSE 0 END),
                MAX(fi.created_at),
                MAX(fi.created_at)
            FROM feed_interactions fi
            INNER JOIN tags_posts tp ON tp.post_id = fi.post_id
            INNER JOIN tags t ON t.id = tp.tag_id
            WHERE fi.account_id = ?1
            GROUP BY fi.account_id, t.name, t.group_type
            "#,
            params![account_id],
        )
        .map_err(|e| format!("Failed to rebuild tag feedback: {e}"))?;
        Ok(())
    })
}

/// Clear the interaction-derived recommendation state for one account owned by
/// this device. Favorites, blacklist, profile, and account links are retained.
pub fn clear_feed_interactions(owner_token: &str, account_id: i32) -> Result<usize, String> {
    let deleted = super::with_write_tx(|tx| {
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
    })?;
    hydrate_cache::clear_seen(i64::from(account_id));
    Ok(deleted)
}

/// One row that [`record_feed_interactions_batch`] actually wrote, carrying
/// the A/B arm it was filed under.
///
/// The caller needs this because `INSERT OR IGNORE` collapses duplicates: the
/// frontend submits interactions through the batched route and **retries** a
/// batch that failed to send, so counting *submitted* interactions would
/// inflate the metrics on every retry while the table stayed the same size.
#[derive(Debug, Clone)]
pub struct RecordedInteraction {
    /// `feed_interactions.experiment_bucket` as stored; `None` when no
    /// experiment buckets are configured.
    pub bucket: Option<String>,
    /// `event_type` exactly as written to the row, i.e. already the label the
    /// Prometheus series uses.
    pub event: String,
}

/// Write a batch of interactions, returning one entry per row that was
/// actually inserted (retries and duplicates yield none).
pub fn record_feed_interactions_batch(
    owner_token: &str,
    interactions: &[FeedInteractionRequest],
) -> Result<Vec<RecordedInteraction>, String> {
    if interactions.is_empty() {
        return Ok(Vec::new());
    }
    let recorded = super::with_write_tx(|tx| {
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

        // One entry per row actually written: a retried batch inserts nothing
        // (`INSERT OR IGNORE`) and must therefore move no counter.
        let mut recorded: Vec<RecordedInteraction> = Vec::with_capacity(interactions.len());

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
                recorded.push(RecordedInteraction {
                    bucket: bucket.clone(),
                    event: interaction.event_type.to_string(),
                });

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

        Ok(recorded)
    })?;
    {
        let mut seen_accounts: Vec<i64> = Vec::new();
        for it in interactions {
            let a = i64::from(it.account_id);
            if !seen_accounts.contains(&a) {
                seen_accounts.push(a);
            }
        }
        for a in seen_accounts {
            hydrate_cache::clear_seen(a);
        }
    }
    Ok(recorded)
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
    let account_id = i64::from(account_id);
    if let Some(cached) = hydrate_cache::recent_seen(account_id) {
        return Ok((*cached).clone());
    }
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
    let set = rows
        .collect::<Result<HashSet<_>, _>>()
        .map_err(|e| format!("collect get_recently_seen_post_ids: {e}"))?;
    Ok((*hydrate_cache::store_recent_seen(account_id, set)).clone())
}

/// Long-term seen post IDs (up to `long_days` back). Used in addition to
/// `get_recently_seen_post_ids` for OLD posts (local candidates) so that
/// even posts shown weeks ago are excluded from the "mix-in" pool.
/// Fresh live posts from e621 still use the short window.
pub fn get_long_term_seen_post_ids(
    account_id: i32,
    long_days: i64,
) -> Result<HashSet<i64>, String> {
    let account_id = i64::from(account_id);
    if let Some(cached) = hydrate_cache::long_term_seen(account_id) {
        return Ok((*cached).clone());
    }
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
    let set = rows
        .collect::<Result<HashSet<_>, _>>()
        .map_err(|e| format!("collect get_long_term_seen_post_ids: {e}"))?;
    Ok((*hydrate_cache::store_long_term_seen(account_id, set)).clone())
}

/// Posts already in the user's favourites — they don't belong in a "discover"
/// feed because the user has already curated them.
pub fn get_owned_post_ids(account_id: i32) -> Result<HashSet<i64>, String> {
    let account_id = i64::from(account_id);
    if let Some(cached) = hydrate_cache::owned(account_id) {
        return Ok((*cached).clone());
    }
    let conn = open_db()?;
    let mut stmt = conn
        .prepare("SELECT post_id FROM accounts_post WHERE account_id = ?1")
        .map_err(|e| format!("prep get_owned_post_ids: {e}"))?;
    let rows = stmt
        .query_map(params![account_id], |r| r.get::<_, i64>(0))
        .map_err(|e| format!("query get_owned_post_ids: {e}"))?;
    let set = rows
        .collect::<Result<HashSet<_>, _>>()
        .map_err(|e| format!("collect get_owned_post_ids: {e}"))?;
    Ok((*hydrate_cache::store_owned(account_id, set)).clone())
}

/// Posts authored by the user's top-N favourite tags within `group_type`,
/// randomized instead of ranked — pulls from a wider pool and shuffles
/// so the same best posts don't dominate every feed request.
/// Random LOW `rowid` anchor into `posts` so a following
/// `AND p.rowid >= ? ORDER BY p.rowid LIMIT n` samples a pseudo-random window
/// without the full `ORDER BY RANDOM()` sort (the dominant `db_hydrate` cost
/// on large catalogs). Returns `0` when `posts` is empty (callers treat that as
/// "no window constraint").
fn random_rowid_anchor(conn: &Connection) -> Result<i64, String> {
    let anchor: i64 = conn
        .query_row(
            "SELECT CASE WHEN (SELECT COALESCE(MAX(rowid),0) FROM posts) > 0
                         THEN ABS(RANDOM()) % (SELECT COALESCE(MAX(rowid),0) FROM posts) + 1
                         ELSE 0 END",
            [],
            |r| r.get(0),
        )
        .map_err(|e| format!("random_rowid_anchor: {e}"))?;
    Ok(anchor)
}

/// Run a query that returns a single `id` column, collecting into a `Vec`.
fn run_id_query(
    conn: &Connection,
    sql: &str,
    p: &[&dyn rusqlite::ToSql],
) -> Result<Vec<i64>, String> {
    let mut stmt = conn.prepare(sql).map_err(|e| format!("prep: {e}"))?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(p.iter().copied()), |r| {
            r.get::<_, i64>(0)
        })
        .map_err(|e| format!("query: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("collect: {e}"))
}

/// Per-stream ceilings for the local candidate pool.
///
/// Streams are capped independently because their densities differ by orders of
/// magnitude: artist tags match far more posts than character tags. A single
/// shared counter let the densest stream fill the pool while the others broke
/// out on their first element, so the pool was `cap` posts of one stream rather
/// than the union of five.
#[derive(Debug, Clone, Copy)]
pub struct CandidatePoolTuning {
    pub anchor_tags: usize,
    pub recent_days: i64,
    pub stream_share: f64,
    pub preferred_limit: usize,
    pub cooccurrence_limit: usize,
    pub cooccurrence_min_cooc: i64,
}

impl CandidatePoolTuning {
    #[must_use]
    pub fn from_config() -> Self {
        let rt = &crate::models::cfg().runtime;
        Self {
            anchor_tags: rt.candidate_anchor_tags,
            recent_days: rt.candidate_recent_days,
            stream_share: rt.candidate_stream_share,
            preferred_limit: rt.candidate_preferred_limit,
            cooccurrence_limit: rt.candidate_cooccurrence_limit,
            cooccurrence_min_cooc: rt.candidate_cooccurrence_min_cooc,
        }
    }

    /// Floored at 1 so a degenerate `stream_share` cannot silently starve every
    /// stream and leave the pool empty.
    #[must_use]
    fn stream_cap(&self, limit: i64) -> i64 {
        ((limit as f64 * self.stream_share.clamp(0.05, 1.0)).ceil() as i64).max(1)
    }
}

/// Account-derived inputs the pool is drawn from.
///
/// Split out from the SQL on purpose: `bin/calibrate` has no
/// `account_tag_counts` / `account_tag_cooccurrence` rows it may use for the
/// accounts it evaluates — those tables are built from *all* favourites,
/// including the test split it is scoring. It derives the same inputs from the
/// training posts and calls the same builder, so the harness competes on the
/// candidate distribution production actually produces.
#[derive(Debug, Clone, Default)]
pub struct CandidatePoolInput {
    /// `(tag_name, group_type, count)`.
    pub tag_counts: Vec<(String, String, i64)>,
    /// Co-occurrence partner tag names, strongest first.
    pub cooccurrence: Vec<String>,
    /// Explicit preferred tags, strongest first.
    pub preferred: Vec<String>,
    /// `account_quality_profile.avg_fav_count` — the recent-popular baseline.
    pub fav_baseline: f64,
}

/// Candidate posts carrying any of `tags`, sampled through a random rowid
/// window with an `ORDER BY RANDOM()` top-up.
///
/// Matched on tag name alone: `tags` is `UNIQUE(name, group_type)` and e621
/// keeps a name in exactly one category, so threading the group through would
/// add a parameter to every stream (preferred tags and neighbours mix groups)
/// without changing the result.
///
/// One `?` per tag, so the list must stay short — every caller caps it well
/// below SQLite's 999-variable floor.
///
/// The window runs as an `EXISTS` probe over a rowid scan rather than as a
/// join plus `DISTINCT`. The join form fans out one row per matching tag, so
/// it needs `DISTINCT` — and `DISTINCT` forces SQLite to materialise *every*
/// match into a temp B-tree before `LIMIT` can stop anything, which makes the
/// cost proportional to the match count instead of to `limit`. On a generic
/// tag that is the whole catalog (`male` alone carries ~63% of it, and the
/// co-occurrence stream samples exactly such tags), which is where the multi
/// second `db_hydrate` came from. Measured on the production DB over the same
/// 80 generic tags: 6.81 s -> 0.06 s for the same 136 ids.
/// The top-up is not restricted to the part of the catalog the window skipped,
/// so it can repeat ids the window already returned. Callers are expected to
/// absorb that: `collect_candidate_ids` dedups through `seen` and charges only
/// genuinely new ids against a stream's budget.
fn local_candidates_for_tag_names(
    conn: &Connection,
    tags: &[String],
    limit: i64,
) -> Result<Vec<i64>, String> {
    if tags.is_empty() || limit <= 0 {
        return Ok(Vec::new());
    }
    let anchor = random_rowid_anchor(conn)?;
    local_candidates_for_tag_names_from(conn, tags, limit, anchor)
}

/// The body of [`local_candidates_for_tag_names`] with the anchor passed in.
///
/// Split out so tests can pin the anchor: the window is the contract this
/// function has to keep, and a randomly drawn anchor can only ever be checked
/// probabilistically. Callers must pass a non-empty `tags` and a positive
/// `limit`; the public wrapper enforces both.
fn local_candidates_for_tag_names_from(
    conn: &Connection,
    tags: &[String],
    limit: i64,
    anchor: i64,
) -> Result<Vec<i64>, String> {
    let n = tags.len();
    // Tag placeholders are numbered explicitly: a bare `?` takes the index
    // *after the largest one seen so far*, so an anonymous placeholder sitting
    // behind `?n+2` would become `?n+3` and ask for a parameter that does not
    // exist. Numbering every placeholder makes binding independent of the
    // order the clauses happen to appear in.
    let placeholders = (1..=n)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(",");
    // `?1..?n` are tag names, `?n+1` the limit, `?n+2` the rowid anchor.
    let mut params: Vec<&dyn rusqlite::ToSql> =
        tags.iter().map(|t| t as &dyn rusqlite::ToSql).collect();
    params.push(&limit as &dyn rusqlite::ToSql);
    params.push(&anchor as &dyn rusqlite::ToSql);

    let window_sql = format!(
        "SELECT p.id
         FROM posts p
         WHERE p.is_deleted = 0
           AND p.preview_url IS NOT NULL
           AND p.rowid >= ?{}
           AND EXISTS (
               SELECT 1
               FROM tags_posts tp
               INNER JOIN tags t ON t.id = tp.tag_id
               WHERE tp.post_id = p.id AND t.name IN ({placeholders})
           )
         ORDER BY p.rowid
         LIMIT ?{}",
        n + 2,
        n + 1
    );
    let mut out = run_id_query(conn, &window_sql, &params)?;
    if (out.len() as i64) < limit {
        // The top-up keeps the join + `ORDER BY RANDOM()` shape on purpose: it
        // only fires when the rowid window under-filled, which means the tag
        // matched fewer posts than `limit` inside the tail — that is, the match
        // set is small, which is exactly when sorting all of it is cheap. In
        // `EXISTS` form this query would instead probe every row of `posts`.
        let random_sql = format!(
            "SELECT DISTINCT p.id
             FROM posts p
             INNER JOIN tags_posts tp ON tp.post_id = p.id
             INNER JOIN tags t ON t.id = tp.tag_id
             WHERE t.name IN ({placeholders})
               AND p.is_deleted = 0
               AND p.preview_url IS NOT NULL
             ORDER BY RANDOM() LIMIT ?{}",
            n + 1
        );
        out.extend(run_id_query(conn, &random_sql, &params[..n + 1])?);
    }
    out.truncate(limit as usize);
    Ok(out)
}

/// Rank `(tag_name, count)` rows by IDF-weighted frequency, keep the best
/// `want`. The DF comes from the passed index so both callers order identically.
///
/// Ties break on the tag name: the anchor set decides the candidate pool, and
/// the pool decides the calibration channel-cache key, so an order that varies
/// per run makes the harness non-reproducible.
#[must_use]
pub fn rank_tags_by_idf(rows: Vec<(String, i64)>, idf: &IdfIndex, want: usize) -> Vec<String> {
    if want == 0 {
        return Vec::new();
    }
    let mut scored: Vec<(f32, String)> = rows
        .into_iter()
        .map(|(name, count)| {
            // Fixed knobs rather than `priors`: this only decides *which* tags
            // seed the pool, and pinning them keeps the anchor set stable while
            // the scoring knobs are being tuned.
            let weight = idf.idf_tempered_from_df(idf.df_for(&name), 1.0, 100.0, 0.5, 1.0, 1.0);
            (count.max(1) as f32 * weight, name)
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.cmp(&b.1))
    });
    scored.truncate(want);
    scored.into_iter().map(|(_, name)| name).collect()
}

/// Recent posts above the account's own popularity baseline.
///
/// `?1=created_at cutoff ?2=fav_count threshold ?3=limit`. The baseline is
/// passed in rather than read from `account_quality_profile` so `bin/calibrate`
/// can build the same stream from its training split.
///
/// Randomized instead of ranked — picks from a wider pool to reduce repetition
/// of the same popular posts across requests.
const RECENT_POPULAR_BODY: &str = "
            SELECT p.id
            FROM posts p
            WHERE p.created_at >= ?1
              AND p.is_deleted = 0
              AND p.preview_url IS NOT NULL
              AND p.fav_count >= ?2
";

/// Recent posts above the account's own popularity baseline.
///
/// Randomized instead of ranked — picks from a wider pool to reduce repetition
/// of the same popular posts across requests.
fn local_candidates_recent_popular(
    conn: &Connection,
    fav_baseline: f64,
    recent_days: i64,
    limit: i64,
) -> Result<Vec<i64>, String> {
    if limit <= 0 {
        return Ok(Vec::new());
    }
    let cutoff = (Utc::now() - chrono::Duration::days(recent_days.max(1))).to_rfc3339();
    // 0.6 × the account's own average favourited count: below that a post is
    // not "popular for this user". Floored at 1.0 so a fresh account (average
    // 0) still gets a usable stream instead of an empty one.
    let threshold = (fav_baseline.max(0.0) * 0.6).max(1.0);
    let anchor = random_rowid_anchor(conn)?;
    // Fast path: a random rowid window, ordered by rowid — avoids the full
    // `ORDER BY RANDOM()` sort over the entire recent set.
    //
    // It is still a walk, though: `EXPLAIN QUERY PLAN` shows
    // `SEARCH p USING INTEGER PRIMARY KEY (rowid>?)` with every other predicate
    // evaluated per row, so the statement reads until it has collected `limit`
    // qualifying posts. Measured on the production DB (6.77 M posts, 23 931 of
    // them inside the 30-day window, `fav_count >= 6` matching 23 318): **86–144
    // ms** per call, the largest single term of the cold `db_hydrate` phase
    // (242.7 ms for an account with no taste profile at all). Driving the same
    // predicate from `idx_posts_created_at` returns 400 ids in **0.3 ms**, but
    // it would return the *newest* 400 every time, and the window exists to
    // sample uniformly across the recent set — so replacing it is a change to
    // design deliberately, not a swap.
    let window_sql = format!("{RECENT_POPULAR_BODY} AND p.rowid >= ?4 ORDER BY p.rowid LIMIT ?3");
    let base: &[&dyn rusqlite::ToSql] = &[
        &cutoff as &dyn rusqlite::ToSql,
        &threshold as &dyn rusqlite::ToSql,
        &limit as &dyn rusqlite::ToSql,
    ];
    let mut out = {
        let with_anchor: Vec<&dyn rusqlite::ToSql> = base
            .iter()
            .copied()
            .chain(std::iter::once(&anchor as &dyn rusqlite::ToSql))
            .collect();
        run_id_query(conn, &window_sql, &with_anchor)?
    };
    if (out.len() as i64) < limit {
        let random_sql = format!("{RECENT_POPULAR_BODY} ORDER BY RANDOM() LIMIT ?3");
        let extra = run_id_query(conn, &random_sql, base)?;
        out.extend(extra);
    }
    out.truncate(limit as usize);
    Ok(out)
}

/// Build the local candidate pool from pre-loaded sources.
///
/// Streams, in order: IDF-ranked artist anchors, IDF-ranked character anchors,
/// recent-popular, explicit preferred tags, co-occurrence neighbours. Every
/// stream draws up to its own cap and the union is deduped in insertion order,
/// so a stream that under-fills leaves room for the next one instead of the
/// first dense stream consuming the whole pool.
pub fn collect_candidate_ids(
    conn: &Connection,
    input: &CandidatePoolInput,
    idf: &IdfIndex,
    tuning: &CandidatePoolTuning,
    limit: i64,
) -> Result<Vec<i64>, String> {
    if limit <= 0 {
        return Ok(Vec::new());
    }
    let cap = tuning.stream_cap(limit);

    let anchors_for = |group: &str| -> Vec<String> {
        let rows: Vec<(String, i64)> = input
            .tag_counts
            .iter()
            .filter(|(_, g, _)| g == group)
            .map(|(name, _, count)| (name.clone(), *count))
            .collect();
        rank_tags_by_idf(rows, idf, tuning.anchor_tags)
    };
    let artist_anchors = anchors_for("artist");
    let character_anchors = anchors_for("character");

    let preferred: Vec<String> = input
        .preferred
        .iter()
        .take(tuning.preferred_limit)
        .cloned()
        .collect();
    let neighbours: Vec<String> = input
        .cooccurrence
        .iter()
        .take(tuning.cooccurrence_limit)
        .cloned()
        .collect();

    let streams = [
        local_candidates_for_tag_names(conn, &artist_anchors, cap)?,
        local_candidates_for_tag_names(conn, &character_anchors, cap)?,
        local_candidates_recent_popular(conn, input.fav_baseline, tuning.recent_days, cap)?,
        local_candidates_for_tag_names(conn, &preferred, cap)?,
        local_candidates_for_tag_names(conn, &neighbours, cap)?,
    ];

    let mut out: Vec<i64> = Vec::with_capacity(limit as usize);
    let mut seen: HashSet<i64> = HashSet::with_capacity(limit as usize);
    for ids in streams {
        // Count each stream's own contribution, not the pool's running size:
        // a shared counter let the first (densest) stream fill the pool and
        // every later stream break out before adding anything, so the pool was
        // one stream's output dressed up as five.
        let mut added = 0usize;
        for id in ids {
            if added >= cap as usize || out.len() as i64 >= limit {
                break;
            }
            // `insert` returning false means the id was already in the pool, so
            // it must not be charged against this stream's budget.
            if seen.insert(id) {
                out.push(id);
                added += 1;
            }
        }
    }
    Ok(out)
}

/// Read the account-side inputs the pool is built from.
fn load_candidate_pool_input(
    conn: &Connection,
    account_id: i32,
    tuning: &CandidatePoolTuning,
) -> Result<CandidatePoolInput, String> {
    let mut stmt = conn
        .prepare("SELECT tag_name, group_type, count FROM account_tag_counts WHERE account_id = ?1")
        .map_err(|e| format!("prep pool input/tags: {e}"))?;
    let tag_counts = stmt
        .query_map(params![account_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })
        .map_err(|e| format!("query pool input/tags: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("collect pool input/tags: {e}"))?;
    drop(stmt);

    // Aggregate in SQL instead of shipping every pair into Rust: an active
    // account holds millions of rows and only the top few dozen partners are
    // ever used. `cooc_count >= ?2` keeps the scan on
    // `idx_atc_account_cooc(account_id, cooc_count)` rather than a full table
    // walk, and both directions of a pair are folded together because the
    // table's ordering is not guaranteed to be canonical.
    //
    // The index does not make the *aggregate* cheap: measured on the production
    // DB, accounts holding 2.6–2.7 M co-occurrence rows take **1.25–1.64 s**
    // here, because `cooc_count >= 2` admits nearly every row, so both
    // `UNION ALL` branches feed ~5.5 M rows into a temp B-tree `GROUP BY` (see
    // `EXPLAIN QUERY PLAN`: `USE TEMP B-TREE FOR GROUP BY` + `FOR ORDER BY`).
    // That is the shape of the original ~2.7 s `db_hydrate` figure, and it is
    // query cost, not any writer-lock wait — reads never touch `WRITE_CONN`.
    // Accounts with no co-occurrence rows (and no `account_tag_counts`, which
    // is the whole local DB today) measure 0 ms here.
    let mut stmt = conn
        .prepare(
            "SELECT partner, SUM(c) AS total
             FROM (
                 SELECT tag1_name AS partner, cooc_count AS c
                 FROM account_tag_cooccurrence
                 WHERE account_id = ?1 AND cooc_count >= ?2
                 UNION ALL
                 SELECT tag2_name AS partner, cooc_count AS c
                 FROM account_tag_cooccurrence
                 WHERE account_id = ?1 AND cooc_count >= ?2
             )
             GROUP BY partner
             ORDER BY total DESC, partner ASC
             LIMIT ?3",
        )
        .map_err(|e| format!("prep pool input/cooc: {e}"))?;
    let cooccurrence = stmt
        .query_map(
            params![
                account_id,
                tuning.cooccurrence_min_cooc,
                tuning.cooccurrence_limit as i64
            ],
            |r| r.get::<_, String>(0),
        )
        .map_err(|e| format!("query pool input/cooc: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("collect pool input/cooc: {e}"))?;
    drop(stmt);

    let mut stmt = conn
        .prepare(
            "SELECT tag_name FROM account_preferred_tags
             WHERE account_id = ?1
             ORDER BY weight DESC, tag_name ASC",
        )
        .map_err(|e| format!("prep pool input/preferred: {e}"))?;
    let preferred = stmt
        .query_map(params![account_id], |r| r.get::<_, String>(0))
        .map_err(|e| format!("query pool input/preferred: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("collect pool input/preferred: {e}"))?;
    drop(stmt);

    // A missing profile row means "no favourites yet": the recent-popular
    // stream then falls back to its `.max(1.0)` floor instead of the whole pool
    // failing to build.
    let fav_baseline: f64 = conn
        .query_row(
            "SELECT COALESCE(avg_fav_count, 0.0) FROM account_quality_profile WHERE account_id = ?1",
            params![account_id],
            |r| r.get(0),
        )
        .unwrap_or(0.0);

    Ok(CandidatePoolInput {
        tag_counts,
        cooccurrence,
        preferred,
        fav_baseline,
    })
}

/// Five SQL streams unioned and capped at `limit`. Caller dedups against
/// seen/owned in memory.
///
/// ## Randomisation
/// Each stream samples a pseudo-random rowid window (with an `ORDER BY
/// RANDOM()` fallback) so the same best posts don't dominate every feed request.
pub fn collect_local_candidate_ids(account_id: i32, limit: i64) -> Result<Vec<i64>, String> {
    let cache_key = i64::from(account_id);
    if let Some(cached) = hydrate_cache::candidates(cache_key) {
        return Ok((*cached).clone());
    }
    let conn = open_db()?;
    let tuning = CandidatePoolTuning::from_config();
    let input = load_candidate_pool_input(&conn, account_id, &tuning)?;
    let idf = crate::utils::current_idf();
    let result = collect_candidate_ids(&conn, &input, &idf, &tuning, limit)?;
    Ok((*hydrate_cache::store_candidates(cache_key, result)).clone())
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
    let mut result = HashSet::new();

    // SQLITE_MAX_VARIABLE_NUMBER defaults to 999. A valid blacklist can hold
    // well over 999 simple tags (16 KB cap / 64-char lines), so building one
    // giant IN (...) with a bind per tag trips the ceiling and breaks the
    // feed. Chunk both lists so each query stays under the limit and then
    // union the results.
    const CHUNK: usize = 400;
    for tag_chunk in blacklisted_tags.chunks(CHUNK) {
        for post_chunk in post_ids.chunks(CHUNK) {
            let tag_ph = vec!["?"; tag_chunk.len()].join(",");
            let post_ph = vec!["?"; post_chunk.len()].join(",");
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
            let mut params: Vec<rusqlite::types::Value> =
                Vec::with_capacity(tag_chunk.len() + post_chunk.len());
            for tag in tag_chunk {
                params.push(rusqlite::types::Value::Text(tag.clone()));
            }
            for id in post_chunk {
                params.push(rusqlite::types::Value::Integer(*id));
            }
            let rows = stmt
                .query_map(rusqlite::params_from_iter(params), |r| r.get::<_, i64>(0))
                .map_err(|e| format!("query load_blacklisted_post_ids: {e}"))?;
            for row in rows {
                result.insert(row.map_err(|e| format!("collect load_blacklisted_post_ids: {e}"))?);
            }
        }
    }
    Ok(result)
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

    #[test]
    fn random_rowid_anchor_within_range_or_zero() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE posts (id INTEGER PRIMARY KEY, created_at TEXT NOT NULL);",
        )
        .unwrap();
        // Empty table -> anchor 0 (no constraint).
        assert_eq!(random_rowid_anchor(&conn).unwrap(), 0);
        for i in 1..=10i64 {
            conn.execute(
                "INSERT INTO posts (id, created_at) VALUES (?1, '2026-08-14T00:00:00Z')",
                [i],
            )
            .unwrap();
        }
        let a = random_rowid_anchor(&conn).unwrap();
        assert!((1..=10).contains(&a), "anchor {a} out of [1,10]");
    }

    #[test]
    fn recent_popular_samples_only_qualifying() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE posts (
                id INTEGER PRIMARY KEY,
                created_at TEXT NOT NULL,
                score_total INTEGER NOT NULL,
                fav_count INTEGER NOT NULL,
                rating TEXT NOT NULL,
                last_seen_at TEXT NOT NULL,
                is_deleted INTEGER NOT NULL DEFAULT 0,
                preview_url TEXT
             );
             CREATE TABLE account_quality_profile (
                account_id INTEGER PRIMARY KEY,
                avg_score_total REAL NOT NULL DEFAULT 0,
                avg_fav_count REAL NOT NULL DEFAULT 0,
                avg_comment_count REAL NOT NULL DEFAULT 0,
                avg_duration REAL NOT NULL DEFAULT 0
             );",
        )
        .unwrap();
        let now = chrono::Utc::now();
        // 100 qualifying + 30 disqualified (deleted / no preview / old / low-fav).
        for i in 1..=130i64 {
            let deleted = i > 120;
            let no_preview = i > 100 && i <= 110;
            let old = i > 110 && i <= 120;
            let low_fav = i > 120;
            let created = if old {
                (now - chrono::Duration::days(400))
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
            } else {
                (now - chrono::Duration::days(i % 10))
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
            };
            let fav = if low_fav { 2 } else { 50 };
            let preview: Option<String> = if no_preview {
                None
            } else {
                Some(format!("p/{i}.jpg"))
            };
            conn.execute(
                "INSERT INTO posts (id, created_at, score_total, fav_count, rating, last_seen_at, is_deleted, preview_url)
                 VALUES (?1,?2,100,?3,'q',?2,?4,?5)",
                rusqlite::params![i, created, fav, if deleted {1} else {0}, preview],
            ).unwrap();
        }
        conn.execute(
            "INSERT INTO account_quality_profile (account_id, avg_fav_count) VALUES (1, 50)",
            [],
        )
        .unwrap();

        let ids = local_candidates_recent_popular(&conn, 50.0, 60, 100).unwrap();
        // Only qualifying posts may be returned: non-deleted, preview_url set, recent, fav >= MAX(1,50*0.6=30).
        assert!(!ids.is_empty());
        assert!(ids.len() <= 100);
        for id in &ids {
            assert!((1..=100).contains(id), "non-qualifying id {id} returned");
        }
        // With 100 qualifying posts the window + fallback must fill to the limit.
        assert_eq!(ids.len(), 100, "expected full fill, got {}", ids.len());
        // idempotent: repeated calls still respect the guarantee.
        let ids2 = local_candidates_recent_popular(&conn, 50.0, 60, 100).unwrap();
        assert_eq!(ids2.len(), 100);
    }

    #[test]
    fn top_tags_sampling_returns_matching_posts() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE posts (
                id INTEGER PRIMARY KEY, created_at TEXT NOT NULL, score_total INTEGER NOT NULL,
                fav_count INTEGER NOT NULL, rating TEXT NOT NULL, last_seen_at TEXT NOT NULL,
                is_deleted INTEGER NOT NULL DEFAULT 0, preview_url TEXT
             );
             CREATE TABLE account_tag_counts (
                account_id INTEGER NOT NULL, tag_name TEXT NOT NULL,
                group_type TEXT NOT NULL, count INTEGER NOT NULL
             );
             CREATE TABLE tags (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, group_type TEXT NOT NULL);
             CREATE TABLE tags_posts (tag_id INTEGER NOT NULL, post_id INTEGER NOT NULL);",
        ).unwrap();
        // tags: ids 1=artist_tag_1, 2=artist_tag_2, 3=general_1.
        conn.execute_batch(
            "INSERT INTO tags (name, group_type) VALUES ('artist_tag_1','artist'),('artist_tag_2','artist'),('general_1','general');
             INSERT INTO account_tag_counts (account_id, tag_name, group_type, count)
                VALUES (1,'artist_tag_1','artist',100),(1,'artist_tag_2','artist',90);
             INSERT INTO posts (id, created_at, score_total, fav_count, rating, last_seen_at, preview_url)
                SELECT column1 AS id, '2026-08-13T00:00:00Z', 50, 5, 'q', '2026-08-13T00:00:00Z', 'p/'||column1||'.jpg'
                FROM (VALUES (1),(2),(3),(4),(5),(6),(7),(8),(9),(10),(11),(12),(13),(14),(15),(16),(17),(18),(19),(20),(21),(22),(23),(24),(25),(26),(27),(28),(29),(30),(31),(32),(33),(34),(35),(36),(37),(38),(39),(40));",
        ).unwrap();
        for i in 1..=40i64 {
            let tag_id = if i <= 20 { 1 } else { 2 };
            conn.execute(
                "INSERT INTO tags_posts (tag_id, post_id) VALUES (?1,?2)",
                [tag_id, i],
            )
            .unwrap();
        }
        let ids = local_candidates_for_tag_names(&conn, &["artist_tag_1".to_string()], 10).unwrap();
        assert_eq!(ids.len(), 10, "sample fills to limit");
        for id in &ids {
            assert!((1..=20).contains(id), "id {id} out of range");
        }
        // Union across tags: both artist tags together must reach the wider set.
        let both = local_candidates_for_tag_names(
            &conn,
            &["artist_tag_1".to_string(), "artist_tag_2".to_string()],
            40,
        )
        .unwrap();
        assert_eq!(both.len(), 40, "union covers both tags");
        // A tag nobody carries yields nothing rather than an error.
        assert!(
            local_candidates_for_tag_names(&conn, &["general_1".to_string()], 10)
                .unwrap()
                .is_empty()
        );
        assert!(
            local_candidates_for_tag_names(&conn, &[], 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn the_tag_window_is_the_first_matches_after_the_anchor_in_rowid_order() {
        // The stream promises "the first `limit` matches at or after the anchor,
        // in rowid order". The `EXISTS` rewrite replaced a join + `DISTINCT`
        // (which materialised every match) with a rowid scan, and a scan that
        // dropped `ORDER BY` — or an `EXISTS` that restarted the window per tag
        // — would still return *matching* ids while breaking that promise. So
        // pin the promise itself, with the anchor fixed rather than drawn at
        // random: a random anchor can only ever test this probabilistically.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE posts (
                id INTEGER PRIMARY KEY, created_at TEXT NOT NULL, score_total INTEGER NOT NULL,
                fav_count INTEGER NOT NULL, rating TEXT NOT NULL, last_seen_at TEXT NOT NULL,
                is_deleted INTEGER NOT NULL DEFAULT 0, preview_url TEXT
             );
             CREATE TABLE tags (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, group_type TEXT NOT NULL);
             CREATE TABLE tags_posts (tag_id INTEGER NOT NULL, post_id INTEGER NOT NULL);
             INSERT INTO tags (name, group_type) VALUES ('wtag','artist');",
        )
        .unwrap();
        // 42 posts all carrying the tag, so the window is the only thing that
        // decides membership. Two of them must never surface: 41 is deleted and
        // 42 has no preview. The preview must also be rendered by a separate
        // statement because `REFERENTIAL`-less in-memory SQLite needs the rows
        // first.
        conn.execute_batch(
            "INSERT INTO posts (id, created_at, score_total, fav_count, rating, last_seen_at, is_deleted, preview_url)
             SELECT v, '2026-08-13T00:00:00Z', 50, 5, 'q', '2026-08-13T00:00:00Z',
                    CASE WHEN v = 41 THEN 1 ELSE 0 END,
                    CASE WHEN v = 42 THEN NULL ELSE 'p/'||v||'.jpg' END
             FROM (WITH RECURSIVE seq(v) AS (SELECT 1 UNION ALL SELECT v+1 FROM seq WHERE v < 42) SELECT v FROM seq);
             INSERT INTO tags_posts (tag_id, post_id)
             SELECT 1, v FROM (WITH RECURSIVE seq(v) AS (SELECT 1 UNION ALL SELECT v+1 FROM seq WHERE v < 42) SELECT v FROM seq);",
        )
        .unwrap();

        let tags = vec!["wtag".to_string()];

        // A window wide enough to hold every eligible post: the window prefix
        // must be exactly the non-deleted, preview-bearing rows in rowid order.
        // Only the prefix is pinned: because 40 < limit the top-up also fires
        // and may repeat ids the window already returned. That is harmless —
        // `collect_candidate_ids` dedups through `seen` and charges only new
        // insertions against a stream's budget — but it does mean the tail of
        // this vector is not ordered.
        let all = local_candidates_for_tag_names_from(&conn, &tags, 100, 1).unwrap();
        assert_eq!(
            &all[..40],
            &(1..=40).collect::<Vec<i64>>()[..],
            "the window must return every eligible match in rowid order"
        );
        assert!(
            all.iter().all(|id| (1..=40).contains(id)),
            "neither deleted post 41 nor preview-less post 42 may appear"
        );

        // A window that fills exactly.
        let full = local_candidates_for_tag_names_from(&conn, &tags, 10, 11).unwrap();
        assert_eq!(full, (11..=20).collect::<Vec<i64>>());

        // A window that cannot fill: the top-up may add anything, but the
        // window's own prefix must survive, in order, ahead of it.
        let short = local_candidates_for_tag_names_from(&conn, &tags, 10, 35).unwrap();
        assert_eq!(short.len(), 10, "the top-up must still fill to the limit");
        assert_eq!(
            &short[..6],
            &[35, 36, 37, 38, 39, 40],
            "the window prefix must stay ordered and come first"
        );

        // An anchor past every post yields no window at all, so the result is
        // purely the top-up — which must respect the same filters.
        let beyond = local_candidates_for_tag_names_from(&conn, &tags, 100, 100_000).unwrap();
        assert_eq!(
            beyond.len(),
            40,
            "only the 40 eligible posts may be topped up"
        );
        assert!(
            !beyond.contains(&41) && !beyond.contains(&42),
            "the top-up must not resurrect a deleted or preview-less post"
        );
    }

    #[test]
    fn idf_ranking_prefers_specific_tags_over_generic_ones() {
        // 1000 posts carry `generic`, 50 carry `specific`. Ranking on raw count
        // alone would take `generic` first and collapse every account's pool
        // onto the same globally popular posts.
        let mut df = HashMap::new();
        df.insert("generic".to_string(), 9000);
        df.insert("specific".to_string(), 10);
        let idf = IdfIndex::from_df(&df, 10_000);
        let rows = vec![("generic".to_string(), 1000), ("specific".to_string(), 50)];
        assert_eq!(
            rank_tags_by_idf(rows, &idf, 1),
            vec!["specific".to_string()],
            "the specific tag must outrank the generic one"
        );
    }

    #[test]
    fn idf_ranking_is_deterministic_on_ties() {
        // The anchor set decides the pool, and the pool decides the calibration
        // channel-cache key, so equal scores must not reorder between runs.
        let mut df = HashMap::new();
        df.insert("a".to_string(), 10);
        df.insert("b".to_string(), 10);
        let idf = IdfIndex::from_df(&df, 1000);
        let rows = vec![("b".to_string(), 7), ("a".to_string(), 7)];
        let expected = vec!["a".to_string(), "b".to_string()];
        assert_eq!(rank_tags_by_idf(rows.clone(), &idf, 2), expected);
        assert_eq!(rank_tags_by_idf(rows, &idf, 2), expected);
    }

    #[test]
    fn stream_cap_never_starves_a_stream() {
        let tuning = CandidatePoolTuning {
            anchor_tags: 20,
            recent_days: 60,
            stream_share: 0.0,
            preferred_limit: 10,
            cooccurrence_limit: 10,
            cooccurrence_min_cooc: 2,
        };
        assert_eq!(
            tuning.stream_cap(0),
            1,
            "a zero share still allows one post"
        );
        assert_eq!(tuning.stream_cap(400), 20, "share is floored at 0.05");
        let full = CandidatePoolTuning {
            stream_share: 1.0,
            ..tuning
        };
        assert_eq!(full.stream_cap(400), 400);
    }

    /// Regression: the pool used to come out of a single stream. The loop
    /// tracked the pool's *total* size against the cap, so the first (densest)
    /// stream filled it and every later stream broke out on its first element —
    /// a 400-post pool measured 134 posts of artist anchors and nothing else.
    #[test]
    fn every_stream_contributes_to_the_pool() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE posts (
                id INTEGER PRIMARY KEY, created_at TEXT NOT NULL, score_total INTEGER NOT NULL,
                fav_count INTEGER NOT NULL, rating TEXT NOT NULL, last_seen_at TEXT NOT NULL,
                is_deleted INTEGER NOT NULL DEFAULT 0, preview_url TEXT
             );
             CREATE TABLE tags (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, group_type TEXT NOT NULL);
             CREATE TABLE tags_posts (tag_id INTEGER NOT NULL, post_id INTEGER NOT NULL);",
        )
        .unwrap();
        let now = Utc::now().to_rfc3339();
        // 1-100 carry the dense artist anchor, 101-120 the sparse character
        // anchor, 121-160 nothing but are recent and popular.
        for i in 1..=160i64 {
            let fav = if i > 120 { 50 } else { 0 };
            conn.execute(
                "INSERT INTO posts (id, created_at, score_total, fav_count, rating, last_seen_at, is_deleted, preview_url)
                 VALUES (?1,?2,100,?3,'q',?2,0,'p/'||?1||'.jpg')",
                rusqlite::params![i, now, fav],
            )
            .unwrap();
        }
        conn.execute_batch(
            "INSERT INTO tags (name, group_type) VALUES ('art','artist'),('chr','character');",
        )
        .unwrap();
        for i in 1..=100i64 {
            conn.execute(
                "INSERT INTO tags_posts (tag_id, post_id) VALUES (1,?1)",
                [i],
            )
            .unwrap();
        }
        for i in 101..=120i64 {
            conn.execute(
                "INSERT INTO tags_posts (tag_id, post_id) VALUES (2,?1)",
                [i],
            )
            .unwrap();
        }

        let idf = IdfIndex::from_df(&HashMap::new(), 1000);
        let input = CandidatePoolInput {
            tag_counts: vec![
                ("art".to_string(), "artist".to_string(), 100),
                ("chr".to_string(), "character".to_string(), 20),
            ],
            cooccurrence: Vec::new(),
            preferred: Vec::new(),
            fav_baseline: 50.0,
        };
        let tuning = CandidatePoolTuning {
            anchor_tags: 20,
            recent_days: 60,
            stream_share: 0.3,
            preferred_limit: 50,
            cooccurrence_limit: 50,
            cooccurrence_min_cooc: 2,
        };

        let pool = collect_candidate_ids(&conn, &input, &idf, &tuning, 60).unwrap();
        assert!(!pool.is_empty());
        assert!(pool.len() <= 60);

        // cap = ceil(60 * 0.3) = 18, so the artist stream cannot take more than
        // 18 of the 60 slots however many posts it could match.
        let from_artist = pool.iter().filter(|id| (1..=100).contains(*id)).count();
        assert!(
            from_artist <= 18,
            "artist stream overran its cap with {from_artist} posts"
        );
        assert!(
            pool.iter().any(|id| (101..=120).contains(id)),
            "character stream contributed nothing"
        );
        assert!(
            pool.iter().any(|id| *id > 120),
            "recent-popular stream contributed nothing"
        );
    }

    /// The preferred and co-occurrence streams are the account's own signals;
    /// before this they reached the scorer but never the candidate pool.
    #[test]
    fn preferred_and_cooccurrence_streams_seed_the_pool() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE posts (
                id INTEGER PRIMARY KEY, created_at TEXT NOT NULL, score_total INTEGER NOT NULL,
                fav_count INTEGER NOT NULL, rating TEXT NOT NULL, last_seen_at TEXT NOT NULL,
                is_deleted INTEGER NOT NULL DEFAULT 0, preview_url TEXT
             );
             CREATE TABLE tags (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, group_type TEXT NOT NULL);
             CREATE TABLE tags_posts (tag_id INTEGER NOT NULL, post_id INTEGER NOT NULL);",
        )
        .unwrap();
        let now = Utc::now().to_rfc3339();
        for i in 1..=40i64 {
            conn.execute(
                "INSERT INTO posts (id, created_at, score_total, fav_count, rating, last_seen_at, is_deleted, preview_url)
                 VALUES (?1,?2,100,0,'q',?2,0,'p/'||?1||'.jpg')",
                rusqlite::params![i, now],
            )
            .unwrap();
        }
        conn.execute_batch(
            "INSERT INTO tags (name, group_type) VALUES ('fav_tag','general'),('neigh_tag','general');",
        )
        .unwrap();
        for i in 1..=20i64 {
            conn.execute(
                "INSERT INTO tags_posts (tag_id, post_id) VALUES (1,?1)",
                [i],
            )
            .unwrap();
        }
        for i in 21..=40i64 {
            conn.execute(
                "INSERT INTO tags_posts (tag_id, post_id) VALUES (2,?1)",
                [i],
            )
            .unwrap();
        }

        let idf = IdfIndex::from_df(&HashMap::new(), 1000);
        let input = CandidatePoolInput {
            tag_counts: Vec::new(),
            cooccurrence: vec!["neigh_tag".to_string()],
            preferred: vec!["fav_tag".to_string()],
            fav_baseline: 0.0,
        };
        let tuning = CandidatePoolTuning {
            anchor_tags: 20,
            recent_days: 1,
            stream_share: 0.34,
            preferred_limit: 50,
            cooccurrence_limit: 50,
            cooccurrence_min_cooc: 2,
        };

        let pool = collect_candidate_ids(&conn, &input, &idf, &tuning, 60).unwrap();
        assert!(
            pool.iter().any(|id| (1..=20).contains(id)),
            "preferred-tags stream contributed nothing"
        );
        assert!(
            pool.iter().any(|id| (21..=40).contains(id)),
            "co-occurrence stream contributed nothing"
        );
    }
}
