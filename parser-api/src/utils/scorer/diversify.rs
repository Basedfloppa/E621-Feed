//! MMR-style post-list diversification. Runs after `ScoringContext::score`
//! has produced ranked `ScoredPost`s.

use std::collections::HashSet;

use crate::models::{Post, ScoredPost};

use super::priors::Priors;
use super::util::{normalize_tag, FEEDBACK_NEUTRAL};

struct PostFeatures {
    artist: HashSet<String>,
    character: HashSet<String>,
    general: HashSet<String>,
}

impl PostFeatures {
    fn from_post(p: &Post) -> Self {
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

/// Max similarity to any of the last `diversity_window` selected posts.
fn max_redundancy(cand: &PostFeatures, selected: &[PostFeatures], priors: &Priors) -> f32 {
    let window = priors.diversity_window.max(1);
    let mut max_sim = 0.0f32;
    for chosen in selected.iter().rev().take(window) {
        let sim = jaccard(&cand.artist, &chosen.artist) * priors.diversity_w_artist
            + jaccard(&cand.character, &chosen.character) * priors.diversity_w_character
            + jaccard(&cand.general, &chosen.general) * priors.diversity_w_general;
        if sim > max_sim {
            max_sim = sim;
        }
    }
    // Class C v5.3: redundancy^p shaping. p=1 → linear (legacy).
    let exp = priors.mmr_redundancy_exp;
    if (exp - 1.0).abs() < 1e-3 {
        max_sim
    } else {
        max_sim.max(0.0).powf(exp.clamp(0.1, 5.0)).clamp(0.0, 1.0)
    }
}

pub fn diversify_scored_posts(mut posts: Vec<ScoredPost>, priors: &Priors) -> Vec<ScoredPost> {
    let mut features: Vec<PostFeatures> = posts
        .iter()
        .map(|sp| PostFeatures::from_post(&sp.post))
        .collect();
    let mut selected: Vec<ScoredPost> = Vec::with_capacity(posts.len());
    let mut selected_feats: Vec<PostFeatures> = Vec::with_capacity(posts.len());

    // Cache top score; only rescan when the just-picked item carried it.
    let mut top_score = posts.iter().map(|p| p.score).fold(f32::MIN, f32::max);

    while !posts.is_empty() {
        // MMR: penalty = redundancy × gap-from-top. Perfect score → minimal penalty.
        let mut best_idx = 0usize;
        let mut best_value = f32::MIN;
        let mut best_id = i64::MAX;

        for idx in 0..posts.len() {
            let interaction_fit = posts[idx]
                .breakdown
                .as_ref()
                .map(|b| b.interaction_fit)
                .unwrap_or(FEEDBACK_NEUTRAL);
            let redundancy = max_redundancy(&features[idx], &selected_feats, priors);
            let gap = (top_score - posts[idx].score).max(0.0);
            let penalty = (redundancy
                * gap
                * (1.0 - priors.diversity_interaction_damp.clamp(0.0, 1.0) * interaction_fit))
                .clamp(0.0, priors.diversity_max_penalty.clamp(0.0, 1.0));
            let adj = posts[idx].score - penalty;
            let id = posts[idx].post.id;
            if adj > best_value || (adj == best_value && id < best_id) {
                best_value = adj;
                best_idx = idx;
                best_id = id;
            }
        }

        let removed_was_top = (posts[best_idx].score - top_score).abs() < 1e-6;
        selected.push(posts.swap_remove(best_idx));
        selected_feats.push(features.swap_remove(best_idx));
        if removed_was_top && !posts.is_empty() {
            top_score = posts.iter().map(|p| p.score).fold(f32::MIN, f32::max);
        }
    }

    selected
}
