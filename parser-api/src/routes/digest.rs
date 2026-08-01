//! Daily digest route — a lightweight, personalised (or generic) page of
//! up to 20 posts, stratified across randomised tiers.
//!
//! Active users get the full scoring pipeline; infrequent users get a cheap
//! trending/random mix.
//!
//! `GET /digest/<account_id>?<full>&<exclude_saved>`
//!
//! ## Freshness guarantees (v6+)
//!
//! - **TTL reduced** to 6 h (was 24 h) — the cache is a coarse cap, not a
//!   "same all day" guarantee.
//! - **Every stratum randomises** its selection — top-3 are picked from
//!   top-6, trending is a random subset, etc.
//! - **`get_trending_posts`** uses `ORDER BY RANDOM()` over a 30-day window
//!   (was 7 days) with a 6× pool.
//! - **`get_popular_posts_since`** uses a 7-day window (was 2 days).
//! - **Fresh-blood layer** (`get_old_random_posts`): 8 random posts older
//!   than 14 days that the user hasn't seen in `feed_interactions`. Guarantees
//!   new faces even when the user's favourite-tag pool is exhausted.
//! - **Seen-post dedup**: the digest filters out any post shown in the feed
//!   within the last 7 days via `get_digest_seen_ids`.
//!
//! ## Personalised vs generic dispatch
//!
//! The route calls `update_visit_tracker` (fire-and-forget) and checks
//! `get_visit_stats`. Users with `visit_streak >= 2` or `avg_gap_days <= 3`
//! get the personalised pipeline; everyone else gets the generic mix.
//! Pass `?full=true` to force the personalised pipeline.

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
use e621_account_parser_api::models::{Post, Rating, ScoredPost, cfg};
use e621_account_parser_api::utils::{
    CachedPostFeatures, ScoringContext, current_global_relation, current_idf,
};
use e621_account_parser_api::validation;

/// Apply supported blacklist metatags to locally cached candidates (mirrors
/// the same-named function in feed.rs). e621 handles these for live results;
/// cached posts need the equivalent check.
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
            "rating:s" | "rating:safe" => matches!(post.rating, Rating::S),
            "rating:q" | "rating:questionable" => matches!(post.rating, Rating::Q),
            "rating:e" | "rating:explicit" => matches!(post.rating, Rating::E),
            rule if rule.starts_with("uploader:!") => rule[10..]
                .parse::<i64>()
                .is_ok_and(|id| id == post.uploader_id),
            _ => false,
        })
}

// ---------------------------------------------------------------------------
// In-memory TTL cache
// ---------------------------------------------------------------------------

struct CachedDigest {
    posts: Vec<ScoredPost>,
    cached_at: Instant,
}

static DIGEST_CACHE: LazyLock<Mutex<HashMap<String, CachedDigest>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

const DIGEST_TTL_SECS: u64 = 21600; // 6 hours — shorter TTL + randomness in stratified_sample means the digest changes more noticeably day-to-day

/// Get digest seen-post IDs from `feed_interactions` for the last 7 days.
/// This prevents the digest from showing posts the user already saw in
/// their feed recently.
fn get_digest_seen_ids(account_id: i32) -> HashSet<i64> {
    match db::get_recently_seen_post_ids(account_id, 7) {
        Ok(ids) => ids,
        Err(e) => {
            warn!("digest: failed to load seen post IDs: {e}");
            HashSet::new()
        }
    }
}

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
    map.insert(
        key,
        CachedDigest {
            posts,
            cached_at: Instant::now(),
        },
    );
}

// ---------------------------------------------------------------------------
// Stratified sampling helper
// ---------------------------------------------------------------------------

