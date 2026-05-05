//! Feed routes: feed-interaction logging and `/recommendations`.

use std::collections::HashSet;

use chrono::Utc;
use rocket::serde::json::Json;
use rocket_okapi::openapi;

use crate::db_blocking;
use e621_account_parser_api::auth::OwnerToken;
use e621_account_parser_api::{
    api,
    db::{
        self, collect_local_candidate_ids, get_account_by_id, get_account_preference_profile,
        get_owned_post_ids, get_recently_seen_post_ids, get_tag_counts, hydrate_posts_by_ids,
        record_feed_interaction, upsert_catalog_posts,
    },
    errors::ApiError,
    models::{
        self, cfg, FeedInteractionRequest, Post, ScoredPost, TagCount,
    },
    ratelimit, validation,
};
use e621_account_parser_api::utils::{
    current_global_relation, current_idf, diversify_scored_posts, ScoringContext,
};

#[openapi(tag = "Recommendations")]
#[post("/interaction", data = "<payload>")]
pub(crate) async fn log_feed_interaction(
    payload: Json<FeedInteractionRequest>,
    owner: OwnerToken,
) -> Result<(), ApiError> {
    let body = payload.into_inner();
    let owner_token = owner.0;
    validation::validate_feed_interaction(&body)?;

    if matches!(body.event_type, models::FeedInteractionType::Unknown) {
        debug!("[feed] dropped unknown event_type from forward-compat client");
        return Ok(());
    }
    // Per-device cap: feed scrolling fires impressions per card; anything
    // higher than this is either a re-render storm or a write-flood attempt.
    ratelimit::check(&format!("interaction:owner:{owner_token}"), 120, 60)?;
    db_blocking(move || record_feed_interaction(&owner_token, &body))
        .await
        .map_err(ApiError::from)?;
    Ok(())
}

#[openapi(tag = "Recommendations")]
#[get("/recommendations/<account_id>?<page>&<affinity_threshold>")]
pub(crate) async fn get_recommendations(
    account_id: i32,
    owner: OwnerToken,
    page: Option<i32>,
    affinity_threshold: Option<f32>,
) -> Result<Json<Vec<ScoredPost>>, ApiError> {
    validation::validate_account_id(account_id)?;
    let owner_token = owner.0;
    // Cap per device — admin-authenticated round-trip per request, so an
    // infinite-scroll loop must not burn through the admin quota.
    ratelimit::check(&format!("recs:owner:{owner_token}"), 60, 30)?;
    let cfg = cfg();
    let runtime = cfg.runtime.clone();

    let owner = owner_token;
    // Single blocking hop pulls everything SQLite-backed in one go so the
    // network round-trip below overlaps with SQL latency.
    let (
        tags,
        profile,
        account,
        user_relation,
        seen_ids,
        owned_ids,
        local_candidate_ids,
        explicit_bucket,
    ) = db_blocking(move || -> Result<_, String> {
        let tags: Vec<TagCount> =
            get_tag_counts(account_id).map_err(|e| format!("Failed to get tag counts: {e}"))?;
        let profile = get_account_preference_profile(account_id)
            .map_err(|e| format!("Failed to get account profile: {e}"))?;
        let account = get_account_by_id(&owner, account_id)
            .map_err(|e| format!("Failed to get account: {e}"))?;
        let user_relation = db::load_account_tag_relation(account_id, &tags)
            .map_err(|e| format!("Failed to load user tag relation graph: {e}"))?;
        let seen_ids = get_recently_seen_post_ids(account_id, runtime.dedup_lookback_days)
            .map_err(|e| format!("Failed to load seen post ids: {e}"))?;
        let owned_ids = get_owned_post_ids(account_id)
            .map_err(|e| format!("Failed to load owned post ids: {e}"))?;
        let local_ids = collect_local_candidate_ids(account_id, runtime.local_candidate_limit)
            .map_err(|e| format!("Failed to collect local candidate ids: {e}"))?;
        let bucket = db::get_account_experiment_bucket(account_id)
            .map_err(|e| format!("Failed to load experiment bucket: {e}"))?;
        Ok((
            tags,
            profile,
            account,
            user_relation,
            seen_ids,
            owned_ids,
            local_ids,
            bucket,
        ))
    })
    .await?;

    let (bucket_name, mut priors) = cfg.pick_bucket(account_id, explicit_bucket.as_deref());
    if let Some(bucket) = &bucket_name {
        debug!("recommendations account={account_id} bucket={bucket}");
    }
    priors.now = Utc::now();

    let live_posts: Vec<Post> = api::get_posts(&account, page)
        .await
        .map_err(|e| ApiError::Internal(format!("Failed to fetch posts: {e}")))?;

    // Catalog persistence is fire-and-forget: SQLite is single-writer and
    // infinite scroll fires this several times per second. Pull off the
    // request path so response shape doesn't depend on flush order. IDF is
    // bumped incrementally inside `save_posts_tags_batch`.
    {
        let posts_for_persist = live_posts.clone();
        rocket::tokio::spawn(async move {
            let res = rocket::tokio::task::spawn_blocking(move || -> Result<(), String> {
                upsert_catalog_posts(&posts_for_persist)
                    .map_err(|e| format!("Failed to store recommendation catalog posts: {e}"))?;
                // Skip cooccurrence: candidate browse posts aren't user truth.
                db::save_posts_tags_batch(&posts_for_persist, &HashSet::new(), false)
                    .map_err(|e| format!("Failed to store recommendation tags: {e}"))?;
                Ok(())
            })
            .await;
            if let Ok(Err(e)) = res {
                warn!("background recommendation persist failed: {e}");
            } else if let Err(e) = res {
                warn!("background recommendation persist task panicked: {e}");
            }
        });
    }

    // Hydrate locals (skip owned/seen/already-in-live).
    let live_ids: HashSet<i64> = live_posts.iter().map(|p| p.id).collect();
    let local_to_hydrate: Vec<i64> = local_candidate_ids
        .into_iter()
        .filter(|id| !live_ids.contains(id) && !owned_ids.contains(id) && !seen_ids.contains(id))
        .collect();
    let local_posts = if local_to_hydrate.is_empty() {
        Vec::new()
    } else {
        db_blocking(move || hydrate_posts_by_ids(&local_to_hydrate)).await?
    };

    // Filter live posts through the same dedup lens.
    let mut combined: Vec<Post> = Vec::with_capacity(live_posts.len() + local_posts.len());
    for post in live_posts {
        if owned_ids.contains(&post.id) || seen_ids.contains(&post.id) {
            continue;
        }
        combined.push(post);
    }
    let mut combined_ids: HashSet<i64> = combined.iter().map(|p| p.id).collect();
    for post in local_posts {
        if combined_ids.insert(post.id) {
            combined.push(post);
        }
    }

    let idf = current_idf();
    let global_relation = current_global_relation();

    let ctx = ScoringContext::new(
        &tags,
        &priors,
        &idf,
        &profile,
        &global_relation,
        &user_relation,
    );

    let mut scored: Vec<ScoredPost> = Vec::with_capacity(combined.len());
    for post in combined {
        let (s, breakdown) = ctx.score(&post);
        scored.push(ScoredPost {
            post,
            score: s,
            breakdown: Some(breakdown),
        });
    }

    if let Some(threshold) = affinity_threshold {
        scored.retain(|sp| sp.score >= threshold);
    }
    let scored = diversify_scored_posts(scored, &priors);

    Ok(Json(scored))
}
