//! Background in-server media worker (Mode B, saved-posts scope).
//!
//! Spawned once at startup (the media folder is hardcoded to `media/`). It
//! drains **saved** posts (`accounts_post`) that still lack a local original (`media_entries`)
//! and downloads them idly. The moment a favourites sync
//! (`POST /account/<id>/sync`) or `/process` persists a saved post, that post
//! becomes pending here — so "media absent locally" is automatically queued
//! for download on the next pass, without downloading the whole `posts`
//! corpus (which can be hundreds of thousands of rows).
//!
//! This complements the standalone `bin/media-fetch` operator tool: the
//! worker covers saved posts automatically; the bin can still be used for a
//! manual whole-corpus run.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use e621_account_parser_api::{db, media_store, models};

/// Workers logs through `log` so entries appear in the server log file.
const BATCH: i64 = 20;
const ITEM_DELAY: Duration = Duration::from_millis(400);
const PASS_INTERVAL: Duration = Duration::from_secs(30);
/// Transient body-stream failures (CDN dropping a large/slow connection,
/// connection reset mid-read — reqwest reports these as “error decoding
/// response body”) self-heal with a couple of short retries.
const ATTEMPTS: usize = 3;

/// Global soft-switches the in-server media worker exposes to the control
/// routes (`/catalog/<id>/media/pause|resume|kick`). Pause makes the worker
/// stop between passes/items; kick forces the next pass to run immediately
/// instead of waiting for `PASS_INTERVAL`.
static WORKER_PAUSED: AtomicBool = AtomicBool::new(false);
static WORKER_KICK: AtomicBool = AtomicBool::new(false);

/// Pause (`true`) or resume (`false`) the background media worker.
pub fn set_worker_paused(paused: bool) {
    WORKER_PAUSED.store(paused, Ordering::SeqCst);
}

/// Whether the background media worker is currently paused.
pub fn worker_paused() -> bool {
    WORKER_PAUSED.load(Ordering::SeqCst)
}

/// Mark that a pass should run immediately (consumed by the next loop step).
pub fn kick_worker() {
    WORKER_KICK.store(true, Ordering::SeqCst);
}

/// Take-and-reset the kick flag.
fn take_kick() -> bool {
    WORKER_KICK.swap(false, Ordering::SeqCst)
}

/// Outcome of a single download attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DownloadResult {
    /// Original stored successfully (or already present).
    Stored,
    /// The post no longer exists upstream (HTTP 404/410 on the original).
    /// Authoritative — the caller must purge it from the local catalog so the
    /// worker stops retrying a post that can never be downloaded.
    Deleted,
}

/// Download and store one post's original.
///
/// * `true`  — already present or freshly stored;
/// * `false` — the post was purged from the local catalog because e621
///   answered 404/410 for its original (deleted upstream);
/// * `Err`   — transient failure (network, 5xx, …); retried `ATTEMPTS` times
///   with short backoff, and stays pending for the next pass regardless.
///
/// Exposed for integration tests and operator tooling.
pub async fn fetch_original(
    client: &reqwest::Client,
    post_id: i64,
    file_url: &str,
    ext: &str,
) -> Result<bool, String> {
    // Already present? (concurrent runs / resume safety)
    if let Ok(Some(_)) = db::get_media_entry(post_id) {
        return Ok(true);
    }
    let mut last_err: Option<String> = None;
    for attempt in 0..ATTEMPTS {
        if attempt > 0 {
            // Short linear backoff so a flaky connection gets room to recover.
            tokio::time::sleep(Duration::from_millis(400 * attempt as u64)).await;
        }
        match download_once(client, post_id, file_url, ext).await {
            Ok(DownloadResult::Stored) => return Ok(true),
            Ok(DownloadResult::Deleted) => {
                // e621 says the post is gone — no point retrying. Remove it
                // from the local catalog so every pass stops picking it up.
                remove_deleted_post(post_id).await;
                return Ok(false);
            }
            Err(e) => {
                last_err = Some(e.clone());
                debug!(
                    "[media-worker] {post_id}: attempt {}/{} failed: {e}",
                    attempt + 1,
                    ATTEMPTS
                );
            }
        }
    }
    Err(last_err.unwrap_or_else(|| "no attempts ran".into()))
}

/// Purge a post whose original 404/410'd from the e621 CDN: drop any stale
/// media file/index row, then delete the `posts` row. The FK graph is all
/// `ON DELETE CASCADE` (`accounts_post`, `tags_posts`, `feed_interactions`,
/// `pool_posts`, `media_entries`), so one DELETE cleans the whole catalog.
/// Best-effort — failures are logged, not fatal to the pass.
async fn remove_deleted_post(post_id: i64) {
    let _ = media_store::delete_and_unindex(post_id);
    match db::delete_catalog_posts_by_ids(&[post_id]) {
        Ok(0) => {} // already gone
        Ok(_) => {
            info!("[media-worker] {post_id}: removed from catalog (deleted on e621)");
            crate::audit::event("catalog.media.post_deleted")
                .field("post_id", post_id)
                .emit();
        }
        Err(e) => warn!("[media-worker] {post_id}: cleanup failed: {e}"),
    }
}

