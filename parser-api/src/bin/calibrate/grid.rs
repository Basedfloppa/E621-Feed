//! `run_grid` orchestrator: adaptive line search + paired sweep + categorical sweep.

use chrono::DateTime;
use chrono::Utc;

use e621_account_parser_api::models::cfg;

use crate::cache::{M_ALL, ScoreCache, score_with_cache};
use crate::dataset::EvalDataset;
use crate::knobs::{CATEGORICAL_KNOBS, KnobSpec, PAIRED_KNOBS, PASS_SCALES};
use crate::log::{print_diff, write_grid_log};
use crate::metrics::print_metrics;
use crate::options::GridOptions;

fn knob_by_name<'a>(knobs: &[&'a KnobSpec], name: &str) -> Option<&'a KnobSpec> {
    knobs.iter().find(|k| k.name == name).copied()
}

/// Run the grid against a pre-hydrated dataset. `t0` is the caller-side
/// start time so the final "[grid] total time" line includes prep cost
/// when a single invocation chains `eval grid`.
pub(crate) fn run_grid_with_dataset(
    knobs: &[KnobSpec],
    opts: GridOptions,
    dataset: &EvalDataset,
    now: DateTime<Utc>,
    t0: std::time::Instant,
) -> anyhow::Result<()> {
    let cfg_arc = cfg();
    let top_k_ndcg = cfg_arc.backtest.top_k_ndcg;
    let top_k_recall = cfg_arc.backtest.top_k_recall;

    // Skip diversify-only knobs when MMR is off — they don't enter
    // scoring and would just churn no-op probes.
    let active_knobs: Vec<&KnobSpec> = knobs
        .iter()
        .filter(|k| opts.diversify || !k.diversify_only)
        .collect();
    let skipped = knobs.len() - active_knobs.len();

    let total_probes: usize = active_knobs.iter().map(|k| k.probes.len()).sum();
    eprintln!(
        "[grid] {} knobs × ~{} probes/pass × {} passes = up to {} evals + paired sweep{}",
        active_knobs.len(),
        if active_knobs.is_empty() {
            0
        } else {
            total_probes / active_knobs.len()
        },
        PASS_SCALES.len(),
        total_probes * PASS_SCALES.len(),
        if skipped > 0 {
            format!(" (skipping {skipped} diversify-only knobs)")
        } else {
            String::new()
        }
    );
    eprintln!("[grid] adaptive step: {PASS_SCALES:?}");

    eprintln!("[grid] running baseline eval...");
    let t_baseline = std::time::Instant::now();
    let baseline = cfg_arc.priors.clone();
    // Baseline = full rebuild → seeds the channel cache for subsequent probes.
    let (baseline_m, baseline_cache) = score_with_cache(
        dataset,
        &baseline,
        now,
        top_k_ndcg,
        top_k_recall,
        opts.diversify,
        None,
        M_ALL,
    );
    let baseline_m = baseline_m.average();
    eprintln!(
        "[grid] baseline eval took {:.1}s",
        t_baseline.elapsed().as_secs_f32()
    );
    print_metrics("baseline", &baseline_m, top_k_ndcg, top_k_recall);

    let mut best = baseline.clone();
    let mut best_score = baseline_m.ndcg_at_k;
    let mut best_cache: ScoreCache = baseline_cache;
    let mut evals_done: usize = 0;

    // Acceptance: SE-aware threshold instead of a fixed 1e-4. A probe
    // counts as an improvement only if the mean NDCG climbed past the
    // baseline by ≥ Z·SE (1-sided 95% confidence). Floors at 1e-4 so
    // pathological small-N samples still need *some* movement.
    const ACCEPT_Z: f64 = 1.645; // 1-sided 95%
    let acceptance = |new_mean: f64, baseline: f64, se: f64| -> bool {
        let threshold = baseline + (ACCEPT_Z * se).max(1e-4);
        new_mean > threshold
    };
    // Skipped/rejected probes — printed only under `verbose`.
    let mut skipped_nan: usize = 0;
    let mut skipped_early: usize = 0;
    let mut total_evals_planned: usize = 0;

    // Single-knob passes with adaptive scaling + per-knob early exit.
    if !opts.pairs_only {
        for (pass_idx, &scale) in PASS_SCALES.iter().enumerate() {
            let pass = pass_idx + 1;
            let pass_t = std::time::Instant::now();
            let mut pass_changed = false;
            let total_pass_evals: usize = active_knobs.iter().map(|k| k.probes.len()).sum();
            total_evals_planned += total_pass_evals;
            let mut pass_evals = 0usize;
            let heartbeat_every = (total_pass_evals / 10).max(20);
            for k in &active_knobs {
                // Per-knob early exit: after 2 consecutive non-improving
                // probes inside this knob, skip the rest of its probe
                // list for this pass. Saves 30–50% of probe budget on
                // converged knobs.
                let mut consecutive_misses = 0u8;
                for &raw_delta in k.probes {
                    if consecutive_misses >= 2 {
                        let remaining = k.probes.len()
                            - (k.probes.iter().position(|&d| d == raw_delta).unwrap_or(0));
                        skipped_early += remaining;
                        if opts.verbose {
                            eprintln!(
                                "[grid]   pass {pass}(×{scale:.2}) early-exit on {} after 2 misses (skip {remaining})",
                                k.name
                            );
                        }
                        break;
                    }
                    let delta = raw_delta * scale;
                    let mut trial = best.clone();
                    (k.apply)(&mut trial, delta);
                    let prev_cache = Some(&best_cache);
                    let (m_raw, trial_cache) = score_with_cache(
                        dataset,
                        &trial,
                        now,
                        top_k_ndcg,
                        top_k_recall,
                        opts.diversify,
                        prev_cache,
                        k.invalidates,
                    );
                    let m = m_raw.average();
                    pass_evals += 1;
                    evals_done += 1;

                    // Defensive: drop NaN/Inf probes (extreme priors can
                    // overflow) so they don't accidentally count as
                    // improvements via partial_cmp's Equal fallback.
                    if !m.ndcg_at_k.is_finite() {
                        skipped_nan += 1;
                        eprintln!(
                            "[grid]   WARN pass {pass}(×{scale:.2}): {} {delta:+.3} produced NaN/Inf NDCG, treating as fail",
                            k.name
                        );
                        consecutive_misses = consecutive_misses.saturating_add(1);
                        continue;
                    }

                    let new_mean = m.ndcg_at_k;
                    let new_se = m.ndcg_se();
                    if acceptance(new_mean, best_score, new_se) {
                        eprintln!(
                            "pass {pass}(×{scale:.2}): {} {delta:+.3}  NDCG@{top_k_ndcg} {:.4} -> {:.4} (Δ={:+.4}, SE={:.4})",
                            k.name,
                            best_score,
                            new_mean,
                            new_mean - best_score,
                            new_se,
                        );
                        best = trial;
                        best_score = new_mean;
                        best_cache = trial_cache;
                        pass_changed = true;
                        consecutive_misses = 0;
                    } else {
                        consecutive_misses = consecutive_misses.saturating_add(1);
                        if opts.verbose {
                            eprintln!(
                                "[probe] pass {pass}(×{scale:.2}): {} {delta:+.3}  NDCG {:.4} (Δ={:+.4}, threshold={:+.4})",
                                k.name,
                                new_mean,
                                new_mean - best_score,
                                (ACCEPT_Z * new_se).max(1e-4),
                            );
                        }
                    }
                    if pass_evals.is_multiple_of(heartbeat_every) {
                        eprintln!(
                            "[grid]   pass {pass}(×{scale:.2}) heartbeat: {pass_evals}/{total_pass_evals} probes, current best NDCG@{top_k_ndcg} {:.4}, {:.1}s elapsed",
                            best_score,
                            pass_t.elapsed().as_secs_f32()
                        );
                    }
                }
            }
            eprintln!(
                "[grid] pass {pass}(×{scale:.2}) done in {:.1}s ({pass_evals}/{total_pass_evals} probes; {} so far total)",
                pass_t.elapsed().as_secs_f32(),
                evals_done
            );
            if !pass_changed {
                eprintln!("pass {pass}(×{scale:.2}): no improvement, converged");
                break;
            }
        }
    }

    // Paired sweep over known-correlated knobs. Two knobs together → union mask.
    if opts.run_paired || opts.pairs_only {
        eprintln!(
            "\n[grid] paired sweep over {} known-correlated pairs",
            PAIRED_KNOBS.len()
        );
        let pair_scale = if opts.pairs_only { 1.0 } else { 0.5 };
        for &(name_a, name_b) in PAIRED_KNOBS {
            let (Some(ka), Some(kb)) = (
                knob_by_name(&active_knobs, name_a),
                knob_by_name(&active_knobs, name_b),
            ) else {
                continue;
            };
            let da = ka
                .probes
                .iter()
                .map(|v| v.abs())
                .fold(f32::INFINITY, f32::min);
            let db = kb
                .probes
                .iter()
                .map(|v| v.abs())
                .fold(f32::INFINITY, f32::min);
            let pair_mask = ka.invalidates | kb.invalidates;
            for (sa, sb) in [(1.0, 1.0), (1.0, -1.0), (-1.0, 1.0), (-1.0, -1.0)] {
                let mut trial = best.clone();
                (ka.apply)(&mut trial, sa * da * pair_scale);
                (kb.apply)(&mut trial, sb * db * pair_scale);
                let prev_cache = Some(&best_cache);
                let (m_raw, trial_cache) = score_with_cache(
                    dataset,
                    &trial,
                    now,
                    top_k_ndcg,
                    top_k_recall,
                    opts.diversify,
                    prev_cache,
                    pair_mask,
                );
                let m = m_raw.average();
                if !m.ndcg_at_k.is_finite() {
                    skipped_nan += 1;
                    eprintln!(
                        "[grid]   WARN paired probe ({name_a}, {name_b}) produced NaN/Inf NDCG, skipping"
                    );
                    continue;
                }
                if acceptance(m.ndcg_at_k, best_score, m.ndcg_se()) {
                    eprintln!(
                        "pair: ({} {:+.3}, {} {:+.3})  NDCG@{top_k_ndcg} {:.4} -> {:.4} (SE={:.4})",
                        name_a,
                        sa * da * pair_scale,
                        name_b,
                        sb * db * pair_scale,
                        best_score,
                        m.ndcg_at_k,
                        m.ndcg_se(),
                    );
                    best = trial;
                    best_score = m.ndcg_at_k;
                    best_cache = trial_cache;
                }
            }
        }
    }

    // Categorical sweep (Class E v5.3).
    for ck in CATEGORICAL_KNOBS {
        let baseline_value = match ck.name {
            "tag_relation_pair_aggregator" => best.tag_relation_pair_aggregator.clone(),
            _ => continue,
        };
        for &cand in ck.candidates {
            if cand == baseline_value {
                continue;
            }
            let mut trial = best.clone();
            (ck.apply)(&mut trial, cand);
            let prev_cache = Some(&best_cache);
            let (m_raw, trial_cache) = score_with_cache(
                dataset,
                &trial,
                now,
                top_k_ndcg,
                top_k_recall,
                opts.diversify,
                prev_cache,
                ck.invalidates,
            );
            let m = m_raw.average();
            if !m.ndcg_at_k.is_finite() {
                skipped_nan += 1;
                eprintln!(
                    "[grid]   WARN categorical {} = \"{cand}\" produced NaN/Inf NDCG, skipping",
                    ck.name
                );
                continue;
            }
            if acceptance(m.ndcg_at_k, best_score, m.ndcg_se()) {
                eprintln!(
                    "categorical: {} = \"{cand}\"  NDCG@{top_k_ndcg} {:.4} -> {:.4} (SE={:.4})",
                    ck.name,
                    best_score,
                    m.ndcg_at_k,
                    m.ndcg_se(),
                );
                best = trial;
                best_score = m.ndcg_at_k;
                best_cache = trial_cache;
            }
        }
    }

    // Final eval — use the cached path (or full rebuild on diversify).
    let (final_m_raw, _) = score_with_cache(
        dataset,
        &best,
        now,
        top_k_ndcg,
        top_k_recall,
        opts.diversify,
        None,
        M_ALL,
    );
    let final_m = final_m_raw.average();
    println!();
    print_metrics("best", &final_m, top_k_ndcg, top_k_recall);
    println!("\n[best priors — non-default values]");
    let saturated = print_diff(&baseline, &best);
    if !saturated.is_empty() {
        println!("\n[warn] knobs at clamp boundary (search range may be too narrow):");
        for s in &saturated {
            println!("  • {s}");
        }
    }
    if let Err(e) = write_grid_log(&best, &final_m, &opts, t0.elapsed(), &saturated) {
        eprintln!("[grid] failed to write result log: {e}");
    }
    eprintln!(
        "[grid] total time: {:.1}s; evals={evals_done}, skipped (early-exit)={skipped_early}, skipped (NaN/Inf)={skipped_nan}, planned={total_evals_planned}",
        t0.elapsed().as_secs_f32()
    );
    Ok(())
}
