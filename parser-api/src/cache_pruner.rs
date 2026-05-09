//! Single background thread that walks every in-process cache on a
//! configurable cadence and drops entries past their validity window.
//!
//! Caches owned by this worker:
//!  * `api::API_CACHE` — outbound e621 GET cache, TTL-driven
//!  * `jobs::JOBS` — /process job state map; finished entries past
//!    `FINISHED_JOB_RETAIN_SECS`
//!  * `ratelimit::BUCKETS` — token-bucket map; idle and oversized entries
//!
//! Each module exposes a `prune_*` fn returning `(before, after)` so the
//! worker can log a single summary line per tick. `IDF_CACHE` and
//! `GLOBAL_CACHE` are NOT pruned here — they're versioned via ArcSwap +
//! dirty flags and replaced wholesale on rebuild.
//!
//! Cadence comes from `runtime.cache_validate_interval_secs`. Values
//! below 30 are clamped (a 5-second tick on a quiet box just burns a
//! thread); 0 disables the worker entirely.

use std::time::Duration;

use crate::{api, jobs, models::cfg, ratelimit};

const MIN_INTERVAL_SECS: u64 = 30;

pub fn spawn_cache_pruner() {
    std::thread::Builder::new()
        .name("cache-pruner".to_string())
        .spawn(|| loop {
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

            let api_diff = api::prune_api_cache();
            let jobs_diff = jobs::prune_finished_jobs();
            let rl_diff = ratelimit::prune_buckets();
            let total_dropped = (api_diff.0 - api_diff.1)
                + (jobs_diff.0 - jobs_diff.1)
                + (rl_diff.0 - rl_diff.1);
            if total_dropped > 0 {
                info!(
                    "[cache-pruner] dropped {} entries (api {}->{}, jobs {}->{}, ratelimit {}->{})",
                    total_dropped,
                    api_diff.0, api_diff.1,
                    jobs_diff.0, jobs_diff.1,
                    rl_diff.0, rl_diff.1,
                );
            } else {
                debug!(
                    "[cache-pruner] tick clean (api={}, jobs={}, ratelimit={})",
                    api_diff.1, jobs_diff.1, rl_diff.1
                );
            }
        })
        .expect("spawn cache-pruner thread");
}
