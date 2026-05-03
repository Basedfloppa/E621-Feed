//! Background workers that grow and trim the local catalog.
//!
//! Two long-running threads are spawned at startup:
//!
//! * `catalog-prefetch` — every PREFETCH_INTERVAL picks a recently-active
//!   account, queries e621 for its top artist + top character, and upserts the
//!   resulting posts into the catalog so later `/recommendations` calls have
//!   richer local candidates.
//! * `catalog-cleanup`  — every CLEANUP_INTERVAL deletes catalog posts that
//!   neither belong to a user's favourites nor have been served by the
//!   recommendations endpoint within the cleanup window. Stops the catalog
//!   from growing without bound.
//!
//! Both workers use `tokio::spawn` so they share the global `RATE_GATE` in
//! `api.rs`, ensuring background traffic doesn't eat into the live request
//! budget.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use chrono::Utc;
use rusqlite::params;

use crate::api;
use crate::db;
use crate::models::cfg;
use crate::utils::{bump_idf, mark_global_relation_dirty};

static PREFETCH_CONSECUTIVE_FAILS: AtomicU32 = AtomicU32::new(0);

pub fn spawn_prefetch_workers() {
    rocket::tokio::spawn(prefetch_loop());
    rocket::tokio::spawn(cleanup_loop());
}

async fn prefetch_loop() {
    // Initial delay so we don't compete with startup work (migrations,
    // tag-cooccurrence backfill).
    rocket::tokio::time::sleep(Duration::from_secs(60)).await;

    loop {
        let runtime = cfg().runtime.clone();
        let threshold = runtime.prefetch_breaker_threshold;
        let fails = PREFETCH_CONSECUTIVE_FAILS.load(Ordering::Relaxed);

        if threshold > 0 && fails >= threshold {
            let pause = runtime.prefetch_breaker_pause_secs.max(60);
            warn!(
                "[catalog-prefetch] circuit breaker open after {fails} consecutive failures \
                 (threshold {threshold}); pausing {pause}s before resuming"
            );
            rocket::tokio::time::sleep(Duration::from_secs(pause)).await;
            PREFETCH_CONSECUTIVE_FAILS.store(0, Ordering::Relaxed);
            info!("[catalog-prefetch] circuit breaker reset, resuming");
            continue;
        }

        if let Err(e) = run_prefetch_tick().await {
            warn!("[catalog-prefetch] tick failed: {e}");
        }
        let secs = runtime.prefetch_interval_secs.max(10);
        rocket::tokio::time::sleep(Duration::from_secs(secs)).await;
    }
}

async fn cleanup_loop() {
    // Wait longer on first boot — let the catalog accumulate before we start
    // pruning anything.
    rocket::tokio::time::sleep(Duration::from_secs(3600)).await;

    loop {
        match rocket::tokio::task::spawn_blocking(run_catalog_cleanup).await {
            Ok(Ok(deleted)) if deleted > 0 => {
                info!("[catalog-cleanup] deleted {deleted} stale catalog posts");
            }
            Ok(Err(e)) => warn!("[catalog-cleanup] failed: {e}"),
            Err(e) => warn!("[catalog-cleanup] task panicked: {e}"),
            _ => {}
        }

        match rocket::tokio::task::spawn_blocking(db::cleanup_orphan_accounts).await {
            Ok(Ok(deleted)) if deleted > 0 => {
                info!("[account-cleanup] dropped {deleted} orphan accounts");
            }
            Ok(Err(e)) => warn!("[account-cleanup] failed: {e}"),
            Err(e) => warn!("[account-cleanup] task panicked: {e}"),
            _ => {}
        }

        let secs = cfg().runtime.cleanup_interval_secs.max(60);
        rocket::tokio::time::sleep(Duration::from_secs(secs)).await;
    }
}

struct PrefetchTarget {
    account_id: i32,
    blacklist: String,
    top_artist: Option<String>,
    top_character: Option<String>,
}

