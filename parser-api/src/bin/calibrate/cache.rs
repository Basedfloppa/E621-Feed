//! Per-channel score cache for the grid loop.
//!
//! Each grid probe applies a small delta to one knob; most probes affect
//! only a single scoring channel (e.g. `quality_a` only changes
//! `quality_fit`). Without caching we recompute all 8 channels for every
//! (account, post) on every probe — ~624 probes × 1000 accounts × ~220
//! posts × 8 channels = the bulk of the 8-hour run.
//!
//! With this cache:
//!   1. The baseline eval runs every channel and stores the result
//!      per (account, post) in a `ScoreCache`.
//!   2. Each probe gets a `ChannelMask` from the knob registry naming
//!      which channels its delta invalidates. Only those channels are
//!      recomputed; the rest are read out of the prior cache.
//!   3. The final mix-blend / temperature shaping / strong-negative
//!      penalty are always reapplied (they depend on `mix_*`,
//!      `score_temperature`, `strong_negative_penalty` regardless of
//!      what was invalidated).
//!   4. If a probe wins, its (partially-rebuilt) cache is promoted to
//!      the new baseline. If it loses, the cache is dropped.
//!
//! Knob → mask wiring lives in [`crate::knobs`]; if a knob's invalidation
//! shape changes, update both the bit and the comment there.
//!
//! The cache itself is per-grid-run scratch — never persisted across
//! `run_grid` calls.

use chrono::{DateTime, Utc};
use rayon::prelude::*;

use e621_account_parser_api::models::{Post, ScoreBreakdown, ScoredPost};
use e621_account_parser_api::utils::{
    diversify_scored_posts, CachedPostFeatures, Priors, ScoringContext,
};

use crate::dataset::EvalDataset;
use crate::metrics::{mrr_pub, ndcg_at_k_pub, pool, recall_at_k_pub, Metrics};

// ---- ChannelMask -----------------------------------------------------------
//
// Bitmask layout. The wider type leaves room for splitting channels
// later (e.g. separating recency_global vs recency_personal) without
// rewriting the knob registry.

pub(crate) const M_SIM: u16 = 1 << 0;
pub(crate) const M_QUALITY: u16 = 1 << 1;
pub(crate) const M_RECENCY: u16 = 1 << 2;
pub(crate) const M_RATING: u16 = 1 << 3;
pub(crate) const M_MEDIA: u16 = 1 << 4;
pub(crate) const M_POPULARITY: u16 = 1 << 5;
pub(crate) const M_INTERACTION: u16 = 1 << 6;
pub(crate) const M_TAG_RELATION: u16 = 1 << 7;

/// Empty mask: probe touched only mix weights / temperature / penalty.
/// All channels reused from prior cache; only the final blend is rerun.
pub(crate) const M_NONE: u16 = 0;
/// Full mask: rebuild from scratch (used for baseline + diversify path).
pub(crate) const M_ALL: u16 = M_SIM
    | M_QUALITY
    | M_RECENCY
    | M_RATING
    | M_MEDIA
    | M_POPULARITY
    | M_INTERACTION
    | M_TAG_RELATION;

// Common combos used by the knob registry. Defining them as named
// constants makes the registry table easier to read.

/// `coldstart_n0` / `confidence_steepness` change `personal_confidence`,
/// which is consumed by rating / media / recency / tag_relation.
pub(crate) const M_CONFIDENCE_DERIVED: u16 = M_RATING | M_MEDIA | M_RECENCY | M_TAG_RELATION;
/// `discrete_smoothing_alpha` / `discrete_pref_floor` /
/// `coldstart_smoothing_boost` change rating + media smoothing only.
pub(crate) const M_DISCRETE: u16 = M_RATING | M_MEDIA;
/// `one_sided_ratio_exp` changes both quality and popularity ratios.
pub(crate) const M_RATIO_EXP: u16 = M_QUALITY | M_POPULARITY;
/// `group_w_*` change the per-group weight vector → sim+interaction+tag_relation.
pub(crate) const M_GROUP_W: u16 = M_SIM | M_INTERACTION | M_TAG_RELATION;

// ---- ChannelScores + ScoreCache --------------------------------------------

#[derive(Clone, Copy, Default)]
pub(crate) struct ChannelScores {
    pub(crate) sim: f32,
    pub(crate) quality: f32,
    pub(crate) recency: f32,
    pub(crate) rating: f32,
    pub(crate) media: f32,
    pub(crate) popularity: f32,
    pub(crate) interaction: f32,
    pub(crate) tag_relation: f32,
    pub(crate) veto: bool,
}

