use chrono::{NaiveDate, Utc};
use rusqlite::{OptionalExtension as _, params};
use sha2::{Digest, Sha256};

use crate::errors::NOT_FOUND_ERR_MARKER;
use crate::models::{
    AccountFeedSettings, AdminTotals, DeviceAccountLink, DeviceSession, PreferredTag,
    TruncatedAccount, cfg,
};

use super::instance::{IS_OWNER_SQL, MAY_ADDRESS_SQL, VISIBLE_ACCOUNTS_SQL, console_accounts_sql};
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
        // Where the value lives depends on whether this browser holds a link:
        // `account_device_links.blacklisted_tags` is the per-device override and
        // `accounts.blacklisted_tags` the account-wide value, and the read path
        // resolves exactly this COALESCE. Writing to the other row would look
        // like a save that did nothing. The instance owner addresses accounts
        // they hold no link to, and for those the account-wide column is what is
        // read back.
        let found: Option<(bool, String)> = tx
            .query_row(
                &format!(
                    "SELECT adl.owner_token IS NOT NULL,
                            COALESCE(NULLIF(adl.blacklisted_tags, ''), a.blacklisted_tags, '')
                       FROM accounts a
                       LEFT JOIN account_device_links adl
                              ON adl.account_id = a.id AND adl.owner_token = ?1
                      WHERE a.id = ?2 AND (adl.owner_token IS NOT NULL OR {IS_OWNER_SQL})"
                ),
                params![owner_token, account_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|e| format!("Failed to read the current blacklist: {e}"))?;

        let Some((linked, current)) = found else {
            return Err("No account found for this device token".to_string());
        };
        if current == resolved {
            return Ok(false);
        }
        if linked {
            tx.execute(
                "UPDATE account_device_links
                    SET blacklisted_tags = ?3, last_seen_at = ?4
                  WHERE owner_token = ?1 AND account_id = ?2",
                params![owner_token, account_id, resolved, Utc::now().to_rfc3339()],
            )
            .map_err(|e| format!("Failed to update device blacklist: {e}"))?;
        } else {
            tx.execute(
                "UPDATE accounts SET blacklisted_tags = ?1 WHERE id = ?2",
                params![resolved, account_id],
            )
            .map_err(|e| format!("Failed to update account blacklist: {e}"))?;
        }
        Ok(true)
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
        .prepare(&format!(
            r"
        SELECT a.id, a.name, COALESCE(NULLIF(adl.blacklisted_tags, ''), a.blacklisted_tags, '')
        FROM accounts a
        LEFT JOIN account_device_links adl
               ON adl.account_id = a.id AND adl.owner_token = ?1
        WHERE adl.owner_token IS NOT NULL OR {IS_OWNER_SQL}
        ORDER BY adl.last_seen_at DESC, a.name ASC
        "
        ))
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
    get_account_by_id_conn(&conn, owner_token, id)
}

