use chrono::Utc;
use rusqlite::{Connection, params};
use std::collections::{HashMap, HashSet};

use crate::models::{FeedInteractionRequest, FeedInteractionType, InteractionHistoryItem};
use crate::utils::IdfIndex;

use super::instance::MAY_ADDRESS_SQL;
use super::{hydrate_cache, open_db};

/// Fixed `session_id` used when restoring interactions from an export, so the
/// `UNIQUE(account_id, post_id, event_type, session_id)` constraint keeps a
/// re-import of the same export idempotent (no duplicate rows across restores).
const INTERACTION_RESTORE_SESSION: &str = "import-restore";

pub fn record_feed_interaction(
    owner_token: &str,
    interaction: &FeedInteractionRequest,
) -> Result<(), String> {
    let new_row = super::with_write_tx(|tx| {
        // Scope, not a link: the instance owner addresses every account, so
        // "does this browser hold a link row" is the wrong question here.
        let in_scope: bool = tx
            .query_row(
                &format!("SELECT {MAY_ADDRESS_SQL}"),
                params![owner_token, interaction.account_id],
                |row| row.get(0),
            )
            .map_err(|e| format!("Failed to validate feed interaction owner link: {e}"))?;

        if !in_scope {
            return Err("Account is not in scope for this device token".to_string());
        }

        let inserted = tx
            .execute(
                "
                INSERT OR IGNORE INTO feed_interactions (
                    account_id, post_id, event_type, position, session_id, created_at
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                ",
                params![
                    interaction.account_id,
                    interaction.post_id,
                    interaction.event_type.to_string(),
                    interaction.position,
                    interaction.session_id,
                    Utc::now().to_rfc3339(),
                ],
            )
            .map_err(|e| format!("Failed to record feed interaction: {e}"))?;

        if inserted > 0 {
            let (impression_delta, positive_delta, negative_delta) = match interaction.event_type {
                FeedInteractionType::QualifiedImpression => (1, 0, 0),
                FeedInteractionType::Open => (0, 1, 0),
                FeedInteractionType::Unknown => (0, 0, 0),
            };

            let now_iso = Utc::now().to_rfc3339();
            // An interaction is proof the card was on screen, so it also
            // records the exposure. Same transaction: dedup must never learn
            // about a view whose own event was rolled back.
            upsert_shown_posts(tx, interaction.account_id, &[interaction.post_id], &now_iso)?;
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

        Ok(inserted > 0)
    })?;
    if new_row {
        // Additions only, so merging keeps the cached dedup set warm instead
        // of making the next feed request rebuild it. Removals still go
        // through `clear_seen` — see `remove_feed_interaction`.
        let account = i64::from(interaction.account_id);
        hydrate_cache::merge_recent_seen(account, &[interaction.post_id]);
        hydrate_cache::merge_long_term_seen(account, &[interaction.post_id]);
    }
    Ok(())
}

/// Remove one explicit feedback event and reverse its tag-feedback aggregate.
/// The session is part of the identity, so Undo cannot affect another device.
pub fn remove_feed_interaction(
    owner_token: &str,
    interaction: &FeedInteractionRequest,
) -> Result<bool, String> {
    let removed = super::with_write_tx(|tx| {
        let in_scope: bool = tx
            .query_row(
                &format!("SELECT {MAY_ADDRESS_SQL}"),
                params![owner_token, interaction.account_id],
                |row| row.get(0),
            )
            .map_err(|e| format!("Failed to validate interaction owner link: {e}"))?;
        if !in_scope {
            return Err("Account is not in scope for this device token".to_string());
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
            FeedInteractionType::Open => (0, 1, 0),
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
        // Undo means "pretend this never happened", so the exposure row goes
        // with it. Without this the post would stay filtered for the whole
        // dedup window (`feed_shown_posts` is what keeps it out of the feed)
        // and undo would look broken: the event is gone, yet the card never
        // comes back.
        tx.execute(
            "DELETE FROM feed_shown_posts WHERE account_id = ?1 AND post_id = ?2",
            params![interaction.account_id, interaction.post_id],
        )
        .map_err(|e| format!("Failed to drop the exposure row for undo: {e}"))?;
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
        // Rows written before the feedback events were cut (and any future
        // unknown string) read back as Unknown rather than failing the page.
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
    let mut inserted = 0usize;
    super::with_write_tx(|tx| {
        let mut stmt = tx
            .prepare_cached(
                "INSERT OR IGNORE INTO feed_interactions ( \
                    account_id, post_id, event_type, position, session_id, created_at \
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
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
                SUM(CASE WHEN fi.event_type = 'open' THEN 1 ELSE 0 END),
                0,
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
        let in_scope: bool = tx
            .query_row(
                &format!("SELECT {MAY_ADDRESS_SQL}"),
                params![owner_token, account_id],
                |row| row.get(0),
            )
            .map_err(|e| format!("Failed to validate interaction-clear owner link: {e}"))?;
        if !in_scope {
            return Err("Account is not in scope for this device token".to_string());
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
        // Scoped by account, not by `session_id IN (…)`: one session id can be
        // shared by two accounts (the browser keeps it across an account
        // switch), and the old form took the other account's scroll set with
        // it. V37 put the account in the key so this is a single predicate.
        tx.execute(
            "DELETE FROM feed_session_posts WHERE account_id = ?1",
            params![account_id],
        )
        .map_err(|e| format!("Failed to clear feed-session posts: {e}"))?;
        tx.execute(
            "DELETE FROM feed_sessions WHERE account_id = ?1",
            params![account_id],
        )
        .map_err(|e| format!("Failed to clear feed sessions: {e}"))?;
        // The exposure registry is part of the same "forget what I saw"
        // promise. Leaving it behind would keep every cleared post out of the
        // feed for up to `SHOWN_RETENTION_DAYS` while the history page showed
        // nothing — the clearest possible disagreement between two records of
        // the same fact.
        tx.execute(
            "DELETE FROM feed_shown_posts WHERE account_id = ?1",
            params![account_id],
        )
        .map_err(|e| format!("Failed to clear shown posts: {e}"))?;
        Ok(deleted)
    })?;
    hydrate_cache::clear_seen(i64::from(account_id));
    Ok(deleted)
}

/// One row that [`record_feed_interactions_batch`] actually wrote.
///
/// The caller needs this because `INSERT OR IGNORE` collapses duplicates: the
/// frontend submits interactions through the batched route and **retries** a
/// batch that failed to send, so counting *submitted* interactions would
/// inflate the metrics on every retry while the table stayed the same size.
#[derive(Debug, Clone)]
pub struct RecordedInteraction {
    /// `event_type` exactly as written to the row, i.e. already the label the
    /// Prometheus series uses.
    pub event: String,
}

/// Outcome of [`record_feed_interactions_batch`].
#[derive(Debug, Clone)]
pub struct RecordedBatch {
    /// One entry per interaction row that was actually inserted, for the
    /// Prometheus counters.
    pub interactions: Vec<RecordedInteraction>,
    /// Exposure rows written from those interactions. Interactions are proof
    /// the card was on screen, so each one also records that the post was seen.
    pub exposures: usize,
}

/// Write a batch of interactions, returning one entry per row that was
/// actually inserted (retries and duplicates yield none).
pub fn record_feed_interactions_batch(
    owner_token: &str,
    interactions: &[FeedInteractionRequest],
) -> Result<RecordedBatch, String> {
    if interactions.is_empty() {
        return Ok(RecordedBatch {
            interactions: Vec::new(),
            exposures: 0,
        });
    }
    let (recorded, newly_seen, exposures) = super::with_write_tx(|tx| {
        // Pre-verify ownership for each distinct account_id in the batch.
        let distinct_accounts: HashSet<i32> = interactions.iter().map(|i| i.account_id).collect();
        // Build a single query per account.
        for aid in &distinct_accounts {
            let in_scope: bool = tx
                .query_row(
                    &format!("SELECT {MAY_ADDRESS_SQL}"),
                    params![owner_token, aid],
                    |row| row.get(0),
                )
                .map_err(|e| format!("Failed to validate feed interaction owner link: {e}"))?;
            if !in_scope {
                return Err(format!(
                    "Account {aid} is not in scope for this device token"
                ));
            }
        }

        let now_iso = Utc::now().to_rfc3339();

        // Prepare static statements.
        let mut insert_interaction = tx
            .prepare_cached(
                "INSERT OR IGNORE INTO feed_interactions (
                    account_id, post_id, event_type, position, session_id, created_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
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
        // Posts this batch proves the user actually saw. Collected per account
        // and written here, in this transaction, because the interaction row is
        // the evidence: if one lands, the other must too.
        let mut newly_seen: HashMap<i32, Vec<i64>> = HashMap::new();

        for interaction in interactions {
            let inserted = insert_interaction
                .execute(params![
                    interaction.account_id,
                    interaction.post_id,
                    interaction.event_type.to_string(),
                    interaction.position,
                    interaction.session_id,
                    now_iso,
                ])
                .map_err(|e| format!("Failed to record batch feed interaction: {e}"))?;

            if inserted > 0 {
                recorded.push(RecordedInteraction {
                    event: interaction.event_type.to_string(),
                });
                newly_seen
                    .entry(interaction.account_id)
                    .or_default()
                    .push(interaction.post_id);

                let (impression_delta, positive_delta, negative_delta) =
                    match interaction.event_type {
                        FeedInteractionType::QualifiedImpression => (1, 0, 0),
                        FeedInteractionType::Open => (0, 1, 0),
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

        // Exposure rows for the posts this batch confirmed as seen. Written
        // last, per account, so the prepared statement runs once per account
        // instead of once per interaction.
        let mut exposures = 0usize;
        for (account_id, post_ids) in &newly_seen {
            exposures += upsert_shown_posts(tx, *account_id, post_ids, &now_iso)?;
        }

        Ok((recorded, newly_seen, exposures))
    })?;
    // These sets only ever grow on this path, so merging keeps the cached dedup
    // sets warm: invalidating them on every impression batch would make the
    // next feed request rebuild both from scratch while the user scrolls.
    for (account_id, post_ids) in &newly_seen {
        let account = i64::from(*account_id);
        hydrate_cache::merge_recent_seen(account, post_ids);
        hydrate_cache::merge_long_term_seen(account, post_ids);
    }
    Ok(RecordedBatch {
        interactions: recorded,
        exposures,
    })
}

/// Everything the account has already **seen** inside the window: an explicit
/// interaction (`feed_interactions`) **or** an exposure the client confirmed by
/// keeping the card on screen long enough to count (`feed_shown_posts`).
///
/// The two sources are unioned here, on purpose, instead of being read by the
/// callers separately: the exposure registry is written from the client's own
/// confirmation — the only signal that means "a human actually saw this" — and
/// a caller that forgot to union it would silently restore the pre-V36
/// behaviour. `UNION` (not `UNION ALL`) collapses posts that were both seen and
/// interacted with.
///
/// The same statement serves the short and the long window; only the cutoff
/// differs. A confirmed sighting is evidence on its own, so there is no reason
/// for the two windows to grade it differently — and two nearly identical
/// statements only invite them to drift apart.
const SEEN_POST_IDS_SQL: &str = "
    SELECT post_id FROM feed_interactions
     WHERE account_id = ?1
       AND event_type IN ('qualified_impression', 'hide', 'open')
       AND created_at >= ?2
    UNION
    SELECT post_id FROM feed_shown_posts
     WHERE account_id = ?1
       AND last_shown_at >= ?2
";

/// Upsert for the exposure registry. Bumps `last_shown_at` and `shown_count`,
/// leaves `first_shown_at` alone — the first exposure is the one that dates
/// the entry.
const UPSERT_SHOWN_POST_SQL: &str = "
    INSERT INTO feed_shown_posts (account_id, post_id, first_shown_at, last_shown_at, shown_count)
    VALUES (?1, ?2, ?3, ?3, 1)
    ON CONFLICT(account_id, post_id) DO UPDATE SET
        last_shown_at = excluded.last_shown_at,
        shown_count = feed_shown_posts.shown_count + 1
";

/// Age-based cleanup of the exposure registry (it has no TTL of its own).
const PRUNE_SHOWN_POSTS_SQL: &str = "DELETE FROM feed_shown_posts WHERE last_shown_at < ?1";

/// Dedup window for **local** candidates (old cached posts), in days.
///
/// Live posts from e621 only ever needed the short window
/// (`runtime.dedup_lookback_days`, 14 by default) because a post the user has
/// not seen is a legitimate repeat a fortnight later. Local candidates are the
/// opposite: they come from a fixed catalog, so the same old post would keep
/// resurfacing every few weeks without a wider horizon.
///
/// This is a constant rather than a config key on purpose — it is not a knob
/// operators tune per deployment, it is the definition of "old enough to keep
/// out of the mix". [`SHOWN_RETENTION_DAYS`] is derived from it.
pub const LONG_TERM_DEDUP_DAYS: i64 = 60;

/// Retention for [`feed_shown_posts`], in days.
///
/// Must stay above the longest dedup window that reads the table, otherwise
/// pruning would silently shorten a window. `dedup_lookback_days` defaults to
/// 14 and [`LONG_TERM_DEDUP_DAYS`] is 60, so this leaves room for an operator
/// who raises the former.
pub const SHOWN_RETENTION_DAYS: i64 = 90;

/// Posts the user has already **seen** (qualified impression, explicit hide,
/// open-through to e621, **or an exposure the client confirmed**) within the
/// last `days`. Used to drop them from the candidate pool before scoring so the
/// same post doesn't surface across sessions.
///
/// `hide` is the load-bearing addition among the interactions: without it, a
/// hidden post would reappear on the very next page request because the dedup
/// query previously only matched `qualified_impression`. `open` is included
/// because the user has already seen the post in full and likely doesn't need
/// it surfaced again so soon. Confirmed exposure is included because a reload
/// (and every redeploy is one) used to erase the only other record of it — see
/// `migrations/V36__feed_shown_posts.sql`.
pub fn get_recently_seen_post_ids(account_id: i32, days: i64) -> Result<HashSet<i64>, String> {
    let account_id = i64::from(account_id);
    if let Some(cached) = hydrate_cache::recent_seen(account_id) {
        return Ok((*cached).clone());
    }
    let conn = open_db()?;
    let cutoff = (Utc::now() - chrono::Duration::days(days.max(1))).to_rfc3339();
    let mut stmt = conn
        .prepare(SEEN_POST_IDS_SQL)
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
///
/// Same evidence as the short window, only a longer cutoff: exposure recorded
/// in `feed_shown_posts` means a human actually saw the card, so it counts here
/// from the first sighting.
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
        .prepare(SEEN_POST_IDS_SQL)
        .map_err(|e| format!("prep get_long_term_seen_post_ids: {e}"))?;
    let rows = stmt
        .query_map(params![account_id, cutoff], |r| r.get::<_, i64>(0))
        .map_err(|e| format!("query get_long_term_seen_post_ids: {e}"))?;
    let set = rows
        .collect::<Result<HashSet<_>, _>>()
        .map_err(|e| format!("collect get_long_term_seen_post_ids: {e}"))?;
    Ok((*hydrate_cache::store_long_term_seen(account_id, set)).clone())
}

/// Write exposure rows for `post_ids` on an open write transaction.
///
/// Takes `&Connection` (a `Transaction` derefs to one) so the same statement
/// runs inside [`record_feed_interactions_batch`]'s transaction — where the
/// exposure row and the interaction that proves it commit together or not at
/// all — and from unit tests that hold a bare connection.
///
/// **Only the client may feed this.** A feed response means "the server
/// selected this post", not "the user saw it": the browser renders a window of
/// the list, and the rest never reaches a screen. Recording the selection is
/// what made good posts disappear from the feed without ever having been shown
/// — the opposite of the goal. The only caller is the interaction path, whose
/// `qualified_impression` is emitted after the card itself was visible long
/// enough to count.
///
/// Returns the number of rows SQLite reports as written (an upsert of an
/// existing `(account_id, post_id)` counts, so this is "exposures recorded").
fn upsert_shown_posts(
    conn: &Connection,
    account_id: i32,
    post_ids: &[i64],
    now: &str,
) -> Result<usize, String> {
    if post_ids.is_empty() {
        return Ok(0);
    }
    let mut stmt = conn
        .prepare_cached(UPSERT_SHOWN_POST_SQL)
        .map_err(|e| format!("Failed to prepare shown-post upsert: {e}"))?;
    let mut written = 0usize;
    for &post_id in post_ids {
        written += stmt
            .execute(params![account_id, post_id, now])
            .map_err(|e| format!("Failed to record shown post {post_id}: {e}"))?;
    }
    Ok(written)
}

/// Drop exposure rows older than `retention_days`. Returns `(before, after)`.
pub fn prune_shown_posts(retention_days: i64) -> Result<(usize, usize), String> {
    let cutoff = (Utc::now() - chrono::Duration::days(retention_days.max(1))).to_rfc3339();
    super::with_write_tx(|tx| {
        let before: i64 = tx
            .query_row("SELECT COUNT(*) FROM feed_shown_posts", [], |r| r.get(0))
            .map_err(|e| format!("Failed to count shown posts: {e}"))?;
        // One COUNT per tick, not two: the prune worker runs on a timer and
        // this table gains a row per confirmed sighting.
        let removed = tx
            .execute(PRUNE_SHOWN_POSTS_SQL, params![cutoff])
            .map_err(|e| format!("Failed to prune shown posts: {e}"))?;
        Ok((before as usize, before as usize - removed))
    })
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

/// Resolve a bounded list of tag ids to names.
///
/// Callers pass only the ids that survived a `LIMIT`, so this stays a handful
/// of primary-key lookups — the alternative (joining `tags` inside an
/// aggregate over millions of co-occurrence rows) pays one lookup per row.
fn tag_names_for_ids(
    conn: &Connection,
    ids: impl IntoIterator<Item = i64>,
) -> Result<HashMap<i64, String>, String> {
    let ids: Vec<i64> = ids.into_iter().collect();
    let mut names = HashMap::with_capacity(ids.len());
    for chunk in ids.chunks(800) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let mut stmt = conn
            .prepare(&format!(
                "SELECT id, name FROM tags WHERE id IN ({placeholders})"
            ))
            .map_err(|e| format!("prep partner names: {e}"))?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(chunk.iter()), |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(|e| format!("query partner names: {e}"))?;
        for row in rows {
            let (id, name) = row.map_err(|e| format!("read partner name: {e}"))?;
            names.insert(id, name);
        }
    }
    Ok(names)
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

/// Candidate posts carrying any of `tags`, newest first per tag.
///
/// Matched on tag name alone: `tags` is `UNIQUE(name, group_type)` and e621
/// keeps a name in exactly one category, so threading the group through would
/// add a parameter to every stream (preferred tags and neighbours mix groups)
/// without changing the result.
///
/// One `?` per tag, so the list must stay short — every caller caps it well
/// below SQLite's 999-variable floor.
///
/// One reverse index scan per tag rather than a single `ORDER BY p.id DESC`
/// over the `EXISTS` form. The anchors are IDF-ranked, so they are *rare* by
/// construction (that is the point of ranking them); walking `posts` backwards
/// until each rare tag matches would cover the catalog, and with several rare
/// tags in one `IN` list it would cover it several times. Scanning
/// `tags_posts(tag_id, post_id)` — the primary key, so the scan is covering and
/// already ordered — stops after `limit` rowids per tag. Measured on the local
/// catalog (2026-10-26): 0.5–3 ms per tag, including a tag carrying 6.7M
/// postings.
fn local_candidates_for_tag_names(
    conn: &Connection,
    tags: &[String],
    limit: i64,
) -> Result<Vec<i64>, String> {
    let mut merged: Vec<i64> = Vec::with_capacity(tags.len() * limit as usize);
    for tag in tags {
        // `?1` is the tag name, `?2` the limit. Resolving the name to `tag_id`
        // in a subquery keeps the caller's `&[String]` shape; `tags.name` is
        // unique, so the plan is a single covering-index probe.
        merged.extend(run_id_query(
            conn,
            "SELECT tp.post_id
             FROM tags_posts tp
             INNER JOIN posts p ON p.id = tp.post_id
             WHERE tp.tag_id = (SELECT id FROM tags WHERE name = ?1)
               AND p.is_deleted = 0
               AND p.preview_url IS NOT NULL
             ORDER BY tp.post_id DESC
             LIMIT ?2",
            &[tag as &dyn rusqlite::ToSql, &limit as &dyn rusqlite::ToSql],
        )?);
    }
    merged.sort_unstable_by(|a, b| b.cmp(a));
    merged.dedup();
    merged.truncate(limit as usize);
    Ok(merged)
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

/// Recent posts above the account's own popularity baseline, newest first.
///
/// `?1=created_at cutoff ?2=fav_count threshold ?3=limit`. The baseline is
/// passed in rather than read from `account_quality_profile` so `bin/calibrate`
/// can build the same stream from its training split.
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
    run_id_query(
        conn,
        "SELECT p.id
         FROM posts p
         WHERE p.created_at >= ?1
           AND p.is_deleted = 0
           AND p.preview_url IS NOT NULL
           AND p.fav_count >= ?2
         ORDER BY p.created_at DESC
         LIMIT ?3",
        &[
            &cutoff as &dyn rusqlite::ToSql,
            &threshold as &dyn rusqlite::ToSql,
            &limit as &dyn rusqlite::ToSql,
        ],
    )
}

/// Build the local candidate pool from pre-loaded sources.
///
/// Streams: IDF-ranked artist anchors, IDF-ranked character anchors,
/// recent-popular, explicit preferred tags, co-occurrence neighbours. Each
/// stream is capped independently and the streams are drained **round-robin**,
/// so the pool is a union of all five rather than the first few that happen to
/// be dense. Round-robin (not one stream after another) is what keeps the last
/// streams alive: a sequential union let artist + character + recent-popular
/// fill every slot, and the explicit preferred-tags stream — the only signal
/// the user states rather than one we infer — contributed nothing.
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

    let mut streams: Vec<std::vec::IntoIter<i64>> = [
        local_candidates_for_tag_names(conn, &artist_anchors, cap)?,
        local_candidates_for_tag_names(conn, &character_anchors, cap)?,
        local_candidates_recent_popular(conn, input.fav_baseline, tuning.recent_days, cap)?,
        local_candidates_for_tag_names(conn, &preferred, cap)?,
        local_candidates_for_tag_names(conn, &neighbours, cap)?,
    ]
    .into_iter()
    .map(Vec::into_iter)
    .collect();

    let mut out: Vec<i64> = Vec::with_capacity(limit as usize);
    let mut seen: HashSet<i64> = HashSet::with_capacity(limit as usize);
    let mut taken = vec![0usize; streams.len()];
    'fill: loop {
        let mut progressed = false;
        for (i, stream) in streams.iter_mut().enumerate() {
            if out.len() as i64 >= limit {
                break 'fill;
            }
            // Charge each stream against its own cap, never against the
            // pool's running size: a shared counter is what let the first
            // (densest) stream consume every slot while the rest broke out on
            // their first element.
            if taken[i] >= cap as usize {
                continue;
            }
            // Skip ids another stream already contributed before charging
            // this one. `insert` returning false means it was a duplicate, so
            // it must not count against this stream's budget.
            for id in stream.by_ref() {
                if seen.insert(id) {
                    out.push(id);
                    taken[i] += 1;
                    progressed = true;
                    break;
                }
            }
        }
        if !progressed {
            break;
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

    // Fold both members of every pair together because the table stores each
    // pair once, canonically as `tag1_id < tag2_id`: a tag that only ever lands
    // in the `tag2_id` column would otherwise lose every one of its counts.
    //
    // Aggregate by tag **id**, and resolve names only for the handful of
    // partners that survive the LIMIT. The previous form joined `tags` inside
    // both `UNION ALL` branches, i.e. one primary-key lookup plus a name copy
    // for every one of the ~5.5M rows an active account contributes. Measured
    // on the production database (2026-10-19, account 18044, 2.7M pairs):
    // 0.77 s for that name-join form versus 0.56 s for the id-only aggregate,
    // with an identical total. Migration V38 replaces
    // `idx_atc_account_cooc(account_id, cooc_count)` with a covering
    // `(account_id, cooc_count, tag1_id, tag2_id)` index, so the two scans
    // below become index-only.
    //
    // The temp B-tree for `GROUP BY` remains — `cooc_count >= 2` admits almost
    // every row — but it no longer carries strings.
    //
    // Over-fetch a little so the final `(total DESC, name ASC)` order is the
    // same one the name-keyed query produced. Ties that span more than the
    // over-fetch window are broken by id; either order is deterministic.
    let fetch = tuning.cooccurrence_limit as i64 * 4 + 16;
    let mut stmt = conn
        .prepare(
            "SELECT partner_id, SUM(c) AS total
             FROM (
                 SELECT tag1_id AS partner_id, cooc_count AS c
                 FROM account_tag_cooccurrence
                 WHERE account_id = ?1 AND cooc_count >= ?2
                 UNION ALL
                 SELECT tag2_id AS partner_id, cooc_count AS c
                 FROM account_tag_cooccurrence
                 WHERE account_id = ?1 AND cooc_count >= ?2
             )
             GROUP BY partner_id
             ORDER BY SUM(c) DESC, partner_id ASC
             LIMIT ?3",
        )
        .map_err(|e| format!("prep pool input/cooc: {e}"))?;
    let partners = stmt
        .query_map(
            params![account_id, tuning.cooccurrence_min_cooc, fetch],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )
        .map_err(|e| format!("query pool input/cooc: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("collect pool input/cooc: {e}"))?;
    drop(stmt);

    let partner_names = tag_names_for_ids(conn, partners.iter().map(|(id, _)| *id))?;
    let mut named: Vec<(i64, String)> = partners
        .iter()
        .filter_map(|(id, total)| partner_names.get(id).map(|name| (*total, name.clone())))
        .collect();
    named.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    named.truncate(tuning.cooccurrence_limit);
    let cooccurrence: Vec<String> = named.into_iter().map(|(_, name)| name).collect();

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
/// ## Ordering
/// Every stream returns its **newest** candidates: the four tag streams walk
/// `tags_posts(post_id)` backwards, recent-popular is ordered by
/// `created_at DESC`. Newest-first is what holds the pool's median age at
/// ~16 days instead of ~1000 (measured 2026-10-27 at both 80 and 400 slots):
/// freshness is a property of the ordering, not of the pool size, so there is
/// deliberately no random window here.
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

    /// `recent` must mean "newest by post id", not "some newest posts in an
    /// arbitrary order" — the pool is later merged into a newest-first page and
    /// the stream order is what decides which posts make the cut when it is
    /// truncated.
    #[test]
    fn recent_tag_sampling_takes_the_newest_posts_newest_first() {
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
        conn.execute_batch(
            "INSERT INTO tags (name, group_type) VALUES ('artist_tag_1','artist'),('artist_tag_2','artist');
             INSERT INTO posts (id, created_at, score_total, fav_count, rating, last_seen_at, preview_url)
                SELECT column1 AS id, '2026-08-13T00:00:00Z', 50, 5, 'q', '2026-08-13T00:00:00Z', 'p/'||column1||'.jpg'
                FROM (VALUES (1),(2),(3),(4),(5),(6),(7),(8),(9),(10),(11),(12),(13),(14),(15),(16),(17),(18),(19),(20));",
        )
        .unwrap();
        for i in 1..=20i64 {
            // Tag 1 carries every post, tag 2 only the even ones, so the union
            // exercises both the merge and the dedup.
            let tag_id = if i % 2 == 0 { 2 } else { 1 };
            conn.execute(
                "INSERT INTO tags_posts (tag_id, post_id) VALUES (?1,?2)",
                [tag_id, i],
            )
            .unwrap();
            if i % 2 == 0 {
                conn.execute(
                    "INSERT INTO tags_posts (tag_id, post_id) VALUES (1,?1)",
                    [i],
                )
                .unwrap();
            }
        }
        let newest =
            local_candidates_for_tag_names(&conn, &["artist_tag_1".to_string()], 5).unwrap();
        assert_eq!(
            newest,
            vec![20, 19, 18, 17, 16],
            "newest five, newest first"
        );

        let both = local_candidates_for_tag_names(
            &conn,
            &["artist_tag_1".to_string(), "artist_tag_2".to_string()],
            6,
        )
        .unwrap();
        assert_eq!(both, vec![20, 19, 18, 17, 16, 15], "merged newest-first");
        assert_eq!(
            both.iter().filter(|id| **id == 20).count(),
            1,
            "a post carrying both tags must appear once"
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

    /// With the pool reduced to a supplement (tens of ids, not hundreds), a
    /// sequential union let the first three streams fill every slot and the
    /// last two contribute nothing — the explicit preferred-tags stream among
    /// them. Round-robin has to give every non-empty stream a share even when
    /// the pool is much smaller than five times the per-stream cap.
    #[test]
    fn every_stream_survives_a_small_pool() {
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
        for i in 1..=50i64 {
            // Only the last ten qualify for recent-popular (fav >= 0.6 * 50).
            let fav = if i > 40 { 50 } else { 0 };
            conn.execute(
                "INSERT INTO posts (id, created_at, score_total, fav_count, rating, last_seen_at, is_deleted, preview_url)
                 VALUES (?1,?2,100,?3,'q',?2,0,'p/'||?1||'.jpg')",
                rusqlite::params![i, now, fav],
            )
            .unwrap();
        }
        conn.execute_batch(
            "INSERT INTO tags (name, group_type) VALUES
                ('a_art','artist'),('c_chr','character'),('p_pref','general'),('n_cooc','general');",
        )
        .unwrap();
        // 1-10 artist, 11-20 character, 21-30 preferred, 31-40 neighbour.
        for (tag_id, start) in [(1i64, 1i64), (2, 11), (3, 21), (4, 31)] {
            for i in start..start + 10 {
                conn.execute(
                    "INSERT INTO tags_posts (tag_id, post_id) VALUES (?1,?2)",
                    rusqlite::params![tag_id, i],
                )
                .unwrap();
            }
        }

        let idf = IdfIndex::from_df(&HashMap::new(), 1000);
        let input = CandidatePoolInput {
            tag_counts: vec![
                ("a_art".to_string(), "artist".to_string(), 50),
                ("c_chr".to_string(), "character".to_string(), 50),
            ],
            cooccurrence: vec!["n_cooc".to_string()],
            preferred: vec!["p_pref".to_string()],
            fav_baseline: 50.0,
        };
        let tuning = CandidatePoolTuning {
            anchor_tags: 20,
            recent_days: 60,
            stream_share: 0.34,
            preferred_limit: 50,
            cooccurrence_limit: 50,
            cooccurrence_min_cooc: 2,
        };

        // cap = ceil(20 * 0.34) = 7, so a sequential union stops after three
        // streams (7 + 7 + 6) and never reaches preferred/co-occurrence.
        let pool = collect_candidate_ids(&conn, &input, &idf, &tuning, 20).unwrap();
        assert_eq!(pool.len(), 20);
        for (name, range) in [
            ("artist", 1..=10),
            ("character", 11..=20),
            ("preferred", 21..=30),
            ("neighbour", 31..=40),
            ("recent-popular", 41..=50),
        ] {
            assert!(
                pool.iter().any(|id| range.contains(id)),
                "{name} stream contributed nothing: {pool:?}"
            );
        }
    }

    // ── Exposure registry (V36) ─────────────────────────────────────
    //
    // These exercise the SQL constants against a hand-built twin of the real
    // schema. They are hermetic on purpose: the shared integration DB is
    // mutated by dozens of other tests, and a dedup assertion that depends on
    // what those left behind is the failure mode this file keeps producing.

    /// Minimal twin of the two registries — only the columns the dedup queries
    /// read, so a passing test cannot be an accident of a migration.
    fn seen_registry_db() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE feed_interactions (
                account_id INTEGER NOT NULL,
                post_id INTEGER NOT NULL,
                event_type TEXT NOT NULL,
                created_at TEXT NOT NULL
             );
             CREATE TABLE feed_shown_posts (
                account_id INTEGER NOT NULL,
                post_id INTEGER NOT NULL,
                first_shown_at TEXT NOT NULL,
                last_shown_at TEXT NOT NULL,
                shown_count INTEGER NOT NULL DEFAULT 1,
                PRIMARY KEY (account_id, post_id)
             );",
        )
        .unwrap();
        conn
    }

    fn seen_ids(conn: &rusqlite::Connection, sql: &str, days: i64) -> HashSet<i64> {
        let cutoff = (Utc::now() - chrono::Duration::days(days)).to_rfc3339();
        let mut stmt = conn.prepare(sql).unwrap();
        let rows = stmt
            .query_map(params![7i64, cutoff], |r| r.get::<_, i64>(0))
            .unwrap();
        rows.collect::<Result<HashSet<_>, _>>().unwrap()
    }

    /// The whole point of V36: a post the client confirmed as seen is excluded
    /// from the pool exactly like one the user interacted with, and the
    /// exclusion is scoped to the account and to the window.
    #[test]
    fn seen_set_unions_confirmed_exposures_with_interactions() {
        let conn = seen_registry_db();
        let now = Utc::now().to_rfc3339();
        let month_ago = (Utc::now() - chrono::Duration::days(30)).to_rfc3339();

        // 1 — explicit interaction, in window.
        conn.execute(
            "INSERT INTO feed_interactions VALUES (7, 1, 'open', ?1)",
            params![now],
        )
        .unwrap();
        // 2 — confirmed as seen by the client, no interaction of its own. This
        // row is the one V36 adds.
        conn.execute(
            "INSERT INTO feed_shown_posts VALUES (7, 2, ?1, ?1, 1)",
            params![now],
        )
        .unwrap();
        // 3 — confirmed a month ago: outside the short window.
        conn.execute(
            "INSERT INTO feed_shown_posts VALUES (7, 3, ?1, ?1, 1)",
            params![month_ago],
        )
        .unwrap();
        // 4 — another account's exposure must not leak into account 7.
        conn.execute(
            "INSERT INTO feed_shown_posts VALUES (8, 4, ?1, ?1, 1)",
            params![now],
        )
        .unwrap();

        assert_eq!(
            seen_ids(&conn, SEEN_POST_IDS_SQL, 14),
            HashSet::from([1, 2]),
            "short window must cover interactions and fresh sightings only"
        );
    }

    /// The short and the long window read the same statement, so one confirmed
    /// sighting counts in both. Only the cutoff differs — there is no second
    /// sighting required, because nothing here is an over-recorded serving.
    #[test]
    fn long_term_window_keeps_sightings_past_the_short_one() {
        let conn = seen_registry_db();
        let month_ago = (Utc::now() - chrono::Duration::days(30)).to_rfc3339();

        conn.execute(
            "INSERT INTO feed_shown_posts VALUES (7, 10, ?1, ?1, 1)",
            params![month_ago],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO feed_interactions VALUES (7, 11, 'hide', ?1)",
            params![month_ago],
        )
        .unwrap();

        assert!(
            seen_ids(&conn, SEEN_POST_IDS_SQL, 14).is_empty(),
            "a month-old sighting is outside the short window"
        );
        assert_eq!(
            seen_ids(&conn, SEEN_POST_IDS_SQL, 60),
            HashSet::from([10, 11]),
            "the long window must keep a sighting without a repeat"
        );
    }

    /// The upsert must not reset `first_shown_at` — the entry's age is what
    /// retention prunes on, and a post seen daily would otherwise never age
    /// out of its own row. Exercises the same helper the interaction batch
    /// calls, so a passing test cannot be about a different statement.
    #[test]
    fn shown_post_upsert_counts_exposures_and_keeps_first_shown_at() {
        let conn = seen_registry_db();
        let first = "2026-10-01T00:00:00+00:00";
        let later = "2026-10-02T00:00:00+00:00";

        assert_eq!(
            upsert_shown_posts(&conn, 7, &[100], first).unwrap(),
            1,
            "first sighting inserts one row"
        );
        assert_eq!(
            upsert_shown_posts(&conn, 7, &[100], later).unwrap(),
            1,
            "a repeat sighting updates the existing row"
        );

        let (first_at, last_at, count): (String, String, i64) = conn
            .query_row(
                "SELECT first_shown_at, last_shown_at, shown_count FROM feed_shown_posts \
                 WHERE account_id = 7 AND post_id = 100",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(count, 2, "second exposure must bump shown_count");
        assert_eq!(first_at, first, "first_shown_at must not move");
        assert_eq!(
            last_at, later,
            "last_shown_at must follow the newest exposure"
        );
    }

    /// Retention deletes by age and touches nothing younger.
    #[test]
    fn prune_shown_posts_drops_only_expired_rows() {
        let conn = seen_registry_db();
        let now = Utc::now().to_rfc3339();
        let ancient = (Utc::now() - chrono::Duration::days(200)).to_rfc3339();
        conn.execute(
            "INSERT INTO feed_shown_posts VALUES (7, 1, ?1, ?1, 1)",
            params![now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO feed_shown_posts VALUES (7, 2, ?1, ?1, 1)",
            params![ancient],
        )
        .unwrap();

        let cutoff = (Utc::now() - chrono::Duration::days(SHOWN_RETENTION_DAYS)).to_rfc3339();
        let removed = conn
            .execute(PRUNE_SHOWN_POSTS_SQL, params![cutoff])
            .unwrap();
        assert_eq!(removed, 1);

        let left: Vec<i64> = conn
            .prepare("SELECT post_id FROM feed_shown_posts ORDER BY post_id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(left, vec![1], "fresh exposure must survive the prune");
    }

    /// Losing the primary-key prefix would turn every feed request into a
    /// full scan of the exposure registry — correct, but slower every day.
    #[test]
    fn seen_lookup_uses_the_exposure_primary_key() {
        let conn = seen_registry_db();
        let plan = {
            let mut stmt = conn
                .prepare(&format!("EXPLAIN QUERY PLAN {SEEN_POST_IDS_SQL}"))
                .unwrap();
            let rows = stmt
                .query_map(params![7i64, "2026-01-01T00:00:00+00:00"], |r| {
                    r.get::<_, String>(3)
                })
                .unwrap();
            rows.collect::<Result<Vec<_>, _>>().unwrap().join("\n")
        };
        let line = plan
            .lines()
            .find(|l| l.contains("feed_shown_posts"))
            .expect("plan mentions feed_shown_posts");
        assert!(
            line.contains("SEARCH") && line.contains("INDEX"),
            "exposure lookup no longer seeks the (account_id, post_id) key:\n{plan}"
        );
    }
}
