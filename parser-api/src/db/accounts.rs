use chrono::{NaiveDate, Utc};
use rusqlite::params;

use crate::models::{PreferredTag, TruncatedAccount, cfg};

use super::open_db;

/// Visit-activity summary returned by `get_visit_stats`.
#[derive(Debug, Clone)]
pub struct VisitStats {
    pub visit_streak: i32,
    pub avg_gap_days: f64,
    pub total_visits_30d: i32,
}

/// Record or refresh the visit for `account_id` as of today.
/// Idempotent — calling twice in the same day is a no-op for streak/gap.
pub fn update_visit_tracker(account_id: i32) -> Result<VisitStats, String> {
    let today = Utc::now().date_naive().to_string(); // YYYY-MM-DD
    super::with_write_tx(|tx| {
        // Read current row (if any).
        let existing: Option<(String, i32, f64, i32)> = tx
            .query_row(
                "SELECT last_visit_date, visit_streak, avg_visit_gap_days, total_visits_30d \
                 FROM user_visit_tracker WHERE account_id = ?1",
                params![account_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .ok();

        if let Some((last_date, streak, avg_gap, total_30d)) = existing {
            if last_date == today {
                // Already recorded for today — just return current stats.
                return Ok(VisitStats {
                    visit_streak: streak,
                    avg_gap_days: avg_gap,
                    total_visits_30d: total_30d,
                });
            }

            // Calculate days since last visit.
            let last = NaiveDate::parse_from_str(&last_date, "%Y-%m-%d")
                .map_err(|e| format!("Failed to parse last_visit_date: {e}"))?;
            let current = NaiveDate::parse_from_str(&today, "%Y-%m-%d")
                .map_err(|e| format!("Failed to parse today: {e}"))?;
            let gap_days = (current - last).num_days() as f64;

            let new_streak = if gap_days == 1.0 { streak + 1 } else { 0 };
            let new_avg_gap = avg_gap * 0.9 + gap_days * 0.1;
            let new_total_30d = total_30d + 1;

            tx.execute(
                "UPDATE user_visit_tracker SET \
                 last_visit_date = ?1, visit_streak = ?2, \
                 avg_visit_gap_days = ?3, total_visits_30d = ?4 \
                 WHERE account_id = ?5",
                params![today, new_streak, new_avg_gap, new_total_30d, account_id],
            )
            .map_err(|e| format!("Failed to update visit tracker: {e}"))?;

            Ok(VisitStats {
                visit_streak: new_streak,
                avg_gap_days: new_avg_gap,
                total_visits_30d: new_total_30d,
            })
        } else {
            // First-ever visit.
            tx.execute(
                "INSERT INTO user_visit_tracker (account_id, last_visit_date, visit_streak, avg_visit_gap_days, total_visits_30d) \
                 VALUES (?1, ?2, 1, 7.0, 1)",
                params![account_id, today],
            )
            .map_err(|e| format!("Failed to insert visit tracker: {e}"))?;

            Ok(VisitStats {
                visit_streak: 1,
                avg_gap_days: 7.0,
                total_visits_30d: 1,
            })
        }
    })
}

/// Read current visit stats without side effects.
pub fn get_visit_stats(account_id: i32) -> Result<VisitStats, String> {
    let conn = open_db()?;
    let row = conn
        .query_row(
            "SELECT visit_streak, avg_visit_gap_days, total_visits_30d \
             FROM user_visit_tracker WHERE account_id = ?1",
            params![account_id],
            |r| {
                Ok(VisitStats {
                    visit_streak: r.get(0)?,
                    avg_gap_days: r.get(1)?,
                    total_visits_30d: r.get(2)?,
                })
            },
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => {
                "Visit tracker row not found for account — call update_visit_tracker first"
                    .to_string()
            }
            other => format!("get_visit_stats: {other}"),
        })?;
    Ok(row)
}

/// Return account IDs that are active enough to warrant a personalised
/// digest precompute. `min_streak` and `max_gap_days` define the threshold.
pub fn get_active_accounts_for_prefetch(
    min_streak: i32,
    max_gap_days: f64,
) -> Result<Vec<i32>, String> {
    let conn = open_db()?;
    let mut stmt = conn
        .prepare(
            "SELECT account_id FROM user_visit_tracker \
             WHERE visit_streak >= ?1 AND avg_visit_gap_days <= ?2",
        )
        .map_err(|e| format!("get_active_accounts query: {e}"))?;
    let rows = stmt
        .query_map(params![min_streak, max_gap_days], |r| r.get::<_, i32>(0))
        .map_err(|e| format!("get_active_accounts query_map: {e}"))?;
    let mut ids = Vec::new();
    for row in rows {
        ids.push(row.map_err(|e| format!("get_active_accounts row: {e}"))?);
    }
    Ok(ids)
}

/// Update `last_digest_date` after a successful personalised digest build.
pub fn mark_digest_built(account_id: i32) -> Result<(), String> {
    let today = Utc::now().date_naive().to_string();
    super::with_write_tx(|tx| {
        tx.execute(
            "UPDATE user_visit_tracker SET last_digest_date = ?1 WHERE account_id = ?2",
            params![today, account_id],
        )
        .map_err(|e| format!("mark_digest_built: {e}"))?;
        Ok(())
    })
}

fn default_blacklist_text() -> String {
    cfg().default_account_blacklist.join("\n")
}

pub fn set_account(
    owner_token: &str,
    account_id: i32,
    name: &str,
    blacklisted_tags: &str,
) -> Result<TruncatedAccount, String> {
    let owned: String;
    let resolved: &str = if blacklisted_tags.is_empty() {
        owned = default_blacklist_text();
        &owned
    } else {
        blacklisted_tags
    };

    super::with_write_tx(|tx| {
        tx.execute(
            "
            INSERT INTO accounts (id, name, blacklisted_tags)
            VALUES (?1, ?2, ?3)
            ON CONFLICT(id) DO UPDATE SET
            name = excluded.name,
            blacklisted_tags = excluded.blacklisted_tags",
            params![account_id, name, resolved],
        )
        .map_err(|e| format!("Failed to upsert account: {e}"))?;

        tx.execute(
            "
            INSERT INTO account_device_links (owner_token, account_id, linked_at, last_seen_at)
            VALUES (?1, ?2, ?3, ?3)
            ON CONFLICT(owner_token, account_id) DO UPDATE SET
                last_seen_at = excluded.last_seen_at
            ",
            params![owner_token, account_id, Utc::now().to_rfc3339()],
        )
        .map_err(|e| format!("Failed to link device token to account: {e}"))?;
        Ok(())
    })?;

    get_account_by_id(owner_token, account_id)
}

pub fn update_device_blacklist(
    owner_token: &str,
    account_id: i32,
    blacklisted_tags: &str,
) -> Result<TruncatedAccount, String> {
    let owned: String;
    let resolved: &str = if blacklisted_tags.is_empty() {
        owned = default_blacklist_text();
        &owned
    } else {
        blacklisted_tags
    };

    let affected = super::with_write_tx(|tx| {
        tx.execute(
            "
            UPDATE account_device_links
            SET blacklisted_tags = ?3, last_seen_at = ?4
            WHERE owner_token = ?1 AND account_id = ?2
            ",
            params![owner_token, account_id, resolved, Utc::now().to_rfc3339(),],
        )
        .map_err(|e| format!("Failed to update device blacklist: {e}"))
    })?;

    if affected == 0 {
        return Err("No account found for this device token".to_string());
    }

    get_account_by_id(owner_token, account_id)
}

pub fn get_accounts_for_owner(owner_token: &str) -> Result<Vec<TruncatedAccount>, String> {
    let conn = open_db()?;

    // Pure read — bookkeeping `UPDATE last_seen_at` was removed because it
    // ran on a pool connection and (after the WRITE_CONN refactor)
    // contended with the writer mutex via SQLite's busy_timeout, holding
    // pool slots and blocking unrelated reads under load.
    let mut stmt = conn
        .prepare(
            r#"
        SELECT a.id, a.name, COALESCE(NULLIF(adl.blacklisted_tags, ''), a.blacklisted_tags, '')
        FROM accounts a
        INNER JOIN account_device_links adl ON adl.account_id = a.id
        WHERE adl.owner_token = ?
        ORDER BY adl.last_seen_at DESC, a.name ASC
        "#,
        )
        .map_err(|e| format!("Failed to construct query: {e}"))?;

    stmt.query_map([owner_token], |row| {
        Ok(TruncatedAccount {
            id: row.get(0)?,
            name: row.get(1)?,
            blacklist: row.get(2)?,
        })
    })
    .map_err(|e| format!("Failed to get accounts: {e}"))?
    .collect::<Result<Vec<_>, _>>()
    .map_err(|e| format!("Failed to enumerate accounts: {e}"))
}

pub fn get_account_by_name(owner_token: &str, name: String) -> Result<TruncatedAccount, String> {
    let conn = open_db()?;

    let mut stmt = conn
        .prepare(
            r#"
        SELECT a.id, a.name, COALESCE(NULLIF(adl.blacklisted_tags, ''), a.blacklisted_tags, '')
        FROM accounts a
        INNER JOIN account_device_links adl ON adl.account_id = a.id
        WHERE a.name = ?1 AND adl.owner_token = ?2
        "#,
        )
        .map_err(|e| format!("Failed to construct query: {e}"))?;

    let accounts = stmt
        .query_map(params![name, owner_token], |row| {
            Ok(TruncatedAccount {
                id: row.get(0)?,
                name: row.get(1)?,
                blacklist: row.get(2)?,
            })
        })
        .map_err(|e| format!("Failed to get accounts: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to enumerate accounts: {e}"))?;

    if let Some(account) = accounts.first() {
        Ok(account.clone())
    } else {
        Err("No account found".to_string())
    }
}

pub fn get_account_by_id(owner_token: &str, id: i32) -> Result<TruncatedAccount, String> {
    let conn = open_db()?;

    let mut stmt = conn
        .prepare(
            r#"
        SELECT a.id, a.name, COALESCE(NULLIF(adl.blacklisted_tags, ''), a.blacklisted_tags, '')
        FROM accounts a
        INNER JOIN account_device_links adl ON adl.account_id = a.id
        WHERE a.id = ?1 AND adl.owner_token = ?2
        "#,
        )
        .map_err(|e| format!("Failed to construct query: {e}"))?;

    let accounts = stmt
        .query_map(params![id, owner_token], |row| {
            Ok(TruncatedAccount {
                id: row.get(0)?,
                name: row.get(1)?,
                blacklist: row.get(2)?,
            })
        })
        .map_err(|e| format!("Failed to get accounts: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to enumerate accounts: {e}"))?;

    if let Some(account) = accounts.first() {
        Ok(account.clone())
    } else {
        Err("No account found".to_string())
    }
}

/// Sever the device → account link, leaving the `accounts` row alone
/// since other devices may still own it. Returns the number of links
/// removed; 0 means this device never owned the account (callers should
/// map to 404 so the click isn't silently a no-op).
pub fn delete_device_link(owner_token: &str, account_id: i32) -> Result<usize, String> {
    // Step 1: drop the link itself, and decide whether the account row
    // should be torn down. Kept in its own short tx so concurrent device
    // operations can't observe a half-broken account.
    let (removed, cascade) = super::with_write_tx(|tx| {
        let n = tx
            .execute(
                "DELETE FROM account_device_links \
                 WHERE owner_token = ?1 AND account_id = ?2",
                params![owner_token, account_id],
            )
            .map_err(|e| format!("Failed to delete device link: {e}"))?;
        let cascade = if n > 0 {
            let remaining: i64 = tx
                .query_row(
                    "SELECT COUNT(*) FROM account_device_links WHERE account_id = ?1",
                    params![account_id],
                    |r| r.get(0),
                )
                .map_err(|e| format!("Failed to count remaining device links: {e}"))?;
            remaining == 0
        } else {
            false
        };
        Ok((n, cascade))
    })?;

    if !cascade {
        return Ok(removed);
    }

    // Step 2: the two unbounded per-account tables can reach millions of
    // rows on heavy users. Running them inside the cascade tx pinned the
    // writer mutex for minutes during account teardown. Wipe in batched
    // chunks so the writer can yield to other operations between
    // chunks. Non-atomic with the rest of the cascade: if we crash here
    // the account row survives with empty cooc/feed history, which is
    // self-healing on the next /process run.
    let batch_size = crate::models::cfg().runtime.drop_cooc_batch_size.max(1_000);
    let cooc_dropped = super::drop_account_cooccurrence_batched(account_id, batch_size, |_, _| {})?;
    let feed_dropped =
        super::drop_account_feed_interactions_batched(account_id, batch_size, |_, _| {})?;
    if cooc_dropped > 0 || feed_dropped > 0 {
        info!(
            "delete_device_link {account_id}: pre-cascade dropped cooc={cooc_dropped} feed_int={feed_dropped}"
        );
    }

    // Step 3: the remaining cascade tables are bounded (a few hundred
    // rows per account at most), so finish atomically in one short tx.
    super::with_write_tx(|tx| {
        for stmt in [
            "DELETE FROM accounts_post WHERE account_id = ?1",
            "DELETE FROM account_tag_counts WHERE account_id = ?1",
            "DELETE FROM account_rating_profile WHERE account_id = ?1",
            "DELETE FROM account_media_profile WHERE account_id = ?1",
            "DELETE FROM account_quality_profile WHERE account_id = ?1",
            "DELETE FROM account_tag_feedback WHERE account_id = ?1",
            "DELETE FROM accounts WHERE id = ?1",
        ] {
            let _ = tx.execute(stmt, params![account_id]).map_err(|e| {
                warn!("delete_device_link cascade '{stmt}': {e}");
                e.to_string()
            });
        }
        Ok(())
    })?;

    Ok(removed)
}

pub fn cleanup_orphan_accounts() -> Result<i64, String> {
    let deleted = super::with_write_tx(|tx| {
        tx.execute(
            "DELETE FROM accounts
             WHERE id NOT IN (SELECT account_id FROM account_device_links)
               AND id NOT IN (SELECT DISTINCT account_id FROM accounts_post)",
            [],
        )
        .map_err(|e| format!("delete orphan accounts: {e}"))
    })?;
    Ok(deleted as i64)
}

pub fn get_account_experiment_bucket(account_id: i32) -> Result<Option<String>, String> {
    let conn = open_db()?;
    conn.query_row(
        "SELECT experiment_bucket FROM accounts WHERE id = ?1",
        params![account_id],
        |r| r.get::<_, Option<String>>(0),
    )
    .or_else(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => Ok(None),
        other => Err(format!("get_account_experiment_bucket: {other}")),
    })
}

