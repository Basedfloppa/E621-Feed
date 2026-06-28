use chrono::{DateTime, Utc};
use rusqlite::params;

use crate::models::{
    AccountMediaStat, AccountPreferenceProfile, AccountQualityProfile, AccountRatingStat,
    AccountRecencyProfile, AccountTagFeedback, AccountUploaderStat,
};

use super::{
    cooccurrence::set_account_tag_cooccurrence, open_db, parse_db_datetime, set_tag_counts,
};

pub fn set_rating_profile(account_id: i32) -> Result<(), String> {
    super::with_write_tx(|tx| {
        tx.execute(
            "DELETE FROM account_rating_profile WHERE account_id = ?1",
            params![account_id],
        )
        .map_err(|e| format!("Failed to clear rating profile: {e}"))?;

        tx.execute(
            "
            INSERT INTO account_rating_profile (account_id, rating, count)
            SELECT ?1, p.rating, COUNT(*)
            FROM posts p
            INNER JOIN accounts_post ap ON ap.post_id = p.id
            WHERE ap.account_id = ?1
            GROUP BY p.rating
            ",
            params![account_id],
        )
        .map_err(|e| format!("Failed to populate rating profile: {e}"))?;
        Ok(())
    })
}

pub fn set_media_profile(account_id: i32) -> Result<(), String> {
    super::with_write_tx(|tx| {
        tx.execute(
            "DELETE FROM account_media_profile WHERE account_id = ?1",
            params![account_id],
        )
        .map_err(|e| format!("Failed to clear media profile: {e}"))?;

        tx.execute(
            "
            INSERT INTO account_media_profile (account_id, media_type, count)
            SELECT
                ?1,
                CASE
                    WHEN LOWER(COALESCE(p.file_ext, '')) = 'gif' THEN 'animated'
                    WHEN LOWER(COALESCE(p.file_ext, '')) IN ('webm', 'mp4') OR COALESCE(p.duration, 0) > 0 THEN 'video'
                    WHEN COALESCE(p.is_animated, 0) = 1 THEN 'animated'
                    ELSE 'image'
                END AS media_type,
                COUNT(*)
            FROM posts p
            INNER JOIN accounts_post ap ON ap.post_id = p.id
            WHERE ap.account_id = ?1
            GROUP BY media_type
            ",
            params![account_id],
        )
        .map_err(|e| format!("Failed to populate media profile: {e}"))?;
        Ok(())
    })
}

pub fn set_quality_profile(account_id: i32) -> Result<(), String> {
    super::with_write_tx(|tx| {
        tx.execute(
            "DELETE FROM account_quality_profile WHERE account_id = ?1",
            params![account_id],
        )
        .map_err(|e| format!("Failed to clear quality profile: {e}"))?;

        tx.execute(
            "
            INSERT INTO account_quality_profile (
                account_id, avg_score_total, avg_fav_count, avg_comment_count, avg_duration
            )
            SELECT
                ?1,
                COALESCE(AVG(p.score_total), 0),
                COALESCE(AVG(p.fav_count), 0),
                COALESCE(AVG(p.comment_count), 0),
                COALESCE(AVG(COALESCE(p.duration, 0)), 0)
            FROM posts p
            INNER JOIN accounts_post ap ON ap.post_id = p.id
            WHERE ap.account_id = ?1
            ",
            params![account_id],
        )
        .map_err(|e| format!("Failed to populate quality profile: {e}"))?;
        Ok(())
    })
}

pub fn set_recency_profile(account_id: i32) -> Result<(), String> {
    super::with_write_tx(|tx| {
        // The `WHERE true` disambiguates ON CONFLICT from a join clause
        // (SQLite UPSERT-with-SELECT quirk).
        tx.execute(
            "
            WITH ages AS (
                SELECT max(0.0, julianday('now') - julianday(p.created_at)) AS age
                FROM posts p
                INNER JOIN accounts_post ap ON ap.post_id = p.id
                WHERE ap.account_id = ?1
            ),
            m AS (SELECT COALESCE(AVG(age), 0.0) AS mean FROM ages)
            INSERT INTO account_recency_profile (account_id, avg_age_days, avg_abs_dev_days)
            SELECT
                ?1,
                m.mean,
                COALESCE((SELECT AVG(ABS(age - m.mean)) FROM ages), 0.0)
            FROM m
            WHERE true
            ON CONFLICT(account_id) DO UPDATE SET
                avg_age_days = excluded.avg_age_days,
                avg_abs_dev_days = excluded.avg_abs_dev_days
            ",
            params![account_id],
        )
        .map_err(|e| format!("Failed to upsert recency profile: {e}"))?;
        Ok(())
    })
}

