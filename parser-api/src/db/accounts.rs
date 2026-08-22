use chrono::{NaiveDate, Utc};
use rusqlite::{OptionalExtension as _, params};
use sha2::{Digest, Sha256};

use crate::models::{
    AccountFeedSettings, DeviceAccountLink, DeviceSession, PreferredTag, TruncatedAccount, cfg,
};

use super::open_db;

/// A device (owner token) is reported as "active" if it was last seen within
/// this many days. Linked `last_seen_at` is updated on state changes (link,
/// blacklist write); reads deliberately don't touch it to avoid writer
/// contention, so this is a conservative recency signal.
const DEVICE_ACTIVE_WINDOW_DAYS: i64 = 30;

/// Enumerate every device (owner token) that shares any account with the
/// requesting `owner_token`, plus the accounts each device is linked to.
///
/// The returned [`DeviceSession::id`] is a stable `sha256` hex of the raw
/// owner token — the token itself is never returned, so the payload carries
/// no secrets usable to impersonate another device.
///
/// The current device is flagged `is_current`; its own token is, of course,
/// included in the result.
pub fn list_device_sessions(owner_token: &str) -> Result<Vec<DeviceSession>, String> {
    let conn = open_db()?;
    let mut stmt = conn
        .prepare(
            r"
            SELECT adl.owner_token, adl.account_id, a.name, adl.linked_at, adl.last_seen_at
            FROM account_device_links adl
            JOIN accounts a ON a.id = adl.account_id
            WHERE adl.account_id IN (
                SELECT account_id FROM account_device_links WHERE owner_token = ?1
            )
            ORDER BY adl.owner_token ASC, adl.last_seen_at DESC
            ",
        )
        .map_err(|e| format!("Failed to construct device-session query: {e}"))?;

    let rows = stmt
        .query_map([owner_token], |row| {
            Ok((
                row.get::<_, String>(0)?, // owner_token
                row.get::<_, i32>(1)?,    // account_id
                row.get::<_, String>(2)?, // account name
                row.get::<_, String>(3)?, // linked_at
                row.get::<_, String>(4)?, // last_seen_at
            ))
        })
        .map_err(|e| format!("Failed to query device sessions: {e}"))?;

    let mut devices: Vec<DeviceSession> = Vec::new();
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    for row in rows {
        let (token, account_id, name, linked_at, last_seen_at) =
            row.map_err(|e| format!("Failed to read device-session row: {e}"))?;
        let pos = match index.get(&token) {
            Some(&p) => p,
            None => {
                let mut hasher = Sha256::new();
                hasher.update(token.as_bytes());
                let id = hex(&hasher.finalize());
                let active = days_since_rfc3339(&last_seen_at) <= DEVICE_ACTIVE_WINDOW_DAYS;
                devices.push(DeviceSession {
                    id,
                    is_current: token == owner_token,
                    first_seen_at: linked_at.clone(),
                    last_seen_at: last_seen_at.clone(),
                    active,
                    accounts: Vec::new(),
                });
                index.insert(token.clone(), devices.len() - 1);
                devices.len() - 1
            }
        };
        let device = &mut devices[pos];
        // RFC 3339 timestamps sort lexicographically: pick min/max by string.
        if linked_at < device.first_seen_at {
            device.first_seen_at = linked_at.clone();
        }
        if last_seen_at > device.last_seen_at {
            device.last_seen_at = last_seen_at.clone();
        }
        device.accounts.push(DeviceAccountLink {
            account_id,
            name,
            linked_at,
            last_seen_at,
        });
    }
    Ok(devices)
}

/// Lowercase-hex encoding of a checksum digest.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Days since an RFC 3339 timestamp; `i64::MAX` when it can't be parsed
/// (treated as never-seen, i.e. inactive).
fn days_since_rfc3339(rfc: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(rfc)
        .map(|dt| (Utc::now() - dt.with_timezone(&Utc)).num_days())
        .unwrap_or(i64::MAX)
}

