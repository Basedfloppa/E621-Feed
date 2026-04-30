use crate::models::{
    AccountMediaStat, AccountPreferenceProfile, AccountQualityProfile, AccountRatingStat,
    AccountRecencyProfile, AccountTagFeedback, FeedInteractionRequest, FeedInteractionType, Post,
    TagCount, TagRelationEdge, TagRelationGraphPayload, TagRelationNode, TruncatedAccount,
};
use chrono::{DateTime, Utc};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rocket::{
    Build, Rocket,
    fairing::{Fairing, Info, Kind},
};
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::OnceLock;

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
                spawn_tag_cooccurrence_backfill_if_needed();
                Ok(rocket)
            }
            Err(e) => {
                println!("Database initialization failed: {e}");
                Err(rocket)
            }
        }
    }
}

pub type DbPool = Pool<SqliteConnectionManager>;
pub type DbConn = r2d2::PooledConnection<SqliteConnectionManager>;

static POOL: OnceLock<DbPool> = OnceLock::new();

fn pool() -> &'static DbPool {
    POOL.get_or_init(|| {
        let manager = SqliteConnectionManager::file("database.db").with_init(|conn| {
            conn.execute_batch(
                "
                PRAGMA foreign_keys = ON;
                PRAGMA journal_mode = WAL;
                PRAGMA synchronous  = NORMAL;
                PRAGMA busy_timeout = 5000;
                PRAGMA temp_store   = MEMORY;
                ",
            )
        });
        Pool::builder()
            .max_size(8)
            .build(manager)
            .expect("build sqlite pool")
    })
}

