//! One-shot tag-cooccurrence backfill — populates `tag_cooccurrence` and
//! `account_tag_cooccurrence` from `tags_posts` / `accounts_post` for DBs
//! that predate V9. Spawned at server start and exits when nothing's left
//! to do; see [`super::cooccurrence`] for the live-update path.

use rusqlite::{params, Connection};

use super::cooccurrence::set_account_tag_cooccurrence;
use super::open_db;

const COOC_BACKFILL_BATCH: i64 = 25;
const COOC_BACKFILL_INTER_BATCH_SLEEP_MS: u64 = 25;

pub fn spawn_tag_cooccurrence_backfill_if_needed() {
    std::thread::Builder::new()
        .name("cooc-backfill".to_string())
        .spawn(|| {
            if let Err(e) = backfill_tag_cooccurrence_if_needed() {
                error!("[cooc-backfill] failed: {e}");
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
        info!("[cooc-backfill] starting global tag co-occurrence backfill");
        backfill_global_tag_cooccurrence()?;
        crate::utils::mark_global_relation_dirty();
        info!("[cooc-backfill] global backfill complete");
    }

    for account_id in pending_accounts {
        info!("[cooc-backfill] backfilling account {account_id}");
        if let Err(e) = set_account_tag_cooccurrence(account_id) {
            error!("[cooc-backfill] account {account_id} failed: {e}");
        }
    }

    info!("[cooc-backfill] all done");
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
        .query_row("SELECT EXISTS(SELECT 1 FROM tags_posts LIMIT 1)", [], |r| {
            r.get::<_, i64>(0)
        })
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
        .query_row("SELECT COUNT(DISTINCT post_id) FROM tags_posts", [], |r| {
            r.get(0)
        })
        .map_err(|e| format!("count tagged posts: {e}"))?;
    if total_posts == 0 {
        return Ok(());
    }
    drop(conn);

    let mut last_id: i64 = -1;
    let mut processed: i64 = 0;

    loop {
        // Read post-ids on a pool connection so we don't hold the writer
        // mutex across the SELECT.
        let post_ids: Vec<i64> = {
            let connection = open_db()?;
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

        super::with_write_tx(|tx| {
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
            Ok(())
        })?;

        processed += post_ids.len() as i64;
        if processed % 2_000 == 0 || processed >= total_posts {
            info!("[cooc-backfill] global progress: {processed}/{total_posts} posts");
        }

        std::thread::sleep(std::time::Duration::from_millis(
            COOC_BACKFILL_INTER_BATCH_SLEEP_MS,
        ));
    }

    Ok(())
}