/// Resolve a device's raw owner token from its public `sha256` id, among the
/// devices sharing any account with `owner_token`. Excludes the current device
/// itself (revoking your own token is `DELETE /api/session`, not this route).
///
/// Never returns another account's unrelated token: only tokens that share an
/// account with the caller are considered (same reachability as
/// [`list_device_sessions`]).
pub fn find_device_token_by_id(
    owner_token: &str,
    device_id: &str,
) -> Result<Option<String>, String> {
    let conn = open_db()?;
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT owner_token FROM account_device_links \
             WHERE account_id IN (\
                SELECT account_id FROM account_device_links WHERE owner_token = ?1\
             )",
        )
        .map_err(|e| format!("Failed to prepare device-token query: {e}"))?;
    let tokens: Vec<String> = stmt
        .query_map([owner_token], |r| r.get::<_, String>(0))
        .map_err(|e| format!("Failed to query device tokens: {e}"))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("Failed to collect device tokens: {e}"))?;
    for token in tokens {
        if token == owner_token {
            continue;
        }
        let mut hasher = Sha256::new();
        hasher.update(token.as_bytes());
        if hex(&hasher.finalize()) == device_id {
            return Ok(Some(token));
        }
    }
    Ok(None)
}

/// Sever every account link owned by `token`, running the same per-account
/// teardown cascade as `delete_device_link` (cooc / feed-interactions wipes)
/// for the last link on each account. Returns the number of links removed.
pub fn delete_all_device_links_for_token(token: &str) -> Result<usize, String> {
    let ids: Vec<i32> = {
        let conn = open_db()?;
        let mut stmt = conn
            .prepare("SELECT account_id FROM account_device_links WHERE owner_token = ?1")
            .map_err(|e| format!("Failed to prepare link-id query: {e}"))?;
        stmt.query_map([token], |r| r.get::<_, i32>(0))
            .map_err(|e| format!("Failed to query link ids: {e}"))?
            .collect::<Result<_, _>>()
            .map_err(|e| format!("Failed to collect link ids: {e}"))?
    };
    let mut removed = 0usize;
    for id in ids {
        removed += delete_device_link(token, id)?;
    }
    Ok(removed)
}

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
        // `accounts.blacklisted_tags` is the SHARED fallback used by every
        // device link of that account id. Keep it on insert only — on a
        // re-create (any device linking an already-known public account) we
        // must not let one visitor overwrite the fallback the other devices
        // rely on. Per-device blacklists live in `account_device_links` and
        // are set separately via `update_device_blacklist`.
        tx.execute(
            "
            INSERT INTO accounts (id, name, blacklisted_tags)
            VALUES (?1, ?2, ?3)
            ON CONFLICT(id) DO UPDATE SET
            name = excluded.name
            /* keep existing blacklisted_tags — shared fallback */",
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
) -> Result<(TruncatedAccount, bool), String> {
    let owned: String;
    let resolved: &str = if blacklisted_tags.is_empty() {
        owned = default_blacklist_text();
        &owned
    } else {
        blacklisted_tags
    };

    // Returns `true` only when the effective blacklist actually changed, so
    // callers can skip the process-wide e621 cache flush on a no-op write
    // (replaying an unchanged blacklist must not keep the shared cache cold).
    let changed = super::with_write_tx(|tx| {
        let existing: Option<String> = tx
            .query_row(
                "SELECT blacklisted_tags FROM account_device_links \
                 WHERE owner_token = ?1 AND account_id = ?2",
                params![owner_token, account_id],
                |r| r.get(0),
            )
            .ok();
        match existing {
            None => Err("No account found for this device token".to_string()),
            Some(cur) if cur == resolved => Ok(false),
            Some(_) => {
                tx.execute(
                    "
            UPDATE account_device_links
            SET blacklisted_tags = ?3, last_seen_at = ?4
            WHERE owner_token = ?1 AND account_id = ?2
            ",
                    params![owner_token, account_id, resolved, Utc::now().to_rfc3339(),],
                )
                .map_err(|e| format!("Failed to update device blacklist: {e}"))?;
                Ok(true)
            }
        }
    })?;

    let account = get_account_by_id(owner_token, account_id)?;
    Ok((account, changed))
}

