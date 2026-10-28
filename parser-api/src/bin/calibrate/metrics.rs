//! Metrics + scoring loop. NDCG@k / Recall@k / MRR over per-account ranked
//! lists. Parallelism via a bounded rayon pool sized from `calibrate_threads`.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::{DateTime, Utc};
use rayon::ThreadPool;
use rayon::prelude::*;

use e621_account_parser_api::models::cfg;
use e621_account_parser_api::utils::{
    CachedPostFeatures, Group, Priors, ScoringContext, diversify_indices,
};

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
    /// What the head of the list *is*, not just how well it ranks the labels.
    /// Summed over accounts, divided in `average()`. Motivation: two
    /// configurations whose NDCG differs by less than one standard error can
    /// still hand the reader completely different feeds, and relevance alone
    /// cannot say which is which — which is why `eval` also describes the head.
    pub(crate) dist_age_days: f64,
    pub(crate) dist_fav_count: f64,
    pub(crate) dist_artists: f64,
    pub(crate) dist_tag_jaccard: f64,
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
            dist_age_days: self.dist_age_days / n,
            dist_fav_count: self.dist_fav_count / n,
            dist_artists: self.dist_artists / n,
            dist_tag_jaccard: self.dist_tag_jaccard / n,
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
    /// `n_resamples` resamples. ~5–10 ms at N=915, `n_resamples=1000`.
    /// Used at print-time only (`[baseline]`, `[best]`); the grid loop
    /// uses the cheaper SE test for per-probe acceptance.
    pub(crate) fn ndcg_ci95(&self, n_resamples: usize) -> (f64, f64) {
        bootstrap_ci_95(&self.ndcg_per_account, n_resamples)
    }
}

/// Mean and standard error of the **paired** per-account difference
/// `candidate - incumbent`.
///
/// The grid compares two configurations on the *same* accounts, so the
/// between-account spread (some accounts are intrinsically harder to
/// rank) cancels in the difference. Returns `None` when the vectors are
/// not comparable, so the caller can fall back to the unpaired
/// one-sample test.
pub(crate) fn paired_mean_se(candidate: &[f64], incumbent: &[f64]) -> Option<(f64, f64)> {
    if candidate.len() != incumbent.len() || candidate.len() < 2 {
        return None;
    }
    let deltas: Vec<f64> = candidate
        .iter()
        .zip(incumbent.iter())
        .map(|(a, b)| a - b)
        .collect();
    let mean = deltas.iter().sum::<f64>() / deltas.len() as f64;
    Some((mean, se_of_mean(&deltas)))
}

/// Upper-tail 0.95 quantile of Student's t with `df` degrees of freedom.
///
/// A fixed 1.645 (normal) quantile is wrong in the regime this grid
/// actually runs in: the eligible-account count is small (51 accounts
/// carry ≥100 favourites in the 2026-10-09 prod snapshot, and hydration
/// losses cut that further), and t(15)=1.753 is 6.5% wider than the
/// normal quantile. Values are tabulated and linearly interpolated
/// between rows, converging to 1.6449 above df=120.
pub(crate) fn t_quantile_95(df: usize) -> f64 {
    const TABLE: &[(usize, f64)] = &[
        (1, 6.3138),
        (2, 2.9200),
        (3, 2.3534),
        (4, 2.1318),
        (5, 2.0150),
        (6, 1.9432),
        (7, 1.8946),
        (8, 1.8595),
        (9, 1.8331),
        (10, 1.8125),
        (12, 1.7823),
        (15, 1.7531),
        (20, 1.7247),
        (30, 1.6973),
        (60, 1.6706),
        (120, 1.6577),
    ];
    // A degenerate 0-account sample still has to produce a finite
    // threshold; `se_of_mean` returns 0 there, so the caller's 1e-4
    // floor is what actually decides.
    let df = df.max(1);
    if df > 120 {
        return 1.6449;
    }
    let mut prev = TABLE[0];
    for &(d, t) in TABLE {
        if d == df {
            return t;
        }
        if d > df {
            // Linear interpolation between the surrounding tabulated rows.
            let frac = (df - prev.0) as f64 / (d - prev.0) as f64;
            return prev.1 + frac * (t - prev.1);
        }
        prev = (d, t);
    }
    1.6449
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
        let mean = if n > 0 {
            xs.iter().sum::<f64>() / n as f64
        } else {
            0.0
        };
        return (mean, mean);
    }
    let mut means: Vec<f64> = Vec::with_capacity(n_resamples);
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let n_u = n as u64;
    for _ in 0..n_resamples {
        let mut sum = 0.0f64;
        for _ in 0..n {
            // Linear-congruential generator (LCG) — Numerical Recipes constants
            // chosen for full 2^64 period. We take the high bits (>> 33) to
            // avoid the low-order bit's well-known poor randomness.
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
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

/// Bounded rayon pool for the **database-backed** parallel sections —
/// account hydration in `dataset`. `0 = auto = nproc/2`, configurable via
/// `backtest.calibrate_threads`.
///
/// Deliberately smaller than the core count: this pool drives the r2d2
/// reader pool in `db`, which holds 16 connections. More consumers than
/// that turns hydration into `timed out waiting for connection` for a
/// large share of accounts. Built once on first call.
pub(crate) fn pool() -> &'static ThreadPool {
    static POOL: OnceLock<ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let cores = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
        let configured = cfg().backtest.calibrate_threads;
        let n_threads = if configured == 0 {
            (cores / 2).max(1)
        } else {
            configured.min(cores).max(1)
        };
        eprintln!("[prep] rayon pool sized at {n_threads} threads (of {cores} cores)");
        rayon::ThreadPoolBuilder::new()
            .num_threads(n_threads)
            .thread_name(|i| format!("calibrate-{i}"))
            .build()
            .expect("build calibrate pool")
    })
}

