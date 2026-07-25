//! End-to-end orchestration of the `/process` job: download a user's
//! favourites from e621, tear down the previous per-account state, and
//! re-import. Lives in the library so integration tests can exercise it
//! end-to-end against a mock e621 (see `tests/integration_pipeline.rs`).
//!
//! ## Modes
//!
//! [`ProcessMode`] picks between two strategies:
//!
//! * **Full** — drop `accounts_post` and `account_tag_cooccurrence`
//!   for this account, refetch every favourites page from e621,
//!   rebuild the profile. Authoritative but expensive: on a 2.6M-row
//!   cooc account the drop phase alone is ~20 min.
//!
//! * **Incremental** — assume `accounts_post` is current except for
//!   recently added favs. Iterate e621 pages from the top; stop as
//!   soon as a page contains a post we already own. Save only the
//!   new posts via `save_posts_tags_batch` (which keeps cooc
//!   consistent incrementally). Skip drop phases entirely.
//!   Caveat: doesn't detect unfavourites — those rows linger until
//!   the next full sweep.
//!
//! * **Auto** — pick Full when the local set is empty (first run) or
//!   when `favorite_count_remote < local_count` (something was
//!   unfavourited), otherwise Incremental. Default for /process.
//!
//! Failure modes:
//!   * Auth check fails    → returns `Err` before any teardown.
//!   * `get_account` fails → returns `Err`, no teardown.
//!   * page fetch fails    → recorded as a consecutive-failure count.
//!     After `MAX_CONSECUTIVE_PAGE_FAILURES` adjacent timeouts/5xx,
//!     `run_process` aborts with the upstream error rather than silently
//!     completing with dropped favourites. The old "treat fetch error
//!     as empty page" behaviour produced cosmetically-finished jobs
//!     whose tag-counts profile was missing whole pages worth of data,
//!     which the user then couldn't tell apart from a real empty page.

use std::collections::HashSet;

use crate::{
    api, audit, db, db_blocking,
    db::{get_account_by_id, refresh_account_profiles_skip_cooc},
    jobs,
    models::{cfg, Post, TruncatedAccount, UserApiResponse},
    utils::{mark_idf_dirty, PipelineMetrics},
};

/// Which strategy `run_process` should use. Wire-form (`?mode=...`)
/// is parsed by the HTTP handler via [`ProcessMode::from_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessMode {
    /// Tear down and rebuild from scratch. Authoritative; expensive.
    Full,
    /// Fetch only new favs since the last run. Cheap; doesn't reap
    /// unfavourites until a full sweep happens.
    Incremental,
    /// Decide between Full and Incremental based on local vs remote
    /// favourite counts. Default.
    Auto,
}

impl ProcessMode {
    /// Parse `?mode=...` query strings. Accepts the documented
    /// values plus a permissive empty/None fallback to `Auto`.
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Ok(ProcessMode::Auto),
            "full" => Ok(ProcessMode::Full),
            "incremental" | "incr" | "delta" => Ok(ProcessMode::Incremental),
            other => Err(format!(
                "unknown process mode '{other}'; expected one of: auto, full, incremental"
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            ProcessMode::Full => "full",
            ProcessMode::Incremental => "incremental",
            ProcessMode::Auto => "auto",
        }
    }
}

/// Strip every blacklisted tag from a post's tag groups in place.
/// Exposed so callers that prepare posts outside the pipeline (e.g.,
/// the `seed` binary, or tests fabricating fake feeds) can apply the
/// same filter the live ingest path uses.
pub fn strip_blacklisted_tags(mut p: Post, blacklist: &HashSet<String>) -> Post {
    let filter = |v: &mut Vec<String>| {
        v.retain(|t| !blacklist.contains(&t.to_lowercase().trim().to_string()));
    };
    filter(&mut p.tags.artist);
    filter(&mut p.tags.character);
    filter(&mut p.tags.copyright);
    filter(&mut p.tags.general);
    filter(&mut p.tags.lore);
    filter(&mut p.tags.meta);
    filter(&mut p.tags.species);
    p
}


/// Two adjacent hard fetch failures abort the whole job — e621 is
/// reliably unhappy at that point and we'd just burn ~2 minutes per
/// page producing junk data the user couldn't tell apart from a real
/// empty page. Shared between full and incremental paths.
const MAX_CONSECUTIVE_PAGE_FAILURES: u32 = 2;

/// Back-compat shim — `mode = Auto` is the most useful default and
/// matches the old behaviour for cold accounts.
pub async fn run_process(account_id: i32, owner_token: String) -> Result<(), String> {
    run_process_with_mode(account_id, owner_token, ProcessMode::Auto).await
}

