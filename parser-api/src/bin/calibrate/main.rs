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
mod dataset;
mod grid;
mod knobs;
mod log;
mod metrics;
mod options;
mod probe;
mod sampling;

use crate::dataset::{prepare_eval_dataset, EvalDataset};
use crate::grid::run_grid_with_dataset;
use crate::knobs::{GRID_KNOBS, KnobSpec, MIX_ONLY_KNOBS};
use crate::metrics::{print_metrics, score_with_progress};
use crate::options::{GridOptions, NegMode, SplitStrategy};
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
        return run_modes(&[Mode::Eval], false, GridOptions::default());
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
            "no mode given. Use 'eval', 'grid [mix-only]', 'probe', or chain them \
             (e.g. 'eval grid'). Flags: pairs-only, no-pairs, with-diversify, \
             split=random|post_id|time_causal, neg=uniform|mixed|hybrid, verbose"
        );
        std::process::exit(2);
    }

    let opts = parse_grid_flags(tail.into_iter());
    run_modes(&modes, mix_only, opts)
}

fn run_modes(modes: &[Mode], mix_only: bool, opts: GridOptions) -> anyhow::Result<()> {
    let needs_dataset = modes.iter().any(|m| matches!(m, Mode::Eval | Mode::Grid));

    let t_total = std::time::Instant::now();
    let dataset_pair = if needs_dataset {
        let t_prep = std::time::Instant::now();
        eprintln!("[run] preparing dataset (shared across {} mode(s))...", modes.len());
        let ds = prepare_eval_dataset(&opts)?;
        eprintln!(
            "[run] dataset ready in {:.1}s",
            t_prep.elapsed().as_secs_f32()
        );
        Some((ds, Utc::now()))
    } else {
        None
    };

    for mode in modes {
        match mode {
            Mode::Eval => {
                let (dataset, now) = dataset_pair
                    .as_ref()
                    .expect("eval requires a hydrated dataset");
                run_eval_with_dataset(dataset, &opts, *now)?;
            }
            Mode::Grid => {
                let (dataset, now) = dataset_pair
                    .as_ref()
                    .expect("grid requires a hydrated dataset");
                let knobs: &[KnobSpec] = if mix_only { MIX_ONLY_KNOBS } else { GRID_KNOBS };
                run_grid_with_dataset(knobs, opts, dataset, *now, t_total)?;
            }
            Mode::Probe => run_probe()?,
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
    eprintln!("[eval] timings: score={:.1}s", score_secs);
    Ok(())
}

fn parse_grid_flags(args: impl Iterator<Item = String>) -> GridOptions {
    let mut opts = GridOptions::default();
    for arg in args {
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
            other => eprintln!("[run] unknown flag: {other} (ignored)"),
        }
    }
    opts
}
