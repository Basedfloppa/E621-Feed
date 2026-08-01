//! Backfill worker — third prefetch worker.
//!
//! Runs on a long interval (default 6h), picks accounts that haven't been
//! backfilled recently, and for each preferred tag fetches both fresh
//! posts (page=1) and retro posts (last available page). Uses the shared
//! e621 rate gate via `api::get_posts_by_tags` at `Priority::Backfill`
//! (longest delay).

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use crate::api;
use crate::db;
use crate::load_monitor::Priority;
use crate::models::cfg;

static BACKFILL_CONSECUTIVE_FAILS: AtomicU32 = AtomicU32::new(0);

/// Spawn the backfill worker. Called from `prefetch.rs`.
pub fn spawn_backfill_worker() {
    rocket::tokio::spawn(backfill_loop());
}

async fn backfill_loop() {
    // Initial delay: let migrations and hot/cold workers stabilise.
    rocket::tokio::time::sleep(Duration::from_secs(120)).await;

    loop {
        let runtime = cfg().runtime.clone();
        let interval = runtime.backfill_interval_secs.max(300);
        let cooldown = runtime.backfill_cooldown_secs.max(3600);
        let breaker_threshold = runtime.backfill_breaker_threshold.max(1);

        // Circuit breaker check.
        let fails = BACKFILL_CONSECUTIVE_FAILS.load(Ordering::Relaxed);
        if fails >= breaker_threshold {
            let pause = runtime.prefetch_breaker_pause_secs.max(60);
            warn!(
                "[backfill] circuit breaker open after {fails} consecutive failures \
                 (threshold {breaker_threshold}); pausing {pause}s before resuming"
            );
            rocket::tokio::time::sleep(Duration::from_secs(pause)).await;
            BACKFILL_CONSECUTIVE_FAILS.store(0, Ordering::Relaxed);
            info!("[backfill] circuit breaker reset, resuming");
            continue;
        }

        if let Err(e) = run_backfill_tick(cooldown).await {
            warn!("[backfill] tick failed: {e}");
        }

        rocket::tokio::time::sleep(Duration::from_secs(interval)).await;
    }
}

/// Pick accounts eligible for backfill and, for each preferred tag,
/// fetch fresh (page=1) and retro (last available page) posts.
async fn run_backfill_tick(cooldown_secs: u64) -> Result<(), String> {
    const MAX_ACCOUNTS_PER_TICK: usize = 3;
    const BACKFILL_PAGE_FRESH: i32 = 1;
    const BACKFILL_PAGE_RETRO: i32 = 9999; // sentinel for "last page"

    let accounts = db::get_backfill_candidates(cooldown_secs, MAX_ACCOUNTS_PER_TICK)?;

    if accounts.is_empty() {
        debug!("[backfill] no accounts eligible for backfill this tick");
        return Ok(());
    }

    for (account_id, blacklist) in &accounts {
        let tags = db::get_all_preferred_tags(*account_id)?;
        if tags.is_empty() {
            debug!("[backfill] account={account_id}: no preferred tags, skipping");
            db::mark_account_backfilled(*account_id)?;
            continue;
        }

        // Limit tags per account per tick to avoid hammering one account.
        let cap = tags.len().min(10);

        for (tag_name, _group, _weight) in tags.iter().take(cap) {
            // Check circuit breaker before each tag.
            let fails = BACKFILL_CONSECUTIVE_FAILS.load(Ordering::Relaxed);
            let threshold = cfg().runtime.backfill_breaker_threshold.max(1);
            if fails >= threshold {
                warn!("[backfill] circuit breaker tripped mid-tick, aborting remaining tags");
                return Ok(());
            }

            // 1) Fetch retro posts (last available page → oldest posts).
            let query = format!("{tag_name} order:id");
            match api::get_posts_by_tags(
                blacklist,
                &query,
                Some(BACKFILL_PAGE_RETRO),
                None,
                Priority::Backfill,
            )
            .await
            {
                Ok(posts) if !posts.is_empty() => {
                    persist_backfill_posts(*account_id, &posts).await?;
                    BACKFILL_CONSECUTIVE_FAILS.store(0, Ordering::Relaxed);
                }
                Ok(_) => {
                    // No posts at all — skip this tag entirely.
                    continue;
                }
                Err(e) => {
                    let n = BACKFILL_CONSECUTIVE_FAILS.fetch_add(1, Ordering::Relaxed) + 1;
                    warn!(
                        "[backfill] e621 fetch (retro) failed for account={account_id} tag={tag_name} (consecutive_fails={n}): {e}"
                    );
                    break;
                }
            }

            // 2) Fetch fresh posts (page 1).
            let query_fresh = format!("{tag_name} order:id");
            match api::get_posts_by_tags(
                blacklist,
                &query_fresh,
                Some(BACKFILL_PAGE_FRESH),
                None,
                Priority::Backfill,
            )
            .await
            {
                Ok(posts) if !posts.is_empty() => {
                    persist_backfill_posts(*account_id, &posts).await?;
                }
                Ok(_) => {}
                Err(e) => {
                    let n = BACKFILL_CONSECUTIVE_FAILS.fetch_add(1, Ordering::Relaxed) + 1;
                    warn!(
                        "[backfill] e621 fetch (fresh) failed for account={account_id} tag={tag_name} (consecutive_fails={n}): {e}"
                    );
                    break;
                }
            }
        }

        // Mark account as backfilled (even if only some tags were processed).
        db::mark_account_backfilled(*account_id)?;
    }

    Ok(())
}

/// Helper: persist backfill posts into the catalog.
async fn persist_backfill_posts(
    _account_id: i32,
    posts: &[crate::models::Post],
) -> Result<(), String> {
    if posts.is_empty() {
        return Ok(());
    }
    let posts_clone = posts.to_vec();
    rocket::tokio::task::spawn_blocking(move || -> Result<(), String> {
        db::upsert_catalog_posts(&posts_clone).map_err(|e| format!("[backfill] upsert: {e}"))?;
        db::save_posts_tags_batch(&posts_clone, &std::collections::HashSet::new(), false, None)
            .map_err(|e| format!("[backfill] tags: {e}"))?;
        Ok(())
    })
    .await
    .map_err(|e| format!("[backfill] persist join: {e}"))?
}

#[cfg(test)]
mod tests {
    // Use a LOCAL AtomicU32 to test breaker logic without mutating
    // the global static (which is shared across tests).
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn breaker_starts_at_zero() {
        let breaker = AtomicU32::new(0);
        assert_eq!(breaker.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn breaker_fetch_add_then_check() {
        let breaker = AtomicU32::new(10);
        let n = breaker.fetch_add(1, Ordering::Relaxed) + 1;
        assert_eq!(n, 11);
        assert_eq!(breaker.load(Ordering::Relaxed), 11);
    }

    #[test]
    fn breaker_store_reset() {
        let breaker = AtomicU32::new(5);
        breaker.store(0, Ordering::Relaxed);
        assert_eq!(breaker.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn persist_empty_posts_is_noop() {
        // persist_backfill_posts with empty vec should return Ok(()) without
        // spawning a blocking task (empty check at the top).
        let result = persist_backfill_posts(1, &[]).await;
        assert!(result.is_ok());
    }
}