/// Full `/process` pipeline with explicit mode selection. Updates the
/// `jobs` registry as it goes so the `/process/{id}/status` poller can
/// observe progress.
///
/// The macro reassigns `phase_start` after every phase; the final
/// reassignment is intentional but unread (function returns), which
/// trips `unused_assignments`. Allowed at function scope so the macro
/// body stays uniform.
#[allow(unused_assignments)]
pub async fn run_process_with_mode(
    account_id: i32,
    owner_token: String,
    mode: ProcessMode,
) -> Result<(), String> {
    audit::event("process.start")
        .field("account_id", account_id)
        .field("requested_mode", mode.as_str())
        .emit();
    crate::metrics::METRICS.process_runs_total
        .with_label_values(&["started"])
        .inc();
    let pipeline_start = std::time::Instant::now();
    let mut pipe = PipelineMetrics::new("process");
    let mut phase_start = std::time::Instant::now();

    let cfg = cfg();
    let blacklist: HashSet<String> = cfg.tag_blacklist.iter().map(|s| s.to_lowercase()).collect();

    let account = db_blocking(move || {
        get_account_by_id(&owner_token, account_id).map_err(|e| e.to_string())
    })
    .await?;
    let user = api::get_account(&account).await?;
    let favcount = match user {
        UserApiResponse::FullCurrentUser(u) => u.favorite_count,
        UserApiResponse::FullUser(u) => u.favorite_count,
    };
    let pages =
        (favcount / cfg.posts_limit) + (if favcount % cfg.posts_limit > 0 { 1 } else { 0 });

    macro_rules! record_phase {
        ($name:expr) => {{
            let elapsed = phase_start.elapsed().as_secs_f64() * 1000.0;
            jobs::record_phase(account_id, $name, elapsed);
            pipe.mark($name);
            let secs = elapsed / 1000.0;
            info!("[process {account_id}] phase '{name}' done in {secs:.1}s", name = $name);
            audit::event("process.phase")
                .field("account_id", account_id)
                .field("phase", $name)
                .field("ms", format!("{:.0}", elapsed))
                .emit();
            phase_start = std::time::Instant::now();
        }};
    }
    record_phase!("init");

    // Decide which strategy to run. Auto checks local vs remote counts
    // to pick safely: Full when the local set is empty (first run) or
    // when the remote dropped below local (unfavourite event), else
    // Incremental.
    let local_count = db_blocking(move || -> Result<i64, String> {
        let conn = db::open_db_for_calibration().map_err(|e| format!("open_db: {e}"))?;
        conn.query_row(
            "SELECT COUNT(*) FROM accounts_post WHERE account_id = ?1",
            rusqlite::params![account_id],
            |r| r.get::<_, i64>(0),
        )
        .map_err(|e| format!("count owned: {e}"))
    })
    .await?;
    let resolved_mode = match mode {
        ProcessMode::Full => ProcessMode::Full,
        ProcessMode::Incremental => ProcessMode::Incremental,
        ProcessMode::Auto => {
            if local_count == 0 || (favcount as i64) < local_count {
                ProcessMode::Full
            } else {
                ProcessMode::Incremental
            }
        }
    };
    info!(
        "[process {account_id}] mode={} (requested={}, local={}, remote={favcount})",
        resolved_mode.as_str(),
        mode.as_str(),
        local_count
    );
    audit::event("process.mode_picked")
        .field("account_id", account_id)
        .field("mode", resolved_mode.as_str())
        .field("local_count", local_count)
        .field("remote_count", favcount)
        .emit();

    // ── Teardown phases (Full only) ─────────────────────────────
    if resolved_mode == ProcessMode::Full {
        db_blocking(move || {
            db::drop_account_posts(account_id)
                .map_err(|e| format!("Failed to drop account posts: {e}"))
        })
        .await?;
        record_phase!("drop_old");

        let drop_cooc_batch = cfg.runtime.drop_cooc_batch_size.max(1_000);
        let deleted_cooc = db_blocking(move || {
            db::drop_account_cooccurrence_batched(account_id, drop_cooc_batch, |batch, total| {
                info!(
                    "[process {account_id}] drop_cooc deleted {batch} rows (total: {total})"
                );
                audit::event("process.drop_cooc_progress")
                    .field("account_id", account_id)
                    .field("deleted", total)
                    .emit();
            })
            .map_err(|e| format!("Failed to drop account cooccurrence: {e}"))
        })
        .await?;
        info!("[process {account_id}] drop_cooc complete: {deleted_cooc} rows");
        audit::event("process.drop_cooc_done")
            .field("account_id", account_id)
            .field("deleted", deleted_cooc)
            .emit();
        record_phase!("drop_cooc");
    } else {
        audit::event("process.teardown_skipped")
            .field("account_id", account_id)
            .field("reason", "incremental_mode")
            .emit();
    }

    // ── Fetch + save ─────────────────────────────────────────────
    let new_count = if resolved_mode == ProcessMode::Full {
        run_full_fetch(account_id, &account, &blacklist, pages, cfg.runtime.process_fetch_concurrency.max(1)).await?
    } else {
        run_incremental_fetch(account_id, &account, &blacklist, pages).await?
    };
    mark_idf_dirty();
    record_phase!("fetch_and_save");

    // ── Profile refresh — same for both modes ────────────────────
    db_blocking(move || {
        // Cooccurrence was built incrementally during save_posts_tags_batch,
        // so skip the expensive full rebuild here.
        refresh_account_profiles_skip_cooc(account_id)
            .map_err(|e| format!("Failed to refresh account profiles: {e}"))
    })
    .await?;
    record_phase!("profile_refresh");
    pipe.finish_and_log();

    audit::event("process.done")
        .field("account_id", account_id)
        .field("mode", resolved_mode.as_str())
        .field("favs_remote", favcount)
        .field("pages_total", pages)
        .field("new_or_persisted", new_count)
        .field("ms", pipeline_start.elapsed().as_millis())
        .emit();
    crate::metrics::METRICS.process_runs_total
        .with_label_values(&["success"])
        .inc();
    Ok(())
}

