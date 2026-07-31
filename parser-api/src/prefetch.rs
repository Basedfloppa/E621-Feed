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

// ── LCG constants for deterministic weighted sampling ─────────────────

/// Linear Congruential Generator constants (same as glibc's TYPE_0).
const LCG_A: u64 = 6364136223846793005;
const LCG_C: u64 = 1442695040888963407;

static PREFETCH_CONSECUTIVE_FAILS: AtomicU32 = AtomicU32::new(0);

pub fn spawn_prefetch_workers() {
    let hot_window = cfg().runtime.prefetch_hot_window_hours;
    // Hot worker: active accounts, short interval.
    rocket::tokio::spawn(prefetch_loop(
        "hot",
        |r| r.prefetch_interval_secs.max(10),
        move |_| hot_window,
        hot_window,
        3,
        0, // no exclusion (hot window = itself)
    ));
    // Cold worker: dormant accounts, long interval.
    let cold_window = cfg().runtime.prefetch_active_window_days as u64 * 24;
    rocket::tokio::spawn(prefetch_loop(
        "cold",
        |r| r.prefetch_cold_interval_secs.max(30),
        move |_| cold_window,
        cold_window,
        2,
        hot_window, // exclude accounts in the hot window
    ));
}

async fn prefetch_loop<F1, F2>(
    name: &'static str,
    interval_fn: F1,
    _window_fn: F2,
    window_hours: u64,
    max_targets: usize,
    exclude_window_hours: u64,
) where
    F1: Fn(&crate::models::RuntimeConfig) -> u64,
    F2: Fn(&crate::models::RuntimeConfig) -> u64,
{
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
                "[catalog-prefetch:{name}] circuit breaker open after {fails} consecutive failures \
                 (threshold {threshold}); pausing {pause}s before resuming"
            );
            rocket::tokio::time::sleep(Duration::from_secs(pause)).await;
            PREFETCH_CONSECUTIVE_FAILS.store(0, Ordering::Relaxed);
            info!("[catalog-prefetch:{name}] circuit breaker reset, resuming");
            continue;
        }

        if let Err(e) =
            run_prefetch_tick(name, window_hours, max_targets, exclude_window_hours).await
        {
            warn!("[catalog-prefetch:{name}] tick failed: {e}");
        }
        let secs = interval_fn(&runtime);
        rocket::tokio::time::sleep(Duration::from_secs(secs)).await;
    }
}

/// A set of tag queries to fetch for one account in this tick.
pub struct PrefetchQueries {
    pub account_id: i32,
    blacklist: String,
    /// Artist tag queries to fetch.
    artist_queries: Vec<String>,
    /// Character tag queries to fetch.
    character_queries: Vec<String>,
    /// "Recent popular" queries (ordered by fav_count above user baseline).
    recent_popular: Vec<String>,
}