fn open_db() -> Result<DbConn, String> {
    pool()
        .get()
        .map_err(|e| format!("Failed to acquire sqlite connection: {e}"))
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
    let mut conn = open_db().map_err(|e| e.to_string())?;

    embedded::migrations::runner()
        .run(&mut *conn)
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

pub fn update_device_blacklist(
    owner_token: &str,
    account_id: i32,
    blacklisted_tags: &str,
) -> Result<TruncatedAccount, String> {
    let conn = open_db()?;

    let affected = conn
        .execute(
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
        .map_err(|e| format!("Failed to update device blacklist: {e}"))?;

    if affected == 0 {
        return Err("No account found for this device token".to_string());
    }

    drop(conn);
    get_account_by_id(owner_token, account_id)
}

pub fn get_accounts_for_owner(owner_token: &str) -> Result<Vec<TruncatedAccount>, String> {
    let conn = open_db()?;

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

    // One UPDATE per request instead of N. The `WHERE owner_token = ?1` is
    // already enough — we don't need to enumerate account ids; the device-link
    // table has one row per (token, account) and we want to bump them all.
    let _ = conn.execute(
        "
        UPDATE account_device_links
        SET last_seen_at = ?2
        WHERE owner_token = ?1
        ",
        params![owner_token, Utc::now().to_rfc3339()],
    );

    Ok(accounts)
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

    // Mean and mean-abs-dev computed in SQL via julianday(). The `WHERE true`
    // disambiguates ON CONFLICT from a join clause (SQLite UPSERT-with-SELECT quirk).
    connection
        .execute(
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
}

pub fn refresh_account_profiles(account_id: i32) -> Result<(), String> {
    set_tag_counts(account_id)?;
    set_rating_profile(account_id)?;
    set_media_profile(account_id)?;
    set_quality_profile(account_id)?;
    set_recency_profile(account_id)?;
    set_account_tag_cooccurrence(account_id)?;
    decay_account_tag_feedback(account_id)?;
    Ok(())
}

/// Multiplies per-tag feedback counts by `0.5 ^ (elapsed / half_life)` and
/// deletes rows that hit zero. 1-day gate + `round` (not `floor`) keep
/// frequent calls from bleeding counts. SELECT + UPDATEs in one IMMEDIATE
/// tx so concurrent `record_feed_interaction` can't race the snapshot.
pub fn decay_account_tag_feedback(account_id: i32) -> Result<(), String> {
    let cfg = crate::models::cfg();
    let half_life = cfg.priors.feedback_decay_half_life_days.max(1.0);

    let mut connection = open_db()?;
    let now = Utc::now();
    let now_iso = now.to_rfc3339();

    let tx = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| format!("Failed to start decay tx: {e}"))?;

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

    {
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
    }
    tx.commit()
        .map_err(|e| format!("Failed to commit decay tx: {e}"))?;
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

    tx.commit()
        .map_err(|e| format!("Failed to commit feed interaction transaction: {e}"))?;
    Ok(())
}

/// Set `track_cooccurrence = false` on the recommendations path: candidate
/// browse hits aren't user favourites and shouldn't drive per-tag pair counts.
pub fn save_posts_tags_batch(
    posts: &[Post],
    blacklist: &HashSet<String>,
    track_cooccurrence: bool,
) -> Result<(), String> {
    if posts.is_empty() {
        return Ok(());
    }

    let mut connection = open_db()?;
    let tx = connection.transaction().map_err(|e| format!("tx: {e}"))?;

    let mut cooc_dirty = false;
    let mut touched_tag_ids: HashSet<i64> = HashSet::new();
    // Per-batch cache: a 320-post batch has ~5–10K tag references but only a
    // few hundred distinct (name, group) pairs. Caching collapses repeats to
    // a HashMap hit and avoids the upsert+RETURNING round-trip.
    let mut tag_id_cache: HashMap<(String, &'static str), i64> = HashMap::new();
    {
        // Single statement for "ensure tag exists, give me its id". The
        // `DO UPDATE SET name = name` no-op forces RETURNING to fire on the
        // conflict path too, so we always get a row back.
        let mut upsert_tag = tx
            .prepare_cached(
                "INSERT INTO tags (name, group_type) VALUES (?1, ?2)
                 ON CONFLICT(name, group_type) DO UPDATE SET name = name
                 RETURNING id",
            )
            .map_err(|e| format!("prep upsert tag: {e}"))?;
        let mut link = tx
            .prepare_cached("INSERT OR IGNORE INTO tags_posts(tag_id, post_id) VALUES (?1, ?2)")
            .map_err(|e| format!("prep link: {e}"))?;
        let mut post_has_tags = tx
            .prepare_cached("SELECT EXISTS(SELECT 1 FROM tags_posts WHERE post_id = ?1)")
            .map_err(|e| format!("prep post_has_tags: {e}"))?;

        for post in posts {
            let pid = post.id;
            let had_tags: bool = if track_cooccurrence {
                post_has_tags
                    .query_row(params![pid], |r| r.get(0))
                    .map_err(|e| format!("post_has_tags lookup: {e}"))?
            } else {
                true
            };

            let mut post_tag_ids: Vec<i64> = Vec::new();

            // Meta is stored so the interaction channel can read it; the other
            // scorers filter meta out via `group_wts[Meta] = 0`.
            for (group, tags) in [
                ("artist", &post.tags.artist),
                ("character", &post.tags.character),
                ("copyright", &post.tags.copyright),
                ("general", &post.tags.general),
                ("lore", &post.tags.lore),
                ("species", &post.tags.species),
                ("meta", &post.tags.meta),
            ] {
                for tag in tags {
                    if tag.is_empty() {
                        continue;
                    }
                    let tag_lc = if tag.bytes().any(|b| b.is_ascii_uppercase()) {
                        tag.to_ascii_lowercase()
                    } else {
                        tag.clone()
                    };
                    // Blacklist is lowercased at config load (see main.rs).
                    if blacklist.contains(&tag_lc) {
                        continue;
                    }

                    let cache_key = (tag_lc, group);
                    let tag_id = if let Some(&id) = tag_id_cache.get(&cache_key) {
                        id
                    } else {
                        let id: i64 = upsert_tag
                            .query_row(params![&cache_key.0, group], |r| r.get(0))
                            .map_err(|e| {
                                format!("upsert tag {}:{group}: {e}", cache_key.0)
                            })?;
                        tag_id_cache.insert(cache_key, id);
                        id
                    };

                    link.execute(params![tag_id, pid])
                        .map_err(|e| format!("link tag_id={tag_id} post_id={pid}: {e}"))?;

                    touched_tag_ids.insert(tag_id);
                    post_tag_ids.push(tag_id);
                }
            }

            if track_cooccurrence && !had_tags && post_tag_ids.len() >= 2 {
                post_tag_ids.sort_unstable();
                post_tag_ids.dedup();
                upsert_cooccurrence_pairs(&tx, &post_tag_ids)?;
                cooc_dirty = true;
            }
        }

        if !touched_tag_ids.is_empty() {
            recompute_df_for_tags(&tx, &touched_tag_ids)?;
        }
    }

    tx.commit()
        .map_err(|e| format!("commit save_posts_tags_batch: {e}"))?;

    if cooc_dirty {
        crate::utils::mark_global_relation_dirty();
    }
    Ok(())
}

/// SQLite default SQLITE_MAX_VARIABLE_NUMBER is 999. Each pair contributes 2
/// parameters, plus we want headroom — cap at 200 pairs (400 params) per
/// statement.
const COOC_PAIRS_PER_STATEMENT: usize = 200;

fn upsert_cooccurrence_pairs(tx: &rusqlite::Transaction, tag_ids: &[i64]) -> Result<(), String> {
    if tag_ids.len() < 2 {
        return Ok(());
    }
    let n = tag_ids.len();
    let mut pairs: Vec<(i64, i64)> = Vec::with_capacity(n * (n - 1) / 2);
    for i in 0..n {
        let a = tag_ids[i];
        for &b in &tag_ids[i + 1..] {
            // Canonical ordering enforced by CHECK (tag1_id < tag2_id).
            // tag_ids is sorted+deduped before this call, so a < b is implied,
            // but defend against bad inputs anyway.
            if a < b {
                pairs.push((a, b));
            } else {
                pairs.push((b, a));
            }
        }
    }

    for chunk in pairs.chunks(COOC_PAIRS_PER_STATEMENT) {
        let mut sql = String::from(
            "INSERT INTO tag_cooccurrence (tag1_id, tag2_id, cooc_count) VALUES ",
        );
        for i in 0..chunk.len() {
            if i > 0 {
                sql.push(',');
            }
            sql.push_str("(?,?,1)");
        }
        sql.push_str(
            " ON CONFLICT(tag1_id, tag2_id) DO UPDATE SET cooc_count = cooc_count + 1",
        );

        let mut stmt = tx
            .prepare(&sql)
            .map_err(|e| format!("prepare cooc batch: {e}"))?;
        let mut params_vec: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() * 2);
        for (a, b) in chunk {
            params_vec.push(a);
            params_vec.push(b);
        }
        stmt.execute(rusqlite::params_from_iter(params_vec))
            .map_err(|e| format!("exec cooc batch: {e}"))?;
    }
    Ok(())
}

fn recompute_df_for_tags(
    tx: &rusqlite::Transaction,
    tag_ids: &HashSet<i64>,
) -> Result<(), String> {
    if tag_ids.is_empty() {
        return Ok(());
    }
    // Single set-based UPDATE per chunk of tag ids — much cheaper than the
    // previous per-tag UPDATE issuing a fresh COUNT(*) per row.
    let ids: Vec<i64> = tag_ids.iter().copied().collect();
    for chunk in ids.chunks(500) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "UPDATE tags
                SET df = (SELECT COUNT(*) FROM tags_posts WHERE tag_id = tags.id)
              WHERE id IN ({placeholders})"
        );
        let mut stmt = tx.prepare(&sql).map_err(|e| format!("prep df: {e}"))?;
        let params_vec: Vec<&dyn rusqlite::ToSql> =
            chunk.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
        stmt.execute(rusqlite::params_from_iter(params_vec))
            .map_err(|e| format!("exec df: {e}"))?;
    }
    Ok(())
}