/// Full-mode fetch: sequential page iteration (no parallelism — keeps
/// e621 rate-limits predictable). Returns the number of posts persisted.
async fn run_full_fetch(
    account_id: i32,
    account: &TruncatedAccount,
    blacklist: &HashSet<String>,
    pages: i32,
    _fetch_concurrency: usize,
) -> Result<usize, String> {
    let acc_id = account.id;
    let mut consecutive_failures = 0u32;
    let mut total_persisted = 0usize;
    jobs::set_pages_total(account_id, pages);
    for page in 1..=pages {
        let posts_res = api::get_favorites(account, page).await.map(|posts| {
            posts
                .into_iter()
                .map(|p| strip_blacklisted_tags(p, blacklist))
                .collect::<Vec<Post>>()
        });
        let posts = match posts_res {
            Ok(p) => {
                consecutive_failures = 0;
                p
            }
            Err(e) => {
                consecutive_failures += 1;
                warn!(
                    "[process {account_id}] page {page} fetch failed \
                     ({consecutive_failures}/{MAX_CONSECUTIVE_PAGE_FAILURES} consecutive): {e}"
                );
                audit::event("process.page_failed")
                    .field("account_id", account_id)
                    .field("page", page)
                    .field("consecutive", consecutive_failures)
                    .field("error", &e)
                    .emit_err();
                if e.is_malformed() {
                    return Err(format!(
                        "aborted on malformed favourites response at page {page}: {e}"
                    ));
                }
                if consecutive_failures >= MAX_CONSECUTIVE_PAGE_FAILURES {
                    return Err(format!(
                        "aborted after {consecutive_failures} consecutive page fetch failures; \
                         last error on page {page}: {e}"
                    ));
                }
                jobs::record_page_done(account_id);
                continue;
            }
        };
        let posts_len = posts.len();
        info!("{posts_len} post(s) found on page {page}");
        let bl = blacklist.clone();
        db_blocking(move || -> Result<(), String> {
            db::save_posts(&posts, acc_id)
                .map_err(|e| format!("Failed to save posts: {e}"))?;
            db::save_posts_tags_batch(&posts, &bl, true, Some(acc_id))
                .map_err(|e| format!("Failed to save tags for page {page}: {e}"))?;
            Ok(())
        })
        .await?;
        total_persisted += posts_len;
        jobs::record_page_done(account_id);
        audit::event("process.page_done")
            .field("account_id", account_id)
            .field("page", page)
            .field("posts", posts_len)
            .field("mode", "full")
            .emit();
    }
    Ok(total_persisted)
}