impl ChannelScores {
    fn into_breakdown(self) -> ScoreBreakdown {
        ScoreBreakdown {
            tag_similarity: self.sim,
            quality_fit: self.quality,
            recency_fit: self.recency,
            rating_fit: self.rating,
            media_fit: self.media,
            popularity_fit: self.popularity,
            interaction_fit: self.interaction,
            tag_relation_fit: self.tag_relation,
        }
    }
}

/// Per-account ranked entries. Keeps post ids / positives parallel to
/// channel scores so a partial-mask probe can reblend them without
/// re-reading the dataset.
pub(crate) struct AccountChannelCache {
    pub(crate) channels: Vec<ChannelScores>,
}

pub(crate) struct ScoreCache {
    pub(crate) accounts: Vec<AccountChannelCache>,
}

// ---- Per-post selective recompute ------------------------------------------

#[inline]
fn recompute_one_post(
    ctx: &ScoringContext<'_>,
    post: &Post,
    features: &CachedPostFeatures,
    now: DateTime<Utc>,
    prev: ChannelScores,
    mask: u16,
) -> ChannelScores {
    let mut next = prev;

    if mask & M_SIM != 0 {
        next.sim = ctx.tag_similarity_cached(features);
    }
    if mask & M_QUALITY != 0 {
        next.quality = ctx.quality_fit(post);
    }
    if mask & M_POPULARITY != 0 {
        next.popularity = ctx.popularity_fit(post);
    }
    if mask & M_RATING != 0 {
        next.rating = ctx.rating_fit(post);
    }
    if mask & M_MEDIA != 0 {
        next.media = ctx.media_fit(post);
    }
    if mask & M_INTERACTION != 0 {
        let (val, veto) = ctx.interaction_fit_cached(features);
        next.interaction = val;
        next.veto = veto;
    }
    if mask & M_TAG_RELATION != 0 {
        next.tag_relation = ctx.tag_relation_fit_cached(features);
    }
    if mask & M_RECENCY != 0 {
        let age_days = (now - post.created_at).num_seconds() as f32 / 86_400.0;
        next.recency = ctx.recency_fit(age_days);
    }

    next
}

// ---- score_with_cache (main entry point) ----------------------------------

/// Drop-in replacement for `score_with` that reuses prior-probe channel
/// scores when `prev` is `Some` and `mask` excludes some channels.
///
/// `prev=None` (or `mask=M_ALL`) → full rebuild, equivalent to the
/// uncached path. `diversify=true` always forces a full rebuild and
/// returns an empty cache (callers should not promote on diversify).
pub(crate) fn score_with_cache(
    dataset: &EvalDataset,
    priors: &Priors,
    now: DateTime<Utc>,
    top_k_ndcg: usize,
    top_k_recall: usize,
    diversify: bool,
    prev: Option<&ScoreCache>,
    mask: u16,
) -> (Metrics, ScoreCache) {
    if diversify {
        let m = score_full_with_diversify(dataset, priors, now, top_k_ndcg, top_k_recall);
        return (m, ScoreCache { accounts: Vec::new() });
    }

    let mut priors = priors.clone();
    priors.now = now;
    let priors = &priors;

    let effective_mask = if prev.is_none() { M_ALL } else { mask };
    let total = dataset.accounts.len();

    let per_account: Vec<(AccountChannelCache, (f64, f64, f64))> = pool().install(|| {
        dataset
            .accounts
            .par_iter()
            .enumerate()
            .map(|(acc_idx, fx)| {
                let ctx = ScoringContext::new(
                    &fx.tags,
                    priors,
                    &dataset.idf,
                    &fx.profile,
                    &dataset.global_relation,
                    &dataset.empty_user_relation,
                );

                let total_posts = fx.test_features.len() + fx.neg_features.len();
                let mut next_channels: Vec<ChannelScores> = Vec::with_capacity(total_posts);
                let mut scored: Vec<(i64, f32, bool)> = Vec::with_capacity(total_posts);

                let prev_acc = prev.map(|c| &c.accounts[acc_idx]);

                for (i, (post, features)) in fx
                    .test_posts
                    .iter()
                    .zip(fx.test_features.iter())
                    .enumerate()
                {
                    let prior = prev_acc
                        .map(|a| a.channels[i])
                        .unwrap_or_default();
                    let ch =
                        recompute_one_post(&ctx, post, features, priors.now, prior, effective_mask);
                    let score = ctx.final_blend(
                        ch.sim,
                        ch.quality,
                        ch.recency,
                        ch.rating,
                        ch.media,
                        ch.popularity,
                        ch.interaction,
                        ch.tag_relation,
                        ch.veto,
                    );
                    next_channels.push(ch);
                    scored.push((post.id, score, true));
                }
                for (j, (post, features)) in fx
                    .neg_posts
                    .iter()
                    .zip(fx.neg_features.iter())
                    .enumerate()
                {
                    let cache_idx = fx.test_features.len() + j;
                    let prior = prev_acc
                        .map(|a| a.channels[cache_idx])
                        .unwrap_or_default();
                    let ch =
                        recompute_one_post(&ctx, post, features, priors.now, prior, effective_mask);
                    let score = ctx.final_blend(
                        ch.sim,
                        ch.quality,
                        ch.recency,
                        ch.rating,
                        ch.media,
                        ch.popularity,
                        ch.interaction,
                        ch.tag_relation,
                        ch.veto,
                    );
                    next_channels.push(ch);
                    scored.push((post.id, score, false));
                }

                scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

                let metrics_tuple = (
                    ndcg_at_k_pub(&scored, top_k_ndcg),
                    recall_at_k_pub(&scored, top_k_recall, fx.test_count),
                    mrr_pub(&scored),
                );

                (
                    AccountChannelCache {
                        channels: next_channels,
                    },
                    metrics_tuple,
                )
            })
            .collect()
    });

    let mut totals = Metrics::default();
    let mut accounts_out: Vec<AccountChannelCache> = Vec::with_capacity(total);
    for (acc, (n, r, m)) in per_account {
        accounts_out.push(acc);
        totals.ndcg_at_k += n;
        totals.recall_at_k += r;
        totals.mrr += m;
        totals.n_accounts += 1;
    }

    (totals, ScoreCache { accounts: accounts_out })
}

