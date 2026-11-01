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
pub(crate) use util::MixWeights;
pub use util::{BLEND_CHANNEL_COUNT, BLEND_CHANNEL_NAMES, blend_score, normalised_mix_weights};

use crate::models::ScoredPost;

/// ε-greedy exploration: raise a post's score by `ε × tag novelty × jitter`,
/// where novelty is `1 − tag_similarity` and `jitter ∈ [0, 1]` is a draw from
/// the session's RNG.
///
/// **This has to run before diversification, not after.** The bonus is a
/// ranking signal: the MMR pass and the client-side score cutoff both read
/// `score`, so a bonus applied to an already-ordered list changes a number
/// without changing what the feed promotes (see `docs/algo-review.md`, П1).
///
/// The jitter is what makes the knob an experiment rather than a constant: with
/// `jitter = 1` every novel post gets the same bonus and the same posts win in
/// every session, which is why the deterministic version was filed as П2. With
/// a draw, the *set* of promoted novel posts rotates between sessions while the
/// bonus never exceeds `ε`.
///
/// `ε` is clamped to `[0, 0.5]`, matching `validate_exploration` at the edge.
#[must_use]
pub fn exploration_adjusted_score(score: f32, tag_similarity: f32, eps: f32, jitter: f32) -> f32 {
    if !eps.is_finite() || eps <= 1e-4 {
        return score;
    }
    let eps = eps.clamp(0.0, 0.5);
    let jitter = if jitter.is_finite() {
        jitter.clamp(0.0, 1.0)
    } else {
        0.0
    };
    (score + eps * (1.0 - tag_similarity) * jitter).clamp(0.0, 1.0)
}

/// Seed for one request's exploration draws: account, session and scroll depth.
///
/// Deterministic on purpose — the same session reloading the same depth must
/// see the same order, otherwise a failed request would reshuffle the page
/// under the reader. Different sessions get different draws, which is the
/// exploration.
#[must_use]
pub fn exploration_seed(account_id: i32, session: Option<&str>, depth: usize) -> u64 {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut h = DefaultHasher::new();
    account_id.hash(&mut h);
    session.unwrap_or_default().hash(&mut h);
    depth.hash(&mut h);
    h.finish()
}

/// Exploration strength for a scroll that has already shown `depth` posts.
///
/// The first screen is where a bet on novelty pays; by the time the reader is a
/// few hundred posts deep they are browsing deliberately, and a random boost
/// only costs relevance. Halves every 100 posts and never reaches zero, so a
/// long scroll keeps a little exploration.
#[must_use]
pub fn exploration_decay(depth: usize) -> f32 {
    1.0 / (1.0 + depth as f32 / 100.0)
}

/// xorshift64* — the harness already uses this generator, and the feed needs
/// only a cheap deterministic stream, not cryptographic quality.
struct Jitter(u64);

impl Jitter {
    fn new(seed: u64) -> Self {
        // Zero is a fixed point of xorshift; mix the seed so a zero hash still
        // produces a stream.
        Self(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    fn next_unit(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 40) as f32 / (1u64 << 24) as f32
    }
}

/// Apply [`exploration_adjusted_score`] to every scored post in place.
///
/// `seed` comes from [`exploration_seed`]; the same seed produces the same
/// draws, which is what keeps a session's pages stable.
pub fn apply_exploration_bonus(scored: &mut [ScoredPost], eps: f32, seed: u64) {
    if !eps.is_finite() || eps <= 1e-4 {
        return;
    }
    let mut rng = Jitter::new(seed);
    for sp in scored {
        let sim = sp.breakdown.as_ref().map_or(0.0, |b| b.tag_similarity);
        sp.score = exploration_adjusted_score(sp.score, sim, eps, rng.next_unit());
    }
}

#[cfg(test)]
mod exploration_tests {
    use super::{Jitter, exploration_adjusted_score, exploration_decay, exploration_seed};

    #[test]
    fn exploration_nudges_by_novelty() {
        // jitter = 1 is the old deterministic behaviour and the maximum the
        // knob can spend: 0.10 novelty at ε = 0.2 → +0.02; 0.90 → +0.18.
        assert!((exploration_adjusted_score(0.80, 0.90, 0.2, 1.0) - 0.82).abs() < 1e-6);
        assert!((exploration_adjusted_score(0.40, 0.10, 0.2, 1.0) - 0.58).abs() < 1e-6);
    }

    #[test]
    fn exploration_can_promote_a_novel_post_above_a_similar_one() {
        // On exploitation alone the similar post wins (0.62 > 0.60); at
        // ε = 0.5 it gains 0.05 while the novel one gains 0.45.
        let similar = exploration_adjusted_score(0.62, 0.90, 0.5, 1.0);
        let novel = exploration_adjusted_score(0.60, 0.10, 0.5, 1.0);
        assert!(novel > similar, "novel {novel} must beat similar {similar}");
    }

    #[test]
    fn exploration_is_a_no_op_when_disabled_and_stays_in_range() {
        assert!((exploration_adjusted_score(0.62, 0.90, 0.0, 1.0) - 0.62).abs() < 1e-6);
        assert!((exploration_adjusted_score(0.99, 0.0, 0.5, 1.0) - 1.0).abs() < 1e-6);
        assert!((exploration_adjusted_score(0.0, 1.0, 0.5, 1.0) - 0.0).abs() < 1e-6);
    }

    /// The draw is what turns the knob into an experiment: the bonus is
    /// proportional to it and never exceeds `ε × novelty`.
    #[test]
    fn the_jitter_bounds_the_bonus() {
        let (score, sim, eps) = (0.5, 0.6, 0.5);
        let none = exploration_adjusted_score(score, sim, eps, 0.0);
        let half = exploration_adjusted_score(score, sim, eps, 0.5);
        let full = exploration_adjusted_score(score, sim, eps, 1.0);
        assert!((none - score).abs() < 1e-6);
        assert!((half - (score + 0.5 * 0.4 * 0.5)).abs() < 1e-6);
        assert!((full - (score + 0.5 * 0.4)).abs() < 1e-6);
    }

    #[test]
    fn the_same_seed_repeats_the_draws_and_a_new_session_does_not() {
        let draw = |seed| {
            let mut rng = Jitter::new(seed);
            (0..8).map(|_| rng.next_unit()).collect::<Vec<_>>()
        };
        assert_eq!(draw(42), draw(42), "a session must not reshuffle on retry");
        assert_ne!(draw(42), draw(43), "another session must explore elsewhere");
        assert!(draw(7).iter().all(|v| (0.0..1.0).contains(v)));
    }

    #[test]
    fn exploration_decays_with_the_scroll() {
        assert!((exploration_decay(0) - 1.0).abs() < 1e-6);
        assert!((exploration_decay(100) - 0.5).abs() < 1e-6);
        assert!(exploration_decay(300) < exploration_decay(100));
        // Never exactly zero: a long scroll keeps a little exploration.
        assert!(exploration_decay(1_000_000) > 0.0);
    }

    #[test]
    fn the_seed_depends_on_account_session_and_depth() {
        let base = exploration_seed(7, Some("sess-aaaa"), 0);
        assert_eq!(base, exploration_seed(7, Some("sess-aaaa"), 0));
        assert_ne!(base, exploration_seed(8, Some("sess-aaaa"), 0));
        assert_ne!(base, exploration_seed(7, Some("sess-bbbb"), 0));
        assert_ne!(base, exploration_seed(7, Some("sess-aaaa"), 10));
        assert_ne!(base, exploration_seed(7, None, 0));
    }
}
