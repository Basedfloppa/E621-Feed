//! Metrics + scoring loop. NDCG@k / Recall@k / MRR over per-account ranked
//! lists. Parallelism via a bounded rayon pool sized from `calibrate_threads`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

use chrono::{DateTime, Utc};
use rayon::prelude::*;
use rayon::ThreadPool;

use e621_account_parser_api::models::cfg;
use e621_account_parser_api::utils::{diversify_indices, Priors, ScoringContext};

use crate::dataset::EvalDataset;

#[derive(Default, Clone, Debug)]
pub(crate) struct Metrics {
    pub(crate) ndcg_at_k: f64,
    pub(crate) recall_at_k: f64,
    pub(crate) mrr: f64,
    pub(crate) n_accounts: usize,
    /// Per-account NDCG@K, parallel to `recall_per_account` /
    /// `mrr_per_account`. Populated during scoring; consumed by the SE /
    /// CI helpers below. Cheap (~22 KB at N=915) and lets the grid
    /// distinguish real improvements from sample noise.
    pub(crate) ndcg_per_account: Vec<f64>,
    pub(crate) recall_per_account: Vec<f64>,
    pub(crate) mrr_per_account: Vec<f64>,
}

impl Metrics {
    /// Mean of each metric. Per-account vectors are kept for downstream
    /// CI / SE computation but are not modified here.
    pub(crate) fn average(&self) -> Self {
        if self.n_accounts == 0 {
            return self.clone();
        }
        let n = self.n_accounts as f64;
        Self {
            ndcg_at_k: self.ndcg_at_k / n,
            recall_at_k: self.recall_at_k / n,
            mrr: self.mrr / n,
            n_accounts: self.n_accounts,
            ndcg_per_account: self.ndcg_per_account.clone(),
            recall_per_account: self.recall_per_account.clone(),
            mrr_per_account: self.mrr_per_account.clone(),
        }
    }

    /// Standard error of the per-account NDCG@K mean. Used by the grid
    /// loop to gate probe acceptance: `m.ndcg_at_k > best + Z * se` is a
    /// 1-sided test that the new mean is meaningfully above baseline,
    /// not just better-by-noise.
    pub(crate) fn ndcg_se(&self) -> f64 {
        se_of_mean(&self.ndcg_per_account)
    }

    /// 95% CI on the NDCG@K mean via percentile bootstrap with
    /// `n_resamples` resamples. ~5–10 ms at N=915, n_resamples=1000.
    /// Used at print-time only (`[baseline]`, `[best]`); the grid loop
    /// uses the cheaper SE test for per-probe acceptance.
    pub(crate) fn ndcg_ci95(&self, n_resamples: usize) -> (f64, f64) {
        bootstrap_ci_95(&self.ndcg_per_account, n_resamples)
    }
}

/// Standard error of the sample mean. Clamps n=0/1 to 0.
fn se_of_mean(xs: &[f64]) -> f64 {
    let n = xs.len();
    if n < 2 {
        return 0.0;
    }
    let mean = xs.iter().sum::<f64>() / n as f64;
    let var = xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1) as f64;
    (var / n as f64).sqrt()
}

/// Percentile bootstrap 95% CI of the mean. Linear-congruential RNG —
/// cheap, deterministic, and good enough for ranking-stat resamples.
fn bootstrap_ci_95(xs: &[f64], n_resamples: usize) -> (f64, f64) {
    let n = xs.len();
    if n < 2 || n_resamples == 0 {
        let mean = if n > 0 { xs.iter().sum::<f64>() / n as f64 } else { 0.0 };
        return (mean, mean);
    }
    let mut means: Vec<f64> = Vec::with_capacity(n_resamples);
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let n_u = n as u64;
    for _ in 0..n_resamples {
        let mut sum = 0.0f64;
        for _ in 0..n {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let idx = ((state >> 33) % n_u) as usize;
            sum += xs[idx];
        }
        means.push(sum / n as f64);
    }
    means.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let lo_idx = ((n_resamples as f64) * 0.025).floor() as usize;
    let hi_idx = ((n_resamples as f64) * 0.975).floor() as usize;
    (
        means[lo_idx.min(n_resamples - 1)],
        means[hi_idx.min(n_resamples - 1)],
    )
}

/// Bounded rayon pool. `0 = auto = nproc/2`, configurable via
/// `backtest.calibrate_threads`. Built once on first call.
pub(crate) fn pool() -> &'static ThreadPool {
    static POOL: OnceLock<ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let configured = cfg().backtest.calibrate_threads;
        let n_threads = if configured == 0 {
            (cores / 2).max(1)
        } else {
            configured.min(cores).max(1)
        };
        eprintln!("[score_with] rayon pool sized at {n_threads} threads (of {cores} cores)");
        rayon::ThreadPoolBuilder::new()
            .num_threads(n_threads)
            .thread_name(|i| format!("calibrate-{i}"))
            .build()
            .expect("build rayon pool")
    })
}

/// Score every cached fixture under `priors`. `now` is supplied by the
/// caller so a single eval run can freeze the wall-clock and have all
/// posts see identical ages.
///
/// `progress = true` enables per-batch heartbeats to stderr — used by
/// `eval` (single long-running call). The grid loop calls
/// [`crate::cache::score_with_cache`] instead so probes can skip
/// recomputing channels they don't invalidate.
pub(crate) fn score_with_progress(
    dataset: &EvalDataset,
    priors: &Priors,
    now: DateTime<Utc>,
    top_k_ndcg: usize,
    top_k_recall: usize,
    diversify: bool,
) -> Metrics {
    score_with_opts(
        dataset,
        priors,
        now,
        top_k_ndcg,
        top_k_recall,
        diversify,
        true,
    )
}

