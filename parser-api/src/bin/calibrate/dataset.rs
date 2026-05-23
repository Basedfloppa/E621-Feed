//! Eval-dataset hydration: catalog index + per-account fixtures.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::{DateTime, Utc};
use rayon::prelude::*;
use rusqlite::params;

use e621_account_parser_api::db;
use e621_account_parser_api::models::{
    cfg, AccountMediaStat, AccountPreferenceProfile, AccountQualityProfile, AccountRatingStat,
    AccountRecencyProfile, Post, TagCount,
};
use e621_account_parser_api::utils::{
    CachedPostFeatures, DiversityFeatures, IdfIndex, ScoringContext, TagRelationGraph,
};

use crate::options::{GridOptions, NegMode, SplitStrategy};
use crate::sampling::{
    sample_hard_pool, sample_negatives_mixed, sample_negatives_uniform, split_train_test,
    split_train_test_time,
};

// Thread-local scratch HashSet reused across account hydrations within
// the same rayon thread. Significantly reduces allocator churn compared
// to allocating a new HashSet per account (Point 5).
thread_local! {
    static HYDRATION_SCRATCH: RefCell<std::collections::HashSet<i64>> =
        RefCell::new(std::collections::HashSet::with_capacity(4096));
}

/// Best-effort current RSS in MB. Returns `None` on platforms without
/// `/proc/self/statm` (i.e. anything but Linux). Two values come back:
///   * `rss_mb` — resident set size (what the kernel actually has us
///     using right now).
///   * `vsz_mb` — virtual size (allocator + mapped files; useful to
///     spot huge un-touched HashMap capacity).
fn rss_mb() -> Option<(f64, f64)> {
    let s = std::fs::read_to_string("/proc/self/statm").ok()?;
    let mut it = s.split_whitespace();
    let vsz_pages: u64 = it.next()?.parse().ok()?;
    let rss_pages: u64 = it.next()?.parse().ok()?;
    let page_kb = 4.0; // Linux default; close enough for the log line.
    Some((
        (rss_pages as f64) * page_kb / 1024.0,
        (vsz_pages as f64) * page_kb / 1024.0,
    ))
}

/// Print one `[mem]` line with the current RSS at this point in prep.
/// Cheap enough to call at every major step; `/proc/self/statm` is a
/// single 5-byte read.
fn log_mem(label: &str) {
    if let Some((rss, vsz)) = rss_mb() {
        eprintln!("[mem] {label}: RSS={rss:.0} MB, VSZ={vsz:.0} MB");
    }
}

/// Per-account state needed to score (test ∪ negatives) under any priors.
///
/// `test_features` / `neg_features` are the hot scoring input — tag IDs
/// and df values are pre-resolved so the grid loop avoids HashMap
/// lookups, and they carry everything the cached channel variants in
/// `ScoringContext` read (score, fav_count, rating, media_type, …).
/// `diversity_features` is the parallel MMR input, stored concatenated
/// `[test ‖ neg]`. `user_relation` is the per-account tag-relation
/// graph built from train_posts; it gives the personal `tag_relation`
/// channel real signal under the synthetic split (otherwise `*_user_*`
/// knobs and `tag_relation_w_personal` are gradient-dead). The original
/// `Post` structs are dropped at the end of hydration.
pub(crate) struct AccountFixture {
    pub(crate) profile: AccountPreferenceProfile,
    pub(crate) tags: Vec<TagCount>,
    pub(crate) test_features: Vec<CachedPostFeatures>,
    pub(crate) neg_features: Vec<CachedPostFeatures>,
    pub(crate) diversity_features: Vec<DiversityFeatures>,
    pub(crate) user_relation: TagRelationGraph,
    pub(crate) test_count: usize,
}

pub(crate) struct EvalDataset {
    pub(crate) idf: IdfIndex,
    pub(crate) global_relation: TagRelationGraph,
    pub(crate) accounts: Vec<AccountFixture>,
}

