//! One-shot tag-cooccurrence backfill — populates `tag_cooccurrence` and
//! `account_tag_cooccurrence` from `tags_posts` / `accounts_post` for DBs
//! that predate V9. Spawned at server start and exits when nothing's left
//! to do; see [`super::cooccurrence`] for the live-update path.
//!
//! The global pass is resumable: it walks `tags_posts` in ascending `post_id`
//! and records the highest fully-counted id in
//! `tag_cooccurrence_backfill.last_post_id` (V30). See
//! [`global_cooc_watermark`] for why a plain row count could not be used.

use rusqlite::{Connection, params};

use super::cooccurrence::set_account_tag_cooccurrence;
use super::open_db;

/// Posts per write transaction. At 25 the 1.6-billion-pair build paid ~26k
/// commits; 200 cuts that 8x while keeping the WAL per commit near 25 MiB.
/// Resume granularity becomes this many posts — seconds of rework.
const COOC_BACKFILL_BATCH: i64 = 200;
/// Yields to concurrent `/process` writers between batches. Negligible for the
/// build (~16 s across the whole park) but stops a boot-time backfill from
/// starving a live server.
const COOC_BACKFILL_INTER_BATCH_SLEEP_MS: u64 = 5;

pub fn spawn_tag_cooccurrence_backfill_if_needed() {
    std::thread::Builder::new()
        .name("cooc-backfill".to_string())
        .spawn(|| {
            if let Err(e) = run_tag_cooccurrence_backfill_if_needed() {
                error!("[cooc-backfill] failed: {e}");
            }
        })
        .expect("spawn cooc-backfill thread");
}

/// Synchronous entry point, for callers that want to wait for the graph to be
/// rebuilt (e.g. `calibrate graph` after a fresh `seed`).
///
/// This is the only way to rebuild `tag_cooccurrence` from `tags_posts`
/// without booting the server: `seed` writes with `track_cooccurrence =
/// false`, so a wiped-then-seeded DB has an **empty** global tag-relation
/// graph, and calibrating against it silently measures every `tag_relation_*`
/// knob as inert. See `db::mod.rs` for the boot-time caller.
pub fn run_tag_cooccurrence_backfill_if_needed() -> Result<(), String> {
    let started = std::time::Instant::now();
    // Announce the thread before touching the database. A silent early return
    // used to be indistinguishable from a thread that never started: a
    // database holding hundreds of accounts with no `account_tag_cooccurrence`
    // rows produced no `[cooc-backfill]` output whatsoever, so there was no way
    // to tell "nothing to do" from "died before the first check".
    info!("[cooc-backfill] checking whether a backfill is needed");
    let conn = open_db().map_err(|e| format!("open a connection: {e}"))?;

    let needs_global = needs_global_backfill(&conn)?;
    let pending_accounts = accounts_needing_cooc(&conn)?;
    info!(
        "[cooc-backfill] global={needs_global}, account-level pending={}, probe took {:.2}s",
        pending_accounts.len(),
        started.elapsed().as_secs_f32()
    );

    if !needs_global && pending_accounts.is_empty() {
        info!("[cooc-backfill] nothing to do");
        return Ok(());
    }

    drop(conn);

    // The backfill and a concurrent `/process` job both increment the SAME
    // `tag_cooccurrence` / `account_tag_cooccurrence` pairs from the shared
    // `tags_posts` state. Running them at the same time double-counts those
    // pairs until an operator rebuilds. If any process job is live, defer the
    // one-shot backfill to the next restart (it re-checks the tables then).
    if crate::jobs::any_running() {
        info!("[cooc-backfill] skipping: a /process job is running; will retry next boot");
        return Ok(());
    }

    if needs_global {
        info!("[cooc-backfill] starting global tag co-occurrence backfill");
        backfill_global_tag_cooccurrence()?;
        crate::utils::mark_global_relation_dirty();
        info!("[cooc-backfill] global backfill complete");
    }

    let total = pending_accounts.len();
    let mut failed = 0usize;
    for (i, account_id) in pending_accounts.into_iter().enumerate() {
        info!("[cooc-backfill] account {account_id}: {}/{total}", i + 1);
        if let Err(e) = set_account_tag_cooccurrence(account_id) {
            failed += 1;
            error!("[cooc-backfill] account {account_id} failed: {e}");
        }
    }

    if failed == 0 {
        info!(
            "[cooc-backfill] all done: {total} account(s) in {:.1}s",
            started.elapsed().as_secs_f32()
        );
    } else {
        warn!(
            "[cooc-backfill] done: {failed}/{total} account(s) failed after {:.1}s",
            started.elapsed().as_secs_f32()
        );
    }
    Ok(())
}

