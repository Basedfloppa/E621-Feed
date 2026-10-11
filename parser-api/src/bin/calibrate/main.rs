//! Offline calibration harness. Splits each account's favs into train/test,
//! samples negatives, scores under a `Priors` config, and reports
//! NDCG@20 / Recall@50 / MRR. `grid` runs an adaptive line search +
//! paired sweep + categorical sweep over every measurable knob.
//!
//! See `docs/calibration.md` for the full guide.
//!
//! Modes can be chained on the command line so a single invocation
//! shares one hydration pass: `calibrate eval grid` preps the dataset
//! once, runs the eval, then runs the grid.

// Switch to jemalloc on Linux when the `jemalloc` feature is enabled.
// Build with `cargo build --release --bin calibrate --features jemalloc`
// to get it. If your build host lacks `make` / a C toolchain (jemalloc
// is a C library), leave it off and try `MALLOC_ARENA_MAX=2` /
// `MALLOC_TRIM_THRESHOLD_=131072` env vars at runtime instead — they
// cut glibc fragmentation by ~30-50% without a rebuild.
//
// Why bother: the calibrate hydration path churns hundreds of small
// `String` allocations per Post (tags / description / sources) and
// frees them once the per-account fixture is extracted. ptmalloc holds
// those freed pages in per-arena free lists rather than returning them
// to the OS, so RSS climbs ~20 MB per account even though steady-state
// per-fixture data is < 1 MB.
#[cfg(all(target_os = "linux", feature = "jemalloc"))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use std::env;

use chrono::{DateTime, Utc};

use e621_account_parser_api::db;
use e621_account_parser_api::models::{cfg, default_path, reload_from};

mod cache;
mod cache_file;
mod dataset;
mod fit;
mod grid;
mod knobs;
mod log;
mod metrics;
mod options;
mod probe;
mod sampling;

/// Synthetic dataset builders for the unit tests. Declared only under
/// `cfg(test)`: nothing in the shipped binary may depend on it.
#[cfg(test)]
mod test_support;

use crate::cache::{M_ALL, M_RECENCY, ScoreCache, score_with_cache};
use crate::dataset::{EvalDataset, prepare_eval_dataset};
use crate::grid::run_grid_with_dataset;
use crate::knobs::{GRID_KNOBS, KnobSpec, MIX_ONLY_KNOBS};
use crate::metrics::{Metrics, print_metrics, score_with_progress};
use crate::options::{FitObjective, GridOptions, NegMode, SplitStrategy};
use crate::probe::run_probe;

/// Deterministic seed for negative sampling so grid runs are reproducible.
pub(crate) const NEG_SAMPLE_SEED: u64 = 0xE621_CA118;
/// Deterministic seed for `split_strategy = random`.
pub(crate) const SPLIT_SEED: u64 = 0xE621_5917;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Mode {
    Eval,
    Grid,
    Probe,
    /// Rebuild the global tag-relation graph from `tags_posts`. Required after
    /// a fresh `seed`, which deliberately writes with `track_cooccurrence =
    /// false`; without it every `tag_relation_*` knob measures an empty graph.
    Graph,
}

