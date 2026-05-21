//! Background worker that grows the local catalog by fetching top-artist /
//! top-character posts for recently active accounts.
//!
//! Uses `tokio::spawn` so it shares the global `RATE_GATE` in `api.rs`,
//! ensuring background traffic doesn't eat into the live request budget.
//!
//! Catalog cleanup (prune stale posts, orphan accounts) and WAL checkpoint
//! are handled by `cache_pruner.rs` — see that module for the unified
//! background maintenance tick.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use chrono::Utc;
use rusqlite::params;

use crate::api;
use crate::db;
use crate::models::cfg;

static PREFETCH_CONSECUTIVE_FAILS: AtomicU32 = AtomicU32::new(0);

pub fn spawn_prefetch_workers() {
    rocket::tokio::spawn(prefetch_loop());
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
        match api::get_posts_by_tags(&target.blacklist, &q, Some(1)).await {
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
                        None,
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

    // save_posts_tags_batch already bumps IDF incrementally inside.
    // The old trailing bump_idf(HashMap::new(), 0) kept the cache
    // "warm" between user requests — which actually prevented
    // idle-eviction from ever triggering. Removed.
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