pub fn set_uploader_profile(account_id: i32) -> Result<(), String> {
    super::with_write_tx(|tx| {
        tx.execute(
            "DELETE FROM account_uploader_profile WHERE account_id = ?1",
            params![account_id],
        )
        .map_err(|e| format!("Failed to clear uploader profile: {e}"))?;

        tx.execute(
            "
            INSERT INTO account_uploader_profile (account_id, uploader_id, post_count, avg_score, avg_fav)
            SELECT
                ?1,
                p.uploader_id,
                COUNT(*)                       AS post_count,
                COALESCE(AVG(p.score_total), 0) AS avg_score,
                COALESCE(AVG(p.fav_count), 0)   AS avg_fav
            FROM posts p
            INNER JOIN accounts_post ap ON ap.post_id = p.id
            WHERE ap.account_id = ?1 AND p.uploader_id IS NOT NULL
            GROUP BY p.uploader_id
            ",
            params![account_id],
        )
        .map_err(|e| format!("Failed to populate uploader profile: {e}"))?;
        Ok(())
    })
}

pub fn get_account_uploader_profile(account_id: i32) -> Result<Vec<AccountUploaderStat>, String> {
    let conn = super::open_db()?;
    let mut stmt = conn
        .prepare(
            "SELECT uploader_id, post_count, avg_score, avg_fav
             FROM account_uploader_profile
             WHERE account_id = ?
             ORDER BY post_count DESC",
        )
        .map_err(|e| format!("Failed to prepare uploader profile query: {e}"))?;

    stmt.query_map([account_id], |row| {
        Ok(AccountUploaderStat {
            uploader_id: row.get(0)?,
            post_count: row.get(1)?,
            avg_score: row.get(2)?,
            avg_fav: row.get(3)?,
        })
    })
    .map_err(|e| format!("Failed to fetch uploader profile: {e}"))?
    .collect::<Result<Vec<_>, _>>()
    .map_err(|e| format!("Failed to enumerate uploader profile: {e}"))
}

pub fn refresh_account_profiles(account_id: i32) -> Result<(), String> {
    refresh_account_profiles_impl(account_id, false)
}

/// Like `refresh_account_profiles` but optionally skips the expensive
/// `set_account_tag_cooccurrence` full rebuild. When `skip_cooccurrence`
/// is true, cooccurrence must have been built incrementally during
/// `save_posts_tags_batch` — otherwise recommendations will lack
/// account-level tag-relation signal.
pub fn refresh_account_profiles_skip_cooc(account_id: i32) -> Result<(), String> {
    refresh_account_profiles_impl(account_id, true)
}

fn refresh_account_profiles_impl(account_id: i32, skip_cooccurrence: bool) -> Result<(), String> {
    set_tag_counts(account_id)?;
    set_rating_profile(account_id)?;
    set_media_profile(account_id)?;
    set_quality_profile(account_id)?;
    set_recency_profile(account_id)?;
    set_uploader_profile(account_id)?;
    if !skip_cooccurrence {
        set_account_tag_cooccurrence(account_id)?;
    }
    decay_account_tag_feedback(account_id)?;
    // Record refresh timestamp for time-weighted interaction_fit decay.
    let now = Utc::now().to_rfc3339();
    super::with_write_tx(|tx| {
        tx.execute(
            "UPDATE accounts SET profile_refreshed_at = ?1 WHERE id = ?2",
            params![now, account_id],
        )
        .map_err(|e| format!("Failed to set profile_refreshed_at: {e}"))?;
        Ok(())
    })?;
    // Invalidate the in-memory tag-counts cache so the next request
    // re-reads the fresh data.
    super::tags::clear_tag_counts_cache(account_id);
    Ok(())
}

