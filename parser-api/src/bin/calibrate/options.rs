//! CLI / runtime options for grid and eval modes.

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum SplitStrategy {
    /// Sort by post_id ASC, hold out newest 20% (legacy; biases recency).
    PostId,
    /// Uniform-random hold-out (deterministic per-account seed).
    Random,
}

impl SplitStrategy {
    pub(crate) fn label(self) -> &'static str {
        match self {
            SplitStrategy::PostId => "post_id",
            SplitStrategy::Random => "random",
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum NegMode {
    /// Uniform xorshift over `posts.id` (legacy).
    Uniform,
    /// 40% uniform + 30% popularity-decile + 30% age-window matched.
    Mixed,
}

impl NegMode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            NegMode::Uniform => "uniform",
            NegMode::Mixed => "mixed-hard",
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
}

impl Default for GridOptions {
    fn default() -> Self {
        Self {
            pairs_only: false,
            run_paired: true,
            diversify: false,
            split: SplitStrategy::PostId,
            neg_mode: NegMode::Mixed,
        }
    }
}
