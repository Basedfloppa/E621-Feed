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
    api,
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
#[openapi(tag = "Posts")]
#[get("/posts/<id>")]
pub(crate) async fn get_single_post(id: i64, client_ip: ClientIp) -> Result<Json<Post>, ApiError> {
    if id <= 0 {
        return Err(ApiError::BadRequest(
            "post id must be a positive integer".into(),
        ));
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
    // Per-IP public-viewer budget; each pool request costs two admin-key
    // tokens (envelope + by-ids hydration), so this must be tighter.
    ratelimit::check(&format!("public-viewer:{}", client_ip.0), 60, 15)?;
    ratelimit::check(&format!("pool_posts:{}", client_ip.0), 60, 15)?;
    let posts = api::get_pool_posts(pool_id)
        .await
        .map_err(ApiError::from_string)?;
    Ok(Json(posts))
}
