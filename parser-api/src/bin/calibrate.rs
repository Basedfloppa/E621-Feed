//! Offline mix-weight calibration harness.
//!
//! For each account with enough favourites, splits favs into 80/20 train/test
//! by post id (newer posts → test, older → train), builds a synthetic profile
//! from train, samples N negatives from the catalog, and asks the production
//! `ScoringContext` to rank them. Reports NDCG@20, Recall@50, and MRR.
//!
//! Usage:
//!   cargo run --release --bin calibrate -- eval
//!     # one-off score using priors from config.toml
//!
//!   cargo run --release --bin calibrate -- grid
//!     # greedy line-search over each mix_* weight, three passes
//!
//! Reads database.db from the working directory (same as the server). Does
//! not connect to e621.
//!
//! Parallelism policy: synchronous, single-threaded scoring loop. If you
//! parallelise via rayon, cap the thread pool at half of `nproc` (matches
//! `.cargo/config.toml`'s `jobs = 6` on this 12-core box) so the box stays
//! usable while a multi-hour grid runs in the background.

use std::collections::HashMap;
use std::env;
use std::sync::OnceLock;

use chrono::Utc;
use rayon::ThreadPool;
use rayon::prelude::*;
use rusqlite::params;

use e621_account_parser_api::db;
use e621_account_parser_api::models::{
    AccountMediaStat, AccountPreferenceProfile, AccountQualityProfile, AccountRatingStat,
    AccountRecencyProfile, Post, TagCount, cfg, default_path, reload_from,
};
use e621_account_parser_api::utils::{IdfIndex, Priors, ScoringContext, TagRelationGraph};

/// Deterministic seed for negative sampling so grid runs are reproducible
/// (same negatives across configs → only the priors differ). Kept in source
/// because it's a magic constant for reproducibility, not a tuning knob.
const NEG_SAMPLE_SEED: u64 = 0xE621_CA118;

#[derive(Default, Clone, Copy, Debug)]
struct Metrics {
    ndcg_at_k: f64,
    recall_at_k: f64,
    mrr: f64,
    n_accounts: usize,
}