/// Replace the full preferred-tags list for an account. Verifies device
/// ownership, deletes existing rows, then bulk-inserts the new list.
/// Max 50 tags — caller should validate before calling.
pub fn set_preferred_tags(
    owner_token: &str,
    account_id: i32,
    preferred_tags: &[PreferredTag],
) -> Result<(), String> {
    super::with_write_tx(|tx| {
        let linked: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM account_device_links WHERE owner_token = ?1 AND account_id = ?2)",
                params![owner_token, account_id],
                |row| row.get(0),
            )
            .map_err(|e| format!("Failed to verify account ownership: {e}"))?;

        if !linked {
            return Err("Account is not linked to this device token".to_string());
        }

        tx.execute(
            "DELETE FROM account_preferred_tags WHERE account_id = ?1",
            params![account_id],
        )
        .map_err(|e| format!("Failed to clear preferred tags: {e}"))?;

        if !preferred_tags.is_empty() {
            let mut stmt = tx
                .prepare_cached(
                    "INSERT INTO account_preferred_tags (account_id, tag_name, group_type, weight) VALUES (?1, ?2, ?3, ?4)",
                )
                .map_err(|e| format!("Failed to prepare preferred tag insert: {e}"))?;

            for pt in preferred_tags {
                stmt.execute(params![account_id, pt.tag, pt.group, pt.weight])
                    .map_err(|e| format!("Failed to insert preferred tag '{}': {}", pt.tag, e))?;
            }
        }

        Ok(())
    })
}