fn main() -> anyhow::Result<()> {
    let path = default_path()?;
    reload_from(&path)?;
    db::ensure_sqlite().map_err(|e| anyhow::anyhow!("migrate: {e}"))?;

    // Parse leading positional args until we hit a flag-shaped arg.
    // Modes can chain (e.g. `eval grid`); `mix-only` is a positional
    // grid-modifier that must follow `grid`.
    let raw_args: Vec<String> = env::args().skip(1).collect();
    if raw_args.is_empty() {
        return run_modes(&[Mode::Eval], false, GridOptions::default(), None);
    }

    let mut modes: Vec<Mode> = Vec::new();
    let mut mix_only = false;
    let mut tail: Vec<String> = Vec::new();
    let mut consuming_modes = true;
    for arg in raw_args {
        if consuming_modes {
            match arg.as_str() {
                "eval" => {
                    modes.push(Mode::Eval);
                    continue;
                }
                "grid" => {
                    modes.push(Mode::Grid);
                    continue;
                }
                "probe" => {
                    modes.push(Mode::Probe);
                    continue;
                }
                "graph" => {
                    modes.push(Mode::Graph);
                    continue;
                }
                "mix-only" if modes.contains(&Mode::Grid) => {
                    mix_only = true;
                    continue;
                }
                _ => {
                    consuming_modes = false;
                    tail.push(arg);
                }
            }
        } else {
            tail.push(arg);
        }
    }
    if modes.is_empty() {
        eprintln!(
            "no mode given. Use 'eval', 'grid [mix-only]', 'probe', 'graph', or chain them \
             (e.g. 'eval grid'). Flags: pairs-only, no-pairs, with-diversify, no-cache, \
             fit-mix[=<priors.toml>], fit-mix-objective=pl|ndcg, fit-mix-holdout, \
             split=random|post_id|time_causal, \
             neg=uniform|mixed|hybrid, verbose"
        );
        std::process::exit(2);
    }

    let (opts, reference_path) = parse_grid_flags(tail.into_iter());

    // Loaded here rather than inside the eval: a bad path must fail before the
    // ~60s scoring pass, not after it. `run_modes` then carries the parsed
    // vector, so the eval never touches the filesystem.
    let reference = match reference_path.as_deref() {
        Some(p) => Some(fit::ReferenceVector::load(std::path::Path::new(p))?),
        None => None,
    };

    run_modes(&modes, mix_only, opts, reference.as_ref())
}