impl Metrics {
    fn average(&self) -> Self {
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

fn main() -> anyhow::Result<()> {
    // Force-load config so cfg() returns valid Priors. Matches server startup.
    let path = default_path()?;
    reload_from(&path)?;

    // Apply schema migrations before any read — calibrate uses columns added
    // by V12 (preview_url, score_up, etc.). Idempotent if the DB is already
    // up to date.
    db::ensure_sqlite().map_err(|e| anyhow::anyhow!("migrate: {e}"))?;

    let mode = env::args().nth(1).unwrap_or_else(|| "eval".into());

    match mode.as_str() {
        "eval" => {
            let cfg_arc = cfg();
            let top_k_ndcg = cfg_arc.backtest.top_k_ndcg;
            let top_k_recall = cfg_arc.backtest.top_k_recall;
            let dataset = prepare_eval_dataset()?;
            let priors = cfg_arc.priors.clone();
            let m = score_with(&dataset, &priors, top_k_ndcg, top_k_recall).average();
            print_metrics("baseline", &m, top_k_ndcg, top_k_recall);
        }
        "grid" => {
            // Default: scan all measurable knobs (~26). Pass `grid mix-only`
            // for the original 8-mix-weight behaviour (faster, narrower).
            let knobs = match env::args().nth(2).as_deref() {
                Some("mix-only") => MIX_ONLY_KNOBS,
                _ => GRID_KNOBS,
            };
            run_grid(knobs)?;
        }
        "probe" => {
            run_probe()?;
        }
        other => {
            eprintln!("unknown mode: {other}. Use 'eval', 'grid', 'grid mix-only', or 'probe'.");
            std::process::exit(2);
        }
    }
    Ok(())
}

/// Quick descriptive stats — useful before kicking off a full eval to see how
/// many accounts will actually clear MIN_FAVS.
fn run_probe() -> anyhow::Result<()> {
    let conn = db::open_db_for_calibration().map_err(|e| anyhow::anyhow!(e))?;
    let posts: i64 = conn.query_row("SELECT COUNT(*) FROM posts", [], |r| r.get(0))?;
    let accounts: i64 = conn.query_row("SELECT COUNT(*) FROM accounts", [], |r| r.get(0))?;
    let with_favs: i64 = conn.query_row(
        "SELECT COUNT(DISTINCT account_id) FROM accounts_post",
        [],
        |r| r.get(0),
    )?;
    let total_favs: i64 = conn.query_row("SELECT COUNT(*) FROM accounts_post", [], |r| r.get(0))?;
    println!("posts: {posts}");
    println!("accounts: {accounts}");
    println!("accounts w/ favs: {with_favs}");
    println!("fav links: {total_favs}");

    println!("\nfav-count buckets:");
    for (label, lo, hi) in [
        ("<10", 0i64, 10i64),
        ("10-49", 10, 50),
        ("50-99", 50, 100),
        ("100-499", 100, 500),
        ("500+", 500, i64::MAX),
    ] {
        let c: i64 = conn.query_row(
            "SELECT COUNT(*) FROM (
                 SELECT account_id, COUNT(*) c FROM accounts_post GROUP BY account_id
             ) WHERE c >= ?1 AND c < ?2",
            params![lo, hi],
            |r| r.get(0),
        )?;
        println!("  {label:>8}: {c} accounts");
    }

    println!("\ntop 20 by fav count:");
    let mut stmt = conn.prepare(
        "SELECT account_id, COUNT(*) FROM accounts_post
         GROUP BY account_id ORDER BY COUNT(*) DESC LIMIT 20",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, i32>(0)?, r.get::<_, i64>(1)?)))?;
    for r in rows {
        let (id, c) = r?;
        println!("  account_id={id:>6}  favs={c}");
    }
    Ok(())
}

/// Per-account state needed to score (test ∪ negatives) under any priors
/// config. Built once, reused across every grid trial — `EvalDataset` is
/// the cache that makes 100-eval grid searches feasible.
struct AccountFixture {
    profile: AccountPreferenceProfile,
    tags: Vec<TagCount>,
    test_posts: Vec<Post>,
    neg_posts: Vec<Post>,
    test_count: usize,
}

struct EvalDataset {
    idf: IdfIndex,
    global_relation: TagRelationGraph,
    empty_user_relation: TagRelationGraph,
    group_weights: HashMap<String, f32>,
    accounts: Vec<AccountFixture>,
}

/// Pulls every account's train/test split + a sampled negative pool out of
/// SQLite once. After this returns, `score_with` can iterate any priors over
/// the in-memory data without touching the database.
fn prepare_eval_dataset() -> anyhow::Result<EvalDataset> {
    let cfg_arc = cfg();
    let bt = &cfg_arc.backtest;
    let min_favs = bt.min_favs;
    let test_fraction = bt.test_fraction;
    let negative_ratio = bt.negative_ratio;
    let max_accounts = bt.max_accounts;

    let group_weights = cfg_arc.group_weights.clone();
    let idf = IdfIndex::from_db().map_err(|e| anyhow::anyhow!("idf load: {e}"))?;
    let global_relation =
        db::load_global_tag_relation().map_err(|e| anyhow::anyhow!("global relation: {e}"))?;
    let empty_user_relation = TagRelationGraph::empty();
    let catalog_ids = catalog_post_ids()?;

    let account_ids = eligible_accounts(min_favs as i64, max_accounts)?;
    let mut fixtures = Vec::with_capacity(account_ids.len());

    for account_id in account_ids {
        let fav_ids = account_favorite_ids(account_id)?;
        if fav_ids.len() < min_favs {
            continue;
        }
        let (train_ids, test_ids) = split_train_test(&fav_ids, test_fraction);
        if test_ids.is_empty() {
            continue;
        }

        let train_posts = match db::hydrate_posts_by_ids(&train_ids) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("hydrate train for {account_id} failed: {e}");
                continue;
            }
        };
        let test_posts = match db::hydrate_posts_by_ids(&test_ids) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("hydrate test for {account_id} failed: {e}");
                continue;
            }
        };
        if train_posts.len() < min_favs / 2 || test_posts.is_empty() {
            continue;
        }

        let profile = build_profile(&train_posts);
        let tags = build_tag_counts(&train_posts);

        let test_id_set: std::collections::HashSet<i64> = test_posts.iter().map(|p| p.id).collect();
        let target_negs = test_posts.len() * negative_ratio;
        let neg_ids = sample_negatives(&catalog_ids, &fav_ids, target_negs);
        let mut candidate_ids = neg_ids;
        candidate_ids.retain(|id| !test_id_set.contains(id));
        let neg_posts = db::hydrate_posts_by_ids(&candidate_ids).unwrap_or_default();
        let test_count = test_posts.len();

        fixtures.push(AccountFixture {
            profile,
            tags,
            test_posts,
            neg_posts,
            test_count,
        });
    }

    eprintln!(
        "[prep] {} accounts hydrated; ~{} posts cached in memory",
        fixtures.len(),
        fixtures
            .iter()
            .map(|f| f.test_posts.len() + f.neg_posts.len())
            .sum::<usize>()
    );

    Ok(EvalDataset {
        idf,
        global_relation,
        empty_user_relation,
        group_weights,
        accounts: fixtures,
    })
}

