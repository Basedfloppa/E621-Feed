//! `run_grid` orchestrator: adaptive line search + paired sweep + categorical sweep.

use chrono::DateTime;
use chrono::Utc;

use e621_account_parser_api::models::cfg;

use crate::cache::{M_ALL, ScoreCache, score_with_cache};
use crate::dataset::EvalDataset;
use crate::knobs::{CATEGORICAL_KNOBS, KnobSpec, PAIRED_KNOBS, PASS_SCALES};
use crate::log::{print_diff, write_grid_log};
use crate::metrics::{Metrics, paired_mean_se, print_metrics, t_quantile_95};
use crate::options::GridOptions;

/// Outcome of gating one probe against the incumbent configuration.
///
/// `pub(crate)` so the analytic mix fit (`fit.rs`) can be judged by the same
/// gate as a grid probe — a fitted weight vector that would not have been
/// accepted as a probe must not be reported as an improvement.
pub(crate) struct Gate {
    pub(crate) accept: bool,
    /// Mean NDCG@K delta (candidate − incumbent).
    pub(crate) delta: f64,
    /// SE used for the test: paired (SE of per-account deltas) when the
    /// vectors line up, one-sample otherwise.
    pub(crate) se: f64,
    pub(crate) z: f64,
    pub(crate) paired: bool,
}

impl Gate {
    pub(crate) fn threshold(&self) -> f64 {
        (self.z * self.se).max(MIN_ABS_DELTA)
    }
}

/// Floor on the acceptance threshold. Pathological small-N samples can
/// produce a paired SE of exactly 0 (every account moving by the same
/// amount); without a floor any infinitesimal improvement would pass.
const MIN_ABS_DELTA: f64 = 1e-4;

fn knob_by_name<'a>(knobs: &[&'a KnobSpec], name: &str) -> Option<&'a KnobSpec> {
    knobs.iter().find(|k| k.name == name).copied()
}