pub fn set_account_tag_cooccurrence(account_id: i32) -> Result<(), String> {
    let mut connection = open_db()?;
    let tx = connection
        .transaction()
        .map_err(|e| format!("Failed to get tx: {e}"))?;

    tx.execute(
        "DELETE FROM account_tag_cooccurrence WHERE account_id = ?1",
        params![account_id],
    )
    .map_err(|e| format!("Failed to clear account tag cooccurrence: {e}"))?;

    tx.execute(
        "
        INSERT INTO account_tag_cooccurrence (
            account_id, tag1_name, tag1_group, tag2_name, tag2_group, cooc_count
        )
        SELECT
            ?1,
            t1.name, t1.group_type,
            t2.name, t2.group_type,
            COUNT(*)
        FROM accounts_post ap
        INNER JOIN tags_posts tp1 ON tp1.post_id = ap.post_id
        INNER JOIN tags_posts tp2
            ON tp2.post_id = ap.post_id
           AND tp1.tag_id < tp2.tag_id
        INNER JOIN tags t1 ON t1.id = tp1.tag_id
        INNER JOIN tags t2 ON t2.id = tp2.tag_id
        WHERE ap.account_id = ?1
        GROUP BY t1.name, t1.group_type, t2.name, t2.group_type
        ",
        params![account_id],
    )
    .map_err(|e| format!("Failed to populate account tag cooccurrence: {e}"))?;

    tx.commit()
        .map_err(|e| format!("Failed to commit account tag cooccurrence: {e}"))?;
    Ok(())
}