/// Incremental fetch: iterate e621 pages from the top, save only posts
/// not already in `accounts_post`, stop as soon as a page contains a
/// post we already own. Returns the number of net-new posts saved.
///
/// `known_pages` is the total page count based on `favorite_count` —
/// only used for the frontend progress bar's `pages_total`. The actual
/// loop stops early on overlap; when that happens `pages_total` is
/// trimmed to `pages_done` so the bar snaps to 100%.
///
/// Correctness relies on e621 returning favourites in stable
/// reverse-chronological order. Edge cases the loop handles:
///   * page is entirely new           — save all, continue to next page
///   * page mixes new + known         — save the new ones, stop
///   * page is entirely already-known — stop (cold path: only happens
///                                       if a previous incremental
///                                       run was interrupted mid-save)
///   * page is empty                  — past the end of pagination
///   * page fetch errors              — same consecutive-failure rule
///                                       as full mode
///
/// Skipped vs full: no `drop_*` phases, only new posts go through the
/// DB writer. Saves ~20 min on 2.6M-row accounts.
async fn run_incremental_fetch(
    account_id: i32,
    account: &TruncatedAccount,
    blacklist: &HashSet<String>,
    known_pages: i32,
) -> Result<usize, String> {
    let local_owned: HashSet<i64> = db_blocking(move || {
        db::get_owned_post_ids(account_id)
            .map_err(|e| format!("Failed to load owned post ids: {e}"))
    })
    .await?;

    info!(
        "[process {account_id}] (incremental) local_owned has {} posts, known_pages={known_pages}",
        local_owned.len()
    );
    // Frontend progress: show a page counter. Use the larger of known_pages
    // (total from favcount) and a generous upper bound so the bar makes sense.
    let display_pages = known_pages.max(1);
    jobs::set_pages_total(account_id, display_pages);

    let acc_id = account.id;
    let mut consecutive_failures = 0u32;
    let mut total_new = 0usize;
    // Cap to avoid runaway in pathological cases (e.g. e621 paginates
    // forever returning the same posts). 200 pages × 160 posts =
    // 32k new favs in one incremental pass; far beyond realistic.
    let max_pages = 200i32;
    for page in 1..=max_pages {
        // If we went past the original estimate, bump pages_total so the
        // frontend bar doesn't get stuck at 100%.
        if page > display_pages {
            jobs::set_pages_total(account_id, page);
        }
        let posts_res = api::get_favorites(account, page).await.map(|posts| {
            posts
                .into_iter()
                .map(|p| strip_blacklisted_tags(p, blacklist))
                .collect::<Vec<Post>>()
        });
        let posts = match posts_res {
            Ok(p) => {
                consecutive_failures = 0;
                p
            }
            Err(e) => {
                consecutive_failures += 1;
                warn!(
                    "[process {account_id}] (incremental) page {page} fetch failed \
                     ({consecutive_failures}/{MAX_CONSECUTIVE_PAGE_FAILURES} consecutive): {e}"
                );
                audit::event("process.page_failed")
                    .field("account_id", account_id)
                    .field("page", page)
                    .field("consecutive", consecutive_failures)
                    .field("mode", "incremental")
                    .field("error", &e)
                    .emit_err();
                if e.is_malformed() {
                    return Err(format!(
                        "aborted on malformed favourites response at page {page}: {e}"
                    ));
                }
                if consecutive_failures >= MAX_CONSECUTIVE_PAGE_FAILURES {
                    return Err(format!(
                        "aborted after {consecutive_failures} consecutive page fetch failures; \
                         last error on page {page}: {e}"
                    ));
                }
                jobs::record_page_done(account_id);
                continue;
            }
        };
        if posts.is_empty() {
            // End of pagination.
            break;
        }
        let posts_len = posts.len();
        let new_posts: Vec<Post> = posts
            .iter()
            .filter(|p| !local_owned.contains(&p.id))
            .cloned()
            .collect();
        let new_count = new_posts.len();
        let had_overlap = new_count < posts_len;

        if new_count > 0 {
            let bl = blacklist.clone();
            db_blocking(move || -> Result<(), String> {
                db::save_posts(&new_posts, acc_id)
                    .map_err(|e| format!("Failed to save posts: {e}"))?;
                db::save_posts_tags_batch(&new_posts, &bl, true, Some(acc_id))
                    .map_err(|e| format!("Failed to save tags for page {page}: {e}"))?;
                Ok(())
            })
            .await?;
        }
        total_new += new_count;

        info!(
            "[process {account_id}] (incremental) page {page}: {new_count} new of {posts_len}"
        );
        audit::event("process.page_done")
            .field("account_id", account_id)
            .field("page", page)
            .field("posts", posts_len)
            .field("new", new_count)
            .field("mode", "incremental")
            .emit();
        jobs::record_page_done(account_id);

        if had_overlap {
            // Reached the boundary between new and already-known favs.
            // Everything older is already in our DB. Trim pages_total
            // to pages_done so the frontend progress bar snaps to 100%.
            jobs::set_pages_total(account_id, page);
            break;
        }
    }
    Ok(total_new)
}