/// Global `tag_cooccurrence` only — the per-account rebuild is skipped.
///
/// `calibrate graph` uses this when MMR is off: the grid reads the *global*
/// graph (`tag_relation_*`), while `account_tag_cooccurrence` feeds MMR and the
/// taste profile only.
pub fn run_global_tag_cooccurrence_backfill() -> Result<(), String> {
    let conn = open_db()?;
    let needs_global = needs_global_backfill(&conn)?;
    drop(conn);

    if !needs_global {
        info!("[cooc-backfill] global graph is already complete; nothing to do");
        return Ok(());
    }
    // Same double-count hazard as the full entry point: a concurrent
    // `/process` job increments the same pairs from the shared `tags_posts`.
    if crate::jobs::any_running() {
        info!("[cooc-backfill] skipping global backfill: a /process job is running");
        return Ok(());
    }

    info!("[cooc-backfill] starting global tag co-occurrence backfill");
    backfill_global_tag_cooccurrence()?;
    crate::utils::mark_global_relation_dirty();
    info!("[cooc-backfill] global backfill complete");
    Ok(())
}

/// Highest `post_id` whose tag pairs are fully counted, or `-1` when nothing
/// has been counted yet.
///
/// This replaced a `COUNT(*) FROM tag_cooccurrence > 0` test, which could not
/// tell a finished graph from an interrupted one: the build runs for hours,
/// and a crash leaves the table NON-empty but holding only the pairs of the
/// lowest post ids. A row count therefore read as "already built", the pass
/// was skipped, and `calibrate` went on to measure every `tag_relation_*`
/// knob against a truncated graph — silently, since nothing had errored.
///
/// Falls back to the pre-V30 reading when the state table or its row is
/// absent: a non-empty `tag_cooccurrence` means built, so pin the watermark at
/// the newest post. Keeping that interpretation is deliberate — treating every
/// existing install as unfinished would push it through a multi-hour rebuild
/// on upgrade.
fn global_cooc_watermark(conn: &Connection) -> Result<i64, String> {
    let has_state: i64 = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type = 'table' AND name = 'tag_cooccurrence_backfill')",
            [],
            |r| r.get(0),
        )
        .map_err(|e| format!("probe tag_cooccurrence_backfill: {e}"))?;

    if has_state != 0 {
        match conn.query_row(
            "SELECT last_post_id FROM tag_cooccurrence_backfill WHERE id = 1",
            [],
            |r| r.get::<_, i64>(0),
        ) {
            Ok(watermark) => return Ok(watermark),
            Err(rusqlite::Error::QueryReturnedNoRows) => {}
            Err(e) => return Err(format!("read cooc backfill watermark: {e}")),
        }
    }

    let cooc_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM tag_cooccurrence", [], |r| r.get(0))
        .map_err(|e| format!("count tag_cooccurrence: {e}"))?;
    if cooc_rows == 0 {
        return Ok(-1);
    }
    conn.query_row(
        "SELECT COALESCE(MAX(post_id), -1) FROM tags_posts",
        [],
        |r| r.get::<_, i64>(0),
    )
    .map_err(|e| format!("read max post_id: {e}"))
}

/// Advance the watermark. Call it with the [`rusqlite::Transaction`] that is
/// committing the pairs being accounted for, never on its own: the two must
/// share a commit, or a crash between them either loses a batch (watermark
/// ahead) or double-counts one (watermark behind).
///
/// Upserts rather than updates so a database whose state row was somehow
/// removed can still heal itself.
fn set_global_cooc_watermark(tx: &Connection, post_id: i64) -> Result<(), String> {
    tx.execute(
        "INSERT INTO tag_cooccurrence_backfill (id, last_post_id) VALUES (1, ?1)
         ON CONFLICT(id) DO UPDATE SET last_post_id = excluded.last_post_id",
        params![post_id],
    )
    .map_err(|e| format!("advance cooc watermark to {post_id}: {e}"))?;
    Ok(())
}