async fn download_once(
    client: &reqwest::Client,
    post_id: i64,
    file_url: &str,
    ext: &str,
) -> Result<DownloadResult, String> {
    let resp = client
        .get(file_url)
        .send()
        .await
        .map_err(|e| format!("get {post_id} {file_url}: {}", err_chain(&e)))?;
    let status = resp.status();
    // e621's CDN answers 404/410 for originals of posts deleted upstream (or
    // whose file was removed). That is authoritative: the post can never be
    // downloaded, so surface it as `Deleted` for cleanup instead of retrying.
    if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::GONE {
        return Ok(DownloadResult::Deleted);
    }
    if !status.is_success() {
        // Transient (5xx/429/403) or unexpected — keep retrying on later
        // passes.
        return Err(format!("get {post_id} {file_url}: HTTP {status}"));
    }
    // Capture response metadata BEFORE consuming the body — reqwest rejects
    // reading headers after the body has been read.
    let clen = resp
        .content_length()
        .map(|n| n.to_string())
        .unwrap_or_else(|| "?".to_string());
    let ctype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("?")
        .to_string();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| {
            format!(
                "read {post_id} {file_url}: {} (status={status}, content-length={clen}, content-type={ctype})",
                err_chain(&e)
            )
        })?;
    if bytes.is_empty() {
        return Err(format!(
            "get {post_id} {file_url}: empty body (status={status}, content-length={clen})"
        ));
    }
    let rel =
        media_store::store_original(post_id, ext, &bytes, file_url).map_err(|e| e.to_string())?;
    // Enforce the cache-size budget (LRU).
    let _ = media_store::evict_to_budget();
    info!(
        "[media-worker] stored {post_id} -> {rel} ({} B, {ctype})",
        bytes.len()
    );
    Ok(DownloadResult::Stored)
}

/// Render an error plus its full `source()` chain (e.g. reqwest's terse
/// "error decoding response body" is usually a wrapper around a more concrete
/// underlying cause like a connection reset or a content-length mismatch).
fn err_chain(e: &dyn std::error::Error) -> String {
    let mut s = format!("{e}");
    let mut cur = e.source();
    while let Some(src) = cur {
        // Avoid infinite loops from self-referential error sources.
        s.push_str(&format!(" <- {src}"));
        cur = src.source();
    }
    s
}

/// Spawn the background media worker. Called from the server binary
/// (`main.rs`) at startup. The worker always runs, but whether a pass actually
/// **downloads** originals is gated per-pass on the catalog persistence
/// toggles (`save_favourites` and/or `save_all`) — the media cache follows
/// the collection scopes: with both off nothing is collected and the worker
/// idles. The per-pass check is hot-reload aware, so flipping the toggles at
/// runtime starts/stops downloads without a restart.
pub fn spawn_media_fetcher() {
    let ua = models::cfg().user_agent.clone();
    let client = match reqwest::Client::builder()
        .user_agent(ua)
        .timeout(Duration::from_secs(60))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!("[media-worker] failed to build http client: {e}; media worker disabled");
            return;
        }
    };
    rocket::tokio::spawn(async move {
        info!("[media-worker] started (saved-posts scope, media folder: media/)");
        loop {
            if !worker_paused() {
                match run_pass(&client).await {
                    Ok(0) => {}
                    Ok(n) => info!("[media-worker] pass done: {n} stored"),
                    Err(e) => warn!("[media-worker] pass error: {e}"),
                }
            }
            // Honor a pending kick (immediate next pass) else wait the interval.
            let wait = if take_kick() {
                Duration::from_millis(50)
            } else {
                PASS_INTERVAL
            };
            tokio::time::sleep(wait).await;
        }
    });
}

async fn run_pass(client: &reqwest::Client) -> Result<usize, String> {
    // The media cache follows the collection scopes: with both toggles off
    // nothing is collected, so there is nothing to download — idle until a
    // toggle comes back. Hot-reload aware — flipping the toggles starts or
    // stops downloads on the next pass.
    let c = &models::cfg().catalog;
    if !c.persistence_enabled() {
        return Ok(0);
    }
    let pending =
        rocket::tokio::task::spawn_blocking(move || db::pending_saved_original_posts(BATCH))
            .await
            .map_err(|e| format!("pending task: {e}"))?
            .map_err(|e| e.to_string())?;
    let mut done = 0;
    for (id, url, ext) in pending {
        if worker_paused() {
            break;
        }
        if url.is_empty() || ext.is_empty() {
            continue;
        }
        match fetch_original(client, id, &url, &ext).await {
            Ok(true) => done += 1,
            Ok(false) => {}
            Err(e) => warn!("[media-worker] {id}: {e}"),
        }
        tokio::time::sleep(ITEM_DELAY).await;
    }
    Ok(done)
}