pub fn get_accounts_for_owner(owner_token: &str) -> Result<Vec<TruncatedAccount>, String> {
    let conn = open_db()?;

    // Pure read — bookkeeping `UPDATE last_seen_at` was removed because it
    // ran on a pool connection and (after the WRITE_CONN refactor)
    // contended with the writer mutex via SQLite's busy_timeout, holding
    // pool slots and blocking unrelated reads under load.
    let mut stmt = conn
        .prepare(
            r"
        SELECT a.id, a.name, COALESCE(NULLIF(adl.blacklisted_tags, ''), a.blacklisted_tags, '')
        FROM accounts a
        INNER JOIN account_device_links adl ON adl.account_id = a.id
        WHERE adl.owner_token = ?
        ORDER BY adl.last_seen_at DESC, a.name ASC
        ",
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
            r"
        SELECT a.id, a.name, COALESCE(NULLIF(adl.blacklisted_tags, ''), a.blacklisted_tags, '')
        FROM accounts a
        INNER JOIN account_device_links adl ON adl.account_id = a.id
        WHERE a.name = ?1 AND adl.owner_token = ?2
        ",
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
            r"
        SELECT a.id, a.name, COALESCE(NULLIF(adl.blacklisted_tags, ''), a.blacklisted_tags, '')
        FROM accounts a
        INNER JOIN account_device_links adl ON adl.account_id = a.id
        WHERE a.id = ?1 AND adl.owner_token = ?2
        ",
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

/// Whether `owner_token` is currently linked to `account_id`. Returns a DB
/// error on failure; `Ok(false)` means the link does not exist (callers map
/// that to 403/404 rather than a 500).
pub fn account_is_linked(owner_token: &str, account_id: i32) -> Result<bool, String> {
    let conn = open_db()?;
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM account_device_links \
         WHERE owner_token = ?1 AND account_id = ?2)",
        params![owner_token, account_id],
        |r| r.get(0),
    )
    .map_err(|e| format!("Failed to check account ownership: {e}"))
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

/// Read consolidated feed settings for an account. Verifies device
/// ownership, then reads blacklist (device-specific with global fallback),
/// preferred tags, and experiment bucket in a single read transaction.
pub fn get_account_feed_settings(
    owner_token: &str,
    account_id: i32,
) -> Result<AccountFeedSettings, String> {
    let mut conn = open_db()?;

    // Single read transaction so the three reads see one consistent snapshot.
    let tx = conn
        .transaction()
        .map_err(|e| format!("Failed to begin read transaction: {e}"))?;

    // Verify device ownership first.
    tx.query_row(
        "SELECT 1 FROM account_device_links WHERE owner_token = ?1 AND account_id = ?2",
        params![owner_token, account_id],
        |_| Ok(()),
    )
    .map_err(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => {
            "No account found for this device token".to_string()
        }
        other => format!("Failed to verify account access: {other}"),
    })?;

    // 1) Blacklist: device-specific with global fallback.
    let blacklist: String = tx
        .query_row(
            r"
            SELECT COALESCE(NULLIF(adl.blacklisted_tags, ''), a.blacklisted_tags, '')
            FROM accounts a
            INNER JOIN account_device_links adl ON adl.account_id = a.id
            WHERE a.id = ?1 AND adl.owner_token = ?2
            ",
            params![account_id, owner_token],
            |r| r.get(0),
        )
        .map_err(|e| format!("Failed to read blacklist: {e}"))?;
    let blacklist = if blacklist.is_empty() {
        None
    } else {
        Some(blacklist)
    };

    // 2) Preferred tags.
    let mut stmt = tx
        .prepare(
            "SELECT tag_name, group_type, weight
             FROM account_preferred_tags
             WHERE account_id = ?1
             ORDER BY rowid",
        )
        .map_err(|e| format!("Failed to prepare preferred_tags query: {e}"))?;
    let preferred_tags: Vec<PreferredTag> = stmt
        .query_map(params![account_id], |r| {
            Ok(PreferredTag {
                tag: r.get(0)?,
                group: r.get(1)?,
                weight: r.get(2)?,
            })
        })
        .map_err(|e| format!("Failed to query preferred_tags: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect preferred_tags: {e}"))?;
    drop(stmt);

    // 3) Experiment bucket.
    let experiment_bucket: Option<String> = tx
        .query_row(
            "SELECT experiment_bucket FROM accounts WHERE id = ?1",
            params![account_id],
            |r| r.get(0),
        )
        .ok()
        .flatten();

    let settings = AccountFeedSettings {
        blacklist,
        preferred_tags,
        experiment_bucket,
    };

    tx.commit()
        .map_err(|e| format!("Failed to commit read transaction: {e}"))?;
    Ok(settings)
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

/// Count accounts per A/B bucket. Buckets come from `cfg().buckets`; an
/// account falls into `pick_bucket(id, None)`. Used to seed the
/// `e621_experiment_bucket_accounts` gauge at startup.
/// Get all preferred tags for an account for backfill purposes.
/// Returns (`tag_name`, `group_type`, weight).
pub fn get_all_preferred_tags(account_id: i32) -> Result<Vec<(String, String, f64)>, String> {
    let conn = open_db()?;
    let mut stmt = conn
        .prepare(
            "SELECT tag_name, group_type, weight
             FROM account_preferred_tags
             WHERE account_id = ?1
             ORDER BY rowid",
        )
        .map_err(|e| format!("get_all_preferred_tags prepare: {e}"))?;
    let rows = stmt
        .query_map(params![account_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, f64>(2)?,
            ))
        })
        .map_err(|e| format!("get_all_preferred_tags query: {e}"))?;
    let mut tags = Vec::new();
    for row in rows {
        tags.push(row.map_err(|e| format!("get_all_preferred_tags row: {e}"))?);
    }
    Ok(tags)
}