/// Catalog metadata loaded once. Parallel vectors `ids` / `fav_counts` /
/// `created_at_epoch` (sorted by id), plus `by_fav` / `by_age` index lists
/// for popularity- and time-matched negative sampling.
pub(crate) struct CatalogIndex {
    pub(crate) ids: Vec<i64>,
    pub(crate) fav_counts: Vec<i32>,
    pub(crate) created_at_epoch: Vec<i64>,
    pub(crate) by_fav: Vec<u32>,
    pub(crate) by_age: Vec<u32>,
}

pub(crate) fn load_catalog_index() -> anyhow::Result<CatalogIndex> {
    let conn = db::open_db_for_calibration().map_err(|e| anyhow::anyhow!(e))?;
    let mut stmt = conn.prepare(
        "SELECT id, COALESCE(fav_count, 0), COALESCE(created_at, '')
         FROM posts WHERE is_deleted = 0 ORDER BY id",
    )?;
    let rows = stmt.query_map([], |r| {
        let id: i64 = r.get(0)?;
        let fav: i64 = r.get(1)?;
        let created_at: String = r.get(2)?;
        Ok((id, fav as i32, created_at))
    })?;

    let mut ids = Vec::new();
    let mut fav_counts: Vec<i32> = Vec::new();
    let mut created_at_epoch: Vec<i64> = Vec::new();
    for r in rows {
        let (id, fav, ca_str) = r?;
        ids.push(id);
        fav_counts.push(fav);
        let epoch = DateTime::parse_from_rfc3339(&ca_str)
            .map(|dt| dt.timestamp())
            .unwrap_or(0);
        created_at_epoch.push(epoch);
    }

    let mut by_fav: Vec<u32> = (0..ids.len() as u32).collect();
    by_fav.sort_unstable_by_key(|&i| fav_counts[i as usize]);
    let mut by_age: Vec<u32> = (0..ids.len() as u32).collect();
    by_age.sort_unstable_by_key(|&i| created_at_epoch[i as usize]);

    Ok(CatalogIndex {
        ids,
        fav_counts,
        created_at_epoch,
        by_fav,
        by_age,
    })
}