/// Multiplies per-tag feedback counts by `0.5 ^ (elapsed / half_life)` and
/// deletes rows that hit zero. 1-day gate + `round` keep frequent calls from
/// bleeding counts. SELECT + UPDATEs in one IMMEDIATE tx so concurrent
/// `record_feed_interaction` can't race the snapshot.
pub fn decay_account_tag_feedback(account_id: i32) -> Result<(), String> {
    let cfg = crate::models::cfg();
    let half_life = cfg.priors.feedback_decay_half_life_days.max(1.0);
    let now = Utc::now();
    let now_iso = now.to_rfc3339();

    super::with_write_tx(|tx| {
        let rows: Vec<(String, String, String, i64, i64, i64)> = {
            let mut stmt = tx
                .prepare(
                    "SELECT tag_name, group_type, last_decayed_at,
                            impression_count, positive_count, negative_count
                     FROM account_tag_feedback
                     WHERE account_id = ?1",
                )
                .map_err(|e| format!("Failed to prepare decay query: {e}"))?;

            stmt.query_map(params![account_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })
            .map_err(|e| format!("Failed to fetch decay rows: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("Failed to enumerate decay rows: {e}"))?
        };

        let mut update = tx
            .prepare_cached(
                "UPDATE account_tag_feedback
                 SET impression_count = ?1,
                     positive_count   = ?2,
                     negative_count   = ?3,
                     last_decayed_at  = ?4
                 WHERE account_id = ?5 AND tag_name = ?6 AND group_type = ?7",
            )
            .map_err(|e| format!("Failed to prepare decay update: {e}"))?;
        let mut delete = tx
            .prepare_cached(
                "DELETE FROM account_tag_feedback
                 WHERE account_id = ?1 AND tag_name = ?2 AND group_type = ?3",
            )
            .map_err(|e| format!("Failed to prepare decay delete: {e}"))?;

        for (tag_name, group_type, last_decayed_raw, imp, pos, neg) in rows {
            let last_decayed = if last_decayed_raw.is_empty() {
                now
            } else {
                parse_db_datetime(&last_decayed_raw).unwrap_or(now)
            };
            let elapsed_days = (now - last_decayed).num_seconds() as f32 / 86_400.0;
            if elapsed_days < 1.0 {
                continue;
            }
            let factor = (-std::f32::consts::LN_2 * elapsed_days / half_life).exp();
            let new_imp = (imp as f32 * factor).round() as i64;
            let new_pos = (pos as f32 * factor).round() as i64;
            let new_neg = (neg as f32 * factor).round() as i64;

            if new_imp == 0 && new_pos == 0 && new_neg == 0 {
                delete
                    .execute(params![account_id, tag_name, group_type])
                    .map_err(|e| format!("Failed to delete decayed row: {e}"))?;
            } else {
                update
                    .execute(params![
                        new_imp, new_pos, new_neg, now_iso, account_id, tag_name, group_type
                    ])
                    .map_err(|e| format!("Failed to update decayed row: {e}"))?;
            }
        }
        Ok(())
    })
}

pub fn get_account_rating_profile(account_id: i32) -> Result<Vec<AccountRatingStat>, String> {
    let conn = open_db()?;
    let mut stmt = conn
        .prepare(
            "SELECT rating, count FROM account_rating_profile WHERE account_id = ? ORDER BY count DESC",
        )
        .map_err(|e| format!("Failed to prepare rating profile query: {e}"))?;

    stmt.query_map([account_id], |row| {
        Ok(AccountRatingStat {
            rating: row.get(0)?,
            count: row.get(1)?,
        })
    })
    .map_err(|e| format!("Failed to fetch rating profile: {e}"))?
    .collect::<Result<Vec<_>, _>>()
    .map_err(|e| format!("Failed to enumerate rating profile: {e}"))
}

pub fn get_account_media_profile(account_id: i32) -> Result<Vec<AccountMediaStat>, String> {
    let conn = open_db()?;
    let mut stmt = conn
        .prepare(
            "SELECT media_type, count FROM account_media_profile WHERE account_id = ? ORDER BY count DESC",
        )
        .map_err(|e| format!("Failed to prepare media profile query: {e}"))?;

    stmt.query_map([account_id], |row| {
        Ok(AccountMediaStat {
            media_type: row.get(0)?,
            count: row.get(1)?,
        })
    })
    .map_err(|e| format!("Failed to fetch media profile: {e}"))?
    .collect::<Result<Vec<_>, _>>()
    .map_err(|e| format!("Failed to enumerate media profile: {e}"))
}

pub fn get_account_quality_profile(account_id: i32) -> Result<AccountQualityProfile, String> {
    let conn = open_db()?;
    let mut stmt = conn
        .prepare(
            "
            SELECT avg_score_total, avg_fav_count, avg_comment_count, avg_duration
            FROM account_quality_profile
            WHERE account_id = ?
            ",
        )
        .map_err(|e| format!("Failed to prepare quality profile query: {e}"))?;

    match stmt.query_row([account_id], |row| {
        Ok(AccountQualityProfile {
            avg_score_total: row.get(0)?,
            avg_fav_count: row.get(1)?,
            avg_comment_count: row.get(2)?,
            avg_duration: row.get(3)?,
        })
    }) {
        Ok(profile) => Ok(profile),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(AccountQualityProfile {
            avg_score_total: 0.0,
            avg_fav_count: 0.0,
            avg_comment_count: 0.0,
            avg_duration: 0.0,
        }),
        Err(e) => Err(format!("Failed to fetch quality profile: {e}")),
    }
}

pub fn get_account_recency_profile(account_id: i32) -> Result<AccountRecencyProfile, String> {
    let conn = open_db()?;
    let mut stmt = conn
        .prepare(
            "SELECT avg_age_days, avg_abs_dev_days FROM account_recency_profile WHERE account_id = ?",
        )
        .map_err(|e| format!("Failed to prepare recency profile query: {e}"))?;

    match stmt.query_row([account_id], |row| {
        Ok(AccountRecencyProfile {
            avg_age_days: row.get(0)?,
            avg_abs_dev_days: row.get(1)?,
        })
    }) {
        Ok(profile) => Ok(profile),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(AccountRecencyProfile {
            avg_age_days: 0.0,
            avg_abs_dev_days: 0.0,
        }),
        Err(e) => Err(format!("Failed to fetch recency profile: {e}")),
    }
}

pub fn get_account_preference_profile(account_id: i32) -> Result<AccountPreferenceProfile, String> {
    let conn = open_db()?;
    let refreshed_at: Option<String> = conn
        .query_row(
            "SELECT profile_refreshed_at FROM accounts WHERE id = ?",
            rusqlite::params![account_id],
            |row| row.get(0),
        )
        .ok();
    let last_refreshed_at = match refreshed_at {
        Some(ref s) if !s.is_empty() => {
            Some(s.parse::<DateTime<Utc>>().unwrap_or_else(|_| Utc::now()))
        }
        _ => None,
    };
    Ok(AccountPreferenceProfile {
        rating: get_account_rating_profile(account_id)?,
        media: get_account_media_profile(account_id)?,
        feedback: get_account_tag_feedback(account_id)?,
        quality: get_account_quality_profile(account_id)?,
        recency: get_account_recency_profile(account_id)?,
        uploaders: get_account_uploader_profile(account_id)?,
        last_refreshed_at,
        preferred_tags: get_account_preferred_tags(account_id)?,
    })
}

/// Load preferred tags for an account. Returns empty vec if none set.
pub fn get_account_preferred_tags(
    account_id: i32,
) -> Result<Vec<crate::models::PreferredTag>, String> {
    let conn = open_db()?;
    let mut stmt = conn
        .prepare(
            "SELECT tag_name, group_type, weight
             FROM account_preferred_tags
             WHERE account_id = ?
             ORDER BY tag_name",
        )
        .map_err(|e| format!("Failed to prepare preferred tags query: {e}"))?;

    let rows = stmt
        .query_map([account_id], |row| {
            Ok(crate::models::PreferredTag {
                tag: row.get(0)?,
                group: row.get(1)?,
                weight: row.get(2)?,
            })
        })
        .map_err(|e| format!("Failed to query preferred tags: {e}"))?;

    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to enumerate preferred tags: {e}"))
}

pub fn get_account_tag_feedback(account_id: i32) -> Result<Vec<AccountTagFeedback>, String> {
    let conn = open_db()?;
    let mut stmt = conn
        .prepare(
            "
            SELECT tag_name, group_type, impression_count, positive_count, negative_count, last_interaction_at
            FROM account_tag_feedback
            WHERE account_id = ?
            ORDER BY (positive_count - negative_count) DESC, impression_count DESC
            ",
        )
        .map_err(|e| format!("Failed to prepare tag feedback query: {e}"))?;

    stmt.query_map([account_id], |row| {
        Ok(AccountTagFeedback {
            tag_name: row.get(0)?,
            group_type: row.get(1)?,
            impression_count: row.get(2)?,
            positive_count: row.get(3)?,
            negative_count: row.get(4)?,
            last_interaction_at: row.get(5)?,
        })
    })
    .map_err(|e| format!("Failed to fetch tag feedback: {e}"))?
    .collect::<Result<Vec<_>, _>>()
    .map_err(|e| format!("Failed to enumerate tag feedback: {e}"))
}