async fn run_prefetch_tick(
    name: &str,
    window_hours: u64,
    max_targets: usize,
    exclude_window_hours: u64,
) -> Result<(), String> {
    let targets = rocket::tokio::task::spawn_blocking(move || {
        pick_prefetch_targets(window_hours, max_targets, exclude_window_hours)
    })
    .await
    .map_err(|e| format!("pick targets panicked: {e}"))??;

    if targets.is_empty() {
        return Ok(());
    }

    for target in &targets {
        let mut queries: Vec<String> = Vec::new();
        for q in &target.artist_queries {
            queries.push(q.clone());
        }
        for q in &target.character_queries {
            queries.push(q.clone());
        }
        for q in &target.recent_popular {
            queries.push(q.clone());
        }

        for q in &queries {
            match api::get_posts_by_tags(&target.blacklist, q, Some(1), None).await {
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
                            "[catalog-prefetch:{name}] persist failed for account={} q={q}: {e}",
                            target.account_id
                        );
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    // Count this failure and bail out of the rest of the tick.
                    let n = PREFETCH_CONSECUTIVE_FAILS.fetch_add(1, Ordering::Relaxed) + 1;
                    warn!(
                        "[catalog-prefetch:{name}] e621 fetch failed for account={} q={q} \
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
/// Pick multiple prefetch targets using a recency-weighted random selection.
///
/// `window_hours` — only accounts with a feed interaction within this many
/// hours are considered (e.g. 48 h for hot, `14 * 24` h for cold).
///
/// `max_targets` — how many accounts to pick (3 for hot, 2 for cold).
///
/// `exclude_window_hours` — if > 0, accounts with ANY interaction within
/// this many hours are EXCLUDED from the candidate set (used by the cold
/// worker to avoid picking accounts the hot worker already covers). Pass 0
/// when no exclusion is needed.
///
/// Accounts that were prefetched more recently than `prefetch_cooldown_secs`
/// are excluded. Selection is weighted by recency: the most recently active
/// accounts are ~2× more likely to be picked.
///
/// Public for deterministic integration tests; the scheduled workers are
/// its production caller.
pub fn pick_prefetch_targets(
    window_hours: u64,
    max_targets: usize,
    exclude_window_hours: u64,
) -> Result<Vec<PrefetchQueries>, String> {
    let conn = crate::db::open_db_for_prefetch()?;
    let runtime = cfg().runtime.clone();
    let cooldown_secs = runtime.prefetch_cooldown_secs;
    let n_tags = (runtime.prefetch_tags_per_group.max(1)) as i32;
    let include_recent = runtime.prefetch_include_recent_popular;

    let cutoff = (Utc::now() - chrono::Duration::hours(window_hours.max(1) as i64)).to_rfc3339();
    let cooldown_cutoff = Utc::now().timestamp().saturating_sub(cooldown_secs as i64);
    let hot_cutoff = if exclude_window_hours > 0 {
        Some((Utc::now() - chrono::Duration::hours(exclude_window_hours as i64)).to_rfc3339())
    } else {
        None
    };

    // Build the SQL dynamically. Parameter numbering:
    //   ?1 = cutoff (interaction window)
    //   ?2 = cooldown_cutoff
    //   ?3 = hot_cutoff (only when exclude_window_hours > 0)
    // LIMIT and OFFSET are hardcoded in the SQL string.
    let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> =
        vec![Box::new(cutoff), Box::new(cooldown_cutoff)];
    let mut sql = String::from(
        "SELECT a.id, COALESCE(NULLIF(a.blacklisted_tags, \"\"), \"\")
         FROM accounts a
         WHERE EXISTS (
             SELECT 1 FROM feed_interactions fi
             WHERE fi.account_id = a.id AND fi.created_at >= ?1
         )
           AND (a.last_prefetched_at = \"\"
                OR (julianday(\"now\") - julianday(a.last_prefetched_at)) * 86400.0 >= ?2
           )",
    );

    if let Some(ref hc) = hot_cutoff {
        sql.push_str(
            "\n           AND NOT EXISTS (
                SELECT 1 FROM feed_interactions fi2
                WHERE fi2.account_id = a.id AND fi2.created_at >= ?3
            )",
        );
        params_vec.push(Box::new(hc.clone()));
    }

    // Hardcode LIMIT/OFFSET in the SQL itself to avoid tracking
    // which ?N number to use for the dynamic clause.
    sql.push_str("\n         ORDER BY a.id ASC\n         LIMIT 20 OFFSET 0");

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("pick targets prepare: {e}"))?;
    let params_refs: Vec<&dyn rusqlite::types::ToSql> =
        params_vec.iter().map(|p| p.as_ref()).collect();
    let rows: Vec<(i32, String)> = stmt
        .query_map(params_refs.as_slice(), |r| {
            Ok((r.get::<_, i32>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(|e| format!("pick targets query: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("pick targets collect: {e}"))?;

    if rows.is_empty() {
        return Ok(Vec::new());
    }

    let mut targets: Vec<PrefetchQueries> = Vec::new();
    let rng_seed = (Utc::now().timestamp() % 1_000_000) as u64;
    let n = rows.len();
    let mut picked = vec![false; n];
    weighted_recency_pick(n, max_targets.min(n), rng_seed, &mut picked);

    for (i, (account_id, blacklist)) in rows.into_iter().enumerate() {
        if !picked[i] {
            continue;
        }

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
                recent_popular.push(format!("{} order:fav_count", tag));
            }
            for tag in &character_queries {
                recent_popular.push(format!("{} order:fav_count", tag));
            }
        }

        if !artist_queries.is_empty() || !character_queries.is_empty() || !recent_popular.is_empty()
        {
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

/// Weighted reservoir sampling for prefetch target selection.
///
/// Picks up to `pick_count` distinct indices from `0..n` using recency
/// weights (earlier positions have higher weight). Uses a deterministic
/// LCG seeded with `rng_seed`. Mutates `picked` in place — indices that
/// are already `true` are skipped and remain untouched.
///
/// Separated into its own function so the sampling logic is independently
/// testable without a database connection.
pub fn weighted_recency_pick(n: usize, pick_count: usize, rng_seed: u64, picked: &mut [bool]) {
    if n == 0 || pick_count == 0 {
        return;
    }
    let pick_count = pick_count.min(n);

    // Build weights: recency decreases linearly with position.
    // Top candidate (i=0) gets weight ≈ 2.0, bottom ≈ 1.0.
    let mut state = rng_seed;
    for _ in 0..pick_count {
        // Compute total weight of remaining (not-yet-picked) items.
        let total_weight: f64 = picked[..n]
            .iter()
            .enumerate()
            .filter(|(_, is_picked)| !**is_picked)
            .map(|(i, _)| {
                let recency = 1.0 - (i as f64) / (n as f64 + 1.0);
                1.0 + recency
            })
            .sum();
        if total_weight <= 0.0 {
            break;
        }

        // LCG step
        state = state.wrapping_mul(LCG_A).wrapping_add(LCG_C);
        let r = (state >> 33) as f64 / (u64::MAX as f64) * total_weight;

        let mut accumulated = 0.0f64;
        for (i, picked_i) in picked[..n].iter_mut().enumerate() {
            if *picked_i {
                continue;
            }
            let recency = 1.0 - (i as f64) / (n as f64 + 1.0);
            accumulated += 1.0 + recency;
            if accumulated >= r {
                *picked_i = true;
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::weighted_recency_pick;

    // ── weighted_recency_pick ────────────────────────────────────────

    #[test]
    fn weighted_pick_empty_n() {
        let mut picked = vec![];
        weighted_recency_pick(0, 5, 42, &mut picked);
        assert!(picked.is_empty());
    }

    #[test]
    fn weighted_pick_zero_pick_count() {
        let mut picked = vec![false; 10];
        weighted_recency_pick(10, 0, 42, &mut picked);
        assert!(picked.iter().all(|&p| !p), "no picks when pick_count=0");
    }

    #[test]
    fn weighted_pick_single_item() {
        let mut picked = vec![false; 1];
        weighted_recency_pick(1, 5, 42, &mut picked);
        assert_eq!(picked, vec![true], "one item must be picked");
    }

    #[test]
    fn weighted_pick_picks_exactly_k_items() {
        let mut picked = vec![false; 20];
        weighted_recency_pick(20, 5, 12345, &mut picked);
        assert_eq!(
            picked.iter().filter(|&&p| p).count(),
            5,
            "should pick exactly 5 items from 20"
        );
    }

    #[test]
    fn weighted_pick_when_k_equals_n_picks_all() {
        let mut picked = vec![false; 3];
        weighted_recency_pick(3, 3, 999, &mut picked);
        assert_eq!(
            picked.iter().filter(|&&p| p).count(),
            3,
            "all items picked when k == n"
        );
    }

    #[test]
    fn weighted_pick_deterministic_same_seed() {
        let mut picked_a = vec![false; 10];
        let mut picked_b = vec![false; 10];
        weighted_recency_pick(10, 4, 777, &mut picked_a);
        weighted_recency_pick(10, 4, 777, &mut picked_b);
        assert_eq!(picked_a, picked_b, "same seed must produce same picks");
    }

    #[test]
    fn weighted_pick_large_n_no_panics() {
        // Stress test: with a large pool, different seeds should not
        // cause panics or out-of-bounds access regardless of n/k ratio.
        for seed in 0..20 {
            let mut picked = vec![false; 100];
            weighted_recency_pick(100, 10, seed, &mut picked);
            assert_eq!(
                picked.iter().filter(|&&p| p).count(),
                10,
                "picked exactly 10 from 100 for seed={seed}"
            );
            assert_eq!(picked.len(), 100, "length unchanged");
        }
    }

    #[test]
    fn weighted_pick_earlier_items_more_likely() {
        // Run many trials with small n so statistics are measurable.
        // With n=3, k=1, the first item (i=0) has higher weight and
        // should be picked more often than the last (i=2).
        let mut first_count = 0usize;
        let mut last_count = 0usize;
        let trials = 500;
        for seed in 0..trials {
            let mut picked = vec![false; 3];
            weighted_recency_pick(3, 1, seed as u64, &mut picked);
            if picked[0] {
                first_count += 1;
            }
            if picked[2] {
                last_count += 1;
            }
        }
        assert!(
            first_count > last_count,
            "earlier (higher-weight) items should be picked more often: \
             first={first_count}, last={last_count}"
        );
    }

    #[test]
    fn weighted_pick_respects_existing_true() {
        // If some positions are already picked, the function must
        // skip them and not toggle them back to false.
        let mut picked = vec![false; 10];
        picked[3] = true; // already picked
        weighted_recency_pick(10, 3, 42, &mut picked);
        assert!(picked[3], "position 3 must remain true");
        let total = picked.iter().filter(|&&p| p).count();
        assert!(
            (3..=4).contains(&total),
            "should pick 3 new items + 1 existing = 3 or 4: got {total}"
        );
    }
}