fn run_modes(
    modes: &[Mode],
    mix_only: bool,
    opts: GridOptions,
    reference: Option<&fit::ReferenceVector>,
) -> anyhow::Result<()> {
    if reference.is_some() && !modes.contains(&Mode::Eval) {
        eprintln!(
            "[fit] a reference vector was loaded but no 'eval' mode runs, so it is never \
             re-scored — the reference is read on the matrix the eval pass builds"
        );
    }

    let needs_dataset = modes.iter().any(|m| matches!(m, Mode::Eval | Mode::Grid));

    // One clock for the whole run, read once, before hydration.
    //
    // Previously the dataset read its own `Utc::now()` inside `build_profile`
    // and scoring read a second one after prep. Two consequences: the profile's
    // age statistics and the recency channel disagreed about "now", and — worse
    // — the dataset stopped being reproducible, because the profile feeds the
    // score that ranks hard negatives. Pinning this value (`CALIBRATE_NOW`)
    // pins the entire eval set.
    let run_now = run_clock();
    eprintln!("[run] clock {}", run_now.to_rfc3339());

    let t_total = std::time::Instant::now();
    let dataset_pair = if needs_dataset {
        let t_prep = std::time::Instant::now();
        eprintln!(
            "[run] preparing dataset (shared across {} mode(s))...",
            modes.len()
        );
        let ds = prepare_eval_dataset(&opts, run_now)?;
        eprintln!(
            "[run] dataset ready in {:.1}s",
            t_prep.elapsed().as_secs_f32()
        );
        // Printed because the persisted channel cache keys on it: two runs
        // that print different fingerprints cannot share a matrix, and
        // without this line that is invisible.
        eprintln!(
            "[run] dataset fingerprint {:#018x}",
            cache_file::dataset_fingerprint(&ds)
        );
        Some((ds, run_now))
    } else {
        None
    };

    // `eval grid` is the documented way to run a full calibration, and both
    // modes score the baseline: `eval` through the production scorer, then
    // `grid` rebuilding the same channel cache from scratch. The two agree
    // exactly — on the 2026-10-10 run both printed `NDCG@20=0.3547`
    // (CI 0.3378–0.3724), `Recall@50=0.0595`, `MRR=0.5600` — so the second
    // build is pure duplication worth ~57s. Hand the cache over instead.
    //
    // Only when `eval` actually precedes `grid`: in `grid eval` the grid has
    // already run by the time the eval pass finishes, so there is nothing to
    // hand over.
    let eval_precedes_grid = modes
        .iter()
        .position(|m| matches!(m, Mode::Eval))
        .zip(modes.iter().position(|m| matches!(m, Mode::Grid)))
        .is_some_and(|(e, g)| e < g);
    let mut shared_baseline: Option<(Metrics, ScoreCache)> = None;

    for mode in modes {
        match mode {
            Mode::Eval => {
                let (dataset, now) = dataset_pair
                    .as_ref()
                    .expect("eval requires a hydrated dataset");
                // A persisted matrix from an earlier invocation is only
                // worth loading when this eval feeds a grid, or when the fit
                // asked for the matrix; a standalone `calibrate eval` stays on
                // the production scorer.
                if eval_precedes_grid || opts.fit_mix {
                    let key = cache_key_for(dataset, &opts);
                    let warm = key
                        .as_ref()
                        .and_then(|k| cache_file::load(&cache_path(), k, dataset));
                    let (m, cache) = run_eval_into_cache(dataset, &opts, *now, warm)?;
                    if opts.fit_mix {
                        // Runs on the matrix this pass just built — one
                        // scoring pass, then a pure in-memory solve. A
                        // reference vector rides the same matrix.
                        fit::run_fit_and_report(
                            &cache,
                            &m,
                            opts.diversify,
                            reference,
                            opts.fit_objective,
                            opts.fit_holdout,
                        );
                    }
                    if let Some(k) = key.as_ref() {
                        match cache_file::save(&cache_path(), k, dataset, &cache) {
                            Ok(()) => eprintln!("[cache] wrote {}", cache_path().display()),
                            // A cache that cannot be written is a missed
                            // optimisation, not a failed run.
                            Err(e) => eprintln!("[cache] not saved: {e}"),
                        }
                    }
                    if eval_precedes_grid {
                        shared_baseline = Some((m, cache));
                    }
                } else {
                    run_eval_with_dataset(dataset, &opts, *now)?;
                }
            }
            Mode::Grid => {
                let (dataset, now) = dataset_pair
                    .as_ref()
                    .expect("grid requires a hydrated dataset");
                let knobs: &[KnobSpec] = if mix_only { MIX_ONLY_KNOBS } else { GRID_KNOBS };
                run_grid_with_dataset(knobs, opts, dataset, *now, t_total, shared_baseline.take())?;
            }
            Mode::Probe => run_probe()?,
            Mode::Graph => {
                eprintln!("[graph] rebuilding global tag co-occurrence from tags_posts...");
                let t = std::time::Instant::now();
                // The per-account graph carries four tag-name/group strings per
                // row instead of two integer ids, so it is an order of magnitude
                // larger — and it feeds only MMR / taste-profile personalisation.
                // `grid` drops every `diversify_only` knob unless
                // `--with-diversify` is set, so without that flag this would
                // write tens of gigabytes nothing in the run reads.
                if opts.diversify {
                    db::run_tag_cooccurrence_backfill_if_needed()
                } else {
                    db::run_global_tag_cooccurrence_backfill()
                }
                .map_err(|e| anyhow::anyhow!("cooc backfill: {e}"))?;
                eprintln!("[graph] done in {:.1}s", t.elapsed().as_secs_f32());
            }
        }
    }
    Ok(())
}

