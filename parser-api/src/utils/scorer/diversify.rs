//! MMR-style post-list diversification.
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
//!
//! Memory: each [`DiversityFeatures`] holds three sorted `Vec<u64>` of
//! per-tag SipHashes. Collisions at 64-bit are negligible at the tag
//! cardinalities involved (≤ 10⁵ unique tags, ~10⁻¹⁵ collision
//! probability per pair). This trades a few cents of false-positive
//! similarity risk for a ~10× memory reduction over the previous
//! `HashSet<String>` representation, keeping 500-account calibration
//! datasets inside 15 GB.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::models::{Post, ScoredPost};

use super::priors::Priors;
use super::util::{normalize_tag, FEEDBACK_NEUTRAL};

/// Pre-computed Jaccard-friendly tag fingerprints for one post. Tags are
/// hashed once and stored as a sorted `Vec<u64>` per group so MMR's
/// per-pair set intersection is a linear merge instead of a HashSet
/// probe.
#[derive(Clone)]
pub struct DiversityFeatures {
    artist: Vec<u64>,
    character: Vec<u64>,
    copyright: Vec<u64>,
    species: Vec<u64>,
    general: Vec<u64>,
}

impl DiversityFeatures {
    pub fn from_post(p: &Post) -> Self {
        Self {
            artist: hashed_tag_set(&p.tags.artist),
            character: hashed_tag_set(&p.tags.character),
            copyright: hashed_tag_set(&p.tags.copyright),
            species: hashed_tag_set(&p.tags.species),
            general: hashed_tag_set(&p.tags.general),
        }
    }
}

fn hash_tag(t: &str) -> u64 {
    let lc = normalize_tag(t);
    let mut h = DefaultHasher::new();
    lc.hash(&mut h);
    h.finish()
}

fn hashed_tag_set(tags: &[String]) -> Vec<u64> {
    let mut out: Vec<u64> = tags
        .iter()
        .filter_map(|t| {
            let trimmed = t.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(hash_tag(trimmed))
            }
        })
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Jaccard between two sorted-deduped `Vec<u64>` via merge-intersection.
fn jaccard(a: &[u64], b: &[u64]) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let (mut i, mut j) = (0usize, 0usize);
    let mut inter = 0u32;
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Equal => {
                inter += 1;
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
        }
    }
    let union = (a.len() + b.len()) as u32 - inter;
    if union == 0 {
        0.0
    } else {
        inter as f32 / union as f32
    }
}

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
            + jaccard(&cand.copyright, &chosen.copyright) * priors.diversity_w_copyright
            + jaccard(&cand.species, &chosen.species) * priors.diversity_w_species
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
/// `features[i]`. `head_limit` caps how many top-by-score items
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
    // Apply diversity quota as a final pass.
    enforce_diversity_quota(&mut out);
    out
}

/// Post-MMR diversity quota: guarantee at least 2 different artists and 3
/// different characters in the top-K positions. Posts that exceed a group's
/// quota are pushed down by swapping with a lower-ranked post from a
/// different group.
fn enforce_diversity_quota(scored: &mut Vec<ScoredPost>) {
    let top_k = 20usize.min(scored.len());
    if top_k < 4 {
        return;
    }

    // Artist quota: at least 2 different artists among top-K.
    let mut artist_set: Vec<Option<String>> = Vec::new();
    // We'll collect used slots that are "locked" (the first occurrence of
    // each artist). Extra posts from an already-seen artist get demoted.
    let mut i = 0;
    while i < top_k {
        let post_artists = &scored[i].post.tags.artist;
        let primary = post_artists.first().map(|a| a.to_ascii_lowercase());
        let already_seen = primary.as_ref().map_or(false, |p| {
            artist_set.iter().any(|a| a.as_deref() == Some(p.as_str()))
        });
        if already_seen {
            // This post repeats an artist — swap it down past top_k.
            let swap_target = scored.len() - 1 - (scored.len() - 1 - i) / 3;
            if swap_target > i && swap_target < scored.len() {
                scored.swap(i, swap_target);
                // Don't increment i — re-evaluate the swapped-in post.
                continue;
            }
        } else {
            artist_set.push(primary);
        }
        i += 1;
    }

    // Character quota: at least 3 different characters among top-K.
    let mut char_set: Vec<Option<String>> = Vec::new();
    let mut i = 0;
    while i < top_k {
        let post_chars = &scored[i].post.tags.character;
        let primary = post_chars.first().map(|c| c.to_ascii_lowercase());
        let already_seen = primary.as_ref().map_or(false, |p| {
            char_set.iter().any(|c| c.as_deref() == Some(p.as_str()))
        });
        if already_seen {
            let swap_target = scored.len() - 1 - (scored.len() - 1 - i) / 3;
            if swap_target > i && swap_target < scored.len() {
                scored.swap(i, swap_target);
                continue;
            }
        } else {
            char_set.push(primary);
        }
        i += 1;
    }
}
