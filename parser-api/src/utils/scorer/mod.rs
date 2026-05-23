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
    pub const fn name(self) -> &'static str {
        GROUP_NAMES[self as usize].1
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        GROUP_NAMES.iter().find(|(_, n)| *n == s).map(|(g, _)| *g)
    }

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
pub use context::{context_fingerprint, ContextBase, ScoringContext};
pub use diversify::{diversify_indices, diversify_scored_posts, DiversityFeatures};
pub use metrics::{ChannelTiming, PhaseRecord, PipelineMetrics, ScoringMetrics};
pub use priors::Priors;
