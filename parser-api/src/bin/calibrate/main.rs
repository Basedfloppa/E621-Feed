//! Offline calibration harness. Splits each account's favs into train/test,
//! samples negatives, scores under a `Priors` config, and reports
//! NDCG@20 / Recall@50 / MRR. `grid` runs an adaptive line search +
//! paired sweep + categorical sweep over every measurable knob.
//!
//! See `docs/calibration.md` for the full guide.

use std::env;

use chrono::Utc;

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

use crate::grid::run_grid;
use crate::knobs::{GRID_KNOBS, MIX_ONLY_KNOBS};
use crate::metrics::{print_metrics, score_with_progress};
use crate::options::{GridOptions, NegMode, SplitStrategy};
use crate::probe::run_probe;

/// Deterministic seed for negative sampling so grid runs are reproducible.
pub(crate) const NEG_SAMPLE_SEED: u64 = 0xE621_CA118;
/// Deterministic seed for `split_strategy = random`.
pub(crate) const SPLIT_SEED: u64 = 0xE621_5917;

fn main() -> anyhow::Result<()> {
    let path = default_path()?;
    reload_from(&path)?;
    db::ensure_sqlite().map_err(|e| anyhow::anyhow!("migrate: {e}"))?;

    let mode = env::args().nth(1).unwrap_or_else(|| "eval".into());

    match mode.as_str() {
        "eval" => {
            let cfg_arc = cfg();
            let top_k_ndcg = cfg_arc.backtest.top_k_ndcg;
            let top_k_recall = cfg_arc.backtest.top_k_recall;
            let opts = parse_grid_flags(env::args().skip(2));

            let t_total = std::time::Instant::now();
            let t_prep = std::time::Instant::now();
            let dataset = crate::dataset::prepare_eval_dataset(&opts)?;
            let prep_secs = t_prep.elapsed().as_secs_f32();

            let priors = cfg_arc.priors.clone();
            let now = Utc::now();
            eprintln!(
                "[eval] scoring {} accounts under config.toml priors...",
                dataset.accounts.len()
            );
            let t_score = std::time::Instant::now();
            let m = score_with_progress(
                &dataset,
                &priors,
                now,
                top_k_ndcg,
                top_k_recall,
                opts.diversify,
            )
            .average();
            let score_secs = t_score.elapsed().as_secs_f32();
            print_metrics("baseline", &m, top_k_ndcg, top_k_recall);
            eprintln!(
                "[eval] timings: prep={:.1}s, score={:.1}s, total={:.1}s",
                prep_secs,
                score_secs,
                t_total.elapsed().as_secs_f32()
            );
        }
        "grid" => {
            // Default: full grid of measurable knobs.
            // Flags (any order):
            //   mix-only        — restrict to the 8 mix_* weights
            //   pairs-only      — skip single-knob passes
            //   no-pairs        — skip the paired sweep
            //   with-diversify  — apply diversify_scored_posts before NDCG
            //   split=random    — uniform-random train/test
            //   neg=uniform     — pure uniform negative sampling (legacy)
            //   neg=mixed       — popularity-/time-matched mix (default)
            let mut args = env::args().skip(2).peekable();
            let knobs = if matches!(args.peek().map(|s| s.as_str()), Some("mix-only")) {
                args.next();
                MIX_ONLY_KNOBS
            } else {
                GRID_KNOBS
            };
            let opts = parse_grid_flags(args);
            run_grid(knobs, opts)?;
        }
        "probe" => run_probe()?,
        other => {
            eprintln!(
                "unknown mode: {other}. Use 'eval', 'grid [mix-only] [flags]', or 'probe'.\n\
                 grid flags: pairs-only, no-pairs, with-diversify, split=random|post_id, neg=uniform|mixed"
            );
            std::process::exit(2);
        }
    }
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
            "neg=uniform" => opts.neg_mode = NegMode::Uniform,
            "neg=mixed" => opts.neg_mode = NegMode::Mixed,
            other => eprintln!("[grid] unknown flag: {other} (ignored)"),
        }
    }
    opts
}
