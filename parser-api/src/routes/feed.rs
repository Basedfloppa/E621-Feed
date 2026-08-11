//! Feed routes: feed-interaction logging and `/recommendations`.

use std::collections::HashSet;

use chrono::Utc;
use rayon::prelude::*;
use rocket::serde::json::Json;
use rocket_okapi::openapi;

use crate::db_blocking;
use e621_account_parser_api::auth::OwnerToken;
use e621_account_parser_api::utils::{
    CachedPostFeatures, ChannelTiming, PipelineMetrics, ScoringContext, ScoringMetrics,
    current_global_relation, current_idf, diversify_scored_posts, post_pair_similarity,
};
/// Apply supported blacklist metatags to locally cached candidates. e621
/// handles these for live results; cached posts need the equivalent check.
fn matches_local_blacklist_metatag(post: &Post, blacklist: &str) -> bool {
    let meta = |tag: &str| {
        post.tags
            .meta
            .iter()
            .any(|value| value.eq_ignore_ascii_case(tag))
    };
    blacklist
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .any(|rule| match rule.to_ascii_lowercase().as_str() {
            "video" => meta("video"),
            "animated" => meta("animated"),
            "-animated" => !meta("animated"),
            "rating:s" | "rating:safe" => matches!(post.rating, models::Rating::S),
            "rating:q" | "rating:questionable" => matches!(post.rating, models::Rating::Q),
            "rating:e" | "rating:explicit" => matches!(post.rating, models::Rating::E),
            rule if rule.starts_with("uploader:!") => rule[10..]
                .parse::<i64>()
                .is_ok_and(|id| id == post.uploader_id),
            _ => false,
        })
}

use e621_account_parser_api::{
    api, audit,
    db::{
        self, clear_feed_interactions, collect_local_candidate_ids, find_similar_post_ids,
        get_account_by_id, get_account_interaction_history, get_account_preference_profile,
        get_long_term_seen_post_ids, get_owned_post_ids, get_post_by_id,
        get_recently_seen_post_ids, get_tag_counts, hydrate_posts_by_ids, record_feed_interaction,
        record_feed_interactions_batch, remove_feed_interaction, upsert_catalog_posts,
    },
    errors::ApiError,
    models::{
        self, BatchInteractionRequest, ContinueResponse, FeedInteractionRequest, Post, ScoredPost,
        cfg,
    },
    ratelimit, validation,
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
    let account_id = body.account_id;
    let post_id = body.post_id;
    let event_str = body.event_type.to_string();
    db_blocking(move || record_feed_interaction(&owner_token, &body))
        .await
        .map_err(ApiError::from)?;
    audit::event("feed.interaction")
        .field("account_id", account_id)
        .field("post_id", post_id)
        .field("event", &event_str)
        .emit();
    // Resolve the A/B bucket the same way the DB layer does, so the metric
    // arm matches the one the user actually saw.
    let bucket = e621_account_parser_api::models::cfg()
        .pick_bucket(account_id, None)
        .0
        .unwrap_or_else(|| "none".to_string());
    e621_account_parser_api::metrics::METRICS
        .feed_interactions_total
        .with_label_values(&[&bucket, &event_str])
        .inc();
    e621_account_parser_api::metrics::METRICS
        .experiment_bucket_interactions_total
        .with_label_values(&[&bucket])
        .inc();
    e621_account_parser_api::metrics::METRICS
        .feed_interactions_by_type_total
        .with_label_values(&[&event_str])
        .inc();
    Ok(())
}

#[openapi(tag = "Recommendations")]
#[delete("/interaction", data = "<payload>")]
pub(crate) async fn undo_feed_interaction(
    payload: Json<FeedInteractionRequest>,
    owner: OwnerToken,
) -> Result<Json<bool>, ApiError> {
    let body = payload.into_inner();
    validation::validate_feed_interaction(&body)?;
    let owner_token = owner.0;
    ratelimit::check(&format!("interaction_undo:owner:{owner_token}"), 30, 60)?;
    let removed = db_blocking(move || remove_feed_interaction(&owner_token, &body)).await?;
    Ok(Json(removed))
}

