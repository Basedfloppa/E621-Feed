//! End-to-end orchestration of the `/process` job: download a user's
//! favourites from e621, tear down the previous per-account state, and
//! re-import. Lives in the library so integration tests can exercise it
//! end-to-end against a mock e621 (see `tests/integration_pipeline.rs`).
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

use rocket::futures::stream::StreamExt;

use crate::{
    api, db,
    db::{get_account_by_id, refresh_account_profiles_skip_cooc},
    jobs,
    models::{cfg, Post, UserApiResponse},
    utils::{mark_idf_dirty, PipelineMetrics},
};

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

/// Run a blocking rusqlite closure on `spawn_blocking`. Mirror of the
/// private helper in `main.rs` so this module is self-contained.
async fn db_blocking<F, T>(f: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    match rocket::tokio::task::spawn_blocking(f).await {
        Ok(res) => res,
        Err(e) => Err(format!("DB task panicked: {e}")),
    }
}

/// Full `/process` pipeline. Updates the `jobs` registry as it goes so
/// the `/process/{id}/status` poller can observe progress.
///
/// The macro reassigns `phase_start` after every phase; the final
/// reassignment is intentional but unread (function returns), which
/// trips `unused_assignments`. Allowed at function scope so the macro
/// body stays uniform.
#[allow(unused_assignments)]
pub async fn run_process(account_id: i32, owner_token: String) -> Result<(), String> {
    let mut pipe = PipelineMetrics::new("process");
    // `phase_start` is rebased after every `record_phase!` so the recorded
    // `elapsed_ms` is the delta since the previous phase (matching the
    // docstring on `JobPhaseRecord`), rather than a monotonically growing
    // total since the start of the function.
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
    jobs::set_pages_total(account_id, pages);
    macro_rules! record_phase {
        ($name:expr) => {{
            let elapsed = phase_start.elapsed().as_secs_f64() * 1000.0;
            jobs::record_phase(account_id, $name, elapsed);
            pipe.mark($name);
            let secs = elapsed / 1000.0;
            info!("[process {account_id}] phase '{name}' done in {secs:.1}s", name = $name);
            phase_start = std::time::Instant::now();
        }};
    }
    record_phase!("init");

    db_blocking(move || {
        db::drop_account_posts(account_id)
            .map_err(|e| format!("Failed to drop account posts: {e}"))
    })
    .await?;
    record_phase!("drop_old");

    // Cooccurrence rows can run into the millions for a single account; a
    // monolithic DELETE pins the writer mutex for the full scan and starves
    // every other write (including the status polling-side metadata
    // updates) for minutes. Batch the delete so we release the lock between
    // chunks and emit a log line per batch — gives the user a visible
    // heartbeat instead of a frozen UI.
    let drop_cooc_batch = cfg.runtime.drop_cooc_batch_size.max(1_000);
    let deleted_cooc = db_blocking(move || {
        db::drop_account_cooccurrence_batched(account_id, drop_cooc_batch, |batch, total| {
            info!(
                "[process {account_id}] drop_cooc batch -{batch} (total deleted: {total})"
            );
        })
        .map_err(|e| format!("Failed to drop account cooccurrence: {e}"))
    })
    .await?;
    info!("[process {account_id}] drop_cooc complete: {deleted_cooc} rows");
    record_phase!("drop_cooc");

    // Fetch pages in parallel; writes stay serial (SQLite is single-writer).
    //
    // `get_favorites` now returns `Result<Option<Vec<Post>>, String>` so
    // the loop can distinguish:
    //   * Ok(Some(posts))  — real page (possibly empty if past end)
    //   * Ok(None)         — body parsed but malformed (count as empty)
    //   * Err(msg)         — hard fetch failure (timeouts/5xx exhausted)
    //
    // Two adjacent hard failures abort the whole job — e621 is reliably
    // unhappy and we'd just burn ~2 minutes per page producing junk
    // data the user couldn't tell apart from a real empty page.
    const MAX_CONSECUTIVE_PAGE_FAILURES: u32 = 2;

    let account_for_fetch = account.clone();
    let blacklist_for_fetch = blacklist.clone();
    let mut stream = rocket::futures::stream::iter(1..=pages)
        .map(move |i| {
            let acc = account_for_fetch.clone();
            let bl = blacklist_for_fetch.clone();
            async move {
                let raw = api::get_favorites(&acc, i).await;
                let posts: Result<Vec<Post>, String> = raw.map(|opt| {
                    opt.unwrap_or_default()
                        .into_iter()
                        .map(|p| strip_blacklisted_tags(p, &bl))
                        .collect()
                });
                (i, posts)
            }
        })
        .buffer_unordered(cfg.runtime.process_fetch_concurrency.max(1));

    let acc_id = account.id;
    let mut consecutive_failures = 0u32;
    while let Some((i, posts_res)) = stream.next().await {
        let posts = match posts_res {
            Ok(p) => {
                consecutive_failures = 0;
                p
            }
            Err(e) => {
                consecutive_failures += 1;
                warn!(
                    "[process {account_id}] page {i} fetch failed \
                     ({consecutive_failures}/{MAX_CONSECUTIVE_PAGE_FAILURES} consecutive): {e}"
                );
                if consecutive_failures >= MAX_CONSECUTIVE_PAGE_FAILURES {
                    return Err(format!(
                        "aborted after {consecutive_failures} consecutive page fetch failures; \
                         last error on page {i}: {e}"
                    ));
                }
                // Not yet at the abort threshold — tick `pages_done` so
                // the progress UI keeps advancing and move on.
                jobs::record_page_done(account_id);
                continue;
            }
        };
        let posts_len = posts.len();
        info!("{posts_len} post(s) found on page {i}");
        let bl = blacklist.clone();
        db_blocking(move || -> Result<(), String> {
            db::save_posts(&posts, acc_id)
                .map_err(|e| format!("Failed to save posts: {e}"))?;
            db::save_posts_tags_batch(&posts, &bl, true, Some(acc_id))
                .map_err(|e| format!("Failed to save tags for page {i}: {e}"))?;
            Ok(())
        })
        .await?;
        jobs::record_page_done(account_id);
    }
    mark_idf_dirty();
    record_phase!("fetch_and_save");

    db_blocking(move || {
        // Cooccurrence was built incrementally during save_posts_tags_batch,
        // so skip the expensive full rebuild here.
        refresh_account_profiles_skip_cooc(account_id)
            .map_err(|e| format!("Failed to refresh account profiles: {e}"))
    })
    .await?;
    record_phase!("profile_refresh");
    pipe.finish_and_log();
    Ok(())
}
