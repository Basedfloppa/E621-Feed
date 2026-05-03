use chrono::Utc;
use rusqlite::params;

use crate::models::TruncatedAccount;

use super::open_db;

pub fn set_account(
    owner_token: &str,
    account_id: i32,
    name: &str,
    mut blacklisted_tags: &str,
) -> Result<TruncatedAccount, String> {
    if blacklisted_tags.is_empty() {
        blacklisted_tags = "
gore
scat
watersports
young -rating:s
loli
shota";
    }

    super::with_write_tx(|tx| {
        tx.execute(
            "
            INSERT INTO accounts (id, name, blacklisted_tags)
            VALUES (?1, ?2, ?3)
            ON CONFLICT(id) DO UPDATE SET
            name = excluded.name,
            blacklisted_tags = excluded.blacklisted_tags",
            params![account_id, name, blacklisted_tags],
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
    let affected = super::with_write_tx(|tx| {
        tx.execute(
            "
            UPDATE account_device_links
            SET blacklisted_tags = ?3, last_seen_at = ?4
            WHERE owner_token = ?1 AND account_id = ?2
            ",
            params![
                owner_token,
                account_id,
                blacklisted_tags,
                Utc::now().to_rfc3339(),
            ],
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
        SELECT a.id, a.name, COALESCE(NULLIF(adl.blacklisted_tags, ''), a.blacklisted_tags)
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
        SELECT a.id, a.name, COALESCE(NULLIF(adl.blacklisted_tags, ''), a.blacklisted_tags)
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
        SELECT a.id, a.name, COALESCE(NULLIF(adl.blacklisted_tags, ''), a.blacklisted_tags)
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