#[openapi(tag = "Recommendations")]
#[delete("/account/<account_id>/interaction")]
pub(crate) async fn clear_account_interactions(
    account_id: i32,
    owner: OwnerToken,
) -> Result<Json<usize>, ApiError> {
    validation::validate_account_id(account_id)?;
    let owner_token = owner.0;
    ratelimit::check(&format!("interaction_clear:owner:{owner_token}"), 3, 3)?;
    let deleted = db_blocking(move || clear_feed_interactions(&owner_token, account_id)).await?;
    audit::event("feed.interactions_cleared")
        .field("account_id", account_id)
        .field("deleted", deleted)
        .emit();
    Ok(Json(deleted))
}

/// Interaction history for the history page. Optional `event` filter
/// (`open` / `hide` / `like` / `strong_like` / `qualified_impression`)
/// and `limit` (default 100, max 500). Newest first.
#[openapi(tag = "Recommendations")]
#[get("/account/<account_id>/interactions?<event>&<limit>")]
pub(crate) async fn get_account_interactions(
    account_id: i32,
    event: Option<String>,
    limit: Option<i64>,
    owner: OwnerToken,
) -> Result<Json<Vec<models::InteractionHistoryEntry>>, ApiError> {
    validation::validate_account_id(account_id)?;
    let owner_token = owner.0;
    ratelimit::check(&format!("read:owner:{owner_token}"), 240, 60)?;
    let filter = match event.as_deref() {
        None => None,
        Some("open") => Some(models::FeedInteractionType::Open),
        Some("hide") => Some(models::FeedInteractionType::Hide),
        Some("like") => Some(models::FeedInteractionType::Like),
        Some("strong_like") => Some(models::FeedInteractionType::StrongLike),
        Some("qualified_impression") => Some(models::FeedInteractionType::QualifiedImpression),
        Some(other) => {
            return Err(ApiError::BadRequest(format!(
                "unknown event filter: {other}"
            )));
        }
    };
    let limit = limit.unwrap_or(100);
    // Verify ownership first (history is account-scoped data).
    let owner_for_auth = owner_token.clone();
    db_blocking(move || get_account_by_id(&owner_for_auth, account_id).map_err(|e| e.clone()))
        .await?;
    let items = db_blocking(move || {
        get_account_interaction_history(account_id, filter, limit)
            .map_err(|e| format!("Failed to get interaction history: {e}"))
    })
    .await?;

    // Hydrate catalog posts locally (cheap) so the page can render previews
    // for posts that exist in the catalog; the rest come back `None`.
    let ids: Vec<i64> = items.iter().map(|i| i.post_id).collect();
    let hydrated: std::collections::HashMap<i64, Post> = db_blocking(move || {
        hydrate_posts_by_ids(&ids)
            .map(|posts| posts.into_iter().map(|p| (p.id, p)).collect())
            .map_err(|e| format!("Failed to hydrate history posts: {e}"))
    })
    .await?;
    let entries: Vec<models::InteractionHistoryEntry> = items
        .into_iter()
        .map(|item| models::InteractionHistoryEntry {
            post_id: item.post_id,
            event_type: item.event_type,
            position: item.position,
            created_at: item.created_at,
            post: hydrated.get(&item.post_id).cloned(),
        })
        .collect();

    audit::event("feed.history")
        .field("account_id", account_id)
        .field("returned", entries.len())
        .field_opt("event", event)
        .emit();
    Ok(Json(entries))
}

