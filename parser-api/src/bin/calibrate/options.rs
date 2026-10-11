//! CLI / runtime options for grid and eval modes.

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum SplitStrategy {
    /// Sort by `post_id` ASC, hold out newest 20% (legacy; biases recency).
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

/// Which surrogate `--fit-mix` maximises.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum FitObjective {
    /// Listwise softmax / Plackett–Luce over the account's whole candidate set.
    ///
    /// Concave in `w` (so the optimum is global), cheap, and — measured on the
    /// real cache — pointed the *wrong way* on the dominant channel: its optimum
    /// drops `sim` and inflates `rating`, and the exact gate rejected the result
    /// by 5.5 SE. Kept as the default because it is the documented incumbent and
    /// every past comparison was made against it.
    PlackettLuce,
    /// Differentiable approximation of the acceptance metric itself — soft
    /// NDCG@`backtest.top_k_ndcg` over a truncated head of each account.
    ///
    /// Not concave, so a local optimum is all it can promise; the point of the
    /// option is that its optimum is measured in the same units as the gate.
    /// See `fit::objective_ndcg`.
    ApproxNdcg,
}

impl FitObjective {
    pub(crate) fn label(self) -> &'static str {
        match self {
            FitObjective::PlackettLuce => "pl",
            FitObjective::ApproxNdcg => "ndcg",
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
    /// Reuse the channel matrix persisted by a previous run when it still
    /// matches the dataset and priors (see `cache_file`). Set via the
    /// `no-cache` CLI keyword.
    pub(crate) channel_cache: bool,
    /// After the `eval` pass has built the in-memory channel matrix, solve the
    /// twelve `mix_*` weights analytically from it and report the same paired
    /// gate the grid uses (TODO §2.7). Set via `--fit-mix` / `fit-mix`.
    ///
    /// Costs no extra scoring pass by construction: the fit runs on the cache
    /// `run_eval_into_cache` already produced. It does force the eval through
    /// that cache-building path even without a following `grid`, because the
    /// production-scorer path discards the per-channel matrix.
    pub(crate) fit_mix: bool,
    /// Surrogate the fit maximises. Set via `fit-mix-objective=pl|ndcg`.
    pub(crate) fit_objective: FitObjective,
}

impl Default for GridOptions {
    fn default() -> Self {
        Self {
            pairs_only: false,
            run_paired: true,
            diversify: false,
            split: SplitStrategy::TimeCausal, // post_id biases recency (test posts always newer)
            neg_mode: NegMode::Hybrid { hard_ratio: 0.3 },
            verbose: false,
            channel_cache: true,
            fit_mix: false,
            fit_objective: FitObjective::PlackettLuce,
        }
    }
}
