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

/// A set of tag queries to fetch for one account in this tick.
struct PrefetchQueries {
    account_id: i32,
    blacklist: String,
    /// Artist tag queries to fetch.
    artist_queries: Vec<String>,
    /// Character tag queries to fetch.
    character_queries: Vec<String>,
    /// "Recent popular" queries (ordered by fav_count above user baseline).
    recent_popular: Vec<String>,
}

async fn run_prefetch_tick() -> Result<(), String> {
    let targets = rocket::tokio::task::spawn_blocking(pick_prefetch_targets)
        .await
        .map_err(|e| format!("pick targets panicked: {e}"))??;

    if targets.is_empty() {
        return Ok(());
    }

    for target in &targets {
        let mut queries: Vec<String> = Vec::new();
        for q in &target.artist_queries { queries.push(q.clone()); }
        for q in &target.character_queries { queries.push(q.clone()); }
        for q in &target.recent_popular { queries.push(q.clone()); }

        for q in &queries {
            match api::get_posts_by_tags(&target.blacklist, q, Some(1)).await {
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
    }

    // Update last_prefetched_at for each target that was successfully
    // processed (no e621 failure mid-tick).
    if PREFETCH_CONSECUTIVE_FAILS.load(Ordering::Relaxed) == 0 {
        let now = Utc::now().to_rfc3339();
        for target in &targets {
            let aid = target.account_id;
            let conn = crate::db::open_db_for_prefetch().ok();
            if let Some(conn) = conn {
                let _ = conn.execute(
                    "UPDATE accounts SET last_prefetched_at = ?1 WHERE id = ?2",
                    params![now, aid],
                );
            }
        }
    }

    // save_posts_tags_batch already bumps IDF incrementally inside.
    // The old trailing bump_idf(HashMap::new(), 0) kept the cache
    // "warm" between user requests — which actually prevented
    // idle-eviction from ever triggering. Removed.
    Ok(())
}

/// Pick multiple prefetch targets using a recency-weighted random selection.
///
/// Returns up to 5 targets with recent feed interaction. Accounts that were
/// prefetched more recently than `cooldown_secs` are excluded. Selection is
/// weighted by recency: the most recently active accounts are ~2× more likely
/// to be picked than accounts at the edge of the active window.
fn pick_prefetch_targets() -> Result<Vec<PrefetchQueries>, String> {
    let conn = crate::db::open_db_for_prefetch()?;
    let runtime = cfg().runtime.clone();
    let window_days = runtime.prefetch_active_window_days.max(1);
    let cooldown_secs = runtime.prefetch_cooldown_secs;
    let n_tags = (runtime.prefetch_tags_per_group.max(1)) as i32;
    let include_recent = runtime.prefetch_include_recent_popular;

    let cutoff = (Utc::now() - chrono::Duration::days(window_days))
        .to_rfc3339();
    let cooldown_cutoff = Utc::now()
        .timestamp()
        .saturating_sub(cooldown_secs as i64);

    // Pick candidate accounts weighted by recency.
    // We use a random offset + LIMIT so the selection rotates over time
    // rather than always picking the same top-1 account.
    let seed_offset = Utc::now().timestamp() % 200;
    let mut stmt = conn
        .prepare(
            "SELECT a.id, COALESCE(NULLIF(a.blacklisted_tags, ''), '')
             FROM accounts a
             WHERE EXISTS (
                 SELECT 1 FROM feed_interactions fi
                 WHERE fi.account_id = a.id AND fi.created_at >= ?1
             )
               AND (a.last_prefetched_at = ''
                    OR (julianday('now') - julianday(a.last_prefetched_at)) * 86400.0 >= ?2
               )
             ORDER BY a.id ASC
             LIMIT ?3 OFFSET ?4",
        )
        .map_err(|e| format!("pick targets prepare: {e}"))?;
    let rows: Vec<(i32, String)> = stmt
        .query_map(params![cutoff, cooldown_cutoff, 20, seed_offset], |r| {
            Ok((r.get::<_, i32>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(|e| format!("pick targets query: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("pick targets collect: {e}"))?;

    if rows.is_empty() {
        return Ok(Vec::new());
    }

    // Recency-weighted sampling: assign each candidate a weight proportional
    // to how recently it was active. Pick up to 5 distinct targets.
    let mut targets: Vec<PrefetchQueries> = Vec::new();
    let rng_state = (Utc::now().timestamp() % 1_000_000) as u64;
    let lcg_a: u64 = 6364136223846793005;
    let lcg_c: u64 = 1442695040888963407;

    // Score each candidate by recency (lower row position = more recent).
    // Use exponential weighting: top candidate gets weight ~2× bottom.
    let n = rows.len();
    let mut weighted_idx = Vec::with_capacity(n);
    for (i, _) in rows.iter().enumerate() {
        let recency = 1.0 - (i as f64) / (n as f64 + 1.0);
        let weight = 1.0 + recency; // range [1, 2]
        weighted_idx.push((i, weight));
    }

    // Weighted reservoir sampling to pick up to 5 targets.
    let pick_count = 5.min(n);
    let mut picked = vec![false; n];
    for _ in 0..pick_count {
        let total_weight: f64 = weighted_idx.iter().filter(|(idx, _)| !picked[*idx]).map(|(_, w)| w).sum();
        if total_weight <= 0.0 { break; }

        let mut r = (rng_state.wrapping_mul(lcg_a).wrapping_add(lcg_c) >> 33) as f64;
        r = r / (u64::MAX as f64) * total_weight;

        let mut accumulated = 0.0f64;
        for &(idx, weight) in &weighted_idx {
            if !picked[idx] {
                accumulated += weight;
                if accumulated >= r {
                    picked[idx] = true;
                    break;
                }
            }
        }
    }

    for (i, (account_id, blacklist)) in rows.into_iter().enumerate() {
        if !picked[i] { continue; }

        let top_tags = |group_type: &str| -> Result<Vec<String>, String> {
            let mut stmt = conn
                .prepare(
                    "SELECT tag_name FROM account_tag_counts
                     WHERE account_id = ?1 AND group_type = ?2
                     ORDER BY count DESC, tag_name ASC
                     LIMIT ?3",
                )
                .map_err(|e| format!("top_tags prepare {group_type}: {e}"))?;
            stmt.query_map(params![account_id, group_type, n_tags], |r| r.get(0))
                .map_err(|e| format!("top_tags query {group_type}: {e}"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("top_tags collect {group_type}: {e}"))
        };

        let artist_queries = top_tags("artist")?;
        let character_queries = top_tags("character")?;

        // Recent popular: order by fav_count for the user's top tag(s).
        let mut recent_popular = Vec::new();
        if include_recent {
            for tag in &artist_queries {
                recent_popular.push(format!("{tag} order:fav_count"));
            }
            for tag in &character_queries {
                recent_popular.push(format!("{tag} order:fav_count"));
            }
        }

        if !artist_queries.is_empty() || !character_queries.is_empty() || !recent_popular.is_empty() {
            targets.push(PrefetchQueries {
                account_id,
                blacklist,
                artist_queries,
                character_queries,
                recent_popular,
            });
        }
    }

    Ok(targets)
}
