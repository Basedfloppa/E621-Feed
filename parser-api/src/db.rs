use crate::models::{
    AccountMediaStat, AccountPreferenceProfile, AccountQualityProfile, AccountRatingStat,
    AccountRecencyProfile, AccountTagFeedback, FeedInteractionRequest, FeedInteractionType, Post,
    TagCount, TruncatedAccount,
};
use chrono::{DateTime, Utc};
use rocket::{
    Build, Rocket,
    fairing::{Fairing, Info, Kind},
};
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::{collections::HashSet, fs};

mod embedded {
    use refinery::embed_migrations;
    embed_migrations!("migrations");
}

pub struct DbInit;

#[rocket::async_trait]
impl Fairing for DbInit {
    fn info(&self) -> Info {
        Info {
            name: "SQLite DB Initializer",
            kind: Kind::Ignite,
        }
    }

    async fn on_ignite(&self, rocket: Rocket<Build>) -> rocket::fairing::Result {
        match ensure_sqlite() {
            Ok(_) => {
                println!("SQLite DB Initialized");
                Ok(rocket)
            }
            Err(e) => {
                println!("Database initialization failed: {e}");
                Err(rocket)
            }
        }
    }
}

fn open_db() -> Result<Connection, String> {
    if fs::exists("database.db").is_err() {
        if let Err(e) = fs::File::create("database.db") {
            eprintln!("{e}")
        }
    }

    let connection =
        Connection::open("database.db").map_err(|e| format!("Failed to get connection: {e}"))?;

    connection
        .execute_batch(
            "
            PRAGMA foreign_keys = ON;
            PRAGMA journal_mode=WAL;
            PRAGMA synchronous=NORMAL;
            PRAGMA busy_timeout=5000;
            ",
        )
        .map_err(|e| format!("Failed to assert pragma: {e}"))?;

    Ok(connection)
}

fn parse_db_datetime(raw: &str) -> Result<DateTime<Utc>, String> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.with_timezone(&Utc))
        .or_else(|_| {
            chrono::DateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S%.f %Z")
                .map(|dt| dt.with_timezone(&Utc))
        })
        .or_else(|_| {
            chrono::DateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S %Z")
                .map(|dt| dt.with_timezone(&Utc))
        })
        .map_err(|e| format!("Failed to parse datetime '{raw}': {e}"))
}

pub fn ensure_sqlite() -> Result<(), String> {
    if fs::exists("database.db").is_err() {
        fs::File::create("database.db").map_err(|e| format!("Failed to create file: {e}"))?;
    }

    let mut conn = open_db().map_err(|e| e.to_string())?;

    embedded::migrations::runner()
        .run(&mut conn)
        .map_err(|e| format!("Failed to run migrations: {e}"))?;

    Ok(())
}

fn touch_account_link(conn: &Connection, owner_token: &str, account_id: i32) -> Result<(), String> {
    conn.execute(
        "
        UPDATE account_device_links
        SET last_seen_at = ?3
        WHERE owner_token = ?1 AND account_id = ?2
        ",
        params![owner_token, account_id, Utc::now().to_rfc3339()],
    )
    .map_err(|e| format!("Failed to touch account device link: {e}"))?;

    Ok(())
}

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

    eprint!("{blacklisted_tags:?}");

    let mut conn = open_db()?;
    let tx = conn
        .transaction()
        .map_err(|e| format!("Failed to start account transaction: {e}"))?;

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

    tx.commit()
        .map_err(|e| format!("Failed to commit account transaction: {e}"))?;

    get_account_by_id(owner_token, account_id)
}

pub fn get_accounts_for_owner(owner_token: &str) -> Result<Vec<TruncatedAccount>, String> {
    let conn = open_db()?;

    let mut stmt = conn
        .prepare(
            r#"
        SELECT a.id, a.name, a.blacklisted_tags
        FROM accounts a
        INNER JOIN account_device_links adl ON adl.account_id = a.id
        WHERE adl.owner_token = ?
        ORDER BY adl.last_seen_at DESC, a.name ASC
        "#,
        )
        .map_err(|e| format!("Failed to construct query: {e}"))?;

    let accounts = stmt
        .query_map([owner_token], |row| {
            Ok(TruncatedAccount {
                id: row.get(0)?,
                name: row.get(1)?,
                blacklist: row.get(2)?,
            })
        })
        .map_err(|e| format!("Failed to get accounts: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to enumerate accounts: {e}"))?;

    drop(stmt);

    for account in &accounts {
        let _ = touch_account_link(&conn, owner_token, account.id);
    }

    Ok(accounts)
}

