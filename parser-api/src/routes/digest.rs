//! Daily digest route — a lightweight, personalised (or generic) page of
//! up to 20 posts, stratified across score tiers.  Active users get the
//! full scoring pipeline; infrequent users get a cheap trending/random mix.
//!
//! `GET /digest/<account_id>?<full>`

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

use chrono::Utc;
use rand::seq::SliceRandom;
use rocket::serde::json::Json;
use rocket_okapi::openapi;

use crate::db_blocking;
use e621_account_parser_api::auth::OwnerToken;
use e621_account_parser_api::db::{
    self, get_account_by_id, get_account_preference_profile, get_tag_counts,
};
use e621_account_parser_api::errors::ApiError;
use e621_account_parser_api::models::{cfg, ScoredPost};
use e621_account_parser_api::utils::{
    current_global_relation, current_idf,
    CachedPostFeatures, ScoringContext,
};
use e621_account_parser_api::validation;

// ---------------------------------------------------------------------------
// In-memory TTL cache
// ---------------------------------------------------------------------------

struct CachedDigest {
    posts: Vec<ScoredPost>,
    cached_at: Instant,
}

static DIGEST_CACHE: LazyLock<Mutex<HashMap<String, CachedDigest>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

const DIGEST_TTL_SECS: u64 = 86400; // 24 hours

fn cache_get(key: &str) -> Option<Vec<ScoredPost>> {
    let map = DIGEST_CACHE.lock().expect("digest cache poisoned");
    let entry = map.get(key)?;
    if entry.cached_at.elapsed().as_secs() < DIGEST_TTL_SECS {
        Some(entry.posts.clone())
    } else {
        None
    }
}

fn cache_set(key: String, posts: Vec<ScoredPost>) {
    let mut map = DIGEST_CACHE.lock().expect("digest cache poisoned");
    map.insert(key, CachedDigest {
        posts,
        cached_at: Instant::now(),
    });
}

// ---------------------------------------------------------------------------
// Stratified sampling helper
// ---------------------------------------------------------------------------

/// Sample up to 20 posts from a scored list using stratified strategy:
///   top-3  best picks,
///   4-8    semi-random from upper-mid,
///   9-12   trending,
///   13-15  exploration (low similarity),
///   16-18  wildcard by group,
///   19-20  recent-added + popular-new.
fn stratified_sample(
    scored: &[ScoredPost],
    trending: Vec<ScoredPost>,
    wildcards: Vec<ScoredPost>,
    recent_added: Vec<ScoredPost>,
    popular_new: Vec<ScoredPost>,
    rng: &mut impl rand::Rng,
) -> Vec<ScoredPost> {
    let mut digest = Vec::with_capacity(20);

    // 1. Top-3 best picks
    for sp in scored.iter().take(3) {
        digest.push(sp.clone());
    }

    // 2. Semi-random from middle third of scored list
    if scored.len() > 6 {
        let mid_start = scored.len() / 3;
        let mid_end = (scored.len() * 2 / 3).min(scored.len());
        if mid_end > mid_start {
            let slice = &scored[mid_start..mid_end];
            let sample = slice
                .choose_multiple(rng, 4)
                .cloned()
                .collect::<Vec<_>>();
            digest.extend(sample);
        }
    }

    // 3. Trending
    digest.extend(trending.into_iter().take(4));

    // 4. Exploration: low-similarity posts
    //    (already in trending/top, just fill remaining slots)
    let mut remaining = 20usize.saturating_sub(digest.len());
    for sp in scored.iter().skip(3).rev() {
        if remaining == 0 {
            break;
        }
        if !digest.iter().any(|d| d.post.id == sp.post.id) {
            digest.push(sp.clone());
            remaining -= 1;
        }
    }

    // 5. Wildcards
    for sp in wildcards.into_iter().take(3) {
        if !digest.iter().any(|d| d.post.id == sp.post.id) {
            if digest.len() >= 20 {
                break;
            }
            digest.push(sp);
        }
    }

    // 6. Recent added + popular new
    for sp in recent_added.into_iter().chain(popular_new).take(2) {
        if !digest.iter().any(|d| d.post.id == sp.post.id) {
            if digest.len() >= 20 {
                break;
            }
            digest.push(sp);
        }
    }

    digest.truncate(20);
    digest
}

// ---------------------------------------------------------------------------
// Builders
// ---------------------------------------------------------------------------