async fn run_prefetch_tick() -> Result<(), String> {
    let target = rocket::tokio::task::spawn_blocking(pick_prefetch_target)
        .await
        .map_err(|e| format!("pick target panicked: {e}"))??;
    let Some(target) = target else { return Ok(()) };

    let mut queries: Vec<String> = Vec::new();
    if let Some(t) = target.top_artist.as_ref() {
        queries.push(t.clone());
    }
    if let Some(t) = target.top_character.as_ref() {
        queries.push(t.clone());
    }

    for q in queries {
        match api::get_posts_by_tags(&target.blacklist, &q, Some(0)).await {
            Ok(posts) if !posts.is_empty() => {
                // Successful fetch — reset the breaker.
                PREFETCH_CONSECUTIVE_FAILS.store(0, Ordering::Relaxed);

                let posts_for_persist = posts.clone();
                let res = rocket::tokio::task::spawn_blocking(move || -> Result<(), String> {
                    db::upsert_catalog_posts(&posts_for_persist)
                        .map_err(|e| format!("upsert: {e}"))?;
                    db::save_posts_tags_batch(
                        &posts_for_persist,
                        &std::collections::HashSet::new(),
                        false,
                    )
                    .map_err(|e| format!("tags: {e}"))?;
                    Ok(())
                })
                .await
                .map_err(|e| format!("persist join: {e}"))?;
                if let Err(e) = res {
                    warn!(
                        "[catalog-prefetch] persist failed for account={} q={q}: {e}",
                        target.account_id
                    );
                }
            }
            Ok(_) => {}
            Err(e) => {
                // Count this failure and bail out of the rest of the tick.
                let n = PREFETCH_CONSECUTIVE_FAILS.fetch_add(1, Ordering::Relaxed) + 1;
                warn!(
                    "[catalog-prefetch] e621 fetch failed for account={} q={q} \
                     (consecutive_fails={n}): {e}",
                    target.account_id
                );
                break;
            }
        }
    }

    // No-op if save_posts_tags_batch already bumped IDF; this nudge covers
    // the case where the second e621 call introduced new tags after the
    // first batch's bump landed.
    bump_idf(std::collections::HashMap::new(), 0);
    Ok(())
}

fn pick_prefetch_target() -> Result<Option<PrefetchTarget>, String> {
    let conn = crate::db::open_db_for_prefetch()?;
    let cutoff = (Utc::now() - chrono::Duration::days(cfg().runtime.prefetch_active_window_days))
        .to_rfc3339();

    // Pick the account whose feed has been most recently active. Ties broken
    // by lowest id for determinism.
    let row: Option<(i32, String)> = match conn.query_row(
        "
        SELECT a.id, COALESCE(NULLIF(a.blacklisted_tags, ''), '')
        FROM accounts a
        WHERE EXISTS (
            SELECT 1 FROM feed_interactions fi
            WHERE fi.account_id = a.id AND fi.created_at >= ?1
        )
        ORDER BY (
            SELECT MAX(created_at) FROM feed_interactions fi
            WHERE fi.account_id = a.id
        ) DESC,
        a.id ASC
        LIMIT 1
        ",
        params![cutoff],
        |r| Ok((r.get::<_, i32>(0)?, r.get::<_, String>(1)?)),
    ) {
        Ok(t) => Some(t),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => return Err(format!("pick target query: {e}")),
    };

    let Some((account_id, blacklist)) = row else {
        return Ok(None);
    };

    let top = |group_type: &str| -> Result<Option<String>, String> {
        conn.query_row(
            "
            SELECT tag_name FROM account_tag_counts
            WHERE account_id = ?1 AND group_type = ?2
            ORDER BY count DESC, tag_name ASC LIMIT 1
            ",
            params![account_id, group_type],
            |r| r.get::<_, String>(0),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(format!("top {group_type} query: {other}")),
        })
    };

    Ok(Some(PrefetchTarget {
        account_id,
        blacklist,
        top_artist: top("artist")?,
        top_character: top("character")?,
    }))
}

fn run_catalog_cleanup() -> Result<i64, String> {
    let conn = crate::db::open_db_for_prefetch()?;
    let cutoff =
        (Utc::now() - chrono::Duration::days(cfg().runtime.catalog_retention_days)).to_rfc3339();

    // Delete posts that:
    //   1) aren't anyone's favourite (no row in accounts_post)
    //   2) haven't been re-touched recently (last_seen_at < cutoff)
    // CASCADE on tags_posts/posts FKs handles the link table cleanup.
    let deleted = conn
        .execute(
            "
            DELETE FROM posts
            WHERE last_seen_at < ?1
              AND NOT EXISTS (SELECT 1 FROM accounts_post ap WHERE ap.post_id = posts.id)
            ",
            params![cutoff],
        )
        .map_err(|e| format!("delete stale posts: {e}"))?;

    if deleted > 0 {
        // Tag DFs are now stale by exactly the deleted-post count per tag.
        // Schedule a full IDF rebuild rather than tracking the delta — cleanup
        // runs every 6h, so a one-off rebuild is cheap and avoids walking
        // tags_posts again here.
        crate::utils::mark_idf_dirty();
        mark_global_relation_dirty();
    }

    Ok(deleted as i64)
}
