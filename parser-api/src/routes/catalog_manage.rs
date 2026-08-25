//! Catalog manage + media queue control routes (docs/offline-catalog.md).
//!
//! Owner-gated read/mutating controls for the local catalog and the background
//! media download worker: queue status, pause/resume/kick, queued-item listing,
//! deleting a saved post from the catalog, and (re)categorizing a post by
//! pinning it to an explicit group tag.

use rocket::serde::json::Json;
use rocket_okapi::openapi;

use crate::db_blocking;
use e621_account_parser_api::{
    auth::OwnerToken,
    db::{
        delete_catalog_post, get_account_by_id, pending_saved_original_posts, post_still_saved,
        queue_stats,
    },
    errors::ApiError,
    media_fetch_worker::{kick_worker, set_worker_paused, worker_paused},
    media_store,
    models::{ActionOk, MediaQueueItem, MediaQueueStatus},
    ratelimit::{self, ClientIp},
};

/// Verify the requester owns `account_id` (minted owner token must match).
async fn owner_check(owner: &OwnerToken, account_id: i32) -> Result<(), ApiError> {
    let owner_token = owner.0.clone();
    db_blocking(move || get_account_by_id(&owner_token, account_id))
        .await
        .map_err(ApiError::from_string)?;
    Ok(())
}

/// Read throttle: catalog enabled + ownership + read-bucket rate limits.
async fn read_guard(
    owner: &OwnerToken,
    client_ip: &ClientIp,
    account_id: i32,
) -> Result<(), ApiError> {
    crate::routes::catalog::catalog_gate()?;
    owner_check(owner, account_id).await?;
    ratelimit::check(&format!("catalog-manage-read:{}", owner.0), 120, 30)?;
    ratelimit::check(&format!("catalog-manage-read-ip:{}", client_ip.0), 240, 60)?;
    Ok(())
}

/// Write throttle: catalog enabled + ownership + write-bucket rate limits.
async fn write_guard(
    owner: &OwnerToken,
    client_ip: &ClientIp,
    account_id: i32,
) -> Result<(), ApiError> {
    crate::routes::catalog::catalog_gate()?;
    owner_check(owner, account_id).await?;
    ratelimit::check(&format!("catalog-manage-write:{}", owner.0), 60, 30)?;
    ratelimit::check(&format!("catalog-manage-write-ip:{}", client_ip.0), 120, 30)?;
    Ok(())
}

/// Current queue/worker status as JSON.
fn status_json() -> Result<Json<MediaQueueStatus>, ApiError> {
    let (pending, stored, bytes) = queue_stats().map_err(ApiError::from_string)?;
    Ok(Json(MediaQueueStatus {
        paused: worker_paused(),
        pending,
        stored,
        bytes,
    }))
}

/// Current status of the media download queue / worker.
#[openapi(tag = "Catalog")]
#[get("/catalog/<account_id>/media/status")]
pub(crate) async fn get_media_queue_status(
    account_id: i32,
    owner: OwnerToken,
    client_ip: ClientIp,
) -> Result<Json<MediaQueueStatus>, ApiError> {
    read_guard(&owner, &client_ip, account_id).await?;
    status_json()
}

/// Pause the background media worker.
#[openapi(tag = "Catalog")]
#[post("/catalog/<account_id>/media/pause")]
pub(crate) async fn pause_media_worker(
    account_id: i32,
    owner: OwnerToken,
    client_ip: ClientIp,
) -> Result<Json<MediaQueueStatus>, ApiError> {
    write_guard(&owner, &client_ip, account_id).await?;
    set_worker_paused(true);
    crate::audit::event("catalog.media.pause")
        .field("account_id", account_id)
        .emit();
    status_json()
}