fn run_eval_with_dataset(
    dataset: &EvalDataset,
    opts: &GridOptions,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    let cfg_arc = cfg();
    let top_k_ndcg = cfg_arc.backtest.top_k_ndcg;
    let top_k_recall = cfg_arc.backtest.top_k_recall;
    let priors = cfg_arc.priors.clone();

    eprintln!(
        "[eval] scoring {} accounts under config.toml priors...",
        dataset.accounts.len()
    );
    let t_score = std::time::Instant::now();
    let m = score_with_progress(
        dataset,
        &priors,
        now,
        top_k_ndcg,
        top_k_recall,
        opts.diversify,
    )
    .average();
    let score_secs = t_score.elapsed().as_secs_f32();
    print_metrics("baseline", &m, top_k_ndcg, top_k_recall);
    eprintln!("[eval] timings: score={score_secs:.1}s");
    Ok(())
}

/// Same pass as [`run_eval_with_dataset`], but scored through the calibrate
/// channel cache so the resulting [`ScoreCache`] can seed the grid baseline.
///
/// Used only when a `grid` mode follows. A standalone `calibrate eval` keeps
/// the production-scorer path on purpose: that path is the independent
/// reference the calibrate cache is checked against, and collapsing the two
/// would remove the only thing that can catch a divergence between them.
///
/// `warm` is a channel matrix persisted by an earlier invocation. On a hit
/// every channel is reused and only `M_RECENCY` is recomputed, because the
/// cache is validated on a key that deliberately excludes `Priors::now` —
/// the clock moved, so the one channel that reads it has to be refreshed.
fn run_eval_into_cache(
    dataset: &EvalDataset,
    opts: &GridOptions,
    now: DateTime<Utc>,
    warm: Option<ScoreCache>,
) -> anyhow::Result<(Metrics, ScoreCache)> {
    let cfg_arc = cfg();
    let top_k_ndcg = cfg_arc.backtest.top_k_ndcg;
    let top_k_recall = cfg_arc.backtest.top_k_recall;
    let priors = cfg_arc.priors.clone();

    let (prev, mask) = match warm.as_ref() {
        Some(_) => {
            eprintln!(
                "[cache] hit: reusing the persisted channel matrix, \
                 recomputing recency only"
            );
            (warm.as_ref(), M_RECENCY)
        }
        None => (None, M_ALL),
    };

    eprintln!(
        "[eval] scoring {} accounts under config.toml priors (channel cache kept for the grid baseline)...",
        dataset.accounts.len()
    );
    let t_score = std::time::Instant::now();
    let (m, cache) = score_with_cache(
        dataset,
        &priors,
        now,
        top_k_ndcg,
        top_k_recall,
        opts.diversify,
        prev,
        mask,
    );
    let score_secs = t_score.elapsed().as_secs_f32();
    let m = m.average();
    print_metrics("baseline", &m, top_k_ndcg, top_k_recall);
    eprintln!("[eval] timings: score={score_secs:.1}s");
    Ok((m, cache))
}

/// Path of the persisted channel matrix. Not configurable: it lives beside
/// the `calibration_results/grid_*.toml` files, and a second knob in
/// `config.toml` would have to be kept in sync with the CLI for no gain.
fn cache_path() -> std::path::PathBuf {
    std::path::PathBuf::from(cache_file::DEFAULT_CACHE_PATH)
}

/// `None` disables the cache — either because the run asked for `no-cache`
/// or because the priors cannot be serialised, in which case caching is
/// simply unavailable rather than fatal.
fn cache_key_for(dataset: &EvalDataset, opts: &GridOptions) -> Option<cache_file::CacheKey> {
    if !opts.channel_cache {
        return None;
    }
    let cfg_arc = cfg();
    cache_file::CacheKey::compute(
        dataset,
        &cfg_arc.priors,
        opts.diversify,
        cfg_arc.backtest.top_k_ndcg,
        cfg_arc.backtest.top_k_recall,
    )
}