/// Rayon pool for the scoring loops ([`score_with_progress`] and
/// [`crate::cache::score_with_cache`]). `0 = auto = nproc`, configurable
/// via `backtest.calibrate_score_threads`.
///
/// Those loops read the fully in-memory dataset and never acquire a
/// SQLite connection, so the reader-pool ceiling that pins [`pool`] does
/// not apply — they can use every core. Kept separate precisely because
/// the two phases have different bottlenecks; a single knob cannot serve
/// both without either starving hydration or idling the machine.
pub(crate) fn score_pool() -> &'static ThreadPool {
    static POOL: OnceLock<ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let cores = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
        let configured = cfg().backtest.calibrate_score_threads;
        let n_threads = if configured == 0 {
            cores.max(1)
        } else {
            configured.min(cores).max(1)
        };
        eprintln!("[score] scoring pool sized at {n_threads} threads (of {cores} cores)");
        rayon::ThreadPoolBuilder::new()
            .num_threads(n_threads)
            .thread_name(|i| format!("score-{i}"))
            .build()
            .expect("build scoring pool")
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

    let per_account: Vec<(f64, f64, f64, Distribution)> = score_pool().install(|| {
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
                // Id → features, so the head of the ranking can be described
                // after it has been sorted (the tuple only carries the id).
                let mut by_id: HashMap<i64, &CachedPostFeatures> =
                    HashMap::with_capacity(total_posts);
                for f in fx.test_features.iter().chain(fx.neg_features.iter()) {
                    by_id.insert(f.id, f);
                }

                if diversify {
                    let mut entries: Vec<(f32, i64)> = Vec::with_capacity(total_posts);
                    for f in &fx.test_features {
                        let (s, _) = ctx.score_cached(f);
                        entries.push((s, f.id));
                    }
                    for f in &fx.neg_features {
                        let (s, _) = ctx.score_cached(f);
                        entries.push((s, f.id));
                    }
                    let head_limit = top_k_ndcg.max(top_k_recall).saturating_mul(2).max(50);
                    let order = diversify_indices(
                        &entries,
                        &fx.diversity_features,
                        &dataset.global_relation,
                        None,
                        priors,
                        head_limit,
                    );
                    for i in order {
                        let is_pos = i < n_test;
                        scored.push((entries[i].1, entries[i].0, is_pos));
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
                    head_distribution(&scored, &by_id, top_k_ndcg, now),
                );
                if progress {
                    let done = counter.fetch_add(1, Ordering::Relaxed) + 1;
                    if done.is_multiple_of(report_every) || done == total {
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
    for (n, r, m, d) in per_account {
        totals.ndcg_at_k += n;
        totals.recall_at_k += r;
        totals.mrr += m;
        totals.n_accounts += 1;
        totals.ndcg_per_account.push(n);
        totals.recall_per_account.push(r);
        totals.mrr_per_account.push(m);
        totals.dist_age_days += d.age_days;
        totals.dist_fav_count += d.fav_count;
        totals.dist_artists += d.artists;
        totals.dist_tag_jaccard += d.tag_jaccard;
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

/// Shape of the head of one account's ranking.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Distribution {
    /// Median age of the top-K posts, in days.
    pub(crate) age_days: f64,
    /// Median favourite count of the top-K posts.
    pub(crate) fav_count: f64,
    /// Distinct artists among the top-K posts.
    pub(crate) artists: f64,
    /// Mean pairwise tag Jaccard of the top-K posts: how much of the head is
    /// the same picture with different ids.
    pub(crate) tag_jaccard: f64,
}

/// Describe the first `k` entries of `ranked`.
///
/// Relevance metrics answer "is the label above the noise"; this answers
/// "what does the reader actually get". The two disagree: measured on the
/// live feed, two configurations 0.0084 NDCG apart (one standard error) put
/// 10-year-old posts at the top of one and last month's at the top of the
/// other. A metric that cannot see that cannot steer the weights.
pub(crate) fn head_distribution(
    ranked: &[(i64, f32, bool)],
    by_id: &HashMap<i64, &CachedPostFeatures>,
    k: usize,
    now: DateTime<Utc>,
) -> Distribution {
    let top: Vec<&CachedPostFeatures> = ranked
        .iter()
        .take(k)
        .filter_map(|(id, _, _)| by_id.get(id).copied())
        .collect();
    if top.is_empty() {
        return Distribution::default();
    }

    let artist_group = Group::Artist as u8;
    let mut ages: Vec<f64> = top
        .iter()
        .map(|f| (now - f.created_at).num_days().max(0) as f64)
        .collect();
    let mut favs: Vec<f64> = top.iter().map(|f| f.fav_count as f64).collect();
    let artists: HashSet<&str> = top
        .iter()
        .flat_map(|f| f.tags.iter().filter(|t| t.group == artist_group))
        .map(|t| t.lc.as_str())
        .collect();
    let sets: Vec<HashSet<&str>> = top
        .iter()
        .map(|f| f.tags.iter().map(|t| t.lc.as_str()).collect())
        .collect();

    Distribution {
        age_days: median(&mut ages),
        fav_count: median(&mut favs),
        artists: artists.len() as f64,
        tag_jaccard: mean_pairwise_jaccard(&sets),
    }
}

/// Median of a slice, consuming it. Even length → mean of the two middle
/// values. Empty → 0.0.
pub(crate) fn median(xs: &mut [f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = xs.len() / 2;
    if xs.len().is_multiple_of(2) {
        (xs[mid - 1] + xs[mid]) / 2.0
    } else {
        xs[mid]
    }
}

/// Mean Jaccard similarity over all unordered pairs. Empty or single-element
/// input → 0.0 (nothing to be redundant with).
pub(crate) fn mean_pairwise_jaccard(sets: &[HashSet<&str>]) -> f64 {
    let n = sets.len();
    if n < 2 {
        return 0.0;
    }
    let mut sum = 0.0;
    let mut pairs = 0usize;
    for i in 0..n {
        for j in (i + 1)..n {
            let inter = sets[i].intersection(&sets[j]).count() as f64;
            let union = sets[i].union(&sets[j]).count() as f64;
            if union > 0.0 {
                sum += inter / union;
            }
            pairs += 1;
        }
    }
    if pairs == 0 { 0.0 } else { sum / pairs as f64 }
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
    if m.n_accounts > 0 {
        println!(
            "[{label}] head@{top_k_ndcg}: age={:.0}d  fav={:.0}  artists={:.1}  \
             tag-jaccard={:.2}",
            m.dist_age_days, m.dist_fav_count, m.dist_artists, m.dist_tag_jaccard
        );
    }
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
    if idcg <= 0.0 { 0.0 } else { dcg / idcg }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_handles_odd_even_and_empty() {
        assert_eq!(median(&mut []), 0.0);
        assert_eq!(median(&mut [5.0]), 5.0);
        assert_eq!(median(&mut [3.0, 1.0, 2.0]), 2.0);
        // Even length averages the middle pair, which is what makes the
        // reported age of a 20-item head stable across neighbouring posts.
        assert_eq!(median(&mut [4.0, 1.0, 3.0, 2.0]), 2.5);
    }

    #[test]
    fn pairwise_jaccard_is_a_mean_over_pairs_not_over_items() {
        let a: HashSet<&str> = ["x", "y"].into_iter().collect();
        let b: HashSet<&str> = ["x", "z"].into_iter().collect();
        let c: HashSet<&str> = ["q"].into_iter().collect();
        // Pairs: a-b = 1/3, a-c = 0, b-c = 0 → mean 1/9.
        let got = mean_pairwise_jaccard(&[a.clone(), b, c]);
        assert!((got - 1.0 / 9.0).abs() < 1e-9, "got {got}");
        // One item has nothing to be redundant with.
        assert_eq!(mean_pairwise_jaccard(&[a]), 0.0);
        assert_eq!(mean_pairwise_jaccard(&[]), 0.0);
    }

    #[test]
    fn paired_mean_se_cancels_account_difficulty() {
        // Two accounts worth of intrinsic spread; the candidate lifts
        // both by the same amount, so the paired SE collapses to zero
        // while the level SE stays large.
        let incumbent = [0.10, 0.90, 0.20, 0.80];
        let candidate = [0.15, 0.95, 0.25, 0.85];
        let (mean, se) = paired_mean_se(&candidate, &incumbent).expect("comparable vectors");
        assert!((mean - 0.05).abs() < 1e-12, "mean delta={mean}");
        assert!(se.abs() < 1e-12, "paired SE={se} should be 0");
        let level_se = se_of_mean(&candidate);
        assert!(
            level_se > 0.15,
            "level SE={level_se} is dominated by the account spread"
        );
    }

    #[test]
    fn paired_mean_se_rejects_incomparable_vectors() {
        assert!(
            paired_mean_se(&[1.0, 2.0], &[1.0]).is_none(),
            "length mismatch"
        );
        assert!(paired_mean_se(&[1.0], &[1.0]).is_none(), "single account");
        assert!(paired_mean_se(&[], &[]).is_none(), "empty");
    }

    #[test]
    fn paired_mean_se_is_signed() {
        let (mean, _) = paired_mean_se(&[0.1, 0.2], &[0.3, 0.4]).unwrap();
        assert!((mean + 0.2).abs() < 1e-12, "mean={mean}");
    }

    #[test]
    fn t_quantile_95_hits_tabulated_values_and_converges() {
        assert!((t_quantile_95(1) - 6.3138).abs() < 1e-9);
        assert!((t_quantile_95(4) - 2.1318).abs() < 1e-9);
        assert!((t_quantile_95(15) - 1.7531).abs() < 1e-9);
        assert!((t_quantile_95(120) - 1.6577).abs() < 1e-9);
        // Above the table it converges to the normal quantile.
        assert!((t_quantile_95(500) - 1.6449).abs() < 1e-9);
        // df=0 is clamped rather than panicking.
        assert!((t_quantile_95(0) - 6.3138).abs() < 1e-9);
    }

    #[test]
    fn t_quantile_95_interpolates_and_is_monotone() {
        // 13 and 14 sit between the tabulated 12 and 15.
        let (t12, t13, t14, t15) = (
            t_quantile_95(12),
            t_quantile_95(13),
            t_quantile_95(14),
            t_quantile_95(15),
        );
        assert!(
            t12 > t13 && t13 > t14 && t14 > t15,
            "must be decreasing in df"
        );
        assert!(t13 > t15 && t13 < t12, "t13={t13} interpolates");
        // Strictly wider than the normal quantile for realistic small N.
        assert!(t_quantile_95(16) > 1.6449);
    }

    /// The two calibration phases have different bottlenecks, so they get
    /// different pools. Hydration (`dataset.rs`) drives the r2d2 reader
    /// pool, which holds 16 connections, and must stay narrower than the
    /// core count. Scoring (`metrics.rs`, `cache.rs`) runs entirely off the
    /// in-memory dataset and touches no SQLite, so it may use every core.
    ///
    /// Collapsing them back into one knob either starves hydration or idles
    /// the machine — and both have happened: a single `calibrate_threads`
    /// set to `nproc` produced `timed out waiting for connection` on nine
    /// accounts, while the `nproc/2` default leaves half a 24-core box idle
    /// for the multi-hour grid that is 97% of the wall clock.
    ///
    /// Needles are assembled at runtime so the scanner does not count its
    /// own text — same convention as the guard in `dataset.rs`.
    #[test]
    fn scoring_and_hydration_use_separate_pools() {
        let scoring_install = concat!("score", "_pool()", ".install(");
        let scoring_any = concat!("score", "_pool()");
        let pars = |src: &str| {
            src.matches(concat!(".par", "_iter(")).count()
                + src.matches(concat!(".par", "_iter_mut(")).count()
                + src.matches(concat!(".into", "_par_iter(")).count()
        };

        for (name, src) in [
            ("metrics.rs", include_str!("metrics.rs")),
            ("cache.rs", include_str!("cache.rs")),
        ] {
            let installs = src.matches(scoring_install).count();
            let n_par = pars(src);
            assert!(
                n_par > 0,
                "{name}: no parallel section found — did the scanner rot?"
            );
            assert!(
                installs >= n_par,
                "{name} has {n_par} parallel section(s) but only {installs} wrapped in the \
                 scoring pool: a bare parallel iterator escapes to the global rayon pool"
            );
        }

        let prep = include_str!("dataset.rs");
        assert!(
            !prep.contains(scoring_any),
            "dataset.rs must use the DB-safe pool, not the scoring pool: hydration runs one \
             SQLite consumer per thread against an r2d2 pool of 16 connections"
        );
    }
}