// ---- Diversify fallback (full rebuild, no cache promote) -------------------

fn score_full_with_diversify(
    dataset: &EvalDataset,
    priors: &Priors,
    now: DateTime<Utc>,
    top_k_ndcg: usize,
    top_k_recall: usize,
) -> Metrics {
    let mut priors = priors.clone();
    priors.now = now;
    let priors = &priors;

    let per_account: Vec<(f64, f64, f64)> = pool().install(|| {
        dataset
            .accounts
            .par_iter()
            .map(|fx| {
                let ctx = ScoringContext::new(
                    &fx.tags,
                    priors,
                    &dataset.idf,
                    &fx.profile,
                    &dataset.global_relation,
                    &dataset.empty_user_relation,
                );

                let mut sps: Vec<ScoredPost> =
                    Vec::with_capacity(fx.test_posts.len() + fx.neg_posts.len());
                for (post, features) in fx.test_posts.iter().zip(fx.test_features.iter()) {
                    let ch = recompute_one_post(
                        &ctx,
                        post,
                        features,
                        priors.now,
                        ChannelScores::default(),
                        M_ALL,
                    );
                    let score = ctx.final_blend(
                        ch.sim,
                        ch.quality,
                        ch.recency,
                        ch.rating,
                        ch.media,
                        ch.popularity,
                        ch.interaction,
                        ch.tag_relation,
                        ch.veto,
                    );
                    sps.push(ScoredPost {
                        post: post.clone(),
                        score,
                        breakdown: Some(ch.into_breakdown()),
                    });
                }
                for (post, features) in fx.neg_posts.iter().zip(fx.neg_features.iter()) {
                    let ch = recompute_one_post(
                        &ctx,
                        post,
                        features,
                        priors.now,
                        ChannelScores::default(),
                        M_ALL,
                    );
                    let score = ctx.final_blend(
                        ch.sim,
                        ch.quality,
                        ch.recency,
                        ch.rating,
                        ch.media,
                        ch.popularity,
                        ch.interaction,
                        ch.tag_relation,
                        ch.veto,
                    );
                    sps.push(ScoredPost {
                        post: post.clone(),
                        score,
                        breakdown: Some(ch.into_breakdown()),
                    });
                }

                let positives: std::collections::HashSet<i64> =
                    fx.test_posts.iter().map(|p| p.id).collect();
                let diversified = diversify_scored_posts(sps, priors);
                let mut scored: Vec<(i64, f32, bool)> = Vec::with_capacity(diversified.len());
                for sp in diversified {
                    let id = sp.post.id;
                    scored.push((id, sp.score, positives.contains(&id)));
                }
                (
                    ndcg_at_k_pub(&scored, top_k_ndcg),
                    recall_at_k_pub(&scored, top_k_recall, fx.test_count),
                    mrr_pub(&scored),
                )
            })
            .collect()
    });

    let mut totals = Metrics::default();
    for (n, r, m) in per_account {
        totals.ndcg_at_k += n;
        totals.recall_at_k += r;
        totals.mrr += m;
        totals.n_accounts += 1;
    }
    totals
}
