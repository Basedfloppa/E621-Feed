//! `run_grid` orchestrator: adaptive line search + paired sweep + categorical sweep.

use chrono::Utc;

use e621_account_parser_api::models::cfg;

use crate::dataset::prepare_eval_dataset;
use crate::knobs::{KnobSpec, CATEGORICAL_KNOBS, PAIRED_KNOBS, PASS_SCALES};
use crate::log::{print_diff, write_grid_log};
use crate::metrics::{print_metrics, score_with};
use crate::options::GridOptions;

fn knob_by_name<'a>(knobs: &'a [KnobSpec], name: &str) -> Option<&'a KnobSpec> {
    knobs.iter().find(|k| k.name == name)
}

pub(crate) fn run_grid(knobs: &[KnobSpec], opts: GridOptions) -> anyhow::Result<()> {
    let cfg_arc = cfg();
    let top_k_ndcg = cfg_arc.backtest.top_k_ndcg;
    let top_k_recall = cfg_arc.backtest.top_k_recall;

    let t0 = std::time::Instant::now();
    eprintln!("[grid] preparing dataset...");
    let dataset = prepare_eval_dataset(&opts)?;
    eprintln!("[grid] dataset ready in {:.1}s", t0.elapsed().as_secs_f32());

    // Frozen wall-clock for the entire run.
    let now = Utc::now();

    let total_probes: usize = knobs.iter().map(|k| k.probes.len()).sum();
    eprintln!(
        "[grid] {} knobs × ~{} probes/pass × {} passes = up to {} evals + paired sweep",
        knobs.len(),
        if knobs.is_empty() {
            0
        } else {
            total_probes / knobs.len()
        },
        PASS_SCALES.len(),
        total_probes * PASS_SCALES.len()
    );
    eprintln!("[grid] adaptive step: {:?}", PASS_SCALES);

    let baseline = cfg_arc.priors.clone();
    let baseline_m =
        score_with(&dataset, &baseline, now, top_k_ndcg, top_k_recall, opts.diversify).average();
    print_metrics("baseline", &baseline_m, top_k_ndcg, top_k_recall);

    let mut best = baseline.clone();
    let mut best_score = baseline_m.ndcg_at_k;

    // Single-knob passes with adaptive scaling.
    if !opts.pairs_only {
        for (pass_idx, &scale) in PASS_SCALES.iter().enumerate() {
            let pass = pass_idx + 1;
            let mut pass_changed = false;
            for k in knobs {
                for &raw_delta in k.probes {
                    let delta = raw_delta * scale;
                    let mut trial = best.clone();
                    (k.apply)(&mut trial, delta);
                    let m = score_with(
                        &dataset,
                        &trial,
                        now,
                        top_k_ndcg,
                        top_k_recall,
                        opts.diversify,
                    )
                    .average();
                    if m.ndcg_at_k > best_score + 1e-4 {
                        eprintln!(
                            "pass {pass}(×{scale:.2}): {} {delta:+.3}  NDCG@{top_k_ndcg} {:.4} -> {:.4}",
                            k.name, best_score, m.ndcg_at_k
                        );
                        best = trial;
                        best_score = m.ndcg_at_k;
                        pass_changed = true;
                    }
                }
            }
            if !pass_changed {
                eprintln!("pass {pass}(×{scale:.2}): no improvement, converged");
                break;
            }
        }
    }

    // Paired sweep over known-correlated knobs.
    if opts.run_paired || opts.pairs_only {
        eprintln!("\n[grid] paired sweep over {} known-correlated pairs", PAIRED_KNOBS.len());
        let pair_scale = if opts.pairs_only { 1.0 } else { 0.5 };
        for &(name_a, name_b) in PAIRED_KNOBS {
            let (Some(ka), Some(kb)) = (knob_by_name(knobs, name_a), knob_by_name(knobs, name_b))
            else {
                continue;
            };
            let da = ka.probes.iter().map(|v| v.abs()).fold(f32::INFINITY, f32::min);
            let db = kb.probes.iter().map(|v| v.abs()).fold(f32::INFINITY, f32::min);
            for (sa, sb) in [(1.0, 1.0), (1.0, -1.0), (-1.0, 1.0), (-1.0, -1.0)] {
                let mut trial = best.clone();
                (ka.apply)(&mut trial, sa * da * pair_scale);
                (kb.apply)(&mut trial, sb * db * pair_scale);
                let m = score_with(
                    &dataset,
                    &trial,
                    now,
                    top_k_ndcg,
                    top_k_recall,
                    opts.diversify,
                )
                .average();
                if m.ndcg_at_k > best_score + 1e-4 {
                    eprintln!(
                        "pair: ({} {:+.3}, {} {:+.3})  NDCG@{top_k_ndcg} {:.4} -> {:.4}",
                        name_a,
                        sa * da * pair_scale,
                        name_b,
                        sb * db * pair_scale,
                        best_score,
                        m.ndcg_at_k
                    );
                    best = trial;
                    best_score = m.ndcg_at_k;
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
            let m = score_with(
                &dataset,
                &trial,
                now,
                top_k_ndcg,
                top_k_recall,
                opts.diversify,
            )
            .average();
            if m.ndcg_at_k > best_score + 1e-4 {
                eprintln!(
                    "categorical: {} = \"{cand}\"  NDCG@{top_k_ndcg} {:.4} -> {:.4}",
                    ck.name, best_score, m.ndcg_at_k
                );
                best = trial;
                best_score = m.ndcg_at_k;
            }
        }
    }

    let final_m =
        score_with(&dataset, &best, now, top_k_ndcg, top_k_recall, opts.diversify).average();
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
    eprintln!("[grid] total time: {:.1}s", t0.elapsed().as_secs_f32());
    Ok(())
}