fn needs_global_backfill(conn: &Connection) -> Result<bool, String> {
    let watermark = global_cooc_watermark(conn)?;
    let more: i64 = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM tags_posts WHERE post_id > ?1)",
            params![watermark],
            |r| r.get(0),
        )
        .map_err(|e| format!("probe tags_posts beyond watermark: {e}"))?;
    Ok(more != 0)
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
    let has_posts: i64 = conn
        .query_row("SELECT EXISTS(SELECT 1 FROM tags_posts)", [], |r| r.get(0))
        .map_err(|e| format!("probe tagged posts: {e}"))?;
    if has_posts == 0 {
        return Ok(());
    }
    // Resume at the watermark: everything at or below it was committed by an
    // earlier pass. `-1` when the graph has never been built.
    let mut last_id: i64 = global_cooc_watermark(&conn)?;
    drop(conn);

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
            // Committed together with the pairs above — see the note on
            // `set_global_cooc_watermark`.
            set_global_cooc_watermark(tx, last_id)?;
            Ok(())
        })?;

        processed += post_ids.len() as i64;
        if processed % 2_000 == 0 {
            info!(
                "[cooc-backfill] global progress: {processed} posts this pass, watermark={last_id}"
            );
        }

        std::thread::sleep(std::time::Duration::from_millis(
            COOC_BACKFILL_INTER_BATCH_SLEEP_MS,
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal schema: only what the watermark logic reads. The state table is
    /// optional so a pre-V30 database (and a test schema) can be exercised.
    fn conn_with_tables(state_table: bool) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        let mut sql = String::from(
            "CREATE TABLE tags_posts (tag_id INTEGER NOT NULL, post_id INTEGER NOT NULL);
             CREATE TABLE tag_cooccurrence (
                tag1_id INTEGER NOT NULL,
                tag2_id INTEGER NOT NULL,
                cooc_count INTEGER NOT NULL,
                PRIMARY KEY (tag1_id, tag2_id)
             );",
        );
        if state_table {
            sql.push_str(
                "CREATE TABLE tag_cooccurrence_backfill (
                    id INTEGER PRIMARY KEY CHECK (id = 1),
                    last_post_id INTEGER NOT NULL DEFAULT -1
                 ) STRICT;
                 INSERT INTO tag_cooccurrence_backfill (id, last_post_id) VALUES (1, -1);",
            );
        }
        conn.execute_batch(&sql).unwrap();
        conn
    }

    fn tag_post(conn: &Connection, post_id: i64, tag_ids: &[i64]) {
        for tag_id in tag_ids {
            conn.execute(
                "INSERT INTO tags_posts (tag_id, post_id) VALUES (?1, ?2)",
                rusqlite::params![tag_id, post_id],
            )
            .unwrap();
        }
    }

    fn count_pairs(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM tag_cooccurrence", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn fresh_database_starts_before_the_first_post() {
        let conn = conn_with_tables(true);
        assert_eq!(global_cooc_watermark(&conn).unwrap(), -1);
        assert!(
            !needs_global_backfill(&conn).unwrap(),
            "nothing to count yet"
        );

        tag_post(&conn, 10, &[1, 2]);
        assert!(needs_global_backfill(&conn).unwrap());
    }

    #[test]
    fn an_interrupted_build_is_not_mistaken_for_a_finished_one() {
        // The regression the watermark exists for: a half-built graph is
        // NON-empty, and the pre-V30 `COUNT(*) > 0` test read that as "built".
        let conn = conn_with_tables(true);
        tag_post(&conn, 10, &[1, 2]);
        tag_post(&conn, 20, &[1, 3]);
        tag_post(&conn, 30, &[2, 3]);

        // A pass that reached post 10 and then died.
        conn.execute(
            "INSERT INTO tag_cooccurrence (tag1_id, tag2_id, cooc_count) VALUES (1, 2, 1)",
            [],
        )
        .unwrap();
        set_global_cooc_watermark(&conn, 10).unwrap();

        assert_eq!(
            count_pairs(&conn),
            1,
            "non-empty — the old check stops here"
        );
        assert!(
            needs_global_backfill(&conn).unwrap(),
            "posts 20 and 30 are still uncounted"
        );

        set_global_cooc_watermark(&conn, 30).unwrap();
        assert!(!needs_global_backfill(&conn).unwrap());
    }

    #[test]
    fn resume_starts_strictly_past_the_watermark() {
        // `>` and not `>=`: re-reading the watermark's own post would
        // double-count every pair the interrupted batch already wrote.
        let conn = conn_with_tables(true);
        tag_post(&conn, 10, &[1, 2]);
        tag_post(&conn, 20, &[1, 3]);
        set_global_cooc_watermark(&conn, 10).unwrap();

        let next: i64 = conn
            .query_row(
                "SELECT MIN(post_id) FROM tags_posts WHERE post_id > ?1",
                rusqlite::params![global_cooc_watermark(&conn).unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(next, 20);
    }

    #[test]
    fn pre_v30_database_with_a_populated_graph_is_left_alone() {
        // No state table, rows present: built by the old code. Rebuilding a
        // multi-hour graph on upgrade is not an acceptable cost, so the old
        // reading is kept — pinned at the newest post.
        let conn = conn_with_tables(false);
        tag_post(&conn, 10, &[1, 2]);
        tag_post(&conn, 20, &[1, 3]);
        conn.execute(
            "INSERT INTO tag_cooccurrence (tag1_id, tag2_id, cooc_count) VALUES (1, 2, 1)",
            [],
        )
        .unwrap();

        assert_eq!(global_cooc_watermark(&conn).unwrap(), 20);
        assert!(!needs_global_backfill(&conn).unwrap());
    }

    #[test]
    fn pre_v30_database_with_an_empty_graph_still_backfills() {
        let conn = conn_with_tables(false);
        tag_post(&conn, 10, &[1, 2]);
        assert_eq!(global_cooc_watermark(&conn).unwrap(), -1);
        assert!(needs_global_backfill(&conn).unwrap());
    }

    #[test]
    fn missing_state_row_falls_back_instead_of_dead_ending() {
        let conn = conn_with_tables(true);
        conn.execute("DELETE FROM tag_cooccurrence_backfill", [])
            .unwrap();
        tag_post(&conn, 7, &[1, 2]);
        assert_eq!(global_cooc_watermark(&conn).unwrap(), -1);

        // ...and writing recreates the row rather than failing on it.
        set_global_cooc_watermark(&conn, 7).unwrap();
        assert_eq!(global_cooc_watermark(&conn).unwrap(), 7);
        assert!(!needs_global_backfill(&conn).unwrap());
    }
}