pub(crate) fn prepare_eval_dataset(opts: &GridOptions) -> anyhow::Result<EvalDataset> {
    let cfg_arc = cfg();
    let bt = &cfg_arc.backtest;
    let min_favs = bt.min_favs;
    let test_fraction = bt.test_fraction;
    let negative_ratio = bt.negative_ratio;
    let max_accounts = bt.max_accounts;

    eprintln!(
        "[prep] split={}, neg={}, diversify={}, max_accounts={}, min_favs={}, neg_ratio={}",
        opts.split.label(),
        opts.neg_mode.label(),
        opts.diversify,
        max_accounts,
        min_favs,
        negative_ratio,
    );

    log_mem("prep start");

    let t = std::time::Instant::now();
    eprintln!("[prep] loading IDF index...");
    let idf = IdfIndex::from_db().map_err(|e| anyhow::anyhow!("idf load: {e}"))?;
    eprintln!("[prep]   IDF loaded in {:.1}s", t.elapsed().as_secs_f32());
    log_mem("after IDF load");

    let t = std::time::Instant::now();
    eprintln!("[prep] loading global tag-relation graph...");
    let mut global_relation =
        db::load_global_tag_relation().map_err(|e| anyhow::anyhow!("global relation: {e}"))?;
    eprintln!(
        "[prep]   global relation loaded in {:.1}s ({} tags, {} pairs)",
        t.elapsed().as_secs_f32(),
        global_relation.n_tags(),
        global_relation.n_pairs(),
    );
    log_mem("after global graph load");
    let t = std::time::Instant::now();
    let catalog: CatalogIndex = match opts.neg_mode {
        NegMode::Mixed | NegMode::Hybrid { .. } => {
            eprintln!("[prep] loading catalog index (id + fav_count + created_at)...");
            let c = load_catalog_index()?;
            eprintln!(
                "[prep]   catalog index built in {:.1}s ({} posts)",
                t.elapsed().as_secs_f32(),
                c.ids.len()
            );
            c
        }
        NegMode::Uniform => {
            eprintln!("[prep] loading catalog ids (uniform-neg mode)...");
            let ids = catalog_post_ids()?;
            eprintln!(
                "[prep]   catalog ids loaded in {:.1}s ({} posts)",
                t.elapsed().as_secs_f32(),
                ids.len()
            );
            let n = ids.len();
            CatalogIndex {
                ids,
                fav_counts: vec![0; n],
                created_at_epoch: vec![0; n],
                by_fav: Vec::new(),
                by_age: Vec::new(),
            }
        }
    };

    let account_ids = eligible_accounts(min_favs as i64, max_accounts)?;
    eprintln!(
        "[prep] {} eligible accounts (top by fav count, after min_favs={} filter)",
        account_ids.len(),
        min_favs
    );
    let total_accounts = account_ids.len();
    let report_every = (total_accounts / 20).max(5);
    let counter = AtomicUsize::new(0);
    let posts_counter = AtomicUsize::new(0);
    let t_start = std::time::Instant::now();

    let fixtures: Vec<AccountFixture> = account_ids
        .into_par_iter()
        .filter_map(|account_id| {
            let (train_ids, test_ids) = if matches!(opts.split, SplitStrategy::TimeCausal) {
                let favs = match account_favorite_ids_with_ts(account_id) {
                    Ok(f) => f,
                    Err(e) => {
                        eprintln!("fav query (ts) for {account_id} failed: {e}");
                        return None;
                    }
                };
                if favs.len() < min_favs {
                    return None;
                }
                split_train_test_time(&favs, test_fraction)
            } else {
                let fav_ids = match account_favorite_ids(account_id) {
                    Ok(f) => f,
                    Err(e) => {
                        eprintln!("fav query for {account_id} failed: {e}");
                        return None;
                    }
                };
                if fav_ids.len() < min_favs {
                    return None;
                }
                split_train_test(account_id, &fav_ids, test_fraction, opts.split)
            };
            if test_ids.is_empty() {
                return None;
            }

            let train_posts = match db::hydrate_posts_by_ids(&train_ids) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("hydrate train for {account_id} failed: {e}");
                    return None;
                }
            };
            let test_posts = match db::hydrate_posts_by_ids(&test_ids) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("hydrate test for {account_id} failed: {e}");
                    return None;
                }
            };
            if train_posts.len() < min_favs / 2 || test_posts.is_empty() {
                return None;
            }

            let profile = build_profile(&train_posts);
            let tags = build_tag_counts(&train_posts);

            let target_negs = test_posts.len() * negative_ratio;
            // Negatives must avoid both train and test (the user already favourited them).
            let mut excluded_ids: Vec<i64> = Vec::with_capacity(train_ids.len() + test_ids.len());
            excluded_ids.extend_from_slice(&train_ids);
            excluded_ids.extend_from_slice(&test_ids);
            // Thread-local scratch buffer reuse (Point 5): avoids per-account HashSet alloc.
            let neg_ids = HYDRATION_SCRATCH.with(|cell| {
                let mut scratch = cell.borrow_mut();
                scratch.clear();
                match opts.neg_mode {
                    NegMode::Uniform => {
                        sample_negatives_uniform(&catalog.ids, &excluded_ids, target_negs, &mut scratch)
                    }
                    NegMode::Mixed => sample_negatives_mixed(
                        &catalog,
                        &excluded_ids,
                        &test_posts,
                        target_negs,
                        account_id,
                        &mut scratch,
                    ),
                    NegMode::Hybrid { hard_ratio } => {
                        let n_mixed = (target_negs as f32 * (1.0 - hard_ratio)).round() as usize;
                        let n_hard = target_negs.saturating_sub(n_mixed);

                        // 70% mixed (existing mixed-strategy negatives).
                        let mut negs = if n_mixed > 0 {
                            sample_negatives_mixed(
                                &catalog,
                                &excluded_ids,
                                &test_posts,
                                n_mixed,
                                account_id,
                                &mut scratch,
                            )
                        } else {
                            Vec::new()
                        };

                        // 30% tag-similarity-based hard negatives.
                        if n_hard > 0 {
                            let pool_mult = 5;
                            let pool_size = n_hard.saturating_mul(pool_mult);
                            let pool_ids =
                                sample_hard_pool(&catalog.ids, pool_size, account_id, &mut scratch);

                            if !pool_ids.is_empty() {
                                let pool_posts =
                                    db::hydrate_posts_by_ids(&pool_ids).unwrap_or_default();

                                if !pool_posts.is_empty() {
                                    // Scoring context uses config priors ("контекст бери из
                                    // конфига"). tag_similarity() only reads priors, IDF, and
                                    // the profile's tag vectors — it never touches global_relation
                                    // or user_relation, so we safely pass empty graphs here.
                                    let empty_graph = TagRelationGraph::empty();
                                    let config_priors = cfg().priors.clone();
                                    let ctx = ScoringContext::new(
                                        &tags,
                                        &config_priors,
                                        &idf,
                                        &profile,
                                        &empty_graph,
                                        &empty_graph,
                                    );

                                    let mut scored: Vec<(i64, f32)> = pool_posts
                                        .iter()
                                        .map(|p| (p.id, ctx.tag_similarity(p)))
                                        .collect();
                                    scored.sort_by(|a, b| {
                                        b.1.partial_cmp(&a.1)
                                            .unwrap_or(std::cmp::Ordering::Equal)
                                    });

                                    let take = n_hard.min(scored.len());
                                    for i in 0..take {
                                        let id = scored[i].0;
                                        scratch.insert(id);
                                        negs.push(id);
                                    }
                                }
                            }
                        }

                        negs
                    }
                }
            });
            let mut candidate_ids = neg_ids;
            // Safety filter: sampling functions should already exclude train+test,
            // but this catches any edge-case leaks (e.g. when target exceeds catalog size).
            candidate_ids.retain(|id| !excluded_ids.contains(id));
            let neg_posts = db::hydrate_posts_by_ids(&candidate_ids).unwrap_or_default();
            let test_count = test_posts.len();

            posts_counter.fetch_add(test_posts.len() + neg_posts.len(), Ordering::Relaxed);

            // Per-account user tag-relation graph (cooccurrence on train_posts).
            // Built before features so each cached tag carries a `user_tid`.
            let mut user_relation = TagRelationGraph::from_train_posts(&train_posts);
            // Drop the heavy train-side `Post` structs early.
            drop(train_posts);

            // Pre-resolve post tags once so the grid scoring loop reads
            // (group, lc, df_raw, global_tid, user_tid) directly without
            // HashMap lookups against IDF / relation graphs per probe.
            let test_features: Vec<CachedPostFeatures> = test_posts
                .iter()
                .map(|p| {
                    CachedPostFeatures::from_post_with_user(p, &idf, &global_relation, Some(&user_relation))
                })
                .collect();
            let neg_features: Vec<CachedPostFeatures> = neg_posts
                .iter()
                .map(|p| {
                    CachedPostFeatures::from_post_with_user(p, &idf, &global_relation, Some(&user_relation))
                })
                .collect();
            // Compact pair-storage HashMap → sorted Vec, drop tag_to_id,
            // prune singleton (cooc=1) pairs and pairs neither endpoint of
            // which appears in (test ∪ neg).
            let mut queryable: std::collections::HashSet<
                e621_account_parser_api::utils::TagId,
            > = std::collections::HashSet::new();
            for cf in test_features.iter().chain(neg_features.iter()) {
                for ct in &cf.tags {
                    if let Some(tid) = ct.user_tid {
                        queryable.insert(tid);
                    }
                }
            }
            user_relation.freeze_with_query_set(&queryable, 2);
            // MMR features only when actually needed.
            let diversity_features: Vec<DiversityFeatures> = if opts.diversify {
                let mut v = Vec::with_capacity(test_posts.len() + neg_posts.len());
                v.extend(test_posts.iter().map(|p| DiversityFeatures::from_post(p, &global_relation)));
                v.extend(neg_posts.iter().map(|p| DiversityFeatures::from_post(p, &global_relation)));
                v
            } else {
                Vec::new()
            };
            // Drop the heavy `Post` structs now that features are extracted.
            drop(test_posts);
            drop(neg_posts);

            // Progress heartbeat (thread-safe atomic counter, stderr output may
            // interleave across threads — acceptable for progress logging).
            let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
            if n.is_multiple_of(report_every) || n == total_accounts {
                eprintln!(
                    "[prep]   hydrated {n}/{total_accounts} accounts ({} posts cached so far, {:.1}s elapsed)",
                    posts_counter.load(Ordering::Relaxed),
                    t_start.elapsed().as_secs_f32()
                );
                log_mem(&format!("after {n} accounts"));
            }

            Some(AccountFixture {
                profile,
                tags,
                test_features,
                neg_features,
                diversity_features,
                user_relation,
                test_count,
            })
        })
        .collect();

    eprintln!(
        "[prep] DONE: {} accounts hydrated, {} posts cached in memory, {:.1}s total",
        fixtures.len(),
        posts_counter.load(Ordering::Relaxed),
        t_start.elapsed().as_secs_f32()
    );
    log_mem("after all fixtures hydrated");

    // Freeze the global tag-relation graph. After this, only id-keyed
    // queries (`cooc_by_id` / `marginal_by_id`) are made against it
    // (the cached scoring path); `tag_id(g, &str)` is no longer needed
    // because every fixture's CachedPostFeatures already carries its
    // pre-resolved `global_tid`. The queryable set is the union of
    // every (test ∪ neg) tag's `global_tid` across all fixtures —
    // pairs unreachable from that set are dead weight.
    let t_freeze = std::time::Instant::now();
    let pairs_before = global_relation.n_pairs();
    let mut global_queryable: std::collections::HashSet<
        e621_account_parser_api::utils::TagId,
    > = std::collections::HashSet::new();
    for fx in &fixtures {
        for cf in fx.test_features.iter().chain(fx.neg_features.iter()) {
            for ct in &cf.tags {
                if let Some(tid) = ct.global_tid {
                    global_queryable.insert(tid);
                }
            }
        }
    }
    global_relation.freeze_with_query_set(&global_queryable, 2);
    eprintln!(
        "[prep] global graph frozen: {} → {} pairs ({:.1}s, queryable tids={})",
        pairs_before,
        global_relation.n_pairs(),
        t_freeze.elapsed().as_secs_f32(),
        global_queryable.len()
    );
    drop(global_queryable);
    log_mem("after global graph freeze");

    Ok(EvalDataset {
        idf,
        global_relation,
        accounts: fixtures,
    })
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
        // Synthetic split has no interaction history.
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
        uploaders: Vec::new(),
        last_refreshed_at: None,
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

/// Same as `account_favorite_ids` but returns `(post_id, created_at_epoch)`
/// tuples — needed by `SplitStrategy::TimeCausal` to sort favourites
/// chronologically rather than by post-id.
fn account_favorite_ids_with_ts(account_id: i32) -> anyhow::Result<Vec<(i64, i64)>> {
    let conn = db::open_db_for_calibration().map_err(|e| anyhow::anyhow!(e))?;
    let mut stmt = conn.prepare(
        "SELECT ap.post_id, COALESCE(p.created_at, '')
         FROM accounts_post ap
         JOIN posts p ON p.id = ap.post_id
         WHERE ap.account_id = ?1",
    )?;
    let rows = stmt.query_map(params![account_id], |r| {
        let id: i64 = r.get(0)?;
        let ca: String = r.get(1)?;
        Ok((id, ca))
    })?;
    let mut out: Vec<(i64, i64)> = Vec::new();
    for r in rows {
        let (id, ca) = r?;
        let ts = DateTime::parse_from_rfc3339(&ca)
            .map(|dt| dt.timestamp())
            .unwrap_or(0);
        out.push((id, ts));
    }
    Ok(out)
}