/// Gate one probe against the incumbent configuration: a **paired**
/// 1-sided 95% test over per-account NDCG@K.
///
/// Both configurations are scored on the same accounts, so the
/// between-account spread — which dominates the *level* standard error —
/// cancels in the per-account difference. The previous gate compared the
/// probe mean against the incumbent mean using the probe's one-sample SE
/// (`ndcg_se()`). On the 2026-10-09 prod snapshot (N=16 surviving
/// accounts) that SE was 0.054, making the threshold +0.089 NDCG — larger
/// than any single knob moves — so the grid accepted nothing and
/// early-exited 165 of 335 probes after two misses each. The paired SE on
/// the same data is an order of magnitude smaller, because it measures
/// only how consistently the probe beats the incumbent *account by
/// account*.
///
/// The quantile is Student's t at df=N−1 rather than the normal 1.645,
/// because N is routinely small; see [`t_quantile_95`].
pub(crate) fn gate_probe(cand: &Metrics, inc_mean: f64, inc_per_account: &[f64]) -> Gate {
    match paired_mean_se(&cand.ndcg_per_account, inc_per_account) {
        Some((delta, se)) => {
            let g = Gate {
                delta,
                se,
                z: t_quantile_95(cand.ndcg_per_account.len() - 1),
                paired: true,
                accept: false,
            };
            Gate {
                accept: delta > g.threshold(),
                ..g
            }
        }
        // Unpaired fallback: the vectors are not comparable (length
        // mismatch — e.g. an account dropped from the dataset), so fall
        // back to the pre-existing one-sample test.
        None => {
            let g = Gate {
                delta: cand.ndcg_at_k - inc_mean,
                se: cand.ndcg_se(),
                z: t_quantile_95(cand.n_accounts.saturating_sub(1)),
                paired: false,
                accept: false,
            };
            Gate {
                accept: g.delta > g.threshold(),
                ..g
            }
        }
    }
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
    reused_baseline: Option<(Metrics, ScoreCache)>,
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

    let baseline = cfg_arc.priors.clone();
    // Baseline = full rebuild → seeds the channel cache for subsequent probes.
    //
    // When an `eval` mode ran just before this grid, it already scored the same
    // dataset under the same priors, `now`, `top_k` and diversify flag, so its
    // cache *is* this baseline. Rebuilding it here would repeat identical work:
    // on the 2026-10-10 run the two passes printed byte-identical metrics
    // (`NDCG@20=0.3547`, CI 0.3378–0.3724, `Recall@50=0.0595`, `MRR=0.5600`)
    // for 62.1s + 56.9s — the second 56.9s being pure duplication.
    let (baseline_m, baseline_cache) = match reused_baseline {
        Some((m, cache)) => {
            eprintln!(
                "[grid] baseline reused from the preceding eval pass \
                 (channel cache handed over, rebuild skipped)"
            );
            (m, cache)
        }
        None => {
            eprintln!("[grid] running baseline eval...");
            let t_baseline = std::time::Instant::now();
            let (m, cache) = score_with_cache(
                dataset,
                &baseline,
                now,
                top_k_ndcg,
                top_k_recall,
                opts.diversify,
                None,
                M_ALL,
            );
            eprintln!(
                "[grid] baseline eval took {:.1}s",
                t_baseline.elapsed().as_secs_f32()
            );
            (m.average(), cache)
        }
    };
    print_metrics("baseline", &baseline_m, top_k_ndcg, top_k_recall);

    let mut best = baseline.clone();
    let mut best_score = baseline_m.ndcg_at_k;
    // Incumbent per-account NDCG, parallel to `best_score`. Required by
    // the paired gate; refreshed on every accepted probe.
    let mut best_per_account: Vec<f64> = baseline_m.ndcg_per_account.clone();
    let mut best_cache: ScoreCache = baseline_cache;
    let mut evals_done: usize = 0;

    // Acceptance: a **paired** 1-sided 95% test over per-account NDCG@K.
    //
    // Both configurations are scored on the same accounts, so the
    // between-account spread — which dominates the *level* standard
    // error — cancels in the per-account difference. The previous gate
    // compared the probe mean against the incumbent mean using the
    // probe's one-sample SE, i.e. `ndcg_se()`. On the 2026-10-09 prod
    // snapshot (N=16 surviving accounts) that SE was 0.054, making the
    // threshold +0.089 NDCG — larger than any single knob moves — so the
    // grid accepted nothing and early-exited 165 of 335 probes. The
    // paired SE on the same data is an order of magnitude smaller.
    //
    // Quantile is Student's t at df=N−1 rather than the normal 1.645,
    // because N is routinely small; see `t_quantile_95`.
    eprintln!(
        "[grid] acceptance: paired 1-sided 95% t-test over per-account NDCG (unpaired fallback on mismatch)"
    );
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
                    let g = gate_probe(&m, best_score, &best_per_account);
                    if g.accept {
                        eprintln!(
                            "pass {pass}(×{scale:.2}): {} {delta:+.3}  NDCG@{top_k_ndcg} {:.4} -> {:.4} (Δ={:+.4}, {}SE={:.4}, t={:.3})",
                            k.name,
                            best_score,
                            new_mean,
                            g.delta,
                            if g.paired { "paired " } else { "unpaired " },
                            g.se,
                            g.z,
                        );
                        best = trial;
                        best_score = new_mean;
                        best_per_account = m.ndcg_per_account.clone();
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
                                g.delta,
                                g.threshold(),
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
                let g = gate_probe(&m, best_score, &best_per_account);
                if g.accept {
                    eprintln!(
                        "pair: ({} {:+.3}, {} {:+.3})  NDCG@{top_k_ndcg} {:.4} -> {:.4} ({}SE={:.4}, t={:.3})",
                        name_a,
                        sa * da * pair_scale,
                        name_b,
                        sb * db * pair_scale,
                        best_score,
                        m.ndcg_at_k,
                        if g.paired { "paired " } else { "unpaired " },
                        g.se,
                        g.z,
                    );
                    best = trial;
                    best_score = m.ndcg_at_k;
                    best_per_account = m.ndcg_per_account.clone();
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
            let g = gate_probe(&m, best_score, &best_per_account);
            if g.accept {
                eprintln!(
                    "categorical: {} = \"{cand}\"  NDCG@{top_k_ndcg} {:.4} -> {:.4} ({}SE={:.4}, t={:.3})",
                    ck.name,
                    best_score,
                    m.ndcg_at_k,
                    if g.paired { "paired " } else { "unpaired " },
                    g.se,
                    g.z,
                );
                best = trial;
                best_score = m.ndcg_at_k;
                best_per_account = m.ndcg_per_account.clone();
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
    let diff = print_diff(&baseline, &best);
    if !diff.clamp_saturated.is_empty() {
        println!("\n[warn] knobs at clamp boundary (search range may be too narrow):");
        for s in &diff.clamp_saturated {
            println!("  • {s}");
        }
    }
    if !diff.switched_off.is_empty() {
        println!(
            "\n[note] {} mix weight(s) switched off by the search — every interior probe \
             was measured and rejected before the explicit -1.0 off-probe won:",
            diff.switched_off.len()
        );
        for s in &diff.switched_off {
            println!("  • {s}");
        }
    }
    if let Err(e) = write_grid_log(&best, &final_m, &opts, t0.elapsed(), &diff) {
        eprintln!("[grid] failed to write result log: {e}");
    }
    eprintln!(
        "[grid] total time: {:.1}s; evals={evals_done}, skipped (early-exit)={skipped_early}, skipped (NaN/Inf)={skipped_nan}, planned={total_evals_planned}",
        t0.elapsed().as_secs_f32()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(per_account: &[f64]) -> Metrics {
        Metrics {
            ndcg_at_k: per_account.iter().sum::<f64>() / per_account.len() as f64,
            n_accounts: per_account.len(),
            ndcg_per_account: per_account.to_vec(),
            ..Default::default()
        }
    }

    /// The regression this whole change is about.
    ///
    /// Eight accounts with a wide spread of intrinsic difficulty
    /// (0.00 … 0.70, level SE ≈ 0.091). The candidate improves *every*
    /// account by a small, consistent amount — mean delta +0.021, spread
    /// of deltas ≈ 0.007 — which is exactly what a genuinely better
    /// configuration looks like.
    ///
    /// * Paired test: SE of the deltas ≈ 0.0023, threshold ≈ 0.0044 →
    ///   accepted.
    /// * Old one-sample test: threshold = t(7)·0.091 ≈ 0.173 → rejected,
    ///   because the account spread swamps a real +0.021 gain. That is
    ///   why the grid reported zero wins on the prod dataset.
    #[test]
    fn paired_gate_accepts_a_consistent_win_that_the_level_gate_rejects() {
        let incumbent = [0.0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7];
        let deltas = [0.020, 0.030, 0.010, 0.020, 0.025, 0.015, 0.020, 0.030];
        let candidate: Vec<f64> = incumbent
            .iter()
            .zip(deltas.iter())
            .map(|(b, d)| b + d)
            .collect();
        let inc_mean = incumbent.iter().sum::<f64>() / incumbent.len() as f64;

        let paired = gate_probe(&metrics(&candidate), inc_mean, &incumbent);
        assert!(paired.paired, "vectors line up → paired path");
        assert!(
            paired.accept,
            "consistent +0.021 on every account must be accepted (Δ={}, thr={})",
            paired.delta,
            paired.threshold()
        );
        assert!(paired.se < 0.01, "paired SE={} should be tiny", paired.se);

        // Same candidate, incumbent vector unavailable → unpaired
        // fallback reproduces the old behaviour and rejects it.
        let unpaired = gate_probe(&metrics(&candidate), inc_mean, &[]);
        assert!(!unpaired.paired, "empty incumbent vector → fallback");
        assert!(
            !unpaired.accept,
            "the level gate this replaces rejected the same win (Δ={}, thr={})",
            unpaired.delta,
            unpaired.threshold()
        );
        assert!(
            unpaired.se > 0.05,
            "level SE={} is dominated by spread",
            unpaired.se
        );
    }

    /// A rise in the *mean* that is really just noise: half the accounts
    /// up, half down. The paired test has to reject it — otherwise the
    /// gate would accept sampling noise now that the threshold is small.
    #[test]
    fn paired_gate_rejects_mixed_noise_that_lifts_the_mean() {
        let incumbent = [0.0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7];
        let deltas = [-0.05, 0.10, -0.05, 0.10, -0.05, 0.10, -0.05, 0.10];
        let candidate: Vec<f64> = incumbent
            .iter()
            .zip(deltas.iter())
            .map(|(b, d)| b + d)
            .collect();
        let inc_mean = incumbent.iter().sum::<f64>() / incumbent.len() as f64;

        let g = gate_probe(&metrics(&candidate), inc_mean, &incumbent);
        assert!(g.paired);
        assert!(
            g.delta > 0.0,
            "the mean does rise (Δ={}), so this is not a trivially negative case",
            g.delta
        );
        assert!(
            !g.accept,
            "mixed-sign deltas must not pass (Δ={} < thr={})",
            g.delta,
            g.threshold()
        );
    }

    /// Degenerate guard: a paired SE of exactly zero (every account moved
    /// by the same amount) must still require the 1e-4 floor, or any
    /// infinitesimal change would be accepted.
    #[test]
    fn paired_gate_keeps_the_absolute_floor_when_se_is_zero() {
        let incumbent = [0.1, 0.2, 0.3, 0.4];
        let candidate = [0.1 + 5e-5, 0.2 + 5e-5, 0.3 + 5e-5, 0.4 + 5e-5];
        let inc_mean = incumbent.iter().sum::<f64>() / incumbent.len() as f64;
        let g = gate_probe(&metrics(&candidate), inc_mean, &incumbent);
        assert!(g.paired);
        assert_eq!(g.se, 0.0);
        assert!(!g.accept, "5e-5 < 1e-4 floor");
    }
}