/// Body of [`get_account_by_id`], split out so the scope rule can be pinned
/// against an in-memory schema. This is the single access check behind the
/// catalog, catalog_manage, browse, feed, digest and taste_profile routes, which
/// is why the rule lives here and not in each of them.
///
/// `LEFT JOIN` on the caller's *own* link keeps the per-account blacklist
/// override when it exists and yields exactly one row either way — for the
/// instance owner `adl` is simply NULL on the accounts they hold no link to.
fn get_account_by_id_conn(
    conn: &rusqlite::Connection,
    owner_token: &str,
    id: i32,
) -> Result<TruncatedAccount, String> {
    let mut stmt = conn
        .prepare(&format!(
            r"
        SELECT a.id, a.name, COALESCE(NULLIF(adl.blacklisted_tags, ''), a.blacklisted_tags, '')
        FROM accounts a
        LEFT JOIN account_device_links adl
               ON adl.account_id = a.id AND adl.owner_token = ?1
        WHERE a.id = ?2 AND (adl.owner_token IS NOT NULL OR {IS_OWNER_SQL})
        "
        ))
        .map_err(|e| format!("Failed to construct query: {e}"))?;

    let accounts = stmt
        .query_map(params![owner_token, id], |row| {
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
        // Marked so every route that propagates this string (directly or inside
        // a `format!` wrapper) answers 404 instead of 500: a valid token asking
        // for an account it does not hold is an ordinary authorization outcome.
        Err(format!("{NOT_FOUND_ERR_MARKER}No account found"))
    }
}

/// May `owner_token` address `account_id`?
///
/// This is *scope*, not a link count: the instance owner addresses every
/// account on the instance while holding no link at all, and everyone else only
/// the accounts linked to their token. Returns a DB error on failure;
/// `Ok(false)` means the account is out of scope (callers map that to 403/404
/// rather than a 500).
pub fn account_in_scope(owner_token: &str, account_id: i32) -> Result<bool, String> {
    let conn = open_db()?;
    account_in_scope_conn(&conn, owner_token, account_id)
}

fn account_in_scope_conn(
    conn: &rusqlite::Connection,
    owner_token: &str,
    account_id: i32,
) -> Result<bool, String> {
    conn.query_row(
        &format!("SELECT {MAY_ADDRESS_SQL}"),
        params![owner_token, account_id],
        |r| r.get(0),
    )
    .map_err(|e| format!("Failed to check account ownership: {e}"))
}

/// Record that `owner_token` proved `[runtime].admin_api` for `account_id`.
///
/// The grant is deliberately a property of the *link*, not of the account: the
/// same account can be linked by several browsers, and only the one that
/// presented the key becomes an operator. `fingerprint` is what binds the grant
/// to the key that wrote it, so a later rotation of `admin_api` retires it
/// without anyone having to revoke anything by hand.
///
/// Returns an error when there is no link to grant, rather than silently doing
/// nothing: a caller that has just written the link and still sees no row has a
/// bug, and reporting success would hide it until the guard answered 403.
pub fn set_account_admin_grant(
    owner_token: &str,
    account_id: i32,
    fingerprint: &str,
) -> Result<(), String> {
    super::with_write_tx(|tx| {
        let n = tx
            .execute(
                "UPDATE account_device_links \
                 SET admin_granted_at = ?1, admin_key_fingerprint = ?2 \
                 WHERE owner_token = ?3 AND account_id = ?4",
                params![
                    Utc::now().to_rfc3339(),
                    fingerprint,
                    owner_token,
                    account_id
                ],
            )
            .map_err(|e| format!("Failed to record the administration grant: {e}"))?;
        if n == 0 {
            return Err(format!(
                "no device link for account {account_id} to grant administration on"
            ));
        }
        Ok(())
    })
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
    // writer mutex for minutes during account teardown, so both are wiped
    // outside it: `feed_interactions` in chunks, `account_tag_cooccurrence`
    // in a single indexed DELETE. Non-atomic with the rest of the cascade:
    // if we crash here the account row survives with empty cooc/feed
    // history, which is self-healing on the next /process run.
    let batch_size = crate::models::cfg().runtime.drop_cooc_batch_size.max(1_000);
    let cooc_dropped = super::drop_account_cooccurrence(account_id, |_| {})?;
    let feed_dropped =
        super::drop_account_feed_interactions_batched(account_id, batch_size, |_, _| {})?;
    if cooc_dropped > 0 || feed_dropped > 0 {
        info!(
            "delete_device_link {account_id}: pre-cascade dropped cooc={cooc_dropped} feed_int={feed_dropped}"
        );
    }

    // Step 3: the remaining cascade tables are bounded (a few hundred
    // rows per account at most), so finish atomically in one short tx.
    //
    // Every statement must succeed: this used to log and CONTINUE on failure,
    // then return `Ok(removed)` — so a failed child-table delete (or a failed
    // `DELETE FROM accounts`) was reported to the caller as a successful
    // unlink, leaving a half-deleted account behind. Propagating instead rolls
    // the whole transaction back, which keeps the account consistent.
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
            tx.execute(stmt, params![account_id])
                .map_err(|e| format!("delete_device_link cascade '{stmt}': {e}"))?;
        }
        Ok(())
    })?;

    Ok(removed)
}

/// Accounts `owner` can address.
///
/// The definition of "address" is [`VISIBLE_ACCOUNTS_SQL`], so this count,
/// `/accounts`, the owner console and the settings card cannot drift apart.
/// `None` (no cookie at all) addresses nothing.
pub fn visible_account_count(owner: Option<&str>) -> Result<i64, String> {
    let conn = open_db()?;
    visible_account_count_conn(&conn, owner)
}