/// Sample up to 20 posts from a scored list using stratified strategy:
///   top-3    best picks,
///   4-8      semi-random from upper-mid,
///   9-12     trending (random subset),
///   13-16    fresh blood — random old/unseen posts,
///   17-18    wildcard by group,
///   19-20    popular-new + recent-added.
///
/// Each stratum randomises its selection so the digest changes noticeably
/// from day to day even when the underlying scored pool is similar.
fn stratified_sample(
    scored: &[ScoredPost],
    trending: Vec<ScoredPost>,
    wildcards: Vec<ScoredPost>,
    recent_added: Vec<ScoredPost>,
    popular_new: Vec<ScoredPost>,
    fresh_blood: Vec<ScoredPost>,
    rng: &mut impl rand::Rng,
) -> Vec<ScoredPost> {
    let mut digest = Vec::with_capacity(20);
    let mut seen: HashSet<i64> = HashSet::new();

    let mut try_add = |sp: ScoredPost, max: usize| -> bool {
        if digest.len() >= max {
            return false;
        }
        if seen.insert(sp.post.id) {
            digest.push(sp);
            true
        } else {
            false
        }
    };

    // 1. Top-3 best picks (randomize which of the top-6 make it)
    let top_n = scored.len().min(6);
    for sp in scored[..top_n].choose_multiple(rng, 3).cloned() {
        try_add(sp, 20);
    }

    // 2. Semi-random from middle third of scored list (pick 5 instead of 4)
    if scored.len() > 8 {
        let mid_start = scored.len() / 3;
        let mid_end = (scored.len() * 2 / 3).min(scored.len());
        if mid_end > mid_start {
            let slice = &scored[mid_start..mid_end];
            for sp in slice.choose_multiple(rng, 5).cloned() {
                try_add(sp, 20);
            }
        }
    }

    // 3. Trending — pick a random subset of trending posts
    let trending_count = trending.len().min(4);
    if !trending.is_empty() {
        for sp in trending.choose_multiple(rng, trending_count).cloned() {
            try_add(sp, 20);
        }
    }

    // 4. Fresh blood: random old/unseen posts — guarantees new faces even
    //    when the scored pool hasn't changed. This replaced the old
    //    "exploration from bottom half" stratum.
    if !fresh_blood.is_empty() {
        let blood_count = fresh_blood.len().min(4);
        for sp in fresh_blood.choose_multiple(rng, blood_count).cloned() {
            try_add(sp, 20);
        }
    }

    // 5. Wildcards
    if !wildcards.is_empty() {
        for sp in wildcards.choose_multiple(rng, 2).cloned() {
            try_add(sp, 20);
        }
    }

    // 6. Recent added + popular new
    let extras: Vec<&ScoredPost> = recent_added.iter().chain(popular_new.iter()).collect();
    if !extras.is_empty() {
        for sp in extras.choose_multiple(rng, 2) {
            try_add((*sp).clone(), 20);
        }
    }

    // If we still have room, fill with random from the full scored list.
    // Count remaining BEFORE calling try_add (borrow conflict otherwise).
    if digest.len() < 20 && scored.len() > 3 {
        let still_needed = 20 - digest.len();
        for sp in scored[3..].choose_multiple(rng, still_needed).cloned() {
            if digest.len() >= 20 {
                break;
            }
            if seen.insert(sp.post.id) {
                digest.push(sp);
            }
        }
    }

    digest
}

// ---------------------------------------------------------------------------
// Builders
// ---------------------------------------------------------------------------