pub fn load_global_tag_relation() -> rusqlite::Result<crate::utils::TagRelationGraph> {
    let conn = open_db().expect("open_db failed");
    let n_posts: i64 =
        conn.query_row("SELECT COUNT(*) FROM posts", [], |row| row.get::<_, i64>(0))?;

    let mut graph = crate::utils::TagRelationGraph::with_posts(n_posts);

    let min_cooc = crate::models::cfg().priors.tag_relation_min_cooc.max(1);

    {
        let mut stmt = conn.prepare(
            "
            SELECT t1.name, t1.group_type, t2.name, t2.group_type, c.cooc_count
            FROM tag_cooccurrence c
            INNER JOIN tags t1 ON t1.id = c.tag1_id
            INNER JOIN tags t2 ON t2.id = c.tag2_id
            WHERE c.cooc_count >= ?1
            ",
        )?;
        let rows = stmt.query_map(params![min_cooc], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        for r in rows {
            let (t1, g1, t2, g2, c) = r?;
            if let (Some(gk1), Some(gk2)) = (
                crate::utils::Group::from_str(&g1).map(|g| g as u8),
                crate::utils::Group::from_str(&g2).map(|g| g as u8),
            ) {
                graph.insert_pair(gk1, &t1.to_lowercase(), gk2, &t2.to_lowercase(), c);
            }
        }
    }

    {
        let mut stmt = conn.prepare("SELECT name, group_type, df FROM tags")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<i64>>(2)?.unwrap_or(0),
            ))
        })?;
        for r in rows {
            let (name, g, df) = r?;
            if let Some(g) = crate::utils::Group::from_str(&g) {
                graph.set_marginal(g as u8, &name.to_lowercase(), df);
            }
        }
    }

    Ok(graph)
}

