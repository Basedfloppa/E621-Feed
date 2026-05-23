//! CLI / runtime options for grid and eval modes.

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum SplitStrategy {
    /// Sort by post_id ASC, hold out newest 20% (legacy; biases recency).
    PostId,
    /// Uniform-random hold-out (deterministic per-account seed).
    Random,
    /// Time-causal: sort favourites by `created_at` and hold out the
    /// newest 20% — closer to the "predict the user's *next*
    /// favourite" task and less id-aliasing-sensitive than `PostId`.
    TimeCausal,
}

impl SplitStrategy {
    pub(crate) fn label(self) -> &'static str {
        match self {
            SplitStrategy::PostId => "post_id",
            SplitStrategy::Random => "random",
            SplitStrategy::TimeCausal => "time_causal",
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) enum NegMode {
    /// Uniform xorshift over `posts.id` (legacy).
    Uniform,
    /// 40% uniform + 30% popularity-decile + 30% age-window matched.
    Mixed,
    /// 70% mixed + 30% tag-similarity-based hard negatives (Option C).
    /// `hard_ratio` controls the fraction of negatives that are hard-mined
    /// via `ctx.tag_similarity()` against config priors.
    Hybrid { hard_ratio: f32 },
}

impl NegMode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            NegMode::Uniform => "uniform",
            NegMode::Mixed => "mixed-hard",
            NegMode::Hybrid { .. } => "hybrid-hard",
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub(crate) struct GridOptions {
    pub(crate) pairs_only: bool,
    pub(crate) run_paired: bool,
    pub(crate) diversify: bool,
    pub(crate) split: SplitStrategy,
    pub(crate) neg_mode: NegMode,
    /// Log every probe (kept/rejected) instead of only those that
    /// improved baseline. Useful for post-hoc analysis of why a knob
    /// did/didn't move. Set via the `verbose` CLI keyword.
    pub(crate) verbose: bool,
}

impl Default for GridOptions {
    fn default() -> Self {
        Self {
            pairs_only: false,
            run_paired: true,
            diversify: false,
            split: SplitStrategy::PostId,
            neg_mode: NegMode::Hybrid { hard_ratio: 0.3 },
            verbose: false,
        }
    }
}