/// Bounded rayon pool used by `score_with`. Size pulled from
/// `backtest.calibrate_threads`; 0 means auto = nproc / 2 (matches the
/// cargo-build cap in `.cargo/config.toml`). Built once on first use.
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

/// Score every cached fixture under `priors` and aggregate metrics. No I/O.
/// Per-account scoring is independent (read-only `dataset`), so we
/// parallelise over accounts via the bounded pool above.
fn score_with(
    dataset: &EvalDataset,
    priors: &Priors,
    top_k_ndcg: usize,
    top_k_recall: usize,
) -> Metrics {
    let mut priors = priors.clone();
    priors.now = Utc::now();
    let priors = &priors;

    let per_account: Vec<(f64, f64, f64)> = pool().install(|| {
        dataset
            .accounts
            .par_iter()
            .map(|fx| {
                let ctx = ScoringContext::new(
                    &fx.tags,
                    &dataset.group_weights,
                    priors,
                    &dataset.idf,
                    &fx.profile,
                    &dataset.global_relation,
                    &dataset.empty_user_relation,
                );

                let mut scored: Vec<(i64, f32, bool)> =
                    Vec::with_capacity(fx.test_posts.len() + fx.neg_posts.len());
                for p in &fx.test_posts {
                    let (s, _) = ctx.score(p);
                    scored.push((p.id, s, true));
                }
                for p in &fx.neg_posts {
                    let (s, _) = ctx.score(p);
                    scored.push((p.id, s, false));
                }
                scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

                (
                    ndcg_at_k(&scored, top_k_ndcg),
                    recall_at_k(&scored, top_k_recall, fx.test_count),
                    mrr(&scored),
                )
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

    totals
}

/// One tunable parameter for the grid search.
///
/// `apply` mutates the trial Priors in-place. `probes` are absolute deltas
/// added to the current value — the scale differs per parameter (mix_*
/// fields use ±0.05/±0.10, but recency_tau_days needs days, etc).
struct KnobSpec {
    name: &'static str,
    apply: fn(&mut Priors, f32),
    probes: &'static [f32],
}

const PROBES_MIX: &[f32] = &[-0.10, -0.05, 0.05, 0.10];
const PROBES_FRACTION: &[f32] = &[-0.10, -0.05, 0.05, 0.10];
const PROBES_SMALL_FRACTION: &[f32] = &[-0.05, -0.02, 0.02, 0.05];
const PROBES_WEIGHT: &[f32] = &[-0.20, -0.10, 0.10, 0.20];
const PROBES_DAYS: &[f32] = &[-5.0, -2.0, 2.0, 5.0];
const PROBES_LOG_BIAS: &[f32] = &[-1.0, -0.5, 0.5, 1.0];
const PROBES_SMOOTHING: &[f32] = &[-0.5, -0.2, 0.2, 0.5];
const PROBES_PMI: &[f32] = &[-1.0, -0.5, 0.5, 1.0];
const PROBES_COLDSTART: &[f32] = &[-10.0, -5.0, 5.0, 10.0];
const PROBES_COOC_REF: &[f32] = &[-5.0, -2.0, 2.0, 5.0];

/// Knobs that move NDCG in the offline harness — used by `grid`.
///
/// Excluded on purpose (they don't move in this setup):
///   * `diversity_*` — `diversify_scored_posts` isn't applied in calibrate.
///   * `strong_negative_*`, `meta_interaction_weight`,
///     `feedback_decay_half_life_days` — synthetic profile has no
///     `feed_interactions`, so the gradient is zero.
///   * `tag_relation_min_cooc`, `tag_relation_user_min_cooc` — integer
///     thresholds with discrete jumps; tune by hand if needed.
///   * `recency_log_personal` — boolean; toggle outside the line search.
const GRID_KNOBS: &[KnobSpec] = &[
    // -- mix_* (8) --
    KnobSpec {
        name: "mix_sim",
        apply: |p, v| p.mix_sim = (p.mix_sim + v).max(0.0),
        probes: PROBES_MIX,
    },
    KnobSpec {
        name: "mix_quality",
        apply: |p, v| p.mix_quality = (p.mix_quality + v).max(0.0),
        probes: PROBES_MIX,
    },
    KnobSpec {
        name: "mix_recency",
        apply: |p, v| p.mix_recency = (p.mix_recency + v).max(0.0),
        probes: PROBES_MIX,
    },
    KnobSpec {
        name: "mix_rating",
        apply: |p, v| p.mix_rating = (p.mix_rating + v).max(0.0),
        probes: PROBES_MIX,
    },
    KnobSpec {
        name: "mix_media",
        apply: |p, v| p.mix_media = (p.mix_media + v).max(0.0),
        probes: PROBES_MIX,
    },
    KnobSpec {
        name: "mix_popularity",
        apply: |p, v| p.mix_popularity = (p.mix_popularity + v).max(0.0),
        probes: PROBES_MIX,
    },
    KnobSpec {
        name: "mix_interaction",
        apply: |p, v| p.mix_interaction = (p.mix_interaction + v).max(0.0),
        probes: PROBES_MIX,
    },
    KnobSpec {
        name: "mix_tag_relation",
        apply: |p, v| p.mix_tag_relation = (p.mix_tag_relation + v).max(0.0),
        probes: PROBES_MIX,
    },
    // -- IDF shaping (3) --
    KnobSpec {
        name: "idf_lambda",
        apply: |p, v| p.idf_lambda = (p.idf_lambda + v).clamp(0.0, 1.0),
        probes: PROBES_FRACTION,
    },
    KnobSpec {
        name: "idf_alpha",
        apply: |p, v| p.idf_alpha = (p.idf_alpha + v).clamp(0.0, 1.0),
        probes: PROBES_FRACTION,
    },
    KnobSpec {
        name: "freq_alpha",
        apply: |p, v| p.freq_alpha = (p.freq_alpha + v).clamp(0.0, 1.0),
        probes: PROBES_FRACTION,
    },
    // -- Quality-fit internals (5) --
    KnobSpec {
        name: "quality_a",
        apply: |p, v| p.quality_a = (p.quality_a + v).max(0.0),
        probes: PROBES_SMALL_FRACTION,
    },
    KnobSpec {
        name: "quality_b",
        apply: |p, v| p.quality_b = (p.quality_b + v).max(0.0),
        probes: PROBES_SMALL_FRACTION,
    },
    KnobSpec {
        name: "quality_log_bias",
        apply: |p, v| p.quality_log_bias += v,
        probes: PROBES_LOG_BIAS,
    },
    KnobSpec {
        name: "quality_w_absolute",
        apply: |p, v| p.quality_w_absolute = (p.quality_w_absolute + v).max(0.0),
        probes: PROBES_WEIGHT,
    },
    KnobSpec {
        name: "quality_w_relative_score",
        apply: |p, v| p.quality_w_relative_score = (p.quality_w_relative_score + v).max(0.0),
        probes: PROBES_WEIGHT,
    },
    // -- Recency internals (3) --
    KnobSpec {
        name: "recency_tau_days",
        apply: |p, v| p.recency_tau_days = (p.recency_tau_days + v).max(1.0),
        probes: PROBES_DAYS,
    },
    KnobSpec {
        name: "recency_w_global",
        apply: |p, v| p.recency_w_global = (p.recency_w_global + v).max(0.0),
        probes: PROBES_WEIGHT,
    },
    KnobSpec {
        name: "recency_personal_floor_frac",
        apply: |p, v| {
            p.recency_personal_floor_frac = (p.recency_personal_floor_frac + v).clamp(0.0, 2.0)
        },
        probes: PROBES_SMALL_FRACTION,
    },
    // -- Popularity internals (1) --
    KnobSpec {
        name: "popularity_w_fav",
        apply: |p, v| p.popularity_w_fav = (p.popularity_w_fav + v).max(0.0),
        probes: PROBES_WEIGHT,
    },
    // -- Discrete preference internals (2) --
    KnobSpec {
        name: "discrete_smoothing_alpha",
        apply: |p, v| p.discrete_smoothing_alpha = (p.discrete_smoothing_alpha + v).max(0.0),
        probes: PROBES_SMOOTHING,
    },
    KnobSpec {
        name: "discrete_pref_floor",
        apply: |p, v| p.discrete_pref_floor = (p.discrete_pref_floor + v).clamp(0.0, 0.5),
        probes: PROBES_SMALL_FRACTION,
    },
    // -- Tag-relation internals (4) --
    KnobSpec {
        name: "tag_relation_pmi_scale",
        apply: |p, v| p.tag_relation_pmi_scale = (p.tag_relation_pmi_scale + v).max(1.0),
        probes: PROBES_PMI,
    },
    KnobSpec {
        name: "tag_relation_w_global",
        apply: |p, v| p.tag_relation_w_global = (p.tag_relation_w_global + v).max(0.0),
        probes: PROBES_WEIGHT,
    },
    KnobSpec {
        name: "tag_relation_cooc_ref",
        apply: |p, v| p.tag_relation_cooc_ref = (p.tag_relation_cooc_ref + v).max(1.0),
        probes: PROBES_COOC_REF,
    },
    KnobSpec {
        name: "tag_relation_user_cooc_ref",
        apply: |p, v| p.tag_relation_user_cooc_ref = (p.tag_relation_user_cooc_ref + v).max(1.0),
        probes: PROBES_COOC_REF,
    },
    // -- Cold start (1) --
    KnobSpec {
        name: "coldstart_n0",
        apply: |p, v| p.coldstart_n0 = (p.coldstart_n0 + v).max(1.0),
        probes: PROBES_COLDSTART,
    },
];

const MIX_ONLY_KNOBS: &[KnobSpec] = &[
    KnobSpec {
        name: "mix_sim",
        apply: |p, v| p.mix_sim = (p.mix_sim + v).max(0.0),
        probes: PROBES_MIX,
    },
    KnobSpec {
        name: "mix_quality",
        apply: |p, v| p.mix_quality = (p.mix_quality + v).max(0.0),
        probes: PROBES_MIX,
    },
    KnobSpec {
        name: "mix_recency",
        apply: |p, v| p.mix_recency = (p.mix_recency + v).max(0.0),
        probes: PROBES_MIX,
    },
    KnobSpec {
        name: "mix_rating",
        apply: |p, v| p.mix_rating = (p.mix_rating + v).max(0.0),
        probes: PROBES_MIX,
    },
    KnobSpec {
        name: "mix_media",
        apply: |p, v| p.mix_media = (p.mix_media + v).max(0.0),
        probes: PROBES_MIX,
    },
    KnobSpec {
        name: "mix_popularity",
        apply: |p, v| p.mix_popularity = (p.mix_popularity + v).max(0.0),
        probes: PROBES_MIX,
    },
    KnobSpec {
        name: "mix_interaction",
        apply: |p, v| p.mix_interaction = (p.mix_interaction + v).max(0.0),
        probes: PROBES_MIX,
    },
    KnobSpec {
        name: "mix_tag_relation",
        apply: |p, v| p.mix_tag_relation = (p.mix_tag_relation + v).max(0.0),
        probes: PROBES_MIX,
    },
];

/// Greedy coordinate-descent line search. Per pass, walks each knob in
/// `knobs` and probes a few perturbations; keeps whichever wins NDCG.
fn run_grid(knobs: &[KnobSpec]) -> anyhow::Result<()> {
    let cfg_arc = cfg();
    let top_k_ndcg = cfg_arc.backtest.top_k_ndcg;
    let top_k_recall = cfg_arc.backtest.top_k_recall;

    let t0 = std::time::Instant::now();
    eprintln!("[grid] preparing dataset...");
    let dataset = prepare_eval_dataset()?;
    eprintln!("[grid] dataset ready in {:.1}s", t0.elapsed().as_secs_f32());

    let total_probes: usize = knobs.iter().map(|k| k.probes.len()).sum();
    eprintln!(
        "[grid] {} knobs × ~{} probes/pass = up to {} evals/pass",
        knobs.len(),
        if knobs.is_empty() {
            0
        } else {
            total_probes / knobs.len()
        },
        total_probes
    );

    let baseline = cfg_arc.priors.clone();
    let baseline_m = score_with(&dataset, &baseline, top_k_ndcg, top_k_recall).average();
    print_metrics("baseline", &baseline_m, top_k_ndcg, top_k_recall);

    let mut best = baseline.clone();
    let mut best_score = baseline_m.ndcg_at_k;

    for pass in 1..=3 {
        let mut pass_changed = false;
        for k in knobs {
            for &delta in k.probes {
                let mut trial = best.clone();
                (k.apply)(&mut trial, delta);
                let m = score_with(&dataset, &trial, top_k_ndcg, top_k_recall).average();
                if m.ndcg_at_k > best_score + 1e-4 {
                    eprintln!(
                        "pass {pass}: {} {delta:+.3}  NDCG@{top_k_ndcg} {:.4} -> {:.4}",
                        k.name, best_score, m.ndcg_at_k
                    );
                    best = trial;
                    best_score = m.ndcg_at_k;
                    pass_changed = true;
                }
            }
        }
        if !pass_changed {
            eprintln!("pass {pass}: no improvement, converged");
            break;
        }
    }

    let final_m = score_with(&dataset, &best, top_k_ndcg, top_k_recall).average();
    println!();
    print_metrics("best", &final_m, top_k_ndcg, top_k_recall);
    println!("\n[best priors — non-default values]");
    print_diff(&baseline, &best);
    eprintln!("[grid] total time: {:.1}s", t0.elapsed().as_secs_f32());
    Ok(())
}

/// Print only the priors fields that changed between baseline and best.
/// Cuts down noise vs dumping the whole struct, and surfaces what to copy
/// into config.toml at a glance.
fn print_diff(baseline: &Priors, best: &Priors) {
    macro_rules! diff {
        ($label:literal, $field:ident, $fmt:literal) => {
            if (baseline.$field - best.$field).abs() > 1e-6 {
                println!(
                    concat!("{:<32} = ", $fmt, "    (was ", $fmt, ")"),
                    $label, best.$field, baseline.$field
                );
            }
        };
    }
    diff!("mix_sim", mix_sim, "{:.3}");
    diff!("mix_quality", mix_quality, "{:.3}");
    diff!("mix_recency", mix_recency, "{:.3}");
    diff!("mix_rating", mix_rating, "{:.3}");
    diff!("mix_media", mix_media, "{:.3}");
    diff!("mix_popularity", mix_popularity, "{:.3}");
    diff!("mix_interaction", mix_interaction, "{:.3}");
    diff!("mix_tag_relation", mix_tag_relation, "{:.3}");
    diff!("idf_lambda", idf_lambda, "{:.3}");
    diff!("idf_alpha", idf_alpha, "{:.3}");
    diff!("freq_alpha", freq_alpha, "{:.3}");
    diff!("quality_a", quality_a, "{:.3}");
    diff!("quality_b", quality_b, "{:.3}");
    diff!("quality_log_bias", quality_log_bias, "{:.3}");
    diff!("quality_w_absolute", quality_w_absolute, "{:.3}");
    diff!(
        "quality_w_relative_score",
        quality_w_relative_score,
        "{:.3}"
    );
    diff!("recency_tau_days", recency_tau_days, "{:.2}");
    diff!("recency_w_global", recency_w_global, "{:.3}");
    diff!(
        "recency_personal_floor_frac",
        recency_personal_floor_frac,
        "{:.3}"
    );
    diff!("popularity_w_fav", popularity_w_fav, "{:.3}");
    diff!(
        "discrete_smoothing_alpha",
        discrete_smoothing_alpha,
        "{:.3}"
    );
    diff!("discrete_pref_floor", discrete_pref_floor, "{:.3}");
    diff!("tag_relation_pmi_scale", tag_relation_pmi_scale, "{:.3}");
    diff!("tag_relation_w_global", tag_relation_w_global, "{:.3}");
    diff!("tag_relation_cooc_ref", tag_relation_cooc_ref, "{:.2}");
    diff!(
        "tag_relation_user_cooc_ref",
        tag_relation_user_cooc_ref,
        "{:.2}"
    );
    diff!("coldstart_n0", coldstart_n0, "{:.1}");
}

fn print_metrics(label: &str, m: &Metrics, top_k_ndcg: usize, top_k_recall: usize) {
    println!(
        "[{label}] N={}  NDCG@{top_k_ndcg}={:.4}  Recall@{top_k_recall}={:.4}  MRR={:.4}",
        m.n_accounts, m.ndcg_at_k, m.recall_at_k, m.mrr
    );
}

fn ndcg_at_k(ranked: &[(i64, f32, bool)], k: usize) -> f64 {
    let mut dcg = 0.0;
    for (i, (_, _, is_pos)) in ranked.iter().take(k).enumerate() {
        if *is_pos {
            // Binary relevance → numerator is (2^1 - 1) = 1.
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

fn split_train_test(ids: &[i64], test_frac: f32) -> (Vec<i64>, Vec<i64>) {
    let mut sorted: Vec<i64> = ids.to_vec();
    // Higher post id ≈ favourited more recently (favourites copy upstream
    // creation order well enough for held-out evaluation).
    sorted.sort_unstable();
    let n_test = ((sorted.len() as f32) * test_frac).round() as usize;
    let split_at = sorted.len().saturating_sub(n_test).max(1);
    let train = sorted[..split_at].to_vec();
    let test = sorted[split_at..].to_vec();
    (train, test)
}

fn build_profile(train_posts: &[Post]) -> AccountPreferenceProfile {
    let mut rating_counts: HashMap<String, i64> = HashMap::new();
    let mut media_counts: HashMap<&'static str, i64> = HashMap::new();
    let mut score_total_sum = 0.0f32;
    let mut fav_sum = 0.0f32;
    let mut comment_sum = 0.0f32;
    let mut duration_sum = 0.0f32;
    let now = Utc::now();
    let mut ages = Vec::with_capacity(train_posts.len());

    for p in train_posts {
        *rating_counts.entry(p.rating.to_string()).or_insert(0) += 1;
        *media_counts.entry(p.media_type()).or_insert(0) += 1;
        score_total_sum += p.score.total.max(0) as f32;
        fav_sum += p.fav_count.max(0) as f32;
        comment_sum += p.comment_count.max(0) as f32;
        duration_sum += p.duration.unwrap_or(0.0) as f32;
        let age = (now - p.created_at).num_seconds() as f32 / 86_400.0;
        ages.push(age.max(0.0));
    }

    let n = train_posts.len().max(1) as f32;
    let mean_age: f32 = ages.iter().sum::<f32>() / n;
    let abs_dev: f32 = ages.iter().map(|a| (a - mean_age).abs()).sum::<f32>() / n;

    AccountPreferenceProfile {
        rating: rating_counts
            .into_iter()
            .map(|(rating, count)| AccountRatingStat { rating, count })
            .collect(),
        media: media_counts
            .into_iter()
            .map(|(media_type, count)| AccountMediaStat {
                media_type: media_type.to_string(),
                count,
            })
            .collect(),
        // Synthetic split has no interaction history; leave empty so the
        // interaction signal effectively goes neutral.
        feedback: Vec::new(),
        quality: AccountQualityProfile {
            avg_score_total: score_total_sum / n,
            avg_fav_count: fav_sum / n,
            avg_comment_count: comment_sum / n,
            avg_duration: duration_sum / n,
        },
        recency: AccountRecencyProfile {
            avg_age_days: mean_age,
            avg_abs_dev_days: abs_dev,
        },
    }
}

fn build_tag_counts(train_posts: &[Post]) -> Vec<TagCount> {
    let mut counts: HashMap<(String, &'static str), i64> = HashMap::new();
    for p in train_posts {
        for (group, tags) in [
            ("artist", &p.tags.artist),
            ("character", &p.tags.character),
            ("copyright", &p.tags.copyright),
            ("general", &p.tags.general),
            ("lore", &p.tags.lore),
            ("species", &p.tags.species),
            ("meta", &p.tags.meta),
        ] {
            for t in tags {
                let lc = t.to_ascii_lowercase();
                *counts.entry((lc, group)).or_insert(0) += 1;
            }
        }
    }
    counts
        .into_iter()
        .filter(|(_, c)| *c > 0)
        .map(|((name, group), count)| TagCount {
            name,
            group_type: group.to_string(),
            count,
        })
        .collect()
}

fn sample_negatives(catalog: &[i64], excluded: &[i64], target: usize) -> Vec<i64> {
    let excl: std::collections::HashSet<i64> = excluded.iter().copied().collect();
    if catalog.is_empty() {
        return Vec::new();
    }
    // Pseudo-random sampling without replacement via xorshift64. Stride
    // sampling biased negatives toward older post ids (catalog is sorted by
    // id), which then over-fired the recency penalty against newer test
    // posts. Random sampling decouples post-id distribution from the
    // train/test split.
    let mut rng = NEG_SAMPLE_SEED;
    let n = catalog.len();
    let mut chosen: std::collections::HashSet<i64> =
        std::collections::HashSet::with_capacity(target);
    let mut out: Vec<i64> = Vec::with_capacity(target);
    let mut tries = 0usize;
    let max_tries = target.saturating_mul(10).max(100);
    while out.len() < target && tries < max_tries {
        // xorshift64
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let idx = (rng as usize) % n;
        let id = catalog[idx];
        if !excl.contains(&id) && chosen.insert(id) {
            out.push(id);
        }
        tries += 1;
    }
    out
}

fn catalog_post_ids() -> anyhow::Result<Vec<i64>> {
    let conn = db::open_db_for_calibration().map_err(|e| anyhow::anyhow!(e))?;
    let mut stmt = conn.prepare("SELECT id FROM posts WHERE is_deleted = 0 ORDER BY id")?;
    let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
    let ids = rows.collect::<Result<Vec<_>, _>>()?;
    Ok(ids)
}

fn eligible_accounts(min_favs: i64, cap: usize) -> anyhow::Result<Vec<i32>> {
    let conn = db::open_db_for_calibration().map_err(|e| anyhow::anyhow!(e))?;
    let mut stmt = conn.prepare(
        "SELECT account_id FROM accounts_post
         GROUP BY account_id HAVING COUNT(*) >= ?1
         ORDER BY COUNT(*) DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![min_favs, cap as i64], |r| r.get::<_, i32>(0))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

fn account_favorite_ids(account_id: i32) -> anyhow::Result<Vec<i64>> {
    let conn = db::open_db_for_calibration().map_err(|e| anyhow::anyhow!(e))?;
    let mut stmt = conn.prepare("SELECT post_id FROM accounts_post WHERE account_id = ?1")?;
    let rows = stmt.query_map(params![account_id], |r| r.get::<_, i64>(0))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}