pub fn get_account_tag_relation_graph(
    account_id: i32,
    top: usize,
    min_user_cooc: i64,
) -> Result<TagRelationGraphPayload, String> {
    let conn = open_db()?;

    let catalog_post_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM posts", [], |row| row.get(0))
        .map_err(|e| format!("Failed to count posts: {e}"))?;
    let account_post_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM accounts_post WHERE account_id = ?1",
            params![account_id],
            |row| row.get(0),
        )
        .map_err(|e| format!("Failed to count account posts: {e}"))?;

    let nodes: Vec<TagRelationNode> = {
        let mut stmt = conn
            .prepare(
                "
                SELECT tag_name, group_type, count
                FROM account_tag_counts
                WHERE account_id = ?1
                ORDER BY count DESC, tag_name ASC
                LIMIT ?2
                ",
            )
            .map_err(|e| format!("Failed to prepare top-tags query: {e}"))?;
        stmt.query_map(params![account_id, top as i64], |row| {
            Ok(TagRelationNode {
                name: row.get::<_, String>(0)?,
                group_type: row.get::<_, String>(1)?,
                count: row.get::<_, i64>(2)?,
            })
        })
        .map_err(|e| format!("Failed to fetch top tags: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to enumerate top tags: {e}"))?
    };

    if nodes.is_empty() {
        return Ok(TagRelationGraphPayload {
            nodes,
            edges: Vec::new(),
            catalog_post_count,
            account_post_count,
            scoring: Default::default(),
        });
    }

    let mut node_index: HashMap<(String, String), usize> = HashMap::with_capacity(nodes.len());
    for (i, n) in nodes.iter().enumerate() {
        node_index.insert((n.name.clone(), n.group_type.clone()), i);
    }

    let pair_min = min_user_cooc.max(1);

    let mut stmt = conn
        .prepare(
            "
            SELECT
                atc.tag1_name, atc.tag1_group,
                atc.tag2_name, atc.tag2_group,
                atc.cooc_count,
                COALESCE(c.cooc_count, 0)               AS global_cooc,
                COALESCE(t1.df, 0)                      AS df1,
                COALESCE(t2.df, 0)                      AS df2
            FROM account_tag_cooccurrence atc
            LEFT JOIN tags t1
                   ON t1.name = atc.tag1_name AND t1.group_type = atc.tag1_group
            LEFT JOIN tags t2
                   ON t2.name = atc.tag2_name AND t2.group_type = atc.tag2_group
            LEFT JOIN tag_cooccurrence c
                   ON c.tag1_id = t1.id AND c.tag2_id = t2.id
            WHERE atc.account_id = ?1
              AND atc.cooc_count >= ?2
            ",
        )
        .map_err(|e| format!("Failed to prepare relation graph query: {e}"))?;

    let rows = stmt
        .query_map(params![account_id, pair_min], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
            ))
        })
        .map_err(|e| format!("Failed to fetch relation rows: {e}"))?;

    let n_global = catalog_post_count.max(1) as f32;
    let mut edges: Vec<TagRelationEdge> = Vec::new();

    for row in rows {
        let (t1, g1, t2, g2, user_cooc, global_cooc, df1, df2) =
            row.map_err(|e| format!("Failed to read relation row: {e}"))?;
        let (Some(&i), Some(&j)) = (
            node_index.get(&(t1, g1)),
            node_index.get(&(t2, g2)),
        ) else {
            continue;
        };
        if i == j {
            continue;
        }
        let lift = if global_cooc > 0 && df1 > 0 && df2 > 0 {
            (global_cooc as f32 * n_global) / (df1 as f32 * df2 as f32)
        } else {
            0.0
        };
        edges.push(TagRelationEdge {
            source: i.min(j),
            target: i.max(j),
            user_cooc,
            global_cooc,
            global_lift: lift,
        });
    }

    edges.sort_by(|a, b| {
        b.user_cooc
            .cmp(&a.user_cooc)
            .then_with(|| a.source.cmp(&b.source))
            .then_with(|| a.target.cmp(&b.target))
    });

    Ok(TagRelationGraphPayload {
        nodes,
        edges,
        catalog_post_count,
        account_post_count,
        scoring: Default::default(),
    })
}

