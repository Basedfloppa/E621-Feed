//! Single background thread that walks every in-process cache on a
//! configurable cadence and drops entries past their validity window.
//!
//! Caches owned by this worker:
//!  * `api::API_CACHE` — outbound e621 GET cache, TTL + idle-eviction
//!  * `jobs::JOBS` — /process job state map; finished entries past
//!    `runtime.jobs_finished_retain_secs`
//!  * `ratelimit::BUCKETS` — token-bucket map; idle and oversized entries
//!  * `db::posts` orphan candidates — catalog posts pulled in by
//!    `/recommendations` browse traffic that nobody favourited and that
//!    haven't been re-touched within `runtime.orphan_retention_secs`.
//!    Marks IDF + global graph dirty after deletion so the in-memory
//!    graphs shrink on the next worker tick.
//!  * `IDF_CACHE` / `GLOBAL_CACHE` — heavy recommendation caches loaded
//!    from the catalog (`HashMap`<(u32,u32),i64> of co-occurrence pairs +
//!    `HashMap`<String,i64> of per-tag document frequencies). On a 7 GB
//!    `SQLite` catalog these inflate to 500 MB–1 GB resident. Each cache
//!    tracks its last-touch wall-clock; if untouched for
//!    `runtime.cache_idle_eviction_secs`, the graph/index is swapped
//!    back to empty so the working set returns to the OS. The next
//!    request lazily rebuilds (same as cold startup).
//!
//! Each module exposes a `prune_*` fn returning `(before, after)` so the
//! worker can log a single summary line per tick.
//!
//! Cadence comes from `runtime.cache_validate_interval_secs`. Values
//! below 30 are clamped (a 5-second tick on a quiet box just burns a
//! thread); 0 disables the worker entirely.

use std::time::{Duration, Instant};

use crate::{
    api, auth, db, jobs,
    models::cfg,
    ratelimit,
    utils::{evict_global_relation_if_idle, evict_idf_if_idle},
};

const MIN_INTERVAL_SECS: u64 = 30;

pub fn spawn_cache_pruner() {
    std::thread::Builder::new()
        .name("cache-pruner".to_string())
        .spawn(move || {
            // Track last run of the longer-interval maintenance tasks so they
            // can share the same tick loop without drifting.
            let mut last_catalog_cleanup = Instant::now();

            loop {
                // Re-read on every iteration so a config-watcher reload
                // takes effect on the next tick (matches IDF rebuild's
                // cooldown re-read pattern).
                let raw = cfg().runtime.cache_validate_interval_secs;
                if raw == 0 {
                    info!("[cache-pruner] disabled (cache_validate_interval_secs=0); thread exiting");
                    break;
                }
                let interval = Duration::from_secs(raw.max(MIN_INTERVAL_SECS));
                std::thread::sleep(interval);

                // ── Per-tick (every ~30–300s) ───────────────────────────

                let api_diff = api::prune_api_cache();
                let jobs_diff = jobs::prune_finished_jobs();
                let rl_diff = ratelimit::prune_buckets();
                let orphan_retention = cfg().runtime.orphan_retention_secs;
                let candidate_diff = if orphan_retention > 0 {
                    match db::prune_orphan_candidates(orphan_retention) {
                        Ok((before, after)) => (before, after),
                        Err(e) => {
                            warn!("[cache-pruner] orphan-candidate prune failed: {e}");
                            (0, 0)
                        }
                    }
                } else {
                    (0, 0)
                };
                let revoked_diff = match db::prune_revoked_tokens(auth::REVOKED_TOKEN_RETENTION_SECS) {
                    Ok((before, after)) => {
                        if before != after
                            && let Err(e) = auth::reload_revocation_set()
                        {
                            warn!("[cache-pruner] revoked-token reload failed: {e}");
                        }
                        (before, after)
                    }
                    Err(e) => {
                        warn!("[cache-pruner] revoked-token prune failed: {e}");
                        (0, 0)
                    }
                };

                let idle_evict_secs = cfg().runtime.cache_idle_eviction_secs;
                let api_evicted = api::evict_api_cache_if_idle(idle_evict_secs);
                let idf_evicted = evict_idf_if_idle(idle_evict_secs);
                let relation_evicted = evict_global_relation_if_idle(idle_evict_secs);
                if api_evicted.0 > 0 {
                    info!(
                        "[cache-pruner] idle-evict API cache after {}s: dropped {} entries",
                        idle_evict_secs, api_evicted.0,
                    );
                }
                if idf_evicted.0 > 0 || idf_evicted.1 > 0 {
                    info!(
                        "[cache-pruner] idle-evict IDF after {}s: dropped {} tags / {} posts",
                        idle_evict_secs, idf_evicted.0, idf_evicted.1
                    );
                }
                if relation_evicted.0 > 0 || relation_evicted.1 > 0 {
                    info!(
                        "[cache-pruner] idle-evict tag-relation after {}s: dropped {} pairs / {} tags",
                        idle_evict_secs, relation_evicted.0, relation_evicted.1
                    );
                }

                // WAL checkpoint — cheap no-op when there's nothing to flush.
                if let Err(e) = db::wal_checkpoint() {
                    warn!("[cache-pruner] WAL checkpoint failed: {e}");
                }

                // ── Catalog cleanup (runs at most every cleanup_interval_secs) ──

                let cleanup_int = Duration::from_secs(
                    cfg().runtime.cleanup_interval_secs.max(60),
                );
                if last_catalog_cleanup.elapsed() >= cleanup_int {
                    last_catalog_cleanup = Instant::now();

                    match db::prune_stale_catalog_posts(
                        cfg().runtime.catalog_retention_days.max(1),
                    ) {
                        Ok(n) if n > 0 => {
                            info!("[catalog-cleanup] deleted {n} stale catalog posts");
                        }
                        Ok(_) => {}
                        Err(e) => warn!("[catalog-cleanup] failed: {e}"),
                    }

                    match db::cleanup_orphan_accounts() {
                        Ok(n) if n > 0 => {
                            info!("[account-cleanup] dropped {n} orphan accounts");
                        }
                        Ok(_) => {}
                        Err(e) => warn!("[account-cleanup] failed: {e}"),
                    }
                }

                // ── Summary ─────────────────────────────────────────────

                let total_dropped = (api_diff.0 - api_diff.1)
                    + (api_evicted.0 - api_evicted.1)
                    + (jobs_diff.0 - jobs_diff.1)
                    + (rl_diff.0 - rl_diff.1)
                    + (candidate_diff.0 - candidate_diff.1)
                    + (revoked_diff.0 - revoked_diff.1);
                if total_dropped > 0 {
                    info!(
                        "[cache-pruner] dropped {} entries (api-ttl {}->{}, api-idle {}->{}, jobs {}->{}, ratelimit {}->{}, candidates {}->{}, revoked {}->{})",
                        total_dropped,
                        api_diff.0, api_diff.1,
                        api_evicted.0, api_evicted.1,
                        jobs_diff.0, jobs_diff.1,
                        rl_diff.0, rl_diff.1,
                        candidate_diff.0, candidate_diff.1,
                        revoked_diff.0, revoked_diff.1,
                    );
                } else {
                    debug!(
                        "[cache-pruner] tick clean (api-ttl={}, api-idle={}, jobs={}, ratelimit={}, candidates={}, revoked={})",
                        api_diff.1, api_evicted.1, jobs_diff.1, rl_diff.1, candidate_diff.1, revoked_diff.1
                    );
                }
            }
        })
        .expect("spawn cache-pruner thread");
}
