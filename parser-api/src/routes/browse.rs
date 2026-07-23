//! Browse routes — proxy e621 search queries for Trending, Favorites, etc.
//! These bypass our local scoring pipeline and return raw posts.

use rocket::serde::json::Json;
use rocket_okapi::openapi;

use crate::db_blocking;
use e621_account_parser_api::{
    api, auth::OwnerToken,
    db::get_account_by_id,
    errors::ApiError,
    models::Post,
    validation,
};

/// Fetch trending posts (sorted by hotness on e621).
#[openapi(tag = "Browse")]
#[get("/browse/trending/<account_id>?<page>")]
pub(crate) async fn get_trending(
    account_id: i32,
    page: Option<i32>,
    owner: OwnerToken,
) -> Result<Json<Vec<Post>>, ApiError> {
    validation::validate_account_id(account_id)?;
    let owner_token = owner.0;

    // Verify ownership.
    let account = db_blocking({
        let ot = owner_token.clone();
        move || get_account_by_id(&ot, account_id).map_err(|e| e.to_string())
    })
    .await?;

    let blacklist_tags = &account.blacklist;

    let posts = api::get_posts_by_tags(blacklist_tags, "order:hot", page)
        .await
        .map_err(ApiError::Internal)?;

    Ok(Json(posts))
}

/// Fetch the user's favorited posts from e621.
#[openapi(tag = "Browse")]
#[get("/browse/favorites/<account_id>?<page>")]
pub(crate) async fn get_favorites(
    account_id: i32,
    page: Option<i32>,
    owner: OwnerToken,
) -> Result<Json<Vec<Post>>, ApiError> {
    validation::validate_account_id(account_id)?;
    let owner_token = owner.0;

    let account = db_blocking({
        let ot = owner_token.clone();
        move || get_account_by_id(&ot, account_id).map_err(|e| e.to_string())
    })
    .await?;

    let blacklist_tags = &account.blacklist;
    let query = format!("fav:{}", account.name);

    let posts = api::get_posts_by_tags(blacklist_tags, &query, page)
        .await
        .map_err(ApiError::Internal)?;

    Ok(Json(posts))
}
