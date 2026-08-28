//! Local catalog routes (docs/offline-catalog.md).
//!
//! The catalog is a view over the owner's saved posts (`accounts_post`),
//! searched directly. Post info is always collected (account sync is never
//! gated), so these routes are always available; the catalog persistence
//! toggles only control the local media-cache build (media_fetch_worker).

use rocket::serde::json::Json;
use rocket_okapi::openapi;

use crate::db_blocking;
use e621_account_parser_api::{
    auth::OwnerToken,
    db::{catalog_search_post_ids, catalog_tag_suggest, get_account_by_id, hydrate_posts_by_ids},
    errors::ApiError,
    models::Post,
    ratelimit::{self, ClientIp},
    validation,
};

/// Shared guard for catalog routes: account id valid, per-owner + per-IP
/// throttling, and ownership check against the owner token. No catalog
/// feature toggle here — post info is always collected, so the view is always
/// available; the persistence toggles only gate the media-cache worker.
async fn guard_catalog(
    owner: &OwnerToken,
    client_ip: &ClientIp,
    account_id: i32,
) -> Result<(), ApiError> {
    validation::validate_account_id(account_id)?;
    let owner_token = owner.0.clone();
    ratelimit::check(&format!("catalog:owner:{owner_token}"), 60, 30)?;
    ratelimit::check(&format!("catalog:ip:{}", client_ip.0), 120, 60)?;
    // Ownership first so a minted token can't scan an arbitrary account's
    // saved catalog.
    let _acc = db_blocking(move || get_account_by_id(&owner_token, account_id))
        .await
        .map_err(ApiError::from_string)?;
    Ok(())
}

/// Full-text / tag search over the owner's locally-saved catalog posts.
///
/// `?query=` is whitespace-separated e621 tag terms, ANDed (each word must be
/// a tag the post carries, matched case-insensitively). Mirrors the search
/// page UX but confined to posts already saved in the local catalog. Paginated
/// with `?page=` / `?limit=` (1-based page, like browse).
#[openapi(tag = "Catalog")]
#[get("/catalog/<account_id>/search?<query>&<page>&<limit>")]
pub(crate) async fn get_catalog_search(
    account_id: i32,
    query: String,
    page: Option<i64>,
    limit: Option<i64>,
    owner: OwnerToken,
    client_ip: ClientIp,
) -> Result<Json<Vec<Post>>, ApiError> {
    guard_catalog(&owner, &client_ip, account_id).await?;

    let terms: Vec<String> = query
        .split_whitespace()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(ToString::to_string)
        .collect();
    let limit = limit.unwrap_or(50).clamp(1, 500);
    let page = page.unwrap_or(1).clamp(1, 1_000_000);
    let offset = (page - 1) * limit;
    let ids = db_blocking(move || catalog_search_post_ids(account_id, &terms, limit, offset))
        .await
        .map_err(ApiError::from_string)?;
    let posts: Vec<Post> = db_blocking(move || hydrate_posts_by_ids(&ids))
        .await
        .map_err(ApiError::from_string)?;

    Ok(Json(posts))
}

/// Local tag autocomplete for the catalog search box.
///
/// `?prefix=` returns tag names the account's saved posts carry, matched from
/// the **local** DB (no e621 round-trip), ordered by how many saved posts use
/// each tag. `?limit=` caps the count (default 20).
#[openapi(tag = "Catalog")]
#[get("/catalog/<account_id>/tag/suggest?<prefix>&<limit>")]
pub(crate) async fn get_catalog_tag_suggest(
    account_id: i32,
    prefix: String,
    limit: Option<i64>,
    owner: OwnerToken,
    client_ip: ClientIp,
) -> Result<Json<Vec<String>>, ApiError> {
    guard_catalog(&owner, &client_ip, account_id).await?;

    let limit = limit.unwrap_or(20).clamp(1, 100);
    let tags = db_blocking(move || catalog_tag_suggest(account_id, &prefix, limit))
        .await
        .map_err(ApiError::from_string)?;
    Ok(Json(tags))
}