fn visible_account_count_conn(
    conn: &rusqlite::Connection,
    owner: Option<&str>,
) -> Result<i64, String> {
    let Some(token) = owner else {
        return Ok(0);
    };
    conn.query_row(
        &format!("SELECT COUNT(*) FROM accounts WHERE id IN {VISIBLE_ACCOUNTS_SQL}"),
        params![token],
        |row| row.get(0),
    )
    .map_err(|e| format!("count visible accounts: {e}"))
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

/// Every account id the console covers for `owner_token`.
///
/// Used to count running `/process` jobs across everything the console shows
/// rather than only the page it is showing: job state is in-process, so the only
/// thing that has to come from SQL is the set of ids — and for an operator that
/// set is the instance, not their links (`instance::console_accounts_sql`).
///
/// Deliberately not the account picker's scope: what the console *displays* and
/// what this browser may *act as* are different sets.
pub fn console_account_ids(owner_token: &str) -> Result<Vec<i32>, String> {
    let conn = open_db()?;
    let fingerprint = crate::crypto::admin_key_fingerprint(&cfg().admin_api);
    let mut stmt = conn
        .prepare(&format!(
            "SELECT id FROM accounts WHERE id IN {}",
            console_accounts_sql()
        ))
        .map_err(|e| format!("Failed to construct console-ids query: {e}"))?;
    stmt.query_map(params![owner_token, fingerprint], |row| {
        row.get::<_, i32>(0)
    })
    .map_err(|e| format!("Failed to enumerate console account ids: {e}"))?
    .collect::<Result<Vec<_>, _>>()
    .map_err(|e| format!("Failed to collect console account ids: {e}"))
}

/// One row of the operator console's account table (TODO §3.2).
///
/// Deliberately flat and SQL-only: the route adds the two pieces that are not
/// database state — the *effective* bucket (needs `Config`) and the
/// in-process `/process` job.
#[derive(Debug, Clone)]
pub struct OwnedAccountRow {
    pub id: i32,
    pub name: String,
    /// Bucket pinned in the DB. Kept verbatim even when it no longer names a
    /// configured bucket, so the console can show a stale pin rather than
    /// making the account look unassigned.
    pub bucket_explicit: Option<String>,
    pub has_key: bool,
    pub saved_posts: i64,
    /// Does this browser hold a *direct* link to the account?
    ///
    /// The console's unlink action can only remove a link that exists, and the
    /// instance owner addresses accounts they hold no link to, so the action is
    /// offered from this flag rather than from "the row is visible".
    pub linked: bool,
    /// May this browser *address* the account — sync it, export it, start a
    /// `/process` run, pin its bucket?
    ///
    /// Wider than [`Self::linked`] in one direction (the instance owner
    /// addresses accounts it holds no link to) and identical otherwise. A row
    /// can be visible without being addressable: an operator's console covers
    /// the instance, but operating the machine is not the same as holding the
    /// libraries on it — that is what a link is for. Every per-row action needs
    /// this flag, because each one lands on a route that re-checks
    /// `account_in_scope`.
    pub addressable: bool,
}

/// A page of console accounts plus totals over **all** of them.
///
/// The scope is the console's, not the account picker's: an operator sees every
/// account on the instance while still addressing only its own links. Totals are
/// counted over every account in that scope, not over the page: a panel
/// that adds up only the visible rows under-reports as soon as the list is
/// long enough to paginate, which is precisely the state a 900-account library
/// is in.
///
/// The per-account library size is a correlated subquery rather than a
/// `GROUP BY account_id` derived table: the index is
/// `idx_ap_acc_post(account_id, post_id)` (`V1__init.sql:79`), so counting one
/// account's rows is an index range scan, while grouping the whole table would
/// read every library row in the database to answer a question about one page.
pub fn console_accounts_overview(
    owner_token: &str,
    limit: i64,
    offset: i64,
) -> Result<(Vec<OwnedAccountRow>, AdminTotals), String> {
    let mut conn = open_db()?;
    let fingerprint = crate::crypto::admin_key_fingerprint(&cfg().admin_api);
    // One read transaction so the page and the totals describe one snapshot
    // (same reasoning as `get_account_feed_settings`).
    let tx = conn
        .transaction()
        .map_err(|e| format!("Failed to begin read transaction: {e}"))?;
    console_accounts_overview_tx(&tx, owner_token, &fingerprint, limit, offset)
}

/// Body of [`console_accounts_overview`], separated from connection setup so the
/// paging/total semantics can be pinned against an in-memory schema — those
/// semantics are the whole point of the function and are invisible in a
/// single-page smoke test.
///
/// The fingerprint is a parameter for the same reason it is one in
/// `instance::admin_grant_exists`: the scope rule has to be exercisable without a
/// loaded config, and "the grant matches the key" should stay visible here.
fn console_accounts_overview_tx(
    tx: &rusqlite::Transaction<'_>,
    owner_token: &str,
    fingerprint: &str,
    limit: i64,
    offset: i64,
) -> Result<(Vec<OwnedAccountRow>, AdminTotals), String> {
    // Bound once and borrowed by the `format!` calls below; through the whole
    // function `?1` is the token and `?2` the fingerprint.
    let scope = console_accounts_sql();
    let accounts = tx
        .query_row(
            &format!("SELECT COUNT(*) FROM accounts WHERE id IN {scope}"),
            params![owner_token, fingerprint],
            |r| r.get::<_, i64>(0),
        )
        .map_err(|e| format!("count console accounts: {e}"))?;
    let saved_posts = tx
        .query_row(
            &format!(
                "SELECT COUNT(DISTINCT ap.post_id)
                   FROM accounts_post ap
                  WHERE ap.account_id IN {scope}"
            ),
            params![owner_token, fingerprint],
            |r| r.get::<_, i64>(0),
        )
        .map_err(|e| format!("count console library posts: {e}"))?;
    let accounts_with_key = tx
        .query_row(
            &format!(
                "SELECT COUNT(*)
                   FROM accounts a
                  WHERE a.e621_api_key_encrypted IS NOT NULL
                    AND a.id IN {scope}"
            ),
            params![owner_token, fingerprint],
            |r| r.get::<_, i64>(0),
        )
        .map_err(|e| format!("count console accounts with keys: {e}"))?;

    let mut stmt = tx
        .prepare(&format!(
            r"
        SELECT a.id, a.name, a.experiment_bucket,
               (a.e621_api_key_encrypted IS NOT NULL),
               (SELECT COUNT(*) FROM accounts_post ap WHERE ap.account_id = a.id),
               (adl.owner_token IS NOT NULL),
               (adl.owner_token IS NOT NULL OR {IS_OWNER_SQL})
        FROM accounts a
        LEFT JOIN account_device_links adl
               ON adl.account_id = a.id AND adl.owner_token = ?1
        WHERE a.id IN {scope}
        ORDER BY adl.last_seen_at DESC, a.name ASC, a.id ASC
        LIMIT ?3 OFFSET ?4
        "
        ))
        .map_err(|e| format!("Failed to construct overview query: {e}"))?;

    let rows = stmt
        .query_map(params![owner_token, fingerprint, limit, offset], |row| {
            Ok(OwnedAccountRow {
                id: row.get(0)?,
                name: row.get(1)?,
                bucket_explicit: row.get(2)?,
                has_key: row.get(3)?,
                saved_posts: row.get(4)?,
                linked: row.get(5)?,
                addressable: row.get(6)?,
            })
        })
        .map_err(|e| format!("Failed to enumerate console accounts: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect console accounts: {e}"))?;

    // `stmt` borrows the transaction and is dropped at the end of the scope;
    // the caller owns the connection, so the read snapshot is released there.
    Ok((
        rows,
        AdminTotals {
            accounts,
            saved_posts,
            accounts_with_key,
            // Filled by the route from the in-process job registry.
            jobs_running: 0,
        },
    ))
}

/// Pin or clear an account's explicit A/B bucket.
///
/// Ownership-checked inside the transaction: the `account_device_links` row
/// must name `owner_token`, so a stolen account id alone writes nothing. The
/// caller validates the name against `[buckets.*]`; this function stores what
/// it is given so "pin" and "clear" share one path.
pub fn set_account_experiment_bucket(
    owner_token: &str,
    account_id: i32,
    bucket: Option<&str>,
) -> Result<(), String> {
    super::with_write_tx(|tx| set_account_experiment_bucket_tx(tx, owner_token, account_id, bucket))
}

/// Body of [`set_account_experiment_bucket`], separated from the write
/// transaction so the ownership rule can be pinned on an in-memory schema.
fn set_account_experiment_bucket_tx(
    tx: &rusqlite::Transaction<'_>,
    owner_token: &str,
    account_id: i32,
    bucket: Option<&str>,
) -> Result<(), String> {
    let in_scope: bool = tx
        .query_row(
            &format!("SELECT {MAY_ADDRESS_SQL}"),
            params![owner_token, account_id],
            |r| r.get(0),
        )
        .map_err(|e| format!("verify account access: {e}"))?;
    if !in_scope {
        // Wording matches `get_account_by_id`, so callers that already
        // special-case "No account found" keep working.
        return Err("No account found for this device token".to_string());
    }
    tx.execute(
        "UPDATE accounts SET experiment_bucket = ?1 WHERE id = ?2",
        params![bucket, account_id],
    )
    .map_err(|e| format!("set experiment bucket: {e}"))?;
    Ok(())
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

    // Verify the account is in scope: linked to this device, or the instance
    // owner, who addresses every account on the instance.
    let in_scope: bool = tx
        .query_row(
            &format!("SELECT {MAY_ADDRESS_SQL}"),
            params![owner_token, account_id],
            |r| r.get(0),
        )
        .map_err(|e| format!("Failed to verify account access: {e}"))?;
    if !in_scope {
        return Err("No account found for this device token".to_string());
    }

    // 1) Blacklist: device-specific with global fallback. The account is known
    // to be in scope by now, so this is a plain read of the override.
    let blacklist: String = tx
        .query_row(
            r"
            SELECT COALESCE(NULLIF(adl.blacklisted_tags, ''), a.blacklisted_tags, '')
            FROM accounts a
            LEFT JOIN account_device_links adl
                   ON adl.account_id = a.id AND adl.owner_token = ?2
            WHERE a.id = ?1
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
        let in_scope: bool = tx
            .query_row(
                &format!("SELECT {MAY_ADDRESS_SQL}"),
                params![owner_token, account_id],
                |row| row.get(0),
            )
            .map_err(|e| format!("Failed to verify account ownership: {e}"))?;

        if !in_scope {
            return Err("Account is not in scope for this device token".to_string());
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

/// Total rows in `accounts` — the value `e621_accounts_total` reports.
///
/// Deliberately not built on [`count_accounts_by_bucket`]: that one reads every
/// id and evaluates the bucket rules per account, which is both more expensive
/// and a different question.
pub fn count_accounts() -> Result<i64, String> {
    let conn = open_db()?;
    conn.query_row("SELECT COUNT(*) FROM accounts", [], |row| {
        row.get::<_, i64>(0)
    })
    .map_err(|e| format!("Failed to count accounts: {e}"))
}

/// Number of accounts whose last recorded visit falls inside `window_days`.
///
/// `user_visit_tracker.last_visit_date` is a bare `YYYY-MM-DD` written by
/// [`update_visit_tracker`], and `date('now', ...)` renders in the same shape,
/// so comparing the two as text is chronological. That holds only because both
/// sides are dates: had the column held an RFC3339 timestamp, `datetime('now')`
/// would render with a space separator, and since `'T'` sorts above `' '` every
/// row on the boundary date would compare greater than the cutoff regardless of
/// the time of day.
///
/// Accounts that have never visited have no row at all and are not counted —
/// on the production table this covers 47 of 91 accounts.
pub fn count_accounts_active_within(window_days: i64) -> Result<i64, String> {
    let conn = open_db()?;
    count_active_within_conn(&conn, window_days)
}

/// Split out from [`count_accounts_active_within`] so the window arithmetic can
/// be tested against an in-memory table instead of the live database, which
/// `open_db` insists on.
fn count_active_within_conn(conn: &rusqlite::Connection, window_days: i64) -> Result<i64, String> {
    conn.query_row(
        "SELECT COUNT(*) FROM user_visit_tracker \
         WHERE last_visit_date >= date('now', ?1)",
        params![format!("-{window_days} day")],
        |r| r.get(0),
    )
    .map_err(|e| format!("count_accounts_active_within: {e}"))
}

// ---------------------------------------------------------------------------
// Per-account e621 API key storage (encrypted at rest).
//
// ACCOUNT-scoped: an e621 account has ONE canonical API key (the owner's),
// stored on the `accounts` row so it is available to every linked device for
// direct sync. Ownership is enforced at ACCESS time via `require_account_scope`
// — a token must be linked to the account, or own the instance, to view or
// manage the key — but the key is a single shared account resource, so sync
// works from any device in scope. (The admin_user account syncs with the shared
// admin_api and needs no stored key.) The raw key is AES-256-GCM encrypted
// (`crypto`), never returned over the API, and never exported (the export is
// built from explicit fields and omits these columns).
// ---------------------------------------------------------------------------

/// Verify that `owner_token` may address `account_id`; errors otherwise.
/// Shared by every key accessor so the ownership rule lives in one place.
fn require_account_scope(
    conn: &rusqlite::Connection,
    owner_token: &str,
    account_id: i32,
) -> Result<(), String> {
    let in_scope: bool = conn
        .query_row(
            &format!("SELECT {MAY_ADDRESS_SQL}"),
            params![owner_token, account_id],
            |r| r.get(0),
        )
        .map_err(|e| format!("Failed to verify account ownership: {e}"))?;
    if !in_scope {
        return Err("Account is not in scope for this device token".to_string());
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
        require_account_scope(tx, owner_token, account_id)?;
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
    require_account_scope(&conn, owner_token, account_id)?;
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
    require_account_scope(&conn, owner_token, account_id)?;
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
    require_account_scope(&conn, owner_token, account_id)?;
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
        require_account_scope(tx, owner_token, account_id)?;
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
        require_account_scope(tx, owner_token, account_id)?;
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
        require_account_scope(tx, owner_token, account_id)?;
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
    require_account_scope(&conn, owner_token, account_id)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The window arithmetic, and the text comparison that carries it.
    ///
    /// `last_visit_date` is a bare date, so a text `>=` against `date('now')`
    /// is chronological and the boundary day is inclusive. Both properties are
    /// load-bearing for `e621_active_accounts` and neither is obvious from the
    /// SQL, so they are pinned here — offline, against an in-memory table.
    #[test]
    fn active_within_counts_only_rows_inside_the_window() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE user_visit_tracker (
                 account_id INTEGER PRIMARY KEY,
                 last_visit_date TEXT NOT NULL
             );",
        )
        .unwrap();

        // Anchor every row on the same clock the query uses, so the assertion
        // does not depend on when the test runs.
        for (id, offset) in [0_i64, 1, 2, 6, 7, 8, 29, 30, 31, 400]
            .into_iter()
            .enumerate()
        {
            conn.execute(
                "INSERT INTO user_visit_tracker (account_id, last_visit_date) \
                 VALUES (?1, date('now', ?2))",
                params![id as i64, format!("-{offset} day")],
            )
            .unwrap();
        }

        // rows at 0 and 1 days
        assert_eq!(count_active_within_conn(&conn, 1).unwrap(), 2);
        // +2, +6, +7  (the 7-day boundary itself is inside, 8 is not)
        assert_eq!(count_active_within_conn(&conn, 7).unwrap(), 5);
        // +8, +29, +30 (31 is outside)
        assert_eq!(count_active_within_conn(&conn, 30).unwrap(), 8);
    }

    /// The ordering trap this column avoids, kept as a test so a future
    /// "store a full timestamp instead" change cannot land silently.
    ///
    /// `datetime('now')` renders with a *space* separator and `'T'` sorts above
    /// `' '`, so the separator — not the time of day — decides the comparison
    /// for a row whose **date prefix equals the cutoff's date**. Such a row
    /// counts as inside the window whatever time it holds. Rows on other dates
    /// are decided by the date prefix before the separator is reached, so the
    /// damage is confined to the boundary day.
    ///
    /// `last_visit_date` sidesteps all of it by staying a bare date.
    #[test]
    fn an_rfc3339_value_sorts_above_a_datetime_cutoff_as_text() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();

        let cutoff_sep: String = conn
            .query_row("SELECT substr(datetime('now', '-1 day'), 11, 1)", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(cutoff_sep, " ");

        let date_len: i64 = conn
            .query_row("SELECT length(date('now', '-1 day'))", [], |r| r.get(0))
            .unwrap();
        assert_eq!(date_len, 10, "a bare date must not carry a separator");

        // Same date as the cutoff, but midnight: earlier in the day, yet it
        // compares as *greater* because of the separator.
        let boundary_sorts_above: bool = conn
            .query_row(
                "SELECT (date('now','-1 day') || 'T00:00:00+00:00') > datetime('now','-1 day')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            boundary_sorts_above,
            "a row on the cutoff's own date must sort above it"
        );

        // An earlier date is decided by the prefix, so it still sorts below.
        // This bounds the trap to the boundary day.
        let earlier_date_sorts_below: bool = conn
            .query_row(
                "SELECT '2020-01-01T00:00:00+00:00' > datetime('now','-1 day')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            !earlier_date_sorts_below,
            "an earlier date sorts below regardless of its separator"
        );
    }

    /// Schema for the scope tests: three accounts (linked to the seed token, to
    /// somebody else, and to nobody), the claim table, and the two columns the
    /// overview reads.
    fn scope_fixture() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE accounts (
                 id INTEGER PRIMARY KEY,
                 name TEXT,
                 blacklisted_tags TEXT,
                 experiment_bucket TEXT,
                 e621_api_key_encrypted BLOB
             );
             CREATE TABLE account_device_links (
                 owner_token TEXT NOT NULL,
                 account_id INTEGER NOT NULL,
                 linked_at TEXT NOT NULL,
                 last_seen_at TEXT NOT NULL,
                 blacklisted_tags TEXT,
                 admin_granted_at TEXT,
                 admin_key_fingerprint TEXT,
                 PRIMARY KEY(owner_token, account_id)
             );
             CREATE TABLE instance_ownership (
                 id INTEGER PRIMARY KEY CHECK (id = 1),
                 owner_token TEXT NOT NULL,
                 claimed_at TEXT NOT NULL,
                 claim_method TEXT NOT NULL CHECK (claim_method IN ('bootstrap','secret'))
             );
             CREATE TABLE accounts_post (account_id INTEGER NOT NULL, post_id INTEGER NOT NULL);
             INSERT INTO accounts (id, name, blacklisted_tags) VALUES
                 (1,'unlinked','acct-default'), (2,'seeded',NULL), (3,'someone',NULL);
             INSERT INTO account_device_links
                 (owner_token, account_id, linked_at, last_seen_at, blacklisted_tags) VALUES
                 ('calibration-seed', 2, '2026-01-01T00:00:00Z', '2026-01-02T00:00:00Z', NULL),
                 ('someone-else',     3, '2026-01-01T00:00:00Z', '2026-01-02T00:00:00Z', NULL);
             INSERT INTO accounts_post (account_id, post_id) VALUES (1,10),(2,10),(2,11),(3,12);",
        )
        .unwrap();
        conn
    }

    fn claim_instance_for(conn: &rusqlite::Connection, token: &str) {
        conn.execute(
            "INSERT INTO instance_ownership (id, owner_token, claimed_at, claim_method)
             VALUES (1, ?1, '2026-01-01T00:00:00Z', 'secret')",
            params![token],
        )
        .unwrap();
    }

    /// The scope rule as every account-addressed route sees it. `get_account_by_id`
    /// is the single check behind catalog, catalog_manage, browse, feed, digest and
    /// taste_profile, so the owner/non-owner matrix is pinned here rather than once
    /// per route.
    #[test]
    fn account_by_id_follows_the_instance_claim_not_the_links() {
        let conn = scope_fixture();

        // No claim: scope is exactly the links.
        assert_eq!(
            get_account_by_id_conn(&conn, "calibration-seed", 2)
                .unwrap()
                .id,
            2
        );
        // The denial is marked, because the routes can only propagate the
        // string: without the marker `ApiError::from_string` answers 500.
        let denied = get_account_by_id_conn(&conn, "stranger", 1).unwrap_err();
        assert!(
            denied.starts_with(crate::errors::NOT_FOUND_ERR_MARKER),
            "an ownership miss must carry the 404 marker, got: {denied}"
        );
        assert!(get_account_by_id_conn(&conn, "calibration-seed", 1).is_err());
        assert!(get_account_by_id_conn(&conn, "stranger", 1).is_err());

        claim_instance_for(&conn, "owner");

        // The owner holds no link at all and still reaches every account.
        for id in [1, 2, 3] {
            assert_eq!(get_account_by_id_conn(&conn, "owner", id).unwrap().id, id);
        }
        // The claim grants nothing to anybody else.
        assert!(get_account_by_id_conn(&conn, "stranger", 1).is_err());
        assert!(get_account_by_id_conn(&conn, "calibration-seed", 1).is_err());
        // An unlinked account has no per-device override, so the account-wide
        // blacklist is what comes back — the same COALESCE the write path uses.
        assert_eq!(
            get_account_by_id_conn(&conn, "owner", 1).unwrap().blacklist,
            "acct-default"
        );
    }

    #[test]
    fn visible_account_count_follows_the_claim() {
        let conn = scope_fixture();

        assert_eq!(
            visible_account_count_conn(&conn, Some("calibration-seed")).unwrap(),
            1
        );
        // No cookie at all addresses nothing — `None` is not "everyone".
        assert_eq!(visible_account_count_conn(&conn, None).unwrap(), 0);

        claim_instance_for(&conn, "owner");

        assert_eq!(visible_account_count_conn(&conn, Some("owner")).unwrap(), 3);
        assert_eq!(
            visible_account_count_conn(&conn, Some("stranger")).unwrap(),
            0
        );
    }

    /// The console's `linked` flag decides whether "unlink" can do anything at
    /// all: the instance owner addresses accounts they hold no link to, and
    /// `delete_device_link` removes links, so offering the action on an unlinked
    /// row would promise a 404.
    #[test]
    fn the_overview_marks_which_rows_are_actually_linked() {
        let mut conn = scope_fixture();
        claim_instance_for(&conn, "owner");
        let tx = conn.transaction().unwrap();

        let (rows, totals) = console_accounts_overview_tx(&tx, "owner", "fp", 10, 0).unwrap();
        assert_eq!(totals.accounts, 3, "the owner sees the whole instance");
        assert_eq!(totals.saved_posts, 3, "distinct posts 10, 11 and 12");
        assert_eq!(rows.len(), 3);
        assert!(
            rows.iter().all(|r| !r.linked),
            "the owner holds no direct link"
        );
        assert!(
            rows.iter().all(|r| r.addressable),
            "and still addresses every account it holds no link to"
        );

        let (rows, totals) =
            console_accounts_overview_tx(&tx, "calibration-seed", "fp", 10, 0).unwrap();
        assert_eq!(totals.accounts, 1);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].linked, "this row really is linked");
        assert!(rows[0].addressable);
        assert_eq!(rows[0].id, 2);

        drop(tx);
    }

    /// The operator case the split exists for: the console covers every account
    /// on the instance even though this browser holds a link to only one of
    /// them — and the extra rows carry no link, which is what keeps "unlink" off
    /// them and keeps them out of the account picker.
    #[test]
    fn an_operator_sees_the_instance_without_gaining_any_links() {
        let mut conn = scope_fixture();
        conn.execute(
            "UPDATE account_device_links SET admin_granted_at = '2026-01-01T00:00:00Z', \
             admin_key_fingerprint = 'fp' WHERE owner_token = 'calibration-seed' AND account_id = 2",
            [],
        )
        .unwrap();

        // The picker still covers exactly the one link.
        assert_eq!(
            visible_account_count_conn(&conn, Some("calibration-seed")).unwrap(),
            1,
            "the console scope must not widen the account picker"
        );

        let tx = conn.transaction().unwrap();
        let (rows, totals) =
            console_accounts_overview_tx(&tx, "calibration-seed", "fp", 10, 0).unwrap();
        assert_eq!(totals.accounts, 3, "the console covers the instance");
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows.iter()
                .filter(|r| r.linked)
                .map(|r| r.id)
                .collect::<Vec<_>>(),
            vec![2],
            "only a real link may be offered unlink"
        );
        assert_eq!(
            rows.iter()
                .filter(|r| r.addressable)
                .map(|r| r.id)
                .collect::<Vec<_>>(),
            vec![2],
            "operating the instance must not make a library addressable"
        );
    }

    /// A grant is bound to the key that wrote it, so a rotated `admin_api`
    /// narrows the console back to the links without touching the rows.
    #[test]
    fn a_rotated_key_narrows_the_console_back_to_the_links() {
        let mut conn = scope_fixture();
        conn.execute(
            "UPDATE account_device_links SET admin_granted_at = '2026-01-01T00:00:00Z', \
             admin_key_fingerprint = 'fp' WHERE owner_token = 'calibration-seed' AND account_id = 2",
            [],
        )
        .unwrap();

        let tx = conn.transaction().unwrap();
        let (rows, totals) =
            console_accounts_overview_tx(&tx, "calibration-seed", "rotated", 10, 0).unwrap();
        assert_eq!(totals.accounts, 1, "the old grant is not a grant");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, 2);
    }

    /// The console's page and its totals answer different questions, and the
    /// difference only shows up once the list is long enough to paginate —
    /// exactly the state a several-hundred-account library is in. A totals
    /// panel that counted the visible rows would quietly under-report.
    #[test]
    fn overview_totals_cover_every_owned_account_not_just_the_page() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE accounts (
                 id INTEGER PRIMARY KEY,
                 name TEXT NOT NULL,
                 experiment_bucket TEXT,
                 e621_api_key_encrypted TEXT
             );
             CREATE TABLE account_device_links (
                 owner_token TEXT NOT NULL,
                 account_id INTEGER NOT NULL,
                 linked_at TEXT NOT NULL,
                 last_seen_at TEXT NOT NULL,
                 admin_granted_at TEXT,
                 admin_key_fingerprint TEXT,
                 PRIMARY KEY(owner_token, account_id)
             );
             CREATE TABLE instance_ownership (
                 id INTEGER PRIMARY KEY CHECK (id = 1),
                 owner_token TEXT NOT NULL,
                 claimed_at TEXT NOT NULL,
                 claim_method TEXT NOT NULL CHECK (claim_method IN ('bootstrap','secret'))
             );
             CREATE TABLE accounts_post (post_id INTEGER NOT NULL, account_id INTEGER NOT NULL);
             INSERT INTO accounts (id, name, experiment_bucket, e621_api_key_encrypted) VALUES
                 (1,'a',NULL,NULL), (2,'b','control',NULL), (3,'c',NULL,'sealed'),
                 (9,'other',NULL,NULL);
             INSERT INTO account_device_links
                 (owner_token, account_id, linked_at, last_seen_at) VALUES
                 ('me',1,'t','2026-01-03'), ('me',2,'t','2026-01-02'),
                 ('me',3,'t','2026-01-01'), ('other',9,'t','2026-01-09');
             -- Post 100 is saved by two of my accounts: one post, not two.
             INSERT INTO accounts_post VALUES (100,1), (100,2), (101,2), (200,9);",
        )
        .unwrap();

        let tx = conn.transaction().unwrap();

        // One row on the page...
        let (page, totals) = console_accounts_overview_tx(&tx, "me", "fp", 1, 1).unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].id, 2, "second row in last_seen order");
        assert_eq!(page[0].saved_posts, 2, "account 2 saved 100 and 101");
        assert_eq!(page[0].bucket_explicit.as_deref(), Some("control"));

        // ...but totals over the whole library.
        assert_eq!(totals.accounts, 3);
        assert_eq!(
            totals.saved_posts, 2,
            "100 is shared, so the distinct set is 100 and 101"
        );
        assert_eq!(totals.accounts_with_key, 1);

        let (all, _) = console_accounts_overview_tx(&tx, "me", "fp", 10, 0).unwrap();
        assert_eq!(
            all.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![1, 2, 3],
            "another token's account must never appear"
        );
        assert!(all[2].has_key, "account 3 carries the only stored key");
        assert!(all[0].bucket_explicit.is_none());
        assert!(all[0].name == "a");
        assert!(
            all.iter().all(|r| r.linked),
            "every row here really is linked, so unlink is offered on all of them"
        );
    }

    /// The bucket write is ownership-checked inside the transaction, so a valid
    /// cookie that owns nothing cannot pin an arbitrary account id — and a
    /// refused write leaves no row changed.
    #[test]
    fn pinning_a_bucket_requires_ownership() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE accounts (
                 id INTEGER PRIMARY KEY,
                 name TEXT NOT NULL,
                 experiment_bucket TEXT,
                 e621_api_key_encrypted TEXT
             );
             CREATE TABLE account_device_links (
                 owner_token TEXT NOT NULL,
                 account_id INTEGER NOT NULL,
                 linked_at TEXT NOT NULL,
                 last_seen_at TEXT NOT NULL,
                 PRIMARY KEY(owner_token, account_id)
             );
             CREATE TABLE instance_ownership (
                 id INTEGER PRIMARY KEY CHECK (id = 1),
                 owner_token TEXT NOT NULL,
                 claimed_at TEXT NOT NULL,
                 claim_method TEXT NOT NULL CHECK (claim_method IN ('bootstrap','secret'))
             );
             INSERT INTO accounts (id, name, experiment_bucket, e621_api_key_encrypted) VALUES
                 (1,'mine',NULL,NULL), (2,'theirs',NULL,NULL);
             INSERT INTO account_device_links VALUES
                 ('me',1,'t','t'), ('other',2,'t','t');",
        )
        .unwrap();

        {
            let tx = conn.transaction().unwrap();
            set_account_experiment_bucket_tx(&tx, "me", 1, Some("variant")).unwrap();
            assert!(
                set_account_experiment_bucket_tx(&tx, "me", 2, Some("variant")).is_err(),
                "another owner's account must not be pinnable"
            );
            assert!(
                set_account_experiment_bucket_tx(&tx, "nobody", 1, Some("variant")).is_err(),
                "a token with no link must not be able to pin anything"
            );
            tx.commit().unwrap();
        }

        // The instance owner holds no link to account 2 and still may pin it:
        // the write path takes the claim, not the links.
        conn.execute(
            "INSERT INTO instance_ownership (id, owner_token, claimed_at, claim_method)
             VALUES (1, 'owner', 't', 'secret')",
            [],
        )
        .unwrap();
        {
            let tx = conn.transaction().unwrap();
            set_account_experiment_bucket_tx(&tx, "owner", 2, Some("control")).unwrap();
            tx.commit().unwrap();
        }
        let mine: Option<String> = conn
            .query_row(
                "SELECT experiment_bucket FROM accounts WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(mine.as_deref(), Some("variant"));
        // Account 2 carries the owner's pin, so `me`'s rejected write clearly did
        // not land — and the rejection happened while the column was still NULL.
        let theirs: Option<String> = conn
            .query_row(
                "SELECT experiment_bucket FROM accounts WHERE id = 2",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(theirs.as_deref(), Some("control"));

        // `None` is how the console clears a pin: the account goes back to the
        // hash assignment rather than to a bucket named "".
        {
            let tx = conn.transaction().unwrap();
            set_account_experiment_bucket_tx(&tx, "me", 1, None).unwrap();
            tx.commit().unwrap();
        }
        let cleared: Option<String> = conn
            .query_row(
                "SELECT experiment_bucket FROM accounts WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(cleared.is_none());
    }
}