/// Fetch accounts eligible for backfill: those whose `last_backfilled_at`
/// is older than `cooldown_secs` seconds (or never backfilled).
pub fn get_backfill_candidates(
    cooldown_secs: u64,
    max_accounts: usize,
) -> Result<Vec<(i32, String)>, String> {
    let conn = open_db()?;
    let threshold = Utc::now().timestamp() - cooldown_secs as i64;
    let mut stmt = conn
        .prepare(
            "SELECT a.id, COALESCE(NULLIF(a.blacklisted_tags, ''), '')
             FROM accounts a
             WHERE a.last_backfilled_at IS NULL
                OR a.last_backfilled_at < ?1
             ORDER BY a.last_backfilled_at ASC NULLS FIRST
             LIMIT ?2",
        )
        .map_err(|e| format!("get_backfill_candidates prepare: {e}"))?;
    let rows = stmt
        .query_map(params![threshold, max_accounts as i64], |r| {
            Ok((r.get::<_, i32>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(|e| format!("get_backfill_candidates query: {e}"))?;
    let mut accounts = Vec::new();
    for row in rows {
        accounts.push(row.map_err(|e| format!("get_backfill_candidates row: {e}"))?);
    }
    Ok(accounts)
}

/// Update `last_backfilled_at` for an account after a successful backfill.
pub fn mark_account_backfilled(account_id: i32) -> Result<(), String> {
    let now = Utc::now().timestamp();
    super::with_write_tx(|tx| {
        tx.execute(
            "UPDATE accounts SET last_backfilled_at = ?1 WHERE id = ?2",
            params![now, account_id],
        )
        .map_err(|e| format!("mark_account_backfilled: {e}"))?;
        Ok(())
    })
}

pub fn count_accounts_by_bucket() -> Result<std::collections::HashMap<String, u64>, String> {
    use std::collections::HashMap;
    let conn = open_db()?;
    let mut stmt = conn
        .prepare("SELECT id FROM accounts")
        .map_err(|e| format!("Failed to prepare account count query: {e}"))?;
    let ids = stmt
        .query_map([], |row| row.get::<_, i64>(0))
        .map_err(|e| format!("Failed to query account ids: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect account ids: {e}"))?;
    let mut counts: HashMap<String, u64> = HashMap::new();
    for id in ids {
        let (bucket, _) = crate::models::cfg().pick_bucket(id as i32, None);
        let key = bucket.unwrap_or_else(|| "none".to_string());
        *counts.entry(key).or_insert(0) += 1;
    }
    Ok(counts)
}

// ---------------------------------------------------------------------------
// Per-account e621 API key storage (encrypted at rest).
//
// ACCOUNT-scoped: an e621 account has ONE canonical API key (the owner's),
// stored on the `accounts` row so it is available to every linked device for
// direct sync. Ownership is enforced at ACCESS time via `account_device_links`
// (`require_device_link`) — a token must be linked to the account to view or
// manage the key — but the key is a single shared account resource, so sync
// works from any linked device. (The admin_user account syncs with the shared
// admin_api and needs no stored key.) The raw key is AES-256-GCM encrypted
// (`crypto`), never returned over the API, and never exported (the export is
// built from explicit fields and omits these columns).
// ---------------------------------------------------------------------------

/// Verify that `owner_token` is linked to `account_id`; errors otherwise.
/// Shared by every key accessor so the ownership rule lives in one place.
fn require_device_link(
    conn: &rusqlite::Connection,
    owner_token: &str,
    account_id: i32,
) -> Result<(), String> {
    let linked: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM account_device_links \
             WHERE owner_token = ?1 AND account_id = ?2)",
            params![owner_token, account_id],
            |r| r.get(0),
        )
        .map_err(|e| format!("Failed to verify account ownership: {e}"))?;
    if !linked {
        return Err("Account is not linked to this device token".to_string());
    }
    Ok(())
}

/// Store (or rotate) the plaintext e621 API key for `account_id` (account-wide,
/// shared by every linked device), encrypting it at rest. Access-gated: only a
/// linked device may set it. Sets `added_at`; on rotate the previous
/// `verified_at`/`last_synced_at` are kept (callers refresh them via
/// [`mark_e621_key_verified`] / [`mark_account_direct_synced`]).
pub fn set_account_e621_key(owner_token: &str, account_id: i32, key: &str) -> Result<(), String> {
    let encrypted = crate::crypto::encrypt(key.as_bytes()).map_err(|e| {
        warn!("set_account_e621_key: failed to encrypt key for {account_id}: {e}");
        "Failed to encrypt e621 key".to_string()
    })?;
    super::with_write_tx(|tx| {
        require_device_link(tx, owner_token, account_id)?;
        tx.execute(
            "UPDATE accounts SET e621_api_key_encrypted = ?1, e621_api_key_added_at = ?2 \
             WHERE id = ?3",
            params![encrypted, Utc::now().to_rfc3339(), account_id],
        )
        .map_err(|e| format!("Failed to store e621 key: {e}"))?;
        Ok(())
    })
}

/// Read the account's plaintext e621 API key (shared — available to any linked
/// device for sync/key-test). Access-gated: only a linked device may read it.
/// Returns `Ok(None)` when the account has no key set. Never returned over the
/// API except by deliberately-scoped internal callers (key/test, sync).
pub fn get_account_e621_key(owner_token: &str, account_id: i32) -> Result<Option<String>, String> {
    let conn = open_db()?;
    require_device_link(&conn, owner_token, account_id)?;
    let encrypted: Option<String> = conn
        .query_row(
            "SELECT e621_api_key_encrypted FROM accounts WHERE id = ?1",
            params![account_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .map_err(|e| format!("Failed to read e621 key: {e}"))?;
    match encrypted {
        None => Ok(None),
        Some(blob) => {
            let plain = crate::crypto::decrypt(&blob).map_err(|e| {
                warn!("get_account_e621_key: decrypt failed for {account_id}: {e}");
                "Failed to decrypt e621 key".to_string()
            })?;
            Ok(Some(String::from_utf8(plain).map_err(|e| {
                format!("e621 key is not valid UTF-8: {e}")
            })?))
        }
    }
}

/// Access-gated existence check — does NOT decrypt the key. Used by the
/// key/state endpoint and sync/status without exposing any key material.
pub fn has_account_e621_key(owner_token: &str, account_id: i32) -> Result<bool, String> {
    let conn = open_db()?;
    require_device_link(&conn, owner_token, account_id)?;
    let has: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM accounts \
             WHERE id = ?1 AND e621_api_key_encrypted IS NOT NULL)",
            params![account_id],
            |r| r.get(0),
        )
        .map_err(|e| format!("Failed to check e621 key: {e}"))?;
    Ok(has)
}

/// Non-decrypting metadata about an account's e621 key. Access-gated; returns
/// `has_key`, `added_at`, `verified_at` only — never key material. Drives
/// `GET /account/<id>/key/state`.
#[derive(Debug, Clone)]
pub struct AccountKeyMeta {
    pub has_key: bool,
    pub added_at: Option<String>,
    pub verified_at: Option<String>,
}

pub fn get_account_key_meta(owner_token: &str, account_id: i32) -> Result<AccountKeyMeta, String> {
    let conn = open_db()?;
    require_device_link(&conn, owner_token, account_id)?;
    let row: Option<(bool, Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT e621_api_key_encrypted IS NOT NULL, e621_api_key_added_at, \
             e621_api_key_verified_at FROM accounts WHERE id = ?1",
            params![account_id],
            |r| {
                Ok((
                    r.get::<_, bool>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|e| format!("Failed to read e621 key state: {e}"))?;
    let (has_key, added_at, verified_at) = match row {
        Some((true, a, v)) => (true, a, v),
        // A leftover `added_at` without an encrypted blob would report a key;
        // coerce to no-key so state stays truthful.
        _ => (false, None, None),
    };
    Ok(AccountKeyMeta {
        has_key,
        added_at,
        verified_at,
    })
}

/// Record the last successful verification of the account's key against e621
/// (used by `key/test` and each sync pass). Access-gated.
pub fn mark_e621_key_verified(owner_token: &str, account_id: i32) -> Result<(), String> {
    super::with_write_tx(|tx| {
        require_device_link(tx, owner_token, account_id)?;
        tx.execute(
            "UPDATE accounts SET e621_api_key_verified_at = ?1 WHERE id = ?2",
            params![Utc::now().to_rfc3339(), account_id],
        )
        .map_err(|e| format!("Failed to mark e621 key verified: {e}"))?;
        Ok(())
    })
}

/// Remove the account's e621 API key (revoke). Access-gated — must be a linked
/// device. Clear is account-wide (a single shared key per account).
pub fn clear_account_e621_key(owner_token: &str, account_id: i32) -> Result<(), String> {
    super::with_write_tx(|tx| {
        require_device_link(tx, owner_token, account_id)?;
        tx.execute(
            "UPDATE accounts SET e621_api_key_encrypted = NULL, e621_api_key_added_at = NULL, \
             e621_api_key_verified_at = NULL WHERE id = ?1",
            params![account_id],
        )
        .map_err(|e| format!("Failed to clear e621 key: {e}"))?;
        Ok(())
    })
}

/// Record a successful direct (user-key) sync for the account. Access-gated.
/// Sets `last_direct_synced_at` (account-wide); used by sync/status.
pub fn mark_account_direct_synced(owner_token: &str, account_id: i32) -> Result<(), String> {
    super::with_write_tx(|tx| {
        require_device_link(tx, owner_token, account_id)?;
        tx.execute(
            "UPDATE accounts SET last_direct_synced_at = ?1 WHERE id = ?2",
            params![Utc::now().to_rfc3339(), account_id],
        )
        .map_err(|e| format!("Failed to mark account direct-synced: {e}"))?;
        Ok(())
    })
}

/// Direct-sync status for the account: whether a key is configured and when the
/// last sync ran. Access-gated; no key material.
pub fn get_direct_sync_state(
    owner_token: &str,
    account_id: i32,
) -> Result<DirectSyncState, String> {
    let conn = open_db()?;
    require_device_link(&conn, owner_token, account_id)?;
    let row: Option<(Option<String>, bool)> = conn
        .query_row(
            "SELECT last_direct_synced_at, e621_api_key_encrypted IS NOT NULL \
             FROM accounts WHERE id = ?1",
            params![account_id],
            |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, bool>(1)?)),
        )
        .optional()
        .map_err(|e| format!("Failed to read direct-sync state: {e}"))?;
    let (last_synced_at, has_key) = row.unwrap_or((None, false));
    Ok(DirectSyncState {
        last_synced_at,
        has_key,
    })
}

#[derive(Debug, Clone)]
pub struct DirectSyncState {
    pub last_synced_at: Option<String>,
    pub has_key: bool,
}
