//! Public post-viewer data routes — comments for a post.
//!
//! Comments are public e621 content proxied through the server; the shared
//! admin-key budget in `api::fetch_authed_text` still applies. A per-IP
//! rate limit keeps anonymous traffic from hammering upstream.
//!
//! NOTE: similar posts are NOT defined here — the app already exposes an
//! authenticated, account-scored endpoint at `GET /posts/<post_id>/similar`
//! (`routes::feed::get_similar_posts`), which the viewer reuses.

use rocket::serde::json::Json;
use rocket_okapi::openapi;

use e621_account_parser_api::{
    api, db,
    errors::ApiError,
    models::{Comment, Post},
    ratelimit::{self, ClientIp},
};

/// Public comments on a post (newest first, capped by `?limit`).
#[openapi(tag = "Posts")]
#[get("/posts/<id>/comments?<limit>")]
pub(crate) async fn post_comments(
    id: i64,
    limit: Option<i64>,
    client_ip: ClientIp,
) -> Result<Json<Vec<Comment>>, ApiError> {
    if id <= 0 {
        return Err(ApiError::BadRequest(
            "post id must be a positive integer".into(),
        ));
    }
    // Each IP gets its own public-viewer budget so a single anonymous attacker
    // cannot monopolize the shared anonymous-viewer allowance for every consumer
    // (a single global `e621:public-viewer` cap was exhaustible by one IP). The
    // per-IP route caps also keep the aggregate anonymous spend below the shared
    // `e621:admin-key` pool (240/min).
    ratelimit::check(&format!("public-viewer:{}", client_ip.0), 60, 15)?;
    ratelimit::check(&format!("post_comments:{}", client_ip.0), 60, 10)?;
    let comments = api::get_post_comments(id, limit.unwrap_or(50))
        .await
        .map_err(ApiError::from_string)?;
    Ok(Json(comments))
}

/// Single post by id — used by the viewer for parent/child navigation.
///
/// **Local-first**: if the post is already in the local catalog it is served
/// from the local DB without any e621 request (this is the point of the local
/// catalog — see docs/offline-catalog.md). Live fetch only for IDs absent
/// locally.
#[openapi(tag = "Posts")]
#[get("/posts/<id>")]
pub(crate) async fn get_single_post(id: i64, client_ip: ClientIp) -> Result<Json<Post>, ApiError> {
    if id <= 0 {
        return Err(ApiError::BadRequest(
            "post id must be a positive integer".into(),
        ));
    }
    // Serve from the local catalog first — no network, no admin-key spend.
    if let Some(local) = db::get_post_by_id(id).map_err(ApiError::from_string)? {
        return Ok(Json(local));
    }
    // Per-IP public-viewer budget shared with the other unauthenticated viewer
    // routes — see `post_comments` for the rationale.
    ratelimit::check(&format!("public-viewer:{}", client_ip.0), 60, 15)?;
    ratelimit::check(&format!("post:{}", client_ip.0), 120, 30)?;
    let mut posts = api::get_posts_by_ids(&[id])
        .await
        .map_err(ApiError::from_string)?;
    match posts.pop() {
        Some(p) => Ok(Json(p)),
        None => Err(ApiError::NotFound("post not found or removed".into())),
    }
}

/// Every post in a pool, in pool order — used by the viewer for pool
/// navigation (the current post's `pools` ids drive the request).
///
/// **Local-first**: when `catalog.pool_membership` is on and the pool is known
/// locally, posts are served from `pool_posts` without e621. Otherwise the pool
/// is live-fetched, and the membership is persisted for next time.
#[openapi(tag = "Posts")]
#[get("/pools/<pool_id>/posts")]
pub(crate) async fn get_pool_posts(
    pool_id: i64,
    client_ip: ClientIp,
) -> Result<Json<Vec<Post>>, ApiError> {
    if pool_id <= 0 {
        return Err(ApiError::BadRequest(
            "pool id must be a positive integer".into(),
        ));
    }
    use e621_account_parser_api::models::cfg;
    // Local-first: serve from stored pool membership when enabled and present.
    if cfg().catalog.pool_membership
        && let Ok(members) = db::get_pool_members(pool_id)
        && !members.is_empty()
    {
        let ids: Vec<i64> = members.iter().map(|(id, _pos)| *id).collect();
        let hydrated = db::hydrate_posts_by_ids(&ids).map_err(ApiError::from_string)?;
        if hydrated.len() == ids.len() {
            return Ok(Json(hydrated));
        }
    }
    // Per-IP public-viewer budget; each pool request costs two admin-key
    // tokens (envelope + by-ids hydration), so this must be tighter.
    ratelimit::check(&format!("public-viewer:{}", client_ip.0), 60, 15)?;
    ratelimit::check(&format!("pool_posts:{}", client_ip.0), 60, 15)?;
    let posts = api::get_pool_posts(pool_id)
        .await
        .map_err(ApiError::from_string)?;
    // Persist membership for offline use (opt-in). `pool_posts.post_id` has an
    // FK to `posts(id)`, so the member posts must already be in the catalog or
    // `save_pool` fails the FK check and rolls back (silently breaking offline
    // pool navigation). Upsert the fetched posts first to satisfy the FK.
    if cfg().catalog.pool_membership {
        if let Err(e) = db::upsert_catalog_posts(&posts) {
            warn!("[catalog] pool {pool_id} post persist failed: {e}");
        } else {
            let members = posts
                .iter()
                .enumerate()
                .map(|(i, p)| (p.id, i as i64))
                .collect::<Vec<_>>();
            if let Err(e) = db::save_pool(pool_id, "", &members) {
                warn!("[catalog] pool {pool_id} save failed: {e}");
            }
        }
    }
    Ok(Json(posts))
}
