//! Browse routes — proxy e621 search queries for Trending, Favorites, etc.
//! These bypass our local scoring pipeline and return raw posts.
//!
//! Browse responses are progressively persisted to the local catalog:
//! - `/browse/favorites` saves posts + links them to the account (`accounts_post`)
//! - `/browse/trending`  saves posts to the catalog only (no account link)

use rocket::serde::json::Json;
use rocket_okapi::openapi;

use crate::db_blocking;
use e621_account_parser_api::{
    api, audit,
    auth::OwnerToken,
    db::{get_account_by_id, save_posts, upsert_catalog_posts},
    errors::ApiError,
    models::Post,
    validation,
};

/// Fire-and-forget persistence of browse posts to the local catalog.
///
/// * `source` — "trending" or "favorites", used for audit-logging.
/// * `link_to_account` — when `true`, also inserts `accounts_post` rows
///   so the posts are associated with this account (used for favorites).
///
/// Errors are non-fatal: the response has already been sent to the client.
fn spawn_browse_persist(
    posts: Vec<Post>,
    source: &'static str,
    account_id: i32,
    link_to_account: bool,
) {
    rocket::tokio::spawn(async move {
        let count = posts.len();
        let res = rocket::tokio::task::spawn_blocking(move || -> Result<usize, String> {
            if link_to_account {
                // save_posts handles both catalog upsert AND accounts_post link
                save_posts(&posts, account_id)
                    .map_err(|e| format!("Failed to save posts with account link: {e}"))?;
            } else {
                upsert_catalog_posts(&posts)
                    .map_err(|e| format!("Failed to upsert catalog posts: {e}"))?;
            }

            // Save tags (skip cooccurrence — browse posts aren't user preferences)
            e621_account_parser_api::db::save_posts_tags_batch(
                &posts,
                &std::collections::HashSet::new(),
                false, // track_cooccurrence: false
                None,  // account_id: none — no account-level cooccurrence
            )
            .map_err(|e| format!("Failed to save tags: {e}"))?;

            Ok(count)
        })
        .await;

        match res {
            Ok(Ok(n)) => {
                debug!("[browse] persist {source}: saved {n} posts for account {account_id}");
                audit::event("browse.persist")
                    .field("source", source)
                    .field("account_id", account_id)
                    .field("count", n)
                    .emit();
                e621_account_parser_api::metrics::METRICS
                    .browse_views_total
                    .with_label_values(&[source])
                    .inc();
            }
            Ok(Err(e)) => {
                warn!("[browse] persist {source} failed for account {account_id}: {e}");
                audit::event("browse.persist_failed")
                    .field("source", source)
                    .field("account_id", account_id)
                    .field("error", e)
                    .emit_err();
            }
            Err(e) => {
                warn!("[browse] persist {source} task panicked for account {account_id}: {e}");
                audit::event("browse.persist_failed")
                    .field("source", source)
                    .field("kind", "panic")
                    .field("account_id", account_id)
                    .emit_err();
            }
        }
    });
}

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

    // Сохраняем в каталог (без привязки к аккаунту — trending не является
    // явным предпочтением пользователя). Fire-and-forget, не задерживает ответ.
    spawn_browse_persist(posts.clone(), "trending", account_id, false);

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

    // Сохраняем в каталог И привязываем к аккаунту — это фавориты пользователя.
    // Fire-and-forget, не задерживает ответ.
    spawn_browse_persist(posts.clone(), "favorites", account_id, true);

    Ok(Json(posts))
}