fn score_with_opts(
    dataset: &EvalDataset,
    priors: &Priors,
    now: DateTime<Utc>,
    top_k_ndcg: usize,
    top_k_recall: usize,
    diversify: bool,
    progress: bool,
) -> Metrics {
    let mut priors = priors.clone();
    priors.now = now;
    let priors = &priors;

    let total = dataset.accounts.len();
    let counter = AtomicUsize::new(0);
    let report_every = (total / 10).max(20);
    let t0 = std::time::Instant::now();

    let per_account: Vec<(f64, f64, f64)> = pool().install(|| {
        dataset
            .accounts
            .par_iter()
            .map(|fx| {
                let ctx = ScoringContext::new(
                    &fx.tags,
                    priors,
                    &dataset.idf,
                    &fx.profile,
                    &dataset.global_relation,
                    &fx.user_relation,
                );

                let n_test = fx.test_features.len();
                let total_posts = n_test + fx.neg_features.len();
                let mut scored: Vec<(i64, f32, bool)> = Vec::with_capacity(total_posts);

                if diversify {
                    let mut entries: Vec<(f32, f32, i64)> = Vec::with_capacity(total_posts);
                    for f in &fx.test_features {
                        let (s, breakdown) = ctx.score_cached(f);
                        entries.push((s, breakdown.interaction_fit, f.id));
                    }
                    for f in &fx.neg_features {
                        let (s, breakdown) = ctx.score_cached(f);
                        entries.push((s, breakdown.interaction_fit, f.id));
                    }
                    let head_limit = top_k_ndcg.max(top_k_recall).saturating_mul(2).max(50);
                    let order =
                        diversify_indices(&entries, &fx.diversity_features, priors, head_limit);
                    for i in order {
                        let is_pos = i < n_test;
                        scored.push((entries[i].2, entries[i].0, is_pos));
                    }
                } else {
                    for f in &fx.test_features {
                        let (s, _) = ctx.score_cached(f);
                        scored.push((f.id, s, true));
                    }
                    for f in &fx.neg_features {
                        let (s, _) = ctx.score_cached(f);
                        scored.push((f.id, s, false));
                    }
                    scored
                        .sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                }

                let result = (
                    ndcg_at_k(&scored, top_k_ndcg),
                    recall_at_k(&scored, top_k_recall, fx.test_count),
                    mrr(&scored),
                );
                if progress {
                    let done = counter.fetch_add(1, Ordering::Relaxed) + 1;
                    if done % report_every == 0 || done == total {
                        eprintln!(
                            "[score]   {done}/{total} accounts scored ({:.1}s elapsed)",
                            t0.elapsed().as_secs_f32()
                        );
                    }
                }
                result
            })
            .collect()
    });

    let mut totals = Metrics::default();
    totals.ndcg_per_account.reserve(per_account.len());
    totals.recall_per_account.reserve(per_account.len());
    totals.mrr_per_account.reserve(per_account.len());
    for (n, r, m) in per_account {
        totals.ndcg_at_k += n;
        totals.recall_at_k += r;
        totals.mrr += m;
        totals.n_accounts += 1;
        totals.ndcg_per_account.push(n);
        totals.recall_per_account.push(r);
        totals.mrr_per_account.push(m);
    }
    if progress {
        eprintln!(
            "[score] DONE: {} accounts in {:.1}s",
            totals.n_accounts,
            t0.elapsed().as_secs_f32()
        );
    }
    totals
}

pub(crate) fn print_metrics(label: &str, m: &Metrics, top_k_ndcg: usize, top_k_recall: usize) {
    let (ndcg_lo, ndcg_hi) = m.ndcg_ci95(1000);
    println!(
        "[{label}] N={}  NDCG@{top_k_ndcg}={:.4} (95% CI {:.4}–{:.4}, SE={:.4})  \
         Recall@{top_k_recall}={:.4}  MRR={:.4}",
        m.n_accounts,
        m.ndcg_at_k,
        ndcg_lo,
        ndcg_hi,
        m.ndcg_se(),
        m.recall_at_k,
        m.mrr
    );
}

pub(crate) fn ndcg_at_k_pub(ranked: &[(i64, f32, bool)], k: usize) -> f64 {
    ndcg_at_k(ranked, k)
}

pub(crate) fn recall_at_k_pub(
    ranked: &[(i64, f32, bool)],
    k: usize,
    total_positives: usize,
) -> f64 {
    recall_at_k(ranked, k, total_positives)
}

pub(crate) fn mrr_pub(ranked: &[(i64, f32, bool)]) -> f64 {
    mrr(ranked)
}

fn ndcg_at_k(ranked: &[(i64, f32, bool)], k: usize) -> f64 {
    let mut dcg = 0.0;
    for (i, (_, _, is_pos)) in ranked.iter().take(k).enumerate() {
        if *is_pos {
            dcg += 1.0 / ((i as f64 + 2.0).log2());
        }
    }
    let n_pos = ranked.iter().filter(|(_, _, p)| *p).count().min(k);
    let mut idcg = 0.0;
    for i in 0..n_pos {
        idcg += 1.0 / ((i as f64 + 2.0).log2());
    }
    if idcg <= 0.0 {
        0.0
    } else {
        dcg / idcg
    }
}

fn recall_at_k(ranked: &[(i64, f32, bool)], k: usize, total_positives: usize) -> f64 {
    if total_positives == 0 {
        return 0.0;
    }
    let hit = ranked.iter().take(k).filter(|(_, _, p)| *p).count();
    hit as f64 / total_positives as f64
}

fn mrr(ranked: &[(i64, f32, bool)]) -> f64 {
    for (i, (_, _, is_pos)) in ranked.iter().enumerate() {
        if *is_pos {
            return 1.0 / (i as f64 + 1.0);
        }
    }
    0.0
}