/// Resume the background media worker.
#[openapi(tag = "Catalog")]
#[post("/catalog/<account_id>/media/resume")]
pub(crate) async fn resume_media_worker(
    account_id: i32,
    owner: OwnerToken,
    client_ip: ClientIp,
) -> Result<Json<MediaQueueStatus>, ApiError> {
    write_guard(&owner, &client_ip, account_id).await?;
    set_worker_paused(false);
    kick_worker();
    crate::audit::event("catalog.media.resume")
        .field("account_id", account_id)
        .emit();
    status_json()
}

/// Kick the worker to run a pass immediately (skips the idle interval).
#[openapi(tag = "Catalog")]
#[post("/catalog/<account_id>/media/kick")]
pub(crate) async fn kick_media_worker(
    account_id: i32,
    owner: OwnerToken,
    client_ip: ClientIp,
) -> Result<Json<MediaQueueStatus>, ApiError> {
    write_guard(&owner, &client_ip, account_id).await?;
    kick_worker();
    crate::audit::event("catalog.media.kick")
        .field("account_id", account_id)
        .emit();
    status_json()
}

/// Clear the entire local media cache: remove the on-disk originals and wipe
/// the `media_entries` index (the links to local files). Stored drops to 0 and
/// saved posts become pending again for re-download on the next worker pass.
#[openapi(tag = "Catalog")]
#[delete("/catalog/<account_id>/media")]
pub(crate) async fn delete_media_cache(
    account_id: i32,
    owner: OwnerToken,
    client_ip: ClientIp,
) -> Result<Json<MediaQueueStatus>, ApiError> {
    write_guard(&owner, &client_ip, account_id).await?;
    let cleared = db_blocking(media_store::clear_cache)
        .await
        .map_err(ApiError::from_string)?;
    crate::audit::event("catalog.media.clear")
        .field("account_id", account_id)
        .field("cleared", cleared)
        .emit();
    status_json()
}

/// List the queued (pending) saved posts awaiting their original download.
#[openapi(tag = "Catalog")]
#[get("/catalog/<account_id>/media/queue?<limit>")]
pub(crate) async fn get_media_queue(
    account_id: i32,
    limit: Option<i64>,
    owner: OwnerToken,
    client_ip: ClientIp,
) -> Result<Json<Vec<MediaQueueItem>>, ApiError> {
    read_guard(&owner, &client_ip, account_id).await?;
    let limit = limit.unwrap_or(50).clamp(1, 500);
    let rows = db_blocking(move || pending_saved_original_posts(limit))
        .await
        .map_err(ApiError::from_string)?;
    Ok(Json(
        rows.into_iter()
            .map(|(post_id, file_url, _ext)| MediaQueueItem { post_id, file_url })
            .collect(),
    ))
}

/// Delete a saved post from the local catalog (association + its media file).
#[openapi(tag = "Catalog")]
#[delete("/catalog/<account_id>/post/<post_id>")]
pub(crate) async fn delete_catalog_post_route(
    account_id: i32,
    post_id: i64,
    owner: OwnerToken,
    client_ip: ClientIp,
) -> Result<Json<ActionOk>, ApiError> {
    write_guard(&owner, &client_ip, account_id).await?;
    let removed = db_blocking(move || delete_catalog_post(account_id, post_id))
        .await
        .map_err(ApiError::from_string)?;
    if removed == 0 {
        return Err(ApiError::NotFound("post is not in the catalog".into()));
    }
    // The stored original is global (one file per post, shared by every
    // account that saved it). Cascade it only when this deletion was the
    // LAST owner — other accounts may still reference the same file.
    let still_owned = db_blocking(move || post_still_saved(post_id))
        .await
        .map_err(ApiError::from_string)?;
    let media_removed = if still_owned {
        false
    } else {
        db_blocking(move || media_store::delete_and_unindex(post_id))
            .await
            .map_err(ApiError::from_string)?
    };
    crate::audit::event("catalog.post.delete")
        .field("account_id", account_id)
        .field("post_id", post_id)
        .field("media_removed", media_removed)
        .emit();
    Ok(Json(ActionOk { ok: true }))
}