/// The run's evaluation clock, read once.
///
/// `CALIBRATE_NOW` (unix seconds) pins it. That is what makes a calibration
/// reproducible and lets the persisted channel cache hit at all: the cache key
/// covers the dataset, and the dataset depends on this value through the
/// profile that ranks hard negatives. A pinned value should be at or after the
/// newest catalog post, or every recency age clamps to zero.
///
/// Default is still the wall clock, so an unpinned run behaves exactly as
/// before — but it will not share a cache with any other run.
fn run_clock() -> DateTime<Utc> {
    let Ok(raw) = env::var("CALIBRATE_NOW") else {
        return Utc::now();
    };
    match raw
        .trim()
        .parse::<i64>()
        .ok()
        .and_then(|secs| DateTime::from_timestamp(secs, 0))
    {
        Some(t) => t,
        None => {
            eprintln!("[run] CALIBRATE_NOW={raw:?} is not a unix timestamp; using the wall clock");
            Utc::now()
        }
    }
}

/// Parse the trailing (non-mode) CLI args.
///
/// Returns the grid options plus the optional reference-vector path given via
/// `--fit-mix=<path>`. The path is only collected here, not loaded: the caller
/// owns the IO so that a bad path fails before the scoring pass.
fn parse_grid_flags(args: impl Iterator<Item = String>) -> (GridOptions, Option<String>) {
    let mut opts = GridOptions::default();
    let mut reference: Option<String> = None;

    for arg in args {
        // The value-carrying forms are matched before the literal table below,
        // since `--fit-mix=<path>` is not a fixed string.
        let path = arg
            .strip_prefix("--fit-mix=")
            .or_else(|| arg.strip_prefix("fit-mix="))
            .or_else(|| arg.strip_prefix("--fit-mix-ref="))
            .or_else(|| arg.strip_prefix("fit-mix-ref="));
        if let Some(path) = path {
            if path.is_empty() {
                eprintln!("[run] --fit-mix=<path> needs a path (ignored)");
            } else {
                // A reference implies the fit: the reference is re-scored on
                // the matrix the eval pass builds, and that matrix is only kept
                // when the fit asks for it.
                opts.fit_mix = true;
                reference = Some(path.to_string());
            }
            continue;
        }

        // Value-carrying form too, and checked before the literal table below.
        // It is *not* matched by the `fit-mix=` prefix above: that one requires
        // the `=` right after `fit-mix`.
        let objective = arg
            .strip_prefix("--fit-mix-objective=")
            .or_else(|| arg.strip_prefix("fit-mix-objective="));
        if let Some(value) = objective {
            match value {
                "pl" | "plackett-luce" => opts.fit_objective = FitObjective::PlackettLuce,
                "ndcg" => opts.fit_objective = FitObjective::ApproxNdcg,
                other => eprintln!(
                    "[run] unknown --fit-mix-objective={other} (expected pl|ndcg; ignored)"
                ),
            }
            continue;
        }

        match arg.as_str() {
            "pairs-only" => opts.pairs_only = true,
            "no-pairs" => opts.run_paired = false,
            "with-diversify" => opts.diversify = true,
            "split=random" => opts.split = SplitStrategy::Random,
            "split=post_id" => opts.split = SplitStrategy::PostId,
            "split=time_causal" | "split=time" => opts.split = SplitStrategy::TimeCausal,
            "neg=uniform" => opts.neg_mode = NegMode::Uniform,
            "neg=mixed" => opts.neg_mode = NegMode::Mixed,
            "neg=hybrid" => opts.neg_mode = NegMode::Hybrid { hard_ratio: 0.3 },
            "verbose" | "--verbose" => opts.verbose = true,
            "no-cache" => opts.channel_cache = false,
            "fit-mix" | "--fit-mix" => opts.fit_mix = true,
            // Implies the fit: there is nothing to hold out without a vector
            // to hold it out of.
            "fit-mix-holdout" | "--fit-mix-holdout" => {
                opts.fit_mix = true;
                opts.fit_holdout = true;
            }
            other => eprintln!("[run] unknown flag: {other} (ignored)"),
        }
    }
    (opts, reference)
}