/// Full personalised digest: runs the scoring pipeline on local candidates
/// and applies stratified sampling.
///
/// When `exclude_saved` is true, posts already in the user's owned set are
/// filtered out of every candidate stream before stratification so the
/// final digest never echoes back items the user has already favourited.
async fn build_personalized_digest(
    account_id: i32,
    exclude_saved: bool,
    account_blacklist: &str,
) -> Result<Vec<ScoredPost>, ApiError> {
    use rocket::tokio;

    // Parallel independent reads (same pattern as recommendations).
    let (tags_res, profile_res) = tokio::join!(
        tokio::task::spawn_blocking(move || {
            get_tag_counts(account_id).map_err(|e| format!("Failed to get tag counts: {e}"))
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

    // Parse blacklist (same logic as feed.rs build_recommendations_shared).
    let blacklisted_simple_tags: Vec<String> = account_blacklist
        .lines()
        .flat_map(|l| l.split_whitespace().filter(|t| !t.is_empty()))
        .filter(|t| !t.contains(':') && !t.starts_with('-'))
        .map(str::to_lowercase)
        .collect();
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

    // Owned post IDs serve double duty: as a source for the "recent added"
    // stratum below, and (when exclude_saved is on) as the dedup set against
    // every other candidate stream.
    let owned: HashSet<i64> = db::get_owned_post_ids(account_id).unwrap_or_default();
    let drop_owned = exclude_saved;

    // Get local candidate posts.
    let local_ids =
        db::collect_local_candidate_ids(account_id, cfg().runtime.local_candidate_limit)
            .map_err(|e| format!("Failed to collect local candidates: {e}"))?;
    let local_posts = if local_ids.is_empty() {
        Vec::new()
    } else {
        let mut posts = db::hydrate_posts_by_ids(&local_ids)
            .map_err(|e| format!("Failed to hydrate local posts: {e}"))?;
        posts.retain(|p| !matches_local_blacklist_metatag(p, account_blacklist));
        if drop_owned {
            posts.retain(|p| !owned.contains(&p.id));
        }
        posts
    };

    // Score all candidates in parallel (rayon).
    let scored: Vec<ScoredPost> = if local_posts.is_empty() {
        Vec::new()
    } else {
        use rayon::prelude::*;
        let cached: Vec<CachedPostFeatures> = local_posts
            .par_iter()
            .map(|post| {
                CachedPostFeatures::from_post_with_user(
                    post,
                    &idf,
                    &global_relation,
                    Some(&user_relation),
                )
            })
            .collect();

        let mut s: Vec<ScoredPost> = local_posts
            .into_par_iter()
            .zip(cached.into_par_iter())
            .map(|(post, cf)| {
                let (score, breakdown, _) = ctx.score_cached_with_metrics(&cf);
                ScoredPost {
                    post,
                    score,
                    breakdown: Some(breakdown),
                }
            })
            .collect();
        s.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        s
    };

    let filter_blacklist = |posts: Vec<Post>| -> Vec<Post> {
        posts
            .into_iter()
            .filter(|p| !matches_local_blacklist_metatag(p, account_blacklist))
            .collect()
    };

    let filter_owned = |list: Vec<ScoredPost>| -> Vec<ScoredPost> {
        if drop_owned {
            list.into_iter()
                .filter(|sp| !owned.contains(&sp.post.id))
                .collect()
        } else {
            list
        }
    };

    // Score a batch of un-scored posts using the current ScoringContext.
    let score_batch = |posts: Vec<Post>| -> Vec<ScoredPost> {
        use rayon::prelude::*;
        posts
            .into_par_iter()
            .map(|post| {
                let cf = CachedPostFeatures::from_post_with_user(
                    &post,
                    &idf,
                    &global_relation,
                    Some(&user_relation),
                );
                let (s, breakdown, _) = ctx.score_cached_with_metrics(&cf);
                ScoredPost {
                    post,
                    score: s,
                    breakdown: Some(breakdown),
                }
            })
            .collect()
    };

    // Load seen IDs so digest avoids posts the user just saw in feed.
    let seen_ids = get_digest_seen_ids(account_id);
    let filter_seen = |list: Vec<ScoredPost>| -> Vec<ScoredPost> {
        if seen_ids.is_empty() {
            return list;
        }
        list.into_iter()
            .filter(|sp| !seen_ids.contains(&sp.post.id))
            .collect()
    };

    // Build supporting lists for stratification, now with full scoring.
    let mut rng = rand::thread_rng();
    // Trending: 30-day window instead of 7, wider pool
    let trending = {
        let raw = db::get_trending_posts(30, 6).unwrap_or_default();
        let posts: Vec<Post> = filter_blacklist(raw.into_iter().map(|sp| sp.post).collect());
        filter_seen(filter_owned(score_batch(posts)))
    };
    let wildcards = {
        let raw = db::get_random_posts_by_group(account_id, 5).unwrap_or_default();
        let posts: Vec<Post> = filter_blacklist(raw.into_iter().map(|sp| sp.post).collect());
        filter_seen(filter_owned(score_batch(posts)))
    };
    // "Recent added" surfaces the user's own posts on purpose, so skip it
    // entirely when the user asked to hide saved items.
    let recent_added = if drop_owned {
        Vec::new()
    } else {
        let mut ids: Vec<i64> = owned.iter().copied().collect();
        ids.truncate(3);
        if ids.is_empty() {
            Vec::new()
        } else {
            let posts = filter_blacklist(db::hydrate_posts_by_ids(&ids).unwrap_or_default());
            filter_seen(score_batch(posts))
        }
    };
    // Popular new: 7-day window instead of 2
    let popular_new = {
        let raw = db::get_popular_posts_since(Utc::now() - chrono::Duration::days(7), 3)
            .unwrap_or_default();
        let posts: Vec<Post> = filter_blacklist(raw.into_iter().map(|sp| sp.post).collect());
        filter_seen(filter_owned(score_batch(posts)))
    };
    // Fresh blood: random old posts (older than 14 days) that the user hasn't
    // seen yet. Guarantees new faces even when the user's favourite-tag pool
    // is small and has been exhausted.
    let fresh_blood = {
        let raw = db::get_old_random_posts(14, 8).unwrap_or_default();
        let posts: Vec<Post> = filter_blacklist(raw.into_iter().map(|sp| sp.post).collect());
        filter_seen(filter_owned(score_batch(posts)))
    };

    let digest = stratified_sample(
        &scored,
        trending,
        wildcards,
        recent_added,
        popular_new,
        fresh_blood,
        &mut rng,
    );

    // Mark that a personalised digest was built for this user today.
    // Non-fatal failure: the next request still works, the user just
    // won't have last_digest_date updated. Surface via audit so silent
    // staleness shows up in operator logs.
    if let Err(e) = db::mark_digest_built(account_id) {
        e621_account_parser_api::audit::event("digest.mark_built_failed")
            .field("account_id", account_id)
            .field("error", e)
            .emit_err();
    }

    Ok(digest)
}

/// Generic (non-personalised) digest — trending + popular + random + fresh.
/// Uses the same widened windows and randomness as the personalised path.
/// Scores posts using global defaults so breakdown is always available.
async fn build_generic_digest(
    account_id: i32,
    exclude_saved: bool,
) -> Result<Vec<ScoredPost>, ApiError> {
    use rayon::prelude::*;

    let mut rng = rand::thread_rng();
    // V6: widened windows matching personalised path
    let trending = db::get_trending_posts(30, 10).unwrap_or_default();
    let popular_new =
        db::get_popular_posts_since(Utc::now() - chrono::Duration::days(7), 5).unwrap_or_default();
    let fresh_blood = db::get_old_random_posts(14, 8).unwrap_or_default();
    let random = db::get_random_posts(5).unwrap_or_default();

    // Seen-post dedup via feed_interactions (7 days).
    let seen_ids = get_digest_seen_ids(account_id);

    let mut all_posts: Vec<ScoredPost> = Vec::with_capacity(20);
    all_posts.extend(trending);
    all_posts.extend(popular_new);
    all_posts.extend(fresh_blood);
    all_posts.extend(random);

    if exclude_saved {
        let owned: HashSet<i64> = db::get_owned_post_ids(account_id).unwrap_or_default();
        if !owned.is_empty() {
            all_posts.retain(|sp| !owned.contains(&sp.post.id));
        }
    }
    if !seen_ids.is_empty() {
        all_posts.retain(|sp| !seen_ids.contains(&sp.post.id));
    }

    all_posts.shuffle(&mut rng);
    all_posts.truncate(20);

    // Score with default priors so breakdown is populated even for cold users.
    let idf = current_idf();
    let global_relation = current_global_relation();
    let mut priors = cfg().priors.clone();
    priors.now = Utc::now();
    let profile = e621_account_parser_api::models::AccountPreferenceProfile::default();
    let ctx = ScoringContext::new(
        &[],
        &priors,
        &idf,
        &profile,
        &global_relation,
        &global_relation,
    );

    let scored: Vec<ScoredPost> = all_posts
        .into_par_iter()
        .map(|sp| {
            let cf = CachedPostFeatures::from_post(&sp.post, &idf, &global_relation);
            let (s, breakdown, _) = ctx.score_cached_with_metrics(&cf);
            ScoredPost {
                post: sp.post,
                score: s,
                breakdown: Some(breakdown),
            }
        })
        .collect();

    Ok(scored)
}

// ---------------------------------------------------------------------------
// Route
// ---------------------------------------------------------------------------

#[openapi(tag = "Digest")]
#[get("/digest/<account_id>?<full>&<exclude_saved>")]
pub(crate) async fn get_daily_digest(
    account_id: i32,
    full: Option<bool>,
    exclude_saved: Option<bool>,
    owner: OwnerToken,
) -> Result<Json<Vec<ScoredPost>>, ApiError> {
    validation::validate_account_id(account_id)?;
    let owner_token = owner.0;

    // Verify ownership and load account (needed for blacklist).
    let account = db_blocking({
        let ot = owner_token.clone();
        move || get_account_by_id(&ot, account_id).map_err(|e| e.clone())
    })
    .await?;

    // Update visit tracker (fire-and-forget; failure is non-fatal but
    // affects the personalised-vs-generic dispatch below — losing
    // visit-streak makes the user perpetually "cold", so silent
    // failure here changes behaviour in a confusing way).
    if let Err(e) = db::update_visit_tracker(account_id) {
        e621_account_parser_api::audit::event("digest.visit_tracker_failed")
            .field("account_id", account_id)
            .field("error", e)
            .emit_err();
    }

    // Determine cache key. Each toggle (full / exclude_saved) yields a
    // distinct cached payload so users flipping the switch don't see a
    // stale list from the opposite mode.
    let today = Utc::now().format("%Y-%m-%d");
    let force_full = full.unwrap_or(false);
    let hide_saved = exclude_saved.unwrap_or(false);
    let cache_key = format!(
        "digest:{}:{}{}{}",
        account_id,
        today,
        if force_full { ":full" } else { "" },
        if hide_saved { ":nosaved" } else { "" },
    );

    // Check cache.
    if let Some(cached) = cache_get(&cache_key) {
        e621_account_parser_api::audit::event("feed.digest")
            .field("account_id", account_id)
            .field("mode", "cached")
            .field("returned", cached.len())
            .emit();
        e621_account_parser_api::metrics::METRICS
            .digest_views_total
            .with_label_values(&[&account_id.to_string()])
            .inc();
        return Ok(Json(cached));
    }

    // Decide personalised vs generic.
    let use_personalized = force_full
        || db::get_visit_stats(account_id)
            .is_ok_and(|s| s.visit_streak >= 2 || s.avg_gap_days <= 3.0);

    let posts = if use_personalized {
        build_personalized_digest(account_id, hide_saved, &account.blacklist).await?
    } else {
        build_generic_digest(account_id, hide_saved).await?
    };

    e621_account_parser_api::audit::event("feed.digest")
        .field("account_id", account_id)
        .field(
            "mode",
            if use_personalized {
                "personalized"
            } else {
                "generic"
            },
        )
        .field("returned", posts.len())
        .field("hide_saved", hide_saved)
        .emit();
    e621_account_parser_api::metrics::METRICS
        .digest_views_total
        .with_label_values(&[&account_id.to_string()])
        .inc();
    cache_set(cache_key, posts.clone());
    Ok(Json(posts))
}
