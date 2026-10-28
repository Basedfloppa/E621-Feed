//! Scoring math: per-account `ScoringContext` evaluates posts against a
//! `Priors` config and returns `(score, ScoreBreakdown)`. A separate MMR
//! pass diversifies the resulting ranked list.
//!
//! Module split (v5.3):
//!  * [`priors`] — the `Priors` struct + serde defaults
//!  * [`util`]   — pure helpers (sigmoid, blends, IDF/CTR helpers, …)
//!  * [`context`]— `ScoringContext` and its per-channel methods
//!  * [`diversify`] — `diversify_scored_posts` and MMR helpers

pub(crate) const GROUP_COUNT: usize = 7;

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum Group {
    Artist = 0,
    Character = 1,
    Copyright = 2,
    Species = 3,
    General = 4,
    Lore = 5,
    Meta = 6,
}

const GROUP_NAMES: [(Group, &str); GROUP_COUNT] = [
    (Group::Artist, "artist"),
    (Group::Character, "character"),
    (Group::Copyright, "copyright"),
    (Group::Species, "species"),
    (Group::General, "general"),
    (Group::Lore, "lore"),
    (Group::Meta, "meta"),
];

impl Group {
    #[must_use]
    pub const fn name(self) -> &'static str {
        GROUP_NAMES[self as usize].1
    }

    #[allow(clippy::should_implement_trait)]
    #[must_use]
    pub fn from_str(s: &str) -> Option<Self> {
        GROUP_NAMES.iter().find(|(_, n)| *n == s).map(|(g, _)| *g)
    }

    #[must_use]
    pub const fn is_scoring(self) -> bool {
        !matches!(self, Group::Meta)
    }
}

mod cached;
mod channels;
mod channels_cached;
mod context;
mod diversify;
mod metrics;
mod priors;
mod util;

pub use cached::{CachedPostFeatures, CachedTag};
pub use channels::{post_pair_similarity, post_tag_vector};
pub use context::{ContextBase, ScoringContext, context_fingerprint};
pub use diversify::{DiversityFeatures, diversify_indices, diversify_scored_posts};
pub use metrics::{ChannelTiming, PhaseRecord, PipelineMetrics, ScoringMetrics};
pub use priors::Priors;
pub use util::{BLEND_CHANNEL_COUNT, BLEND_CHANNEL_NAMES, blend_score, normalised_mix_weights};

use crate::models::ScoredPost;

/// ε-greedy exploration: raise a post's score by `ε × tag novelty`, where
/// novelty is `1 − tag_similarity`.
///
/// **This has to run before diversification, not after.** The bonus is a
/// ranking signal: the MMR pass and the client-side score cutoff both read
/// `score`, so a bonus applied to an already-ordered list changes a number
/// without changing what the feed promotes (see `docs/algo-review.md`, П1).
///
/// `ε` is clamped to `[0, 0.5]`, matching `validate_exploration` at the edge.
#[must_use]
pub fn exploration_adjusted_score(score: f32, tag_similarity: f32, eps: f32) -> f32 {
    if !eps.is_finite() || eps <= 1e-4 {
        return score;
    }
    let eps = eps.clamp(0.0, 0.5);
    (score + eps * (1.0 - tag_similarity)).clamp(0.0, 1.0)
}

/// Apply [`exploration_adjusted_score`] to every scored post in place.
pub fn apply_exploration_bonus(scored: &mut [ScoredPost], eps: f32) {
    if !eps.is_finite() || eps <= 1e-4 {
        return;
    }
    for sp in scored {
        let sim = sp.breakdown.as_ref().map_or(0.0, |b| b.tag_similarity);
        sp.score = exploration_adjusted_score(sp.score, sim, eps);
    }
}

#[cfg(test)]
mod exploration_tests {
    use super::exploration_adjusted_score;

    #[test]
    fn exploration_nudges_by_novelty() {
        // 0.10 novelty at ε = 0.2 → +0.02; 0.90 novelty → +0.18.
        assert!((exploration_adjusted_score(0.80, 0.90, 0.2) - 0.82).abs() < 1e-6);
        assert!((exploration_adjusted_score(0.40, 0.10, 0.2) - 0.58).abs() < 1e-6);
    }

    #[test]
    fn exploration_can_promote_a_novel_post_above_a_similar_one() {
        // On exploitation alone the similar post wins (0.62 > 0.60); at
        // ε = 0.5 it gains 0.05 while the novel one gains 0.45.
        let similar = exploration_adjusted_score(0.62, 0.90, 0.5);
        let novel = exploration_adjusted_score(0.60, 0.10, 0.5);
        assert!(novel > similar, "novel {novel} must beat similar {similar}");
    }

    #[test]
    fn exploration_is_a_no_op_when_disabled_and_stays_in_range() {
        assert!((exploration_adjusted_score(0.62, 0.90, 0.0) - 0.62).abs() < 1e-6);
        assert!((exploration_adjusted_score(0.99, 0.0, 0.5) - 1.0).abs() < 1e-6);
        assert!((exploration_adjusted_score(0.0, 1.0, 0.5) - 0.0).abs() < 1e-6);
    }
}
