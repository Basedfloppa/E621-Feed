//! Watchlist routes: tag subscriptions and the "new since last visit" counter.
//!
//! Every handler goes through the same account-access gate as the rest of the
//! per-account surface (`get_account_by_id`), so a subscription cannot be
//! created or read for an account the caller cannot address.

use rocket::serde::json::Json;
use rocket_okapi::openapi;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::db_blocking;
use e621_account_parser_api::auth::OwnerToken;
use e621_account_parser_api::db::{self, Subscription, get_account_by_id};
use e621_account_parser_api::errors::ApiError;
use e621_account_parser_api::models::ScoredPost;
use e621_account_parser_api::ratelimit;
use e621_account_parser_api::validation;

#[derive(Deserialize, JsonSchema)]
pub struct TagBody {
    pub tag: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct SeenBody {
    /// Mark one subscription read; omit to mark every subscription read.
    pub tag_id: Option<i64>,
}

async fn require_account(owner_token: String, account_id: i32) -> Result<(), ApiError> {
    db_blocking(move || {
        get_account_by_id(&owner_token, account_id)
            .map(|_| ())
            .map_err(|e| format!("Failed to get account: {e}"))
    })
    .await?;
    Ok(())
}

#[openapi(tag = "Watchlist")]
#[get("/account/<account_id>/subscriptions")]
pub(crate) async fn list_subscriptions(
    account_id: i32,
    owner: OwnerToken,
) -> Result<Json<Vec<Subscription>>, ApiError> {
    validation::validate_account_id(account_id)?;
    let owner_token = owner.0;
    ratelimit::check(&format!("read:owner:{owner_token}"), 240, 60)?;
    require_account(owner_token, account_id).await?;
    let subs = db_blocking(move || db::list_subscriptions(account_id)).await?;
    Ok(Json(subs))
}

#[openapi(tag = "Watchlist")]
#[post("/account/<account_id>/subscriptions", data = "<body>")]
pub(crate) async fn add_subscription(
    account_id: i32,
    body: Json<TagBody>,
    owner: OwnerToken,
) -> Result<Json<Subscription>, ApiError> {
    validation::validate_account_id(account_id)?;
    let owner_token = owner.0;
    ratelimit::check(&format!("subs:owner:{owner_token}"), 30, 60)?;
    require_account(owner_token, account_id).await?;
    let tag = body.into_inner().tag;
    let sub = db_blocking(move || db::add_subscription(account_id, &tag)).await?;
    Ok(Json(sub))
}

#[openapi(tag = "Watchlist")]
#[delete("/account/<account_id>/subscriptions/<tag_id>")]
pub(crate) async fn remove_subscription(
    account_id: i32,
    tag_id: i64,
    owner: OwnerToken,
) -> Result<Json<Vec<Subscription>>, ApiError> {
    validation::validate_account_id(account_id)?;
    let owner_token = owner.0;
    ratelimit::check(&format!("subs:owner:{owner_token}"), 30, 60)?;
    require_account(owner_token, account_id).await?;
    let removed = db_blocking(move || db::remove_subscription(account_id, tag_id)).await?;
    if !removed {
        return Err(ApiError::NotFound("No such subscription".into()));
    }
    let subs = db_blocking(move || db::list_subscriptions(account_id)).await?;
    Ok(Json(subs))
}

/// Mark subscriptions read and return the list with the new counts, so the
/// client does not have to re-fetch.
#[openapi(tag = "Watchlist")]
#[post("/account/<account_id>/subscriptions/seen", data = "<body>")]
pub(crate) async fn mark_subscriptions_seen(
    account_id: i32,
    body: Json<SeenBody>,
    owner: OwnerToken,
) -> Result<Json<Vec<Subscription>>, ApiError> {
    validation::validate_account_id(account_id)?;
    let owner_token = owner.0;
    ratelimit::check(&format!("subs:owner:{owner_token}"), 30, 60)?;
    require_account(owner_token, account_id).await?;
    let tag_id = body.into_inner().tag_id;
    db_blocking(move || db::mark_seen(account_id, tag_id)).await?;
    let subs = db_blocking(move || db::list_subscriptions(account_id)).await?;
    Ok(Json(subs))
}

#[openapi(tag = "Watchlist")]
#[get("/account/<account_id>/subscriptions/new?<limit>")]
pub(crate) async fn new_subscription_posts(
    account_id: i32,
    limit: Option<usize>,
    owner: OwnerToken,
) -> Result<Json<Vec<ScoredPost>>, ApiError> {
    validation::validate_account_id(account_id)?;
    let owner_token = owner.0;
    ratelimit::check(&format!("read:owner:{owner_token}"), 240, 60)?;
    require_account(owner_token, account_id).await?;
    let limit = limit.unwrap_or(20).clamp(1, 100);
    let posts = db_blocking(move || db::new_subscription_posts(account_id, limit)).await?;
    Ok(Json(posts))
}
