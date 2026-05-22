//! Feed routes: feed-interaction logging and `/recommendations`.

use std::collections::HashSet;

use chrono::Utc;
use rayon::prelude::*;
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
        self, cfg, FeedInteractionRequest, Post, ScoredPost,
    },
    ratelimit, validation,
};
use e621_account_parser_api::utils::{
    current_global_relation, current_idf, diversify_scored_posts, CachedPostFeatures,
    ChannelTiming, PipelineMetrics, ScoringContext, ScoringMetrics,
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
    validation::validate_recommendations_page(page)?;
    let affinity_threshold = validation::validate_affinity_threshold(affinity_threshold)?;
    let owner_token = owner.0;
    // Cap per device — admin-authenticated round-trip per request, so an
    // infinite-scroll loop must not burn through the admin quota.
    ratelimit::check(&format!("recs:owner:{owner_token}"), 60, 30)?;
    let cfg = cfg();
    let runtime = cfg.runtime.clone();

    let mut pipe = PipelineMetrics::new("recommendations");

    let owner = owner_token;
    // Parallelise independent SQLite reads across r2d2 pool connections.
    // load_account_tag_relation depends on get_tag_counts, so it runs
    // in a second wave after tag counts are ready.
    let rt = runtime.clone();
    let (tags_res, profile_res, account_res, seen_res, owned_res, local_res, bucket_res) = rocket::tokio::join!(
        rocket::tokio::task::spawn_blocking(move || {
            get_tag_counts(account_id).map_err(|e| format!("Failed to get tag counts: {e}"))
        }),
        rocket::tokio::task::spawn_blocking(move || {
            get_account_preference_profile(account_id).map_err(|e| format!("Failed to get account profile: {e}"))
        }),
        rocket::tokio::task::spawn_blocking(move || {
            get_account_by_id(&owner, account_id).map_err(|e| format!("Failed to get account: {e}"))
        }),
        rocket::tokio::task::spawn_blocking(move || {
            get_recently_seen_post_ids(account_id, rt.dedup_lookback_days).map_err(|e| format!("Failed to load seen post ids: {e}"))
        }),
        rocket::tokio::task::spawn_blocking(move || {
            get_owned_post_ids(account_id).map_err(|e| format!("Failed to load owned post ids: {e}"))
        }),
        rocket::tokio::task::spawn_blocking(move || {
            collect_local_candidate_ids(account_id, runtime.local_candidate_limit).map_err(|e| format!("Failed to collect local candidate ids: {e}"))
        }),
        rocket::tokio::task::spawn_blocking(move || {
            db::get_account_experiment_bucket(account_id).map_err(|e| format!("Failed to load experiment bucket: {e}"))
        }),
    );

    let tags = tags_res.map_err(|e| format!("Join error: {e}"))??;
    let profile = profile_res.map_err(|e| format!("Join error: {e}"))??;
    let account = account_res.map_err(|e| format!("Join error: {e}"))??;
    let seen_ids = seen_res.map_err(|e| format!("Join error: {e}"))??;
    let owned_ids = owned_res.map_err(|e| format!("Join error: {e}"))??;
    let mut local_candidate_ids = local_res.map_err(|e| format!("Join error: {e}"))??;
    let explicit_bucket = bucket_res.map_err(|e| format!("Join error: {e}"))??;

    // Filter local candidates through the account's blacklist. Live posts
    // from e621 already have the blacklist applied (api.rs sends
    // `-tag1 -tag2` to the e621 API), but local catalog candidates are
    // queried without a blacklist filter — without this step blacklisted
    // content can surface from the local pool.
    //
    // We only extract simple tag names (no e621 search syntax like
    // `-rating:s` or `young furry`) — complex expressions are best-effort
    // and will be ignored here; they still apply to live e621 posts.
    let blacklisted_simple_tags: Vec<String> = account
        .blacklist
        .lines()
        .flat_map(|l| l.split_whitespace().filter(|t| !t.is_empty()))
        .filter(|t| !t.contains(':') && !t.starts_with('-'))
        .map(|t| t.to_lowercase())
        .collect();
    if !blacklisted_simple_tags.is_empty() && !local_candidate_ids.is_empty() {
        let ids_for_filter = local_candidate_ids.clone();
        let tags_for_filter = blacklisted_simple_tags.clone();
        let blacklisted_ids: HashSet<i64> = db_blocking(move || {
            db::load_blacklisted_post_ids(&ids_for_filter, &tags_for_filter)
        })
        .await?;
        local_candidate_ids.retain(|id| !blacklisted_ids.contains(id));
    }

    // Second wave: user_relation depends on tags.
    let tags_for_relation = tags.clone();
    let user_relation = rocket::tokio::task::spawn_blocking(move || {
        db::load_account_tag_relation(account_id, &tags_for_relation)
            .map_err(|e| format!("Failed to load user tag relation graph: {e}"))
    })
    .await
    .map_err(|e| format!("Join error: {e}"))??;
    pipe.mark("db_hydrate");

    let (bucket_name, mut priors) = cfg.pick_bucket(account_id, explicit_bucket.as_deref());
    if let Some(bucket) = &bucket_name {
        debug!("recommendations account={account_id} bucket={bucket}");
    }
    priors.now = Utc::now();

    let live_posts: Vec<Post> = api::get_posts(&account, page)
        .await
        .map_err(|e| ApiError::Internal(format!("Failed to fetch posts: {e}")))?;
    pipe.mark("e621_fetch");

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
                db::save_posts_tags_batch(&posts_for_persist, &HashSet::new(), false, None)
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

    // Build the blacklist set for the IDF prior (scoring-level soft filter).
    let blacklist_set: HashSet<String> = blacklisted_simple_tags.into_iter().collect();
    let ctx = ScoringContext::new_with_blacklist(
        &tags,
        &priors,
        &idf,
        &profile,
        &global_relation,
        &user_relation,
        blacklist_set,
    );

    // Pre-resolve per-post features once so the parallel scoring loop
    // skips HashMap-by-string lookups in IDF and tag-relation graphs.
    let cached: Vec<CachedPostFeatures> = combined
        .iter()
        .map(|post| CachedPostFeatures::from_post_with_user(post, &idf, &global_relation, Some(&user_relation)))
        .collect();
    pipe.mark("cache_build");

    // Parallel scoring via rayon — the closure captures &ctx, &idf,
    // &global_relation, &user_relation which are all Send + Sync.
    let scored_and_timing: Vec<(ScoredPost, ChannelTiming)> = combined
        .into_par_iter()
        .zip(cached.into_par_iter())
        .map(|(post, cf)| {
            let (s, breakdown, timing) = ctx.score_cached_with_metrics(&cf);
            let sp = ScoredPost {
                post,
                score: s,
                breakdown: Some(breakdown),
            };
            (sp, timing)
        })
        .collect();

    // Accumulate performance metrics (trivially cheap when perf_metrics is off).
    let mut metrics = ScoringMetrics::default();
    for (_, timing) in &scored_and_timing {
        metrics.accumulate(timing);
    }
    metrics.log_summary();
    pipe.mark("scoring");

    let mut scored: Vec<ScoredPost> = scored_and_timing
        .into_iter()
        .map(|(sp, _)| sp)
        .collect();

    if let Some(threshold) = affinity_threshold {
        scored.retain(|sp| sp.score >= threshold);
    }
    let mut scored = diversify_scored_posts(scored, &priors);

    // Class F: ε-greedy exploration bonus.
    // Boost posts with novel (low-similarity) tags so users see
    // content outside their established preference bubble.
    if priors.exploration_epsilon > 1e-4 {
        let eps = priors.exploration_epsilon.min(0.5);
        for sp in &mut scored {
            let tag_novelty = 1.0 - sp
                .breakdown
                .as_ref()
                .map(|b| b.tag_similarity)
                .unwrap_or(0.0);
            sp.score = (sp.score + eps * tag_novelty).clamp(0.0, 1.0);
        }
    }
    pipe.mark("diversify_post");

    // Log score breakdown for top-10 and bottom-10 posts (sorted copy).
    let mut sorted_for_log: Vec<&ScoredPost> = scored.iter().collect();
    sorted_for_log.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    info!("── Score breakdown (top 10) ─────────────────");
    for sp in sorted_for_log.iter().take(10) {
        if let Some(b) = &sp.breakdown {
            info!(
                "  post_id={} score={:.4}  sim={:.3} qual={:.3} rec={:.3} rate={:.3} med={:.3} pop={:.3} inter={:.3} rel={:.3} upl={:.3} exc={:.3} nov={:.3}",
                sp.post.id, sp.score,
                b.tag_similarity, b.quality_fit, b.recency_fit, b.rating_fit,
                b.media_fit, b.popularity_fit, b.interaction_fit, b.tag_relation_fit, b.uploader_fit,
                b.exclusivity_fit, b.novelty_fit,
            );
        }
    }
    if sorted_for_log.len() > 20 {
        info!("── Score breakdown (bottom 10) ────────────────");
        for sp in sorted_for_log.iter().rev().take(10) {
            if let Some(b) = &sp.breakdown {
                info!(
                    "  post_id={} score={:.4}  sim={:.3} qual={:.3} rec={:.3} rate={:.3} med={:.3} pop={:.3} inter={:.3} rel={:.3} upl={:.3}",
                    sp.post.id, sp.score,
                    b.tag_similarity, b.quality_fit, b.recency_fit, b.rating_fit,
                    b.media_fit, b.popularity_fit, b.interaction_fit, b.tag_relation_fit, b.uploader_fit,
                );
            }
        }
    }

    pipe.finish_and_log();

    Ok(Json(scored))
}