/// Full personalised digest: runs the scoring pipeline on local candidates
/// and applies stratified sampling.
async fn build_personalized_digest(
    account_id: i32,
) -> Result<Vec<ScoredPost>, ApiError> {
    use rocket::tokio;

    // Parallel independent reads (same pattern as recommendations).
    let (tags_res, profile_res) = tokio::join!(
        tokio::task::spawn_blocking(move || {
            get_tag_counts(account_id)
                .map_err(|e| format!("Failed to get tag counts: {e}"))
        }),
        tokio::task::spawn_blocking(move || {
            get_account_preference_profile(account_id)
                .map_err(|e| format!("Failed to get account profile: {e}"))
        }),
    );
    let tags = tags_res.map_err(|e| format!("Join error: {e}"))??;
    let profile = profile_res.map_err(|e| format!("Join error: {e}"))??;

    let user_relation = tokio::task::spawn_blocking({
        let tags = tags.clone();
        move || {
            db::load_account_tag_relation(account_id, &tags)
                .map_err(|e| format!("Failed to load user tag relation: {e}"))
        }
    })
    .await
    .map_err(|e| format!("Join error: {e}"))??;

    let idf = current_idf();
    let global_relation = current_global_relation();
    let mut priors = cfg().priors.clone();
    priors.now = Utc::now();

    let ctx = ScoringContext::new_with_blacklist(
        &tags,
        &priors,
        &idf,
        &profile,
        &global_relation,
        &user_relation,
        HashSet::new(), // no additional blacklist for digest
    );

    // Get local candidate posts.
    let local_ids = db::collect_local_candidate_ids(account_id, cfg().runtime.local_candidate_limit)
        .map_err(|e| format!("Failed to collect local candidates: {e}"))?;
    let local_posts = if local_ids.is_empty() {
        Vec::new()
    } else {
        db::hydrate_posts_by_ids(&local_ids)
            .map_err(|e| format!("Failed to hydrate local posts: {e}"))?
    };

    if local_posts.is_empty() {
        return Ok(Vec::new());
    }

    // Score all candidates in parallel (rayon).
    use rayon::prelude::*;
    let cached: Vec<CachedPostFeatures> = local_posts
        .par_iter()
        .map(|post| CachedPostFeatures::from_post_with_user(post, &idf, &global_relation, Some(&user_relation)))
        .collect();

    let scored: Vec<ScoredPost> = local_posts
        .into_par_iter()
        .zip(cached.into_par_iter())
        .map(|(post, cf)| {
            let (s, breakdown, _) = ctx.score_cached_with_metrics(&cf);
            ScoredPost {
                post,
                score: s,
                breakdown: Some(breakdown),
            }
        })
        .collect();

    // Build supporting lists for stratification.
    let mut rng = rand::thread_rng();
    let trending = db::get_trending_posts(7, 6).unwrap_or_default();
    let wildcards = db::get_random_posts_by_group(account_id, 5).unwrap_or_default();
    let recent_added = db::get_owned_post_ids(account_id)
        .ok()
        .and_then(|ids| {
            if ids.is_empty() {
                return None;
            }
            let v: Vec<i64> = ids.into_iter().collect();
            db::hydrate_posts_by_ids(&v[..v.len().min(3)]).ok()
        })
        .unwrap_or_default()
        .into_iter()
        .map(|post| ScoredPost { post, score: 0.0, breakdown: None })
        .collect::<Vec<_>>();
    let popular_new = db::get_popular_posts_since(
        Utc::now() - chrono::Duration::days(2), 3,
    )
    .unwrap_or_default();

    let digest = stratified_sample(&scored, trending, wildcards, recent_added, popular_new, &mut rng);

    // Mark that a personalised digest was built for this user today.
    let _ = db::mark_digest_built(account_id);

    Ok(digest)
}

/// Generic (non-personalised) digest — just trending + popular + random.
/// No scoring pipeline, no user profile needed.
async fn build_generic_digest() -> Result<Vec<ScoredPost>, ApiError> {
    let mut rng = rand::thread_rng();
    let trending = db::get_trending_posts(7, 10).unwrap_or_default();
    let popular_new = db::get_popular_posts_since(
        Utc::now() - chrono::Duration::days(2), 5,
    )
    .unwrap_or_default();
    let random = db::get_random_posts(5).unwrap_or_default();

    let mut digest: Vec<ScoredPost> = Vec::with_capacity(20);
    digest.extend(trending);
    digest.extend(popular_new);
    digest.extend(random);
    digest.shuffle(&mut rng);
    digest.truncate(20);
    Ok(digest)
}

// ---------------------------------------------------------------------------
// Route
// ---------------------------------------------------------------------------

#[openapi(tag = "Digest")]
#[get("/digest/<account_id>?<full>")]
pub(crate) async fn get_daily_digest(
    account_id: i32,
    full: Option<bool>,
    owner: OwnerToken,
) -> Result<Json<Vec<ScoredPost>>, ApiError> {
    validation::validate_account_id(account_id)?;
    let owner_token = owner.0;

    // Verify ownership.
    db_blocking({
        let ot = owner_token.clone();
        move || get_account_by_id(&ot, account_id).map_err(|e| e.to_string())
    })
    .await?;

    // Update visit tracker (fire-and-forget; failure is non-fatal).
    let _ = db::update_visit_tracker(account_id);

    // Determine cache key.
    let today = Utc::now().format("%Y-%m-%d");
    let force_full = full.unwrap_or(false);
    let cache_key = if force_full {
        format!("digest:{}:{}:full", account_id, today)
    } else {
        format!("digest:{}:{}", account_id, today)
    };

    // Check cache.
    if let Some(cached) = cache_get(&cache_key) {
        return Ok(Json(cached));
    }

    // Decide personalised vs generic.
    let use_personalized = force_full
        || db::get_visit_stats(account_id)
            .map(|s| s.visit_streak >= 2 || s.avg_gap_days <= 3.0)
            .unwrap_or(false);

    let posts = if use_personalized {
        build_personalized_digest(account_id).await?
    } else {
        build_generic_digest().await?
    };

    cache_set(cache_key, posts.clone());
    Ok(Json(posts))
}
