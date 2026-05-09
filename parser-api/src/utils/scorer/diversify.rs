//! MMR-style post-list diversification. Runs after `ScoringContext::score`
//! has produced ranked posts.
//!
//! Two entry points:
//! * [`diversify_scored_posts`] — owning, prod-side helper. Builds
//!   features on the fly from `Vec<ScoredPost>` and re-orders the full
//!   list. Used by `/recommendations`.
//! * [`diversify_indices`] — calibrate-side helper. Operates on
//!   pre-built [`DiversityFeatures`] + parallel arrays of `(score,
//!   interaction_fit)`, never clones a `Post`, and only runs MMR over
//!   the top-`head_limit` items by raw score (the tail keeps its score
//!   order). The grid loop calls this once per probe with features
//!   computed once at dataset prep.

use std::collections::HashSet;

use crate::models::{Post, ScoredPost};

use super::priors::Priors;
use super::util::{normalize_tag, FEEDBACK_NEUTRAL};

/// Pre-computed Jaccard-friendly tag sets for one post. Build once at
/// prep time so per-probe MMR doesn't redo the lowercase + HashSet
/// allocation work.
#[derive(Clone)]
pub struct DiversityFeatures {
    artist: HashSet<String>,
    character: HashSet<String>,
    general: HashSet<String>,
}

impl DiversityFeatures {
    pub fn from_post(p: &Post) -> Self {
        let collect = |xs: &[String]| -> HashSet<String> {
            xs.iter().map(|t| normalize_tag(t).into_owned()).collect()
        };
        Self {
            artist: collect(&p.tags.artist),
            character: collect(&p.tags.character),
            general: collect(&p.tags.general),
        }
    }
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f32;
    let union = (a.len() + b.len()) as f32 - inter;
    if union <= 0.0 {
        0.0
    } else {
        inter / union
    }
}

/// Max similarity to any of the last `diversity_window` selected posts,
/// resolved through `features` by index.
fn max_redundancy_indexed(
    cand: &DiversityFeatures,
    selected: &[usize],
    features: &[DiversityFeatures],
    priors: &Priors,
) -> f32 {
    let window = priors.diversity_window.max(1);
    let mut max_sim = 0.0f32;
    for &i in selected.iter().rev().take(window) {
        let chosen = &features[i];
        let sim = jaccard(&cand.artist, &chosen.artist) * priors.diversity_w_artist
            + jaccard(&cand.character, &chosen.character) * priors.diversity_w_character
            + jaccard(&cand.general, &chosen.general) * priors.diversity_w_general;
        if sim > max_sim {
            max_sim = sim;
        }
    }
    let exp = priors.mmr_redundancy_exp;
    if (exp - 1.0).abs() < 1e-3 {
        max_sim
    } else {
        max_sim.max(0.0).powf(exp.clamp(0.1, 5.0)).clamp(0.0, 1.0)
    }
}

/// Index-based MMR re-ranker. Returns indices in their final order.
///
/// `entries[i] = (score, interaction_fit, tiebreak_id)` is parallel to
/// `features[i]`. `head_limit` caps how many of the top-by-score items
/// participate in MMR; everything past that keeps its raw-score
/// ordering. Pass `head_limit >= entries.len()` for full-list MMR
/// (legacy behaviour).
pub fn diversify_indices(
    entries: &[(f32, f32, i64)],
    features: &[DiversityFeatures],
    priors: &Priors,
    head_limit: usize,
) -> Vec<usize> {
    let n = entries.len();
    if n == 0 {
        return Vec::new();
    }
    debug_assert_eq!(n, features.len());

    let mut idx_by_score: Vec<usize> = (0..n).collect();
    idx_by_score.sort_by(|&a, &b| {
        entries[b]
            .0
            .partial_cmp(&entries[a].0)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let head_n = head_limit.min(n);
    if head_n <= 1 {
        return idx_by_score;
    }

    let mut available: Vec<usize> = idx_by_score[..head_n].to_vec();
    let mut selected: Vec<usize> = Vec::with_capacity(head_n);
    let mut top_score = available.iter().map(|&i| entries[i].0).fold(f32::MIN, f32::max);

    let damp = priors.diversity_interaction_damp.clamp(0.0, 1.0);
    let max_penalty = priors.diversity_max_penalty.clamp(0.0, 1.0);

    while !available.is_empty() {
        let mut best_pos = 0usize;
        let mut best_value = f32::MIN;
        let mut best_tiebreak = i64::MAX;

        for (pos, &i) in available.iter().enumerate() {
            let (score, interaction, tid) = entries[i];
            let redundancy = max_redundancy_indexed(&features[i], &selected, features, priors);
            let gap = (top_score - score).max(0.0);
            let penalty = (redundancy * gap * (1.0 - damp * interaction)).clamp(0.0, max_penalty);
            let adj = score - penalty;
            if adj > best_value || (adj == best_value && tid < best_tiebreak) {
                best_value = adj;
                best_pos = pos;
                best_tiebreak = tid;
            }
        }

        let chosen_idx = available.swap_remove(best_pos);
        let removed_was_top = (entries[chosen_idx].0 - top_score).abs() < 1e-6;
        selected.push(chosen_idx);
        if removed_was_top && !available.is_empty() {
            top_score = available.iter().map(|&i| entries[i].0).fold(f32::MIN, f32::max);
        }
    }

    selected.extend(idx_by_score[head_n..].iter().copied());
    selected
}

/// Owning re-rank used by the production `/recommendations` route.
/// Builds [`DiversityFeatures`] on the fly and runs MMR over the whole
/// list (no top-K cutoff) to preserve historical behaviour.
pub fn diversify_scored_posts(posts: Vec<ScoredPost>, priors: &Priors) -> Vec<ScoredPost> {
    if posts.is_empty() {
        return posts;
    }
    let features: Vec<DiversityFeatures> = posts
        .iter()
        .map(|sp| DiversityFeatures::from_post(&sp.post))
        .collect();
    let entries: Vec<(f32, f32, i64)> = posts
        .iter()
        .map(|sp| {
            let interaction = sp
                .breakdown
                .as_ref()
                .map(|b| b.interaction_fit)
                .unwrap_or(FEEDBACK_NEUTRAL);
            (sp.score, interaction, sp.post.id)
        })
        .collect();

    let order = diversify_indices(&entries, &features, priors, posts.len());

    let mut slots: Vec<Option<ScoredPost>> = posts.into_iter().map(Some).collect();
    let mut out: Vec<ScoredPost> = Vec::with_capacity(slots.len());
    for i in order {
        if let Some(sp) = slots[i].take() {
            out.push(sp);
        }
    }
    out
}