pub fn load_account_tag_relation(
    account_id: i32,
    user_tag_counts: &[TagCount],
) -> Result<crate::utils::TagRelationGraph, String> {
    let conn = open_db()?;
    let total_posts: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM accounts_post WHERE account_id = ?1",
            params![account_id],
            |row| row.get(0),
        )
        .map_err(|e| format!("Failed to count account posts: {e}"))?;

    let mut graph = crate::utils::TagRelationGraph::with_posts(total_posts);

    for tc in user_tag_counts {
        if let Some(g) = crate::utils::Group::from_str(&tc.group_type) {
            graph.set_marginal(g as u8, &tc.name.to_lowercase(), tc.count);
        }
    }

    let mut stmt = conn
        .prepare(
            "
            SELECT tag1_name, tag1_group, tag2_name, tag2_group, cooc_count
            FROM account_tag_cooccurrence
            WHERE account_id = ?1
            ",
        )
        .map_err(|e| format!("Failed to prepare account tag cooc query: {e}"))?;

    let rows = stmt
        .query_map(params![account_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })
        .map_err(|e| format!("Failed to execute account tag cooc query: {e}"))?;

    for r in rows {
        let (t1, g1, t2, g2, c) = r.map_err(|e| format!("Failed to read account cooc row: {e}"))?;
        if let (Some(gk1), Some(gk2)) = (
            crate::utils::Group::from_str(&g1).map(|g| g as u8),
            crate::utils::Group::from_str(&g2).map(|g| g as u8),
        ) {
            graph.insert_pair(gk1, &t1.to_lowercase(), gk2, &t2.to_lowercase(), c);
        }
    }

    Ok(graph)
}

pub fn post_count() -> i64 {
    let conn = open_db().expect("open_db failed");
    conn.query_row("SELECT COUNT(*) FROM posts", [], |row| row.get::<_, i64>(0))
        .expect("COUNT(*) query failed")
}

const COOC_BACKFILL_BATCH: i64 = 200;
const COOC_BACKFILL_INTER_BATCH_SLEEP_MS: u64 = 25;

pub fn spawn_tag_cooccurrence_backfill_if_needed() {
    std::thread::Builder::new()
        .name("cooc-backfill".to_string())
        .spawn(|| {
            if let Err(e) = backfill_tag_cooccurrence_if_needed() {
                eprintln!("[cooc-backfill] failed: {e}");
            }
        })
        .expect("spawn cooc-backfill thread");
}

fn backfill_tag_cooccurrence_if_needed() -> Result<(), String> {
    let conn = open_db()?;

    let needs_global = needs_global_backfill(&conn)?;
    let pending_accounts = accounts_needing_cooc(&conn)?;

    if !needs_global && pending_accounts.is_empty() {
        return Ok(());
    }

    drop(conn);

    if needs_global {
        eprintln!("[cooc-backfill] starting global tag co-occurrence backfill");
        backfill_global_tag_cooccurrence()?;
        crate::utils::mark_global_relation_dirty();
        eprintln!("[cooc-backfill] global backfill complete");
    }

    for account_id in pending_accounts {
        eprintln!("[cooc-backfill] backfilling account {account_id}");
        if let Err(e) = set_account_tag_cooccurrence(account_id) {
            eprintln!("[cooc-backfill] account {account_id} failed: {e}");
        }
    }

    eprintln!("[cooc-backfill] all done");
    Ok(())
}

fn needs_global_backfill(conn: &Connection) -> Result<bool, String> {
    let cooc_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM tag_cooccurrence", [], |r| r.get(0))
        .map_err(|e| format!("count tag_cooccurrence: {e}"))?;
    if cooc_rows > 0 {
        return Ok(false);
    }
    let pair_source: i64 = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM tags_posts LIMIT 1)",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map_err(|e| format!("probe tags_posts: {e}"))?;
    Ok(pair_source != 0)
}

