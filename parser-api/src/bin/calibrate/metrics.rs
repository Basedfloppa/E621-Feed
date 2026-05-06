//! Metrics + scoring loop. NDCG@k / Recall@k / MRR over per-account ranked
//! lists. Parallelism via a bounded rayon pool sized from `calibrate_threads`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

use chrono::{DateTime, Utc};
use rayon::prelude::*;
use rayon::ThreadPool;

use e621_account_parser_api::models::{cfg, ScoredPost};
use e621_account_parser_api::utils::{diversify_scored_posts, Priors, ScoringContext};

use crate::dataset::EvalDataset;

#[derive(Default, Clone, Copy, Debug)]
pub(crate) struct Metrics {
    pub(crate) ndcg_at_k: f64,
    pub(crate) recall_at_k: f64,
    pub(crate) mrr: f64,
    pub(crate) n_accounts: usize,
}

impl Metrics {
    pub(crate) fn average(&self) -> Self {
        if self.n_accounts == 0 {
            return *self;
        }
        let n = self.n_accounts as f64;
        Self {
            ndcg_at_k: self.ndcg_at_k / n,
            recall_at_k: self.recall_at_k / n,
            mrr: self.mrr / n,
            n_accounts: self.n_accounts,
        }
    }
}

/// Bounded rayon pool. `0 = auto = nproc/2`, configurable via
/// `backtest.calibrate_threads`. Built once on first call.
fn pool() -> &'static ThreadPool {
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
/// caller so a grid run can freeze the wall-clock once and have all
/// probes see identical post-ages.
///
/// `progress = true` enables per-batch heartbeats to stderr — useful for
/// `eval` (single long-running call). The grid loop passes `false` to
/// avoid drowning the per-probe output in heartbeats.
pub(crate) fn score_with(
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
        false,
    )
}

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
                    &dataset.empty_user_relation,
                );

                let mut scored: Vec<(i64, f32, bool)> =
                    Vec::with_capacity(fx.test_posts.len() + fx.neg_posts.len());

                if diversify {
                    let mut sps: Vec<ScoredPost> =
                        Vec::with_capacity(fx.test_posts.len() + fx.neg_posts.len());
                    for p in &fx.test_posts {
                        let (s, breakdown) = ctx.score(p);
                        sps.push(ScoredPost {
                            post: p.clone(),
                            score: s,
                            breakdown: Some(breakdown),
                        });
                    }
                    for p in &fx.neg_posts {
                        let (s, breakdown) = ctx.score(p);
                        sps.push(ScoredPost {
                            post: p.clone(),
                            score: s,
                            breakdown: Some(breakdown),
                        });
                    }
                    let positives: std::collections::HashSet<i64> =
                        fx.test_posts.iter().map(|p| p.id).collect();
                    let diversified = diversify_scored_posts(sps, priors);
                    for sp in diversified {
                        let id = sp.post.id;
                        scored.push((id, sp.score, positives.contains(&id)));
                    }
                } else {
                    for p in &fx.test_posts {
                        let (s, _) = ctx.score(p);
                        scored.push((p.id, s, true));
                    }
                    for p in &fx.neg_posts {
                        let (s, _) = ctx.score(p);
                        scored.push((p.id, s, false));
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
    for (n, r, m) in per_account {
        totals.ndcg_at_k += n;
        totals.recall_at_k += r;
        totals.mrr += m;
        totals.n_accounts += 1;
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
    println!(
        "[{label}] N={}  NDCG@{top_k_ndcg}={:.4}  Recall@{top_k_recall}={:.4}  MRR={:.4}",
        m.n_accounts, m.ndcg_at_k, m.recall_at_k, m.mrr
    );
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