pub fn get_account_by_name(owner_token: &str, name: String) -> Result<TruncatedAccount, String> {
    let conn = open_db()?;

    let mut stmt = conn
        .prepare(
            r#"
        SELECT a.id, a.name, a.blacklisted_tags
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

    drop(stmt);

    if let Some(account) = accounts.first() {
        let _ = touch_account_link(&conn, owner_token, account.id);
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
        SELECT a.id, a.name, a.blacklisted_tags
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

    drop(stmt);

    if let Some(account) = accounts.first() {
        let _ = touch_account_link(&conn, owner_token, account.id);
        Ok(account.clone())
    } else {
        Err("No account found".to_string())
    }
}

pub fn drop_account_posts(account_id: i32) -> Result<(), String> {
    let mut connection = open_db()?;

    let tx = connection
        .transaction()
        .map_err(|e| format!("Failed to get transaction: {e}"))?;

    {
        let mut clear_account_post = tx
            .prepare_cached("DELETE FROM accounts_post WHERE account_id = ?1")
            .map_err(|e| format!("Failed to prepare transaction: {e}"))?;
        clear_account_post
            .execute(params![account_id])
            .map_err(|e| format!("Failed to execute transaction: {e}"))?;
    }

    tx.commit()
        .map_err(|e| format!("Failed to commit transaction: {e}"))?;

    Ok(())
}

pub fn save_posts(posts: &[Post], account_id: i32) -> Result<(), String> {
    let mut connection = open_db()?;

    let tx = connection
        .transaction()
        .map_err(|e| format!("Failed to get transaction: {e}"))?;

    {
        let mut insert_post = tx
            .prepare_cached(
                "
            INSERT INTO posts (
                id, created_at, updated_at, score_total, fav_count, rating, last_seen_at,
                file_ext, file_width, file_height, file_size, is_animated, duration,
                comment_count, has_notes, is_deleted, has_children
            ) 
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
            ON CONFLICT(id) DO UPDATE SET
            updated_at   = excluded.updated_at,
            score_total = excluded.score_total,
            fav_count   = excluded.fav_count,
            rating      = excluded.rating,
            last_seen_at = excluded.last_seen_at,
            file_ext     = excluded.file_ext,
            file_width   = excluded.file_width,
            file_height  = excluded.file_height,
            file_size    = excluded.file_size,
            is_animated  = excluded.is_animated,
            duration     = excluded.duration,
            comment_count= excluded.comment_count,
            has_notes    = excluded.has_notes,
            is_deleted   = excluded.is_deleted,
            has_children = excluded.has_children;",
            )
            .map_err(|e| format!("Failed to prepare transaction: {e}"))?;
        let mut insert_account = tx
            .prepare_cached(
                "INSERT OR IGNORE INTO accounts_post (account_id, post_id) VALUES (?1, ?2);",
            )
            .map_err(|e| format!("Failed to prepare transaction: {e}"))?;

        for post in posts {
            let file_ext = post.file.as_ref().and_then(|f| f.ext.clone());
            let file_width = post.file.as_ref().map(|f| f.width);
            let file_height = post.file.as_ref().map(|f| f.height);
            let file_size = post.file.as_ref().map(|f| f.size);

            insert_post
                .execute(params![
                    post.id,
                    post.created_at.to_rfc3339(),
                    post.updated_at.to_rfc3339(),
                    post.score.total,
                    post.fav_count,
                    post.rating.to_string(),
                    Utc::now().to_rfc3339(),
                    file_ext,
                    file_width,
                    file_height,
                    file_size,
                    if post.is_animated() { 1 } else { 0 },
                    post.duration,
                    post.comment_count,
                    if post.has_notes { 1 } else { 0 },
                    if post.flags.deleted { 1 } else { 0 },
                    if post.relationships.has_children { 1 } else { 0 }
                ])
                .map_err(|e| format!("Failed to execute transaction: {e}"))?;

            insert_account
                .execute(params![account_id, post.id])
                .map_err(|e| format!("Failed to execute transaction: {e}"))?;
        }
    }

    tx.commit()
        .map_err(|e| format!("Failed to commit transaction: {e}"))?;

    Ok(())
}

pub fn upsert_catalog_posts(posts: &[Post]) -> Result<(), String> {
    let mut connection = open_db()?;
    let tx = connection
        .transaction()
        .map_err(|e| format!("Failed to get transaction: {e}"))?;

    {
        let mut insert_post = tx
            .prepare_cached(
                "
            INSERT INTO posts (
                id, created_at, updated_at, score_total, fav_count, rating, last_seen_at,
                file_ext, file_width, file_height, file_size, is_animated, duration,
                comment_count, has_notes, is_deleted, has_children
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
            ON CONFLICT(id) DO UPDATE SET
                updated_at = excluded.updated_at,
                score_total = excluded.score_total,
                fav_count = excluded.fav_count,
                rating = excluded.rating,
                last_seen_at = excluded.last_seen_at,
                file_ext = excluded.file_ext,
                file_width = excluded.file_width,
                file_height = excluded.file_height,
                file_size = excluded.file_size,
                is_animated = excluded.is_animated,
                duration = excluded.duration,
                comment_count = excluded.comment_count,
                has_notes = excluded.has_notes,
                is_deleted = excluded.is_deleted,
                has_children = excluded.has_children
            ",
            )
            .map_err(|e| format!("Failed to prepare catalog upsert: {e}"))?;

        for post in posts {
            let file_ext = post.file.as_ref().and_then(|f| f.ext.clone());
            let file_width = post.file.as_ref().map(|f| f.width);
            let file_height = post.file.as_ref().map(|f| f.height);
            let file_size = post.file.as_ref().map(|f| f.size);

            insert_post
                .execute(params![
                    post.id,
                    post.created_at.to_rfc3339(),
                    post.updated_at.to_rfc3339(),
                    post.score.total,
                    post.fav_count,
                    post.rating.to_string(),
                    Utc::now().to_rfc3339(),
                    file_ext,
                    file_width,
                    file_height,
                    file_size,
                    if post.is_animated() { 1 } else { 0 },
                    post.duration,
                    post.comment_count,
                    if post.has_notes { 1 } else { 0 },
                    if post.flags.deleted { 1 } else { 0 },
                    if post.relationships.has_children { 1 } else { 0 }
                ])
                .map_err(|e| format!("Failed to upsert catalog post: {e}"))?;
        }
    }

    tx.commit()
        .map_err(|e| format!("Failed to commit catalog post transaction: {e}"))?;
    Ok(())
}

pub fn set_rating_profile(account_id: i32) -> Result<(), String> {
    let mut connection = open_db()?;
    let tx = connection
        .transaction()
        .map_err(|e| format!("Failed to get transaction: {e}"))?;

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

    tx.commit()
        .map_err(|e| format!("Failed to commit rating profile transaction: {e}"))?;
    Ok(())
}

pub fn set_media_profile(account_id: i32) -> Result<(), String> {
    let mut connection = open_db()?;
    let tx = connection
        .transaction()
        .map_err(|e| format!("Failed to get transaction: {e}"))?;

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

    tx.commit()
        .map_err(|e| format!("Failed to commit media profile transaction: {e}"))?;
    Ok(())
}

pub fn set_quality_profile(account_id: i32) -> Result<(), String> {
    let mut connection = open_db()?;
    let tx = connection
        .transaction()
        .map_err(|e| format!("Failed to get transaction: {e}"))?;

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

    tx.commit()
        .map_err(|e| format!("Failed to commit quality profile transaction: {e}"))?;
    Ok(())
}

pub fn set_recency_profile(account_id: i32) -> Result<(), String> {
    let connection = open_db()?;
    let mut stmt = connection
        .prepare(
            "
            SELECT p.created_at
            FROM posts p
            INNER JOIN accounts_post ap ON ap.post_id = p.id
            WHERE ap.account_id = ?1
            ",
        )
        .map_err(|e| format!("Failed to prepare recency profile query: {e}"))?;

    let now = Utc::now();
    let created_raw = stmt
        .query_map([account_id], |row| row.get::<_, String>(0))
        .map_err(|e| format!("Failed to fetch recency profile rows: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to enumerate recency rows: {e}"))?;

    let mut ages: Vec<f32> = Vec::with_capacity(created_raw.len());
    for raw in created_raw {
        let created_at = parse_db_datetime(&raw)?;
        let age_days = (now - created_at).num_seconds() as f32 / 86_400.0;
        ages.push(age_days.max(0.0));
    }

    drop(stmt);

    let avg_age_days = if ages.is_empty() {
        0.0
    } else {
        ages.iter().sum::<f32>() / ages.len() as f32
    };
    let avg_abs_dev_days = if ages.is_empty() {
        0.0
    } else {
        ages.iter()
            .map(|age| (age - avg_age_days).abs())
            .sum::<f32>()
            / ages.len() as f32
    };

    connection
        .execute(
            "
            INSERT INTO account_recency_profile (account_id, avg_age_days, avg_abs_dev_days)
            VALUES (?1, ?2, ?3)
            ON CONFLICT(account_id) DO UPDATE SET
                avg_age_days = excluded.avg_age_days,
                avg_abs_dev_days = excluded.avg_abs_dev_days
            ",
            params![account_id, avg_age_days, avg_abs_dev_days],
        )
        .map_err(|e| format!("Failed to upsert recency profile: {e}"))?;

    Ok(())
}

pub fn refresh_account_profiles(account_id: i32) -> Result<(), String> {
    set_tag_counts(account_id)?;
    set_rating_profile(account_id)?;
    set_media_profile(account_id)?;
    set_quality_profile(account_id)?;
    set_recency_profile(account_id)?;
    Ok(())
}

pub fn set_tag_counts(account_id: i32) -> Result<(), String> {
    let mut connection = open_db()?;

    let counts: Vec<TagCount> = {
        let mut stmt = connection
            .prepare(
                r#"
        SELECT t.name, t.group_type, COUNT(*) as count
        FROM tags t
        INNER JOIN tags_posts tp ON t.id = tp.tag_id
        INNER JOIN accounts_post ap ON tp.post_id = ap.post_id
        WHERE ap.account_id = ?
        GROUP BY t.name, t.group_type
        ORDER BY count DESC
        "#,
            )
            .map_err(|e| format!("Failed to construct query: {e}"))?;

        stmt
            .query_map([account_id], |row| {
                Ok(TagCount {
                    name: row.get(0)?,
                    group_type: row.get(1)?,
                    count: row.get(2)?,
                })
            })
            .map_err(|e| format!("Failed to get accounts: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("Failed to enumerate accounts: {e}"))?
    };

    let tx = connection
        .transaction()
        .map_err(|e| format!("Failed to get transaction: {e}"))?;

    {
        tx.execute(
            "DELETE FROM account_tag_counts WHERE account_id = ?1",
            params![account_id],
        )
        .map_err(|e| format!("Failed to delete account_tag_counts: {e}"))?;

        let mut insert_calc = tx
            .prepare_cached(
                "
        INSERT INTO account_tag_counts (account_id, tag_name, group_type, count) 
        VALUES (?1, ?2, ?3, ?4)
        ON CONFLICT(account_id, tag_name, group_type) DO UPDATE SET
        count = excluded.count;
        ",
            )
            .map_err(|e| format!("Failed to prepare transaction: {e}"))?;

        for entry in counts {
            insert_calc
                .execute(params![
                    account_id,
                    entry.name,
                    entry.group_type,
                    entry.count
                ])
                .map_err(|e| format!("Failed to execute transaction: {e}"))?;
        }
    }

    tx.commit()
        .map_err(|e| format!("Failed to commit transaction: {e}"))?;

    Ok(())
}

pub fn get_tag_counts(account_id: i32) -> Result<Vec<TagCount>, String> {
    let conn = open_db()?;

    let mut stmt = conn
        .prepare("SELECT * FROM account_tag_counts WHERE account_id = ?")
        .map_err(|e| format!("Failed to construct query: {e}"))?;

    let counts = stmt
        .query_map([account_id], |row| {
            Ok(TagCount {
                name: row.get(1)?,
                group_type: row.get(2)?,
                count: row.get(3)?,
            })
        })
        .map_err(|e| format!("Failed to get accounts: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to enumerate accounts: {e}"))?;

    Ok(counts)
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
    Ok(AccountPreferenceProfile {
        rating: get_account_rating_profile(account_id)?,
        media: get_account_media_profile(account_id)?,
        feedback: get_account_tag_feedback(account_id)?,
        quality: get_account_quality_profile(account_id)?,
        recency: get_account_recency_profile(account_id)?,
    })
}

pub fn get_account_tag_feedback(account_id: i32) -> Result<Vec<AccountTagFeedback>, String> {
    let conn = open_db()?;
    let mut stmt = conn
        .prepare(
            "
            SELECT tag_name, group_type, impression_count, positive_count, negative_count
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
        })
    })
    .map_err(|e| format!("Failed to fetch tag feedback: {e}"))?
    .collect::<Result<Vec<_>, _>>()
    .map_err(|e| format!("Failed to enumerate tag feedback: {e}"))
}

pub fn record_feed_interaction(interaction: &FeedInteractionRequest) -> Result<(), String> {
    let mut connection = open_db()?;
    let tx = connection
        .transaction()
        .map_err(|e| format!("Failed to get transaction: {e}"))?;

    let linked: bool = tx
        .query_row(
            "
            SELECT EXISTS(
                SELECT 1 FROM account_device_links
                WHERE owner_token = ?1 AND account_id = ?2
            )
            ",
            params![interaction.owner_token, interaction.account_id],
            |row| row.get(0),
        )
        .map_err(|e| format!("Failed to validate feed interaction owner link: {e}"))?;

    if !linked {
        return Err("Account is not linked to this device token".to_string());
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
            FeedInteractionType::Hide => (0, 0, 1),
        };

        tx.execute(
            "
            INSERT INTO account_tag_feedback (
                account_id, tag_name, group_type,
                impression_count, positive_count, negative_count, last_interaction_at
            )
            SELECT
                ?1,
                t.name,
                t.group_type,
                ?2,
                ?3,
                ?4,
                ?5
            FROM tags t
            INNER JOIN tags_posts tp ON tp.tag_id = t.id
            WHERE tp.post_id = ?6
            ON CONFLICT(account_id, tag_name, group_type) DO UPDATE SET
                impression_count = account_tag_feedback.impression_count + excluded.impression_count,
                positive_count = account_tag_feedback.positive_count + excluded.positive_count,
                negative_count = account_tag_feedback.negative_count + excluded.negative_count,
                last_interaction_at = excluded.last_interaction_at
            ",
            params![
                interaction.account_id,
                impression_delta,
                positive_delta,
                negative_delta,
                Utc::now().to_rfc3339(),
                interaction.post_id,
            ],
        )
        .map_err(|e| format!("Failed to update tag feedback aggregates: {e}"))?;
    }

    tx.commit()
        .map_err(|e| format!("Failed to commit feed interaction transaction: {e}"))?;
    Ok(())
}

pub fn save_posts_tags_batch(posts: &[Post], blacklist: &HashSet<String>) -> Result<(), String> {
    if posts.is_empty() {
        return Ok(());
    }

    let mut connection = open_db()?;
    let tx = connection.transaction().map_err(|e| format!("tx: {e}"))?;

    {
        let mut insert_tag = tx
            .prepare_cached("INSERT OR IGNORE INTO tags (name, group_type) VALUES (?1, ?2)")
            .map_err(|e| format!("prep ins tag: {e}"))?;
        let mut select_id = tx
            .prepare_cached("SELECT id FROM tags WHERE name = ?1 AND group_type = ?2")
            .map_err(|e| format!("prep sel id: {e}"))?;
        let mut link = tx
            .prepare_cached("INSERT OR IGNORE INTO tags_posts(tag_id, post_id) VALUES (?1, ?2)")
            .map_err(|e| format!("prep link: {e}"))?;
        let mut df = tx
            .prepare_cached("UPDATE tags SET df = (SELECT count(*) FROM tags_posts WHERE tag_id = ?1) WHERE id = ?1;")
            .map_err(|e| format!("prep df: {e}"))?;

        for post in posts {
            for (group, tags) in [
                ("artist", &post.tags.artist),
                ("character", &post.tags.character),
                ("copyright", &post.tags.copyright),
                ("general", &post.tags.general),
                ("lore", &post.tags.lore),
                ("species", &post.tags.species),
            ] {
                let pid = post.id;
                for tag in tags {
                    if tag.is_empty() || blacklist.contains(tag) {
                        continue;
                    }

                    insert_tag
                        .execute(params![&tag, group])
                        .map_err(|e| format!("ins tag: {e}"))?;

                    let tag_id: i64 = select_id
                        .query_row(params![&tag, group], |r| r.get(0))
                        .map_err(|e| format!("get id {tag}:{group}: {e}"))?;

                    link.execute(params![tag_id, pid])
                        .map_err(|e| format!("link tag_id={tag_id} post_id={pid}: {e}"))?;

                    df.execute(params![tag_id])
                        .map_err(|e| format!("insert tag df: {e}"))?;
                }
            }
        }
    }

    tx.commit()
        .map_err(|e| format!("commit save_posts_tags_batch: {e}"))?;
    Ok(())
}

pub fn post_count() -> i64 {
    let conn = open_db().expect("open_db failed");
    conn.query_row("SELECT COUNT(*) FROM posts", [], |row| row.get::<_, i64>(0))
        .expect("COUNT(*) query failed")
}

pub fn get_tags_df() -> rusqlite::Result<HashMap<String, i64>> {
    let conn = open_db().unwrap();
    let mut stmt = conn.prepare("SELECT name, df FROM tags")?;

    let mut map = HashMap::new();
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;

    for pair in rows {
        let (name, df) = pair?;
        map.insert(name, df);
    }

    Ok(map)
}