fn accounts_needing_cooc(conn: &Connection) -> Result<Vec<i32>, String> {
    let mut stmt = conn
        .prepare(
            "
            SELECT a.id
            FROM accounts a
            WHERE EXISTS(SELECT 1 FROM accounts_post ap WHERE ap.account_id = a.id)
              AND NOT EXISTS(
                    SELECT 1 FROM account_tag_cooccurrence atc
                    WHERE atc.account_id = a.id
              )
            ",
        )
        .map_err(|e| format!("prepare accounts_needing_cooc: {e}"))?;
    let rows = stmt
        .query_map([], |r| r.get::<_, i32>(0))
        .map_err(|e| format!("query accounts_needing_cooc: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("enumerate accounts_needing_cooc: {e}"))
}

fn backfill_global_tag_cooccurrence() -> Result<(), String> {
    let conn = open_db()?;
    let total_posts: i64 = conn
        .query_row(
            "SELECT COUNT(DISTINCT post_id) FROM tags_posts",
            [],
            |r| r.get(0),
        )
        .map_err(|e| format!("count tagged posts: {e}"))?;
    if total_posts == 0 {
        return Ok(());
    }
    drop(conn);

    let mut last_id: i64 = -1;
    let mut processed: i64 = 0;

    loop {
        let mut connection = open_db()?;

        let post_ids: Vec<i64> = {
            let mut stmt = connection
                .prepare(
                    "
                    SELECT post_id FROM (
                        SELECT DISTINCT post_id FROM tags_posts
                        WHERE post_id > ?1
                        ORDER BY post_id ASC
                        LIMIT ?2
                    )
                    ",
                )
                .map_err(|e| format!("prepare post-id batch: {e}"))?;
            stmt.query_map(params![last_id, COOC_BACKFILL_BATCH], |r| r.get(0))
                .map_err(|e| format!("query post-id batch: {e}"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("enumerate post-id batch: {e}"))?
        };

        if post_ids.is_empty() {
            break;
        }

        last_id = *post_ids.last().unwrap();

        let tx = connection
            .transaction()
            .map_err(|e| format!("begin batch tx: {e}"))?;
        {
            let mut upsert = tx
                .prepare_cached(
                    "
                    INSERT INTO tag_cooccurrence (tag1_id, tag2_id, cooc_count) VALUES (?1, ?2, ?3)
                    ON CONFLICT(tag1_id, tag2_id) DO UPDATE SET cooc_count = cooc_count + excluded.cooc_count
                    ",
                )
                .map_err(|e| format!("prep upsert: {e}"))?;
            let mut pairs_for_post = tx
                .prepare_cached(
                    "
                    SELECT tp1.tag_id, tp2.tag_id
                    FROM tags_posts tp1
                    INNER JOIN tags_posts tp2
                        ON tp1.post_id = tp2.post_id
                       AND tp1.tag_id < tp2.tag_id
                    WHERE tp1.post_id = ?1
                    ",
                )
                .map_err(|e| format!("prep pairs_for_post: {e}"))?;

            for pid in &post_ids {
                let pairs = pairs_for_post
                    .query_map(params![pid], |r| {
                        Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
                    })
                    .map_err(|e| format!("query pairs for post {pid}: {e}"))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| format!("collect pairs for post {pid}: {e}"))?;
                for (a, b) in pairs {
                    upsert
                        .execute(params![a, b, 1i64])
                        .map_err(|e| format!("upsert pair {a},{b}: {e}"))?;
                }
            }
        }
        tx.commit().map_err(|e| format!("commit batch: {e}"))?;

        processed += post_ids.len() as i64;
        if processed % 2_000 == 0 || processed >= total_posts {
            eprintln!(
                "[cooc-backfill] global progress: {processed}/{total_posts} posts"
            );
        }

        std::thread::sleep(std::time::Duration::from_millis(
            COOC_BACKFILL_INTER_BATCH_SLEEP_MS,
        ));
    }

    Ok(())
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
