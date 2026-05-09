//! Eval-dataset hydration: catalog index + per-account fixtures.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use rusqlite::params;

use e621_account_parser_api::db;
use e621_account_parser_api::models::{
    cfg, AccountMediaStat, AccountPreferenceProfile, AccountQualityProfile, AccountRatingStat,
    AccountRecencyProfile, Post, TagCount,
};
use e621_account_parser_api::utils::{
    CachedPostFeatures, DiversityFeatures, IdfIndex, TagRelationGraph,
};

use crate::options::{GridOptions, NegMode};
use crate::sampling::{sample_negatives_mixed, sample_negatives_uniform, split_train_test};

/// Per-account state needed to score (test ∪ negatives) under any priors.
///
/// `test_features` / `neg_features` are the hot scoring input; tag IDs
/// and df values are pre-resolved so the grid loop avoids HashMap
/// lookups. `diversity_features` is the parallel Jaccard-friendly tag
/// sets the MMR pass consumes, stored concatenated `[test ‖ neg]` so
/// per-probe diversify can index into one slice without rebuilding
/// HashSets. `test_posts` / `neg_posts` are kept only as the source
/// data for the feature builders above.
pub(crate) struct AccountFixture {
    pub(crate) profile: AccountPreferenceProfile,
    pub(crate) tags: Vec<TagCount>,
    pub(crate) test_posts: Vec<Post>,
    pub(crate) neg_posts: Vec<Post>,
    pub(crate) test_features: Vec<CachedPostFeatures>,
    pub(crate) neg_features: Vec<CachedPostFeatures>,
    pub(crate) diversity_features: Vec<DiversityFeatures>,
    pub(crate) test_count: usize,
}

pub(crate) struct EvalDataset {
    pub(crate) idf: IdfIndex,
    pub(crate) global_relation: TagRelationGraph,
    pub(crate) empty_user_relation: TagRelationGraph,
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

    let t = std::time::Instant::now();
    eprintln!("[prep] loading IDF index...");
    let idf = IdfIndex::from_db().map_err(|e| anyhow::anyhow!("idf load: {e}"))?;
    eprintln!("[prep]   IDF loaded in {:.1}s", t.elapsed().as_secs_f32());

    let t = std::time::Instant::now();
    eprintln!("[prep] loading global tag-relation graph...");
    let global_relation =
        db::load_global_tag_relation().map_err(|e| anyhow::anyhow!("global relation: {e}"))?;
    eprintln!(
        "[prep]   global relation loaded in {:.1}s ({} tags, {} pairs)",
        t.elapsed().as_secs_f32(),
        global_relation.n_tags(),
        global_relation.n_pairs(),
    );
    let empty_user_relation = TagRelationGraph::empty();

    let t = std::time::Instant::now();
    let catalog: CatalogIndex = match opts.neg_mode {
        NegMode::Mixed => {
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

    let t = std::time::Instant::now();
    let account_ids = eligible_accounts(min_favs as i64, max_accounts)?;
    eprintln!(
        "[prep] {} eligible accounts (top by fav count, after min_favs={} filter)",
        account_ids.len(),
        min_favs
    );
    let total_accounts = account_ids.len();
    let mut fixtures = Vec::with_capacity(account_ids.len());
    // Heartbeat cadence: report on percentiles + every N to surface stalls
    // on individual large accounts.
    let report_every = (total_accounts / 20).max(5);
    let mut posts_hydrated: usize = 0;

    for (i, account_id) in account_ids.into_iter().enumerate() {
        let fav_ids = account_favorite_ids(account_id)?;
        if fav_ids.len() < min_favs {
            continue;
        }
        let (train_ids, test_ids) =
            split_train_test(account_id, &fav_ids, test_fraction, opts.split);
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

        let test_id_set: std::collections::HashSet<i64> =
            test_posts.iter().map(|p| p.id).collect();
        let target_negs = test_posts.len() * negative_ratio;
        let neg_ids = match opts.neg_mode {
            NegMode::Uniform => sample_negatives_uniform(&catalog.ids, &fav_ids, target_negs),
            NegMode::Mixed => {
                sample_negatives_mixed(&catalog, &fav_ids, &test_posts, target_negs, account_id)
            }
        };
        let mut candidate_ids = neg_ids;
        candidate_ids.retain(|id| !test_id_set.contains(id));
        let neg_posts = db::hydrate_posts_by_ids(&candidate_ids).unwrap_or_default();
        let test_count = test_posts.len();

        posts_hydrated += test_posts.len() + neg_posts.len();

        // Pre-resolve post tags once so the grid scoring loop reads
        // (group, lc, df_raw, global_tid) directly without HashMap
        // lookups against IDF / global_relation per probe.
        let test_features: Vec<CachedPostFeatures> = test_posts
            .iter()
            .map(|p| CachedPostFeatures::from_post(p, &idf, &global_relation))
            .collect();
        let neg_features: Vec<CachedPostFeatures> = neg_posts
            .iter()
            .map(|p| CachedPostFeatures::from_post(p, &idf, &global_relation))
            .collect();
        let mut diversity_features: Vec<DiversityFeatures> =
            Vec::with_capacity(test_posts.len() + neg_posts.len());
        diversity_features.extend(test_posts.iter().map(DiversityFeatures::from_post));
        diversity_features.extend(neg_posts.iter().map(DiversityFeatures::from_post));

        fixtures.push(AccountFixture {
            profile,
            tags,
            test_posts,
            neg_posts,
            test_features,
            neg_features,
            diversity_features,
            test_count,
        });

        if (i + 1) % report_every == 0 || i + 1 == total_accounts {
            eprintln!(
                "[prep]   hydrated {}/{} accounts ({} posts cached so far, {:.1}s elapsed)",
                fixtures.len(),
                total_accounts,
                posts_hydrated,
                t.elapsed().as_secs_f32()
            );
        }
    }

    eprintln!(
        "[prep] DONE: {} accounts hydrated, {} posts cached in memory, {:.1}s total",
        fixtures.len(),
        posts_hydrated,
        t.elapsed().as_secs_f32()
    );

    Ok(EvalDataset {
        idf,
        global_relation,
        empty_user_relation,
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