#[openapi(tag = "Recommendations")]
#[post("/interaction/batch", data = "<payload>")]
pub(crate) async fn log_feed_interaction_batch(
    payload: Json<BatchInteractionRequest>,
    owner: OwnerToken,
) -> Result<(), ApiError> {
    let body = payload.into_inner();
    let owner_token = owner.0;
    validation::validate_batch_interaction(&body)?;

    // Per-device cap — batch can carry up to 100 interactions, so
    // the effective per-interaction limit is higher than the single
    // endpoint, but the overall write throughput is bounded.
    ratelimit::check(&format!("interaction:owner:{owner_token}"), 240, 60)?;
    let count = body.interactions.len();
    let primary_account = body.interactions.first().map(|i| i.account_id);
    db_blocking(move || record_feed_interactions_batch(&owner_token, &body.interactions))
        .await
        .map_err(ApiError::from)?;
    audit::event("feed.batch")
        .field_opt("account_id", primary_account)
        .field("count", count)
        .emit();
    Ok(())
}

#[openapi(tag = "Recommendations")]
#[get("/recommendations/<account_id>?<page>&<affinity_threshold>&<exploration>&<session>")]
pub(crate) async fn get_recommendations(
    account_id: i32,
    owner: OwnerToken,
    page: Option<i32>,
    affinity_threshold: Option<f32>,
    exploration: Option<f32>,
    session: Option<String>,
) -> Result<Json<Vec<ScoredPost>>, ApiError> {
    validation::validate_account_id(account_id)?;
    validation::validate_recommendations_page(page)?;
    let affinity_threshold = validation::validate_affinity_threshold(affinity_threshold)?;
    let exploration = validation::validate_exploration(exploration)?;
    // The session value is written verbatim into feed_sessions/feed_session_posts,
    // so enforce the same size/charset bound the /continue route already uses;
    // otherwise an authenticated client could insert arbitrary-length keys and
    // bloat those tables indefinitely.
    if let Some(ref sess) = session {
        validation::validate_session_token(sess)?;
    }
    let owner_token = owner.0;
    // Cap per device — admin-authenticated round-trip per request, so an
    // infinite-scroll loop must not burn through the admin quota.
    ratelimit::check(&format!("recs:owner:{owner_token}"), 60, 30)?;

    let mut pipe = PipelineMetrics::new("recommendations");

    // Run the shared pipeline (scoring, threshold, MMR diversification,
    // and exploration bonus). The shared function applies the exploration
    // bonus so both endpoints behave identically — fixing the original bug
    // where build_recommendations_inner skipped it entirely.
    let mut scored = build_recommendations_shared(
        account_id,
        &owner_token,
        page,
        affinity_threshold,
        exploration,
        "recommendations",
        Some(&mut pipe),
    )
    .await?;

    // Cross-page dedup: if a session_id is provided, filter out posts
    // already shown in previous pages and record the new page's IDs.
    // All I/O is against SQLite — nothing accumulates in process memory.
    if let Some(ref sess) = session {
        // Ensure the session row exists (first page of a scroll creates it).
        let sess_for_touch = sess.clone();
        db_blocking(move || db::touch_or_create_feed_session(&sess_for_touch, account_id)).await?;
        let session_for_dedup = sess.clone();
        let shown_ids: std::collections::HashSet<i64> =
            db_blocking(move || db::get_session_shown_post_ids(&session_for_dedup, account_id))
                .await
                .unwrap_or_default(); // session does not exist yet or has expired

        if !shown_ids.is_empty() {
            let before = scored.len();
            scored.retain(|sp| !shown_ids.contains(&sp.post.id));
            if scored.len() < before {
                debug!(
                    "[feed] cross-page dedup: dropped {} posts already shown",
                    before - scored.len()
                );
            }
        }

        // Record this page's shown posts for future dedup.
        let posts_to_record: Vec<(i64, i32)> = scored
            .iter()
            .enumerate()
            .map(|(i, sp)| (sp.post.id, (i + 1) as i32))
            .collect();
        if !posts_to_record.is_empty() {
            let session_for_record = sess.clone();
            db_blocking(move || {
                db::record_session_shown_posts(&session_for_record, account_id, &posts_to_record)
            })
            .await?;
        }
    }

    // Bucket logging (main endpoint only — continue caller handles it).
    let (bucket_name, _) = cfg().pick_bucket(account_id, None);
    if let Some(bucket) = &bucket_name {
        debug!("recommendations account={account_id} bucket={bucket}");
    }

    // Log score breakdown for top-10 and bottom-10 posts (sorted copy).
    let mut sorted_for_log: Vec<&ScoredPost> = scored.iter().collect();
    sorted_for_log.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    info!("── Score breakdown (top 10) ─────────────────");
    for sp in sorted_for_log.iter().take(10) {
        if let Some(b) = &sp.breakdown {
            info!(
                "  post_id={} score={:.4}  sim={:.3} qual={:.3} rec={:.3} rate={:.3} med={:.3} pop={:.3} inter={:.3} rel={:.3} upl={:.3} exc={:.3} nov={:.3}",
                sp.post.id,
                sp.score,
                b.tag_similarity,
                b.quality_fit,
                b.recency_fit,
                b.rating_fit,
                b.media_fit,
                b.popularity_fit,
                b.interaction_fit,
                b.tag_relation_fit,
                b.uploader_fit,
                b.exclusivity_fit,
                b.novelty_fit,
            );
        }
    }
    if sorted_for_log.len() > 20 {
        info!("── Score breakdown (bottom 10) ────────────────");
        for sp in sorted_for_log.iter().rev().take(10) {
            if let Some(b) = &sp.breakdown {
                info!(
                    "  post_id={} score={:.4}  sim={:.3} qual={:.3} rec={:.3} rate={:.3} med={:.3} pop={:.3} inter={:.3} rel={:.3} upl={:.3} exc={:.3} nov={:.3}",
                    sp.post.id,
                    sp.score,
                    b.tag_similarity,
                    b.quality_fit,
                    b.recency_fit,
                    b.rating_fit,
                    b.media_fit,
                    b.popularity_fit,
                    b.interaction_fit,
                    b.tag_relation_fit,
                    b.uploader_fit,
                    b.exclusivity_fit,
                    b.novelty_fit,
                );
            }
        }
    }

    pipe.finish_and_log();

    audit::event("feed.recommend")
        .field("account_id", account_id)
        .field("returned", scored.len())
        .field_opt("page", page)
        .emit();
    e621_account_parser_api::metrics::METRICS
        .feed_views_total
        .inc();

    Ok(Json(scored))
}

