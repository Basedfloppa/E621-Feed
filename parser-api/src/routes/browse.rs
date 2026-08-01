//! Browse routes — proxy e621 search queries for Trending, Favorites, etc.
//! These bypass our local scoring pipeline and return raw posts.
//!
//! Browse responses are progressively persisted to the local catalog:
//! - `/browse/favorites` saves posts + links them to the account (`accounts_post`)
//! - `/browse/trending`  saves posts to the catalog only (no account link)

use chrono::Utc;
use rocket::serde::json::Json;
use rocket_okapi::openapi;

use crate::db_blocking;
use e621_account_parser_api::{
    api, audit,
    auth::OwnerToken,
    db::{
        get_account_by_id, get_account_preference_profile, get_tag_counts, save_posts,
        upsert_catalog_posts,
    },
    errors::ApiError,
    load_monitor::Priority,
    models::{Post, ScoredPost, cfg},
    utils::{CachedPostFeatures, ScoringContext, current_global_relation, current_idf},
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

/// Proxy a user-supplied e621 tag query through the server. The browser never
/// contacts e621 directly, and the selected account's blacklist still applies.
#[openapi(tag = "Browse")]
#[get("/browse/search/<account_id>?<query>&<page>&<limit>")]
pub(crate) async fn search_posts(
    account_id: i32,
    query: &str,
    page: Option<i32>,
    limit: Option<i32>,
    owner: OwnerToken,
) -> Result<Json<Vec<Post>>, ApiError> {
    validation::validate_account_id(account_id)?;
    let query = query.trim();
    if query.is_empty() || query.len() > 250 || query.chars().any(char::is_control) {
        return Err(ApiError::BadRequest(
            "provide a valid e621 tag query (1–250 characters)".into(),
        ));
    }
    let owner_token = owner.0;
    let account =
        db_blocking(move || get_account_by_id(&owner_token, account_id).map_err(|e| e.clone()))
            .await?;
    let posts = api::get_posts_by_tags(&account.blacklist, query, page, limit, Priority::Live)
        .await
        .map_err(ApiError::Internal)?;
    spawn_browse_persist(posts.clone(), "search", account_id, false);
    Ok(Json(posts))
}

/// Search and score the resulting posts against the selected account's profile.
#[openapi(tag = "Browse")]
#[get("/browse/search_scored/<account_id>?<query>&<page>&<limit>")]
pub(crate) async fn search_scored_posts(
    account_id: i32,
    query: &str,
    page: Option<i32>,
    limit: Option<i32>,
    owner: OwnerToken,
) -> Result<Json<Vec<ScoredPost>>, ApiError> {
    validation::validate_account_id(account_id)?;
    let query = query.trim();
    if query.is_empty() || query.len() > 250 || query.chars().any(char::is_control) {
        return Err(ApiError::BadRequest(
            "provide a valid e621 tag query (1–250 characters)".into(),
        ));
    }
    let owner_token = owner.0;
    let (account, tags, profile) = rocket::tokio::join!(
        db_blocking(move || get_account_by_id(&owner_token, account_id).map_err(|e| e.clone())),
        db_blocking(move || get_tag_counts(account_id).map_err(|e| e.clone())),
        db_blocking(move || get_account_preference_profile(account_id).map_err(|e| e.clone())),
    );
    let account = account?;
    let tags = tags?;
    let profile = profile?;
    let relation_tags = tags.clone();
    let user_relation = db_blocking(move || {
        e621_account_parser_api::db::load_account_tag_relation(account_id, &relation_tags)
            .map_err(|e| e.clone())
    })
    .await?;
    let posts = api::get_posts_by_tags(&account.blacklist, query, page, limit, Priority::Live)
        .await
        .map_err(ApiError::Internal)?;
    spawn_browse_persist(posts.clone(), "search", account_id, false);

    let idf = current_idf();
    let global_relation = current_global_relation();
    let mut priors = cfg().priors.clone();
    priors.now = Utc::now();
    let ctx = ScoringContext::new(
        &tags,
        &priors,
        &idf,
        &profile,
        &global_relation,
        &user_relation,
    );
    let mut scored: Vec<ScoredPost> = posts
        .into_iter()
        .map(|post| {
            let features = CachedPostFeatures::from_post_with_user(
                &post,
                &idf,
                &global_relation,
                Some(&user_relation),
            );
            let (score, breakdown, _) = ctx.score_cached_with_metrics(&features);
            ScoredPost {
                post,
                score,
                breakdown: Some(breakdown),
            }
        })
        .collect();
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(Json(scored))
}

/// Fetch trending posts, scored against the account's preference profile.
/// Returns a ranked list of scored posts from the `order:hot` e621 feed,
/// filtered by personal affinity — "Trending for You".
#[openapi(tag = "Browse")]
#[get("/browse/trending_scored/<account_id>?<page>&<affinity_threshold>")]
pub(crate) async fn get_trending_scored(
    account_id: i32,
    page: Option<i32>,
    affinity_threshold: Option<f32>,
    owner: OwnerToken,
) -> Result<Json<Vec<ScoredPost>>, ApiError> {
    validation::validate_account_id(account_id)?;
    let affinity_threshold = validation::validate_affinity_threshold(affinity_threshold)?;
    let owner_token = owner.0;

    // Verify ownership and load profile data in parallel.
    let (account, tags, profile) = rocket::tokio::join!(
        db_blocking(move || get_account_by_id(&owner_token, account_id).map_err(|e| e.clone())),
        db_blocking(move || get_tag_counts(account_id).map_err(|e| e.clone())),
        db_blocking(move || get_account_preference_profile(account_id).map_err(|e| e.clone())),
    );
    let account = account?;
    let tags = tags?;
    let profile = profile?;

    let relation_tags = tags.clone();
    let user_relation = db_blocking(move || {
        e621_account_parser_api::db::load_account_tag_relation(account_id, &relation_tags)
            .map_err(|e| e.clone())
    })
    .await?;

    // Fetch trending posts from e621.
    let posts = api::get_posts_by_tags(&account.blacklist, "order:hot", page, None, Priority::Live)
        .await
        .map_err(ApiError::Internal)?;

    // Fire-and-forget persist to catalog (same as get_trending).
    spawn_browse_persist(posts.clone(), "trending", account_id, false);

    // Score against the account profile.
    let idf = current_idf();
    let global_relation = current_global_relation();
    let mut priors = cfg().priors.clone();
    priors.now = Utc::now();
    let ctx = ScoringContext::new(
        &tags,
        &priors,
        &idf,
        &profile,
        &global_relation,
        &user_relation,
    );

    let mut scored: Vec<ScoredPost> = posts
        .into_iter()
        .map(|post| {
            let features = CachedPostFeatures::from_post_with_user(
                &post,
                &idf,
                &global_relation,
                Some(&user_relation),
            );
            let (score, breakdown, _) = ctx.score_cached_with_metrics(&features);
            ScoredPost {
                post,
                score,
                breakdown: Some(breakdown),
            }
        })
        .collect();

    // Apply affinity threshold if provided.
    if let Some(threshold) = affinity_threshold {
        scored.retain(|sp| sp.score >= threshold);
    }

    // Sort by score descending.
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    audit::event("browse.trending_scored")
        .field("account_id", account_id)
        .field("returned", scored.len())
        .emit();
    e621_account_parser_api::metrics::METRICS
        .browse_views_total
        .with_label_values(&["trending_scored"])
        .inc();

    Ok(Json(scored))
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
        move || get_account_by_id(&ot, account_id).map_err(|e| e.clone())
    })
    .await?;

    let blacklist_tags = &account.blacklist;

    let posts = api::get_posts_by_tags(blacklist_tags, "order:hot", page, None, Priority::Live)
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
        move || get_account_by_id(&ot, account_id).map_err(|e| e.clone())
    })
    .await?;

    let blacklist_tags = &account.blacklist;
    let query = format!("fav:{}", account.name);

    let posts = api::get_posts_by_tags(blacklist_tags, &query, page, None, Priority::Live)
        .await
        .map_err(ApiError::Internal)?;

    // Сохраняем в каталог И привязываем к аккаунту — это фавориты пользователя.
    // Fire-and-forget, не задерживает ответ.
    spawn_browse_persist(posts.clone(), "favorites", account_id, true);

    Ok(Json(posts))
}