/// Shared recommendation pipeline.
///
/// Fetches e621 posts, hydrates local candidates, scores, thresholds, and
/// diversifies — returning posts already sorted by adjusted (MMR) score.
///
/// ## Dedup strategy
/// - **Live posts** from e621: deduped against a short window (14 days via
///   `get_recently_seen_post_ids`).
/// - **Local candidates** (old cached posts): deduped against a **union** of
///   the short window **and** a long window (60 days via
///   `get_long_term_seen_post_ids`). This prevents the same old posts from
///   reappearing across weeks.
/// - **Owned posts** (user's favourites) are always excluded.
///
/// ## Randomisation
/// `collect_local_candidate_ids` now uses `ORDER BY RANDOM()` (see
/// `db/feed.rs`) — the same top-N tags produce different candidates on every
/// call, distributing impressions across the long tail instead of always
/// promoting the highest-scored posts.
///
/// The caller is responsible for:
/// - Score breakdown logging (caller-specific diagnostic)
/// - Pipeline metrics and audit (caller-specific)
async fn build_recommendations_shared(
    account_id: i32,
    owner_token: &str,
    page: Option<i32>,
    affinity_threshold: Option<f32>,
    exploration: Option<f32>,
    ctx_label: &str,
    mut pipe: Option<&mut PipelineMetrics>,
) -> Result<Vec<ScoredPost>, ApiError> {
    let cfg = cfg();
    let runtime = cfg.runtime.clone();

    let owner_clone = owner_token.to_string();
    let (tags_res, profile_res, account_res, seen_res, owned_res, local_res, bucket_res) = rocket::tokio::join!(
        rocket::tokio::task::spawn_blocking(move || {
            get_tag_counts(account_id).map_err(|e| format!("Failed to get tag counts: {e}"))
        }),
        rocket::tokio::task::spawn_blocking(move || {
            get_account_preference_profile(account_id)
                .map_err(|e| format!("Failed to get account profile: {e}"))
        }),
        rocket::tokio::task::spawn_blocking({
            let owner_token = owner_clone.clone();
            move || {
                get_account_by_id(&owner_token, account_id)
                    .map_err(|e| format!("Failed to get account: {e}"))
            }
        }),
        rocket::tokio::task::spawn_blocking(move || {
            get_recently_seen_post_ids(account_id, runtime.dedup_lookback_days)
                .map_err(|e| format!("Failed to load seen post ids: {e}"))
        }),
        rocket::tokio::task::spawn_blocking(move || {
            get_owned_post_ids(account_id)
                .map_err(|e| format!("Failed to load owned post ids: {e}"))
        }),
        rocket::tokio::task::spawn_blocking(move || {
            collect_local_candidate_ids(account_id, runtime.local_candidate_limit)
                .map_err(|e| format!("Failed to collect local candidate ids: {e}"))
        }),
        rocket::tokio::task::spawn_blocking(move || {
            db::get_account_experiment_bucket(account_id)
                .map_err(|e| format!("Failed to load experiment bucket: {e}"))
        }),
    );

    let tags = tags_res.map_err(|e| format!("Join error: {e}"))??;
    let profile = profile_res.map_err(|e| format!("Join error: {e}"))??;
    let account = account_res.map_err(|e| format!("Join error: {e}"))??;
    let seen_ids = seen_res.map_err(|e| format!("Join error: {e}"))??;
    let owned_ids = owned_res.map_err(|e| format!("Join error: {e}"))??;
    let mut local_candidate_ids = local_res.map_err(|e| format!("Join error: {e}"))??;
    let explicit_bucket = bucket_res.map_err(|e| format!("Join error: {e}"))??;

    // Blacklist filter (applies to local candidates only — live posts already
    // have the blacklist applied via e621 search syntax). We only extract
    // simple tag names (no e621 search syntax like `-rating:s` or `young furry`);
    // complex expressions are best-effort and will be ignored here — they still
    // apply to live e621 posts.
    let blacklisted_simple_tags: Vec<String> = account
        .blacklist
        .lines()
        .flat_map(|l| l.split_whitespace().filter(|t| !t.is_empty()))
        .filter(|t| !t.contains(':') && !t.starts_with('-'))
        .map(str::to_lowercase)
        .collect();
    if !blacklisted_simple_tags.is_empty() && !local_candidate_ids.is_empty() {
        let ids_for_filter = local_candidate_ids.clone();
        let tags_for_filter = blacklisted_simple_tags.clone();
        let blacklisted_ids: HashSet<i64> =
            db_blocking(move || db::load_blacklisted_post_ids(&ids_for_filter, &tags_for_filter))
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

    let (bucket_name, mut priors) = cfg.pick_bucket(account_id, explicit_bucket.as_deref());
    priors.now = Utc::now();

    if let Some(explore) = exploration {
        priors.exploration_epsilon = explore.clamp(0.0, 0.5);
    }
    if let Some(p) = &mut pipe {
        p.mark("db_hydrate");
    }

    let live_posts: Vec<Post> = api::get_posts(&account, page)
        .await
        .map_err(|e| ApiError::from_string(format!("Failed to fetch posts: {e}")))?;
    if let Some(p) = &mut pipe {
        p.mark("e621_fetch");
    }

    // Catalog persistence is fire-and-forget: SQLite is single-writer and
    // infinite scroll fires this several times per second. Pull off the
    // request path so response shape doesn't depend on flush order. IDF is
    // bumped incrementally inside `save_posts_tags_batch`.
    {
        let posts_for_persist = live_posts.clone();
        let ctx_label_for_audit = ctx_label.to_string();
        rocket::tokio::spawn(async move {
            let res = rocket::tokio::task::spawn_blocking(move || -> Result<(), String> {
                upsert_catalog_posts(&posts_for_persist)
                    .map_err(|e| format!("Failed to store catalog posts: {e}"))?;
                db::save_posts_tags_batch(&posts_for_persist, &HashSet::new(), false, None) // skip cooccurrence: candidate browse posts aren't user truth
                    .map_err(|e| format!("Failed to store catalog tags: {e}"))?;
                Ok(())
            })
            .await;
            if let Ok(Err(e)) = res {
                warn!("background recommendation persist failed: {e}");
                audit::event("feed.persist_failed")
                    .field("kind", "task_error")
                    .field("ctx", ctx_label_for_audit)
                    .field("error", e)
                    .emit_err();
            } else if let Err(e) = res {
                warn!("background recommendation persist task panicked: {e}");
                audit::event("feed.persist_failed")
                    .field("kind", "panic")
                    .field("ctx", ctx_label_for_audit)
                    .field("error", e)
                    .emit_err();
            }
        });
    }

    // Hydrate locals (skip owned/seen/already-in-live).
    let live_ids: HashSet<i64> = live_posts.iter().map(|p| p.id).collect();
    // Load long-term seen IDs for old-post dedup (local candidates).
    // While live posts from e621 stay deduped against the short window,
    // local candidates (which can be months old) get a longer dedup horizon
    // so the same old post doesn't reappear across weeks.
    let long_term_seen = get_long_term_seen_post_ids(account_id, 60)
        .map_err(|e| format!("Failed to load long-term seen post ids: {e}"))?;
    let local_dedup: HashSet<i64> = seen_ids.union(&long_term_seen).copied().collect();
    let local_to_hydrate: Vec<i64> = local_candidate_ids
        .into_iter()
        .filter(|id| !live_ids.contains(id) && !owned_ids.contains(id) && !local_dedup.contains(id))
        .collect();
    let local_posts = if local_to_hydrate.is_empty() {
        Vec::new()
    } else {
        let mut posts = db_blocking(move || hydrate_posts_by_ids(&local_to_hydrate)).await?;
        posts.retain(|post| !matches_local_blacklist_metatag(post, &account.blacklist));
        posts
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

    // Build the blacklist set for the IDF prior (scoring-level soft filter).
    let idf = current_idf();
    let global_relation = current_global_relation();

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
        .map(|post| {
            CachedPostFeatures::from_post_with_user(
                post,
                &idf,
                &global_relation,
                Some(&user_relation),
            )
        })
        .collect();
    if let Some(p) = &mut pipe {
        p.mark("cache_build");
    }

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
                reasons: Vec::new(),
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

    let mut scored: Vec<ScoredPost> = scored_and_timing.into_iter().map(|(sp, _)| sp).collect();
    if let Some(p) = &mut pipe {
        p.mark("scoring");
    }

    // Threshold (if requested) before diversification so we don't pay
    // MMR cost on posts we're going to drop anyway.
    if let Some(threshold) = affinity_threshold {
        scored.retain(|sp| sp.score >= threshold);
    }

    // Diversify via MMR — same helper the main `/recommendations` route
    // uses. The returned order interleaves diverse picks with high-score
    // ones; we DON'T sort by score after, because that would undo the
    // MMR interleaving. Callers can `truncate(N)` to keep the top-N
    // best-balanced posts. Passes `user_relation` so PMI-based soft
    // similarity personalises around per-account tag co-occurrences.
    let mut scored =
        diversify_scored_posts(scored, &global_relation, Some(&user_relation), &priors);
    if let Some(p) = &mut pipe {
        p.mark("diversify_post");
    }

    // Class F: ε-greedy exploration bonus (applied in the shared pipeline
    // so both endpoints behave identically).
    if priors.exploration_epsilon > 1e-4 {
        let eps = priors.exploration_epsilon.min(0.5);
        for sp in &mut scored {
            let tag_novelty = 1.0 - sp.breakdown.as_ref().map_or(0.0, |b| b.tag_similarity);
            sp.score = (sp.score + eps * tag_novelty).clamp(0.0, 1.0);
        }
    }

    // Human-readable "why this post" reasons for the frontend. Uses the
    // account's tag counts only to name a shared tag — never re-scores.
    let user_tags: std::collections::HashSet<String> =
        tags.iter().map(|t| t.name.to_lowercase()).collect();
    e621_account_parser_api::utils::explain_scored_posts(
        &mut scored,
        &user_tags,
        priors.exploration_epsilon > 1e-4,
    );

    // Drop bucket_name — the caller handles bucket logging.
    let _ = bucket_name;

    Ok(scored)
}

#[openapi(tag = "Recommendations")]
#[get("/recommendations/<account_id>/continue?<session>&<count>")]
pub(crate) async fn get_recommendations_continue(
    account_id: i32,
    session: String,
    count: Option<i32>,
    owner: OwnerToken,
) -> Result<Json<ContinueResponse>, ApiError> {
    use std::collections::HashSet;

    validation::validate_account_id(account_id)?;
    validation::validate_session_token(&session)?;
    let count = validation::validate_continue_count(count)?;
    let owner_token = owner.0;

    ratelimit::check(&format!("recs:owner:{owner_token}"), 60, 30)?;

    // Verify ownership BEFORE touching/creating feed-session state — otherwise
    // a request for an account the caller doesn't own would still commit a
    // feed_session row under that account (and leak account existence via the
    // two distinct failure paths).
    db_blocking({
        let ot = owner_token.clone();
        move || get_account_by_id(&ot, account_id).map_err(|e| format!("account access: {e}"))
    })
    .await?;

    // Single atomic read/check/touch. Three outcomes:
    //   Fresh    — first time this (session_id, account_id) is seen
    //   Active   — existing valid session, touched
    //   Expired  — existed but past TTL; do NOT touch, tell client to rotate
    let session_for_check = session.clone();
    let session_state =
        db_blocking(move || db::touch_or_create_feed_session(&session_for_check, account_id))
            .await?;

    let fresh_start = matches!(session_state, db::FeedSessionState::Expired);

    // Only an Active session has a dedup history to load. Fresh sessions
    // start with an empty set; Expired ones are about to be rotated so
    // anything we've recorded is no longer authoritative.
    let shown_ids: HashSet<i64> = if matches!(session_state, db::FeedSessionState::Active) {
        let session_for_dedup = session.clone();
        db_blocking(move || {
            db::get_session_shown_post_ids(&session_for_dedup, account_id)
                .map_err(|e| format!("Failed to load session shown posts: {e}"))
        })
        .await?
    } else {
        HashSet::new()
    };

    // Build, dedup against the session's shown set, then truncate.
    // `build_recommendations_shared` returns posts already sorted (and
    // diversified) so `truncate` keeps the top-N, not an arbitrary N.
    let mut posts =
        build_recommendations_shared(account_id, &owner_token, None, None, None, "continue", None)
            .await?;
    if !shown_ids.is_empty() {
        posts.retain(|sp| !shown_ids.contains(&sp.post.id));
    }
    posts.truncate(count as usize);

    // Skip recording on Expired — the client is about to switch session_id,
    // so this set will never be queried again.
    if !matches!(session_state, db::FeedSessionState::Expired) {
        let session_for_record = session.clone();
        let shown_for_record: Vec<(i64, i32)> = posts
            .iter()
            .enumerate()
            .map(|(i, sp)| (sp.post.id, i as i32))
            .collect();
        db_blocking(move || {
            db::record_session_shown_posts(&session_for_record, account_id, &shown_for_record)
        })
        .await?;
    }

    let session_state_str = match session_state {
        db::FeedSessionState::Fresh => "fresh",
        db::FeedSessionState::Active => "active",
        db::FeedSessionState::Expired => "expired",
    };
    audit::event("feed.continue")
        .field("account_id", account_id)
        .field("session_state", session_state_str)
        .field("returned", posts.len())
        .emit();

    Ok(Json(ContinueResponse { posts, fresh_start }))
}

#[openapi(tag = "Posts")]
#[get("/posts/<post_id>/similar?<account_id>&<limit>&<min_overlap>&<page>")]
pub(crate) async fn get_similar_posts(
    post_id: i64,
    account_id: i32,
    limit: Option<i32>,
    min_overlap: Option<i32>,
    page: Option<i32>,
    owner: OwnerToken,
) -> Result<Json<Vec<ScoredPost>>, ApiError> {
    validation::validate_account_id(account_id)?;
    let limit = validation::validate_similar_posts_limit(limit)?;
    let min_overlap = validation::validate_similar_posts_min_overlap(min_overlap)?;
    let page = validation::validate_similar_posts_page(page)?;
    let owner_token = owner.0;

    ratelimit::check(&format!("read:owner:{owner_token}"), 240, 60)?;

    // Verify account ownership.
    db_blocking(move || {
        get_account_by_id(&owner_token, account_id)
            .map_err(|e| format!("Failed to verify account access: {e}"))
    })
    .await?;

    // Fetch source post — try local DB first, fall back to e621 API.
    let source_post = db_blocking(move || {
        get_post_by_id(post_id).map_err(|e| format!("Failed to fetch source post: {e}"))
    })
    .await?;

    let source_post = if let Some(p) = source_post {
        p
    } else {
        // Fetch from e621 API.
        let posts = api::get_posts_by_ids(&[post_id])
            .await
            .map_err(|e| ApiError::from_string(format!("Failed to fetch post from e621: {e}")))?;
        posts
            .into_iter()
            .next()
            .ok_or_else(|| ApiError::NotFound(format!("Post {post_id} not found")))?
    };

    // Find candidate IDs via tag overlap.
    let candidate_ids = db_blocking(move || {
        find_similar_post_ids(post_id, account_id, min_overlap, limit * 3, page)
            .map_err(|e| format!("Failed to find similar posts: {e}"))
    })
    .await?;

    if candidate_ids.is_empty() {
        return Ok(Json(Vec::new()));
    }

    // Hydrate candidates with tags.
    let candidates = db_blocking(move || {
        hydrate_posts_by_ids(&candidate_ids)
            .map_err(|e| format!("Failed to hydrate candidates: {e}"))
    })
    .await?;

    // Compute content-based similarity for each candidate.
    let idf = current_idf();
    let priors = &cfg().priors;

    let mut scored: Vec<ScoredPost> = candidates
        .into_iter()
        .map(|post| {
            let sim = post_pair_similarity(&source_post, &post, &idf, priors);
            let breakdown = models::ScoreBreakdown {
                tag_similarity: sim,
                quality_fit: 0.0,
                recency_fit: 0.0,
                rating_fit: 0.0,
                media_fit: 0.0,
                popularity_fit: 0.0,
                interaction_fit: 0.0,
                tag_relation_fit: 0.0,
                uploader_fit: 0.0,
                exclusivity_fit: 0.0,
                novelty_fit: 0.0,
                artist_discovery_fit: 0.0,
            };
            ScoredPost {
                post,
                score: sim,
                breakdown: Some(breakdown),
                reasons: Vec::new(),
            }
        })
        .filter(|sp| sp.score > 0.0)
        .collect();

    // Sort by similarity descending.
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.truncate(limit as usize);

    audit::event("feed.similar")
        .field("account_id", account_id)
        .field("post_id", post_id)
        .field("returned", scored.len())
        .emit();
    e621_account_parser_api::metrics::METRICS
        .feed_views_total
        .inc();

    Ok(Json(scored))
}
