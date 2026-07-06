//! Train/test split strategies and negative sampling.

use std::collections::HashSet;

use e621_account_parser_api::models::Post;

use crate::dataset::CatalogIndex;
use crate::options::SplitStrategy;
use crate::{NEG_SAMPLE_SEED, SPLIT_SEED};

pub(crate) fn split_train_test(
    account_id: i32,
    ids: &[i64],
    test_frac: f32,
    strategy: SplitStrategy,
) -> (Vec<i64>, Vec<i64>) {
    let mut sorted: Vec<i64> = ids.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    let n_test = ((n as f32) * test_frac).round() as usize;
    let n_test = n_test.min(n.saturating_sub(1)).max(1.min(n));

    match strategy {
        SplitStrategy::PostId => {
            let split_at = n.saturating_sub(n_test).max(1);
            let train = sorted[..split_at].to_vec();
            let test = sorted[split_at..].to_vec();
            (train, test)
        }
        SplitStrategy::Random => {
            // Deterministic per-(account, fav-set) Fisher-Yates via xorshift64.
            let mut indices: Vec<usize> = (0..n).collect();
            let mut rng = SPLIT_SEED ^ (account_id as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            for i in (1..n).rev() {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                let j = (rng as usize) % (i + 1);
                indices.swap(i, j);
            }
            let test_idx: HashSet<usize> = indices.iter().take(n_test).copied().collect();
            let mut train = Vec::with_capacity(n - n_test);
            let mut test = Vec::with_capacity(n_test);
            for (i, id) in sorted.iter().enumerate() {
                if test_idx.contains(&i) {
                    test.push(*id);
                } else {
                    train.push(*id);
                }
            }
            (train, test)
        }
        SplitStrategy::TimeCausal => {
            // Caller must use `split_train_test_time` for this variant —
            // we only have post ids here, no created_at to sort by.
            panic!("SplitStrategy::TimeCausal requires split_train_test_time; bug in dispatch");
        }
    }
}

/// TimeCausal split: take `(post_id, created_at_epoch)` tuples, sort by
/// timestamp ascending, and hold the newest `test_frac` as test. Mirrors
/// the production "predict next favourite" task more honestly than the
/// post-id-based split when post ids aren't strictly monotonic with time.
pub(crate) fn split_train_test_time(
    favs: &[(i64, i64)],
    test_frac: f32,
) -> (Vec<i64>, Vec<i64>) {
    let mut sorted: Vec<(i64, i64)> = favs.to_vec();
    sorted.sort_by_key(|(id, ts)| (*ts, *id));
    let n = sorted.len();
    let n_test = ((n as f32) * test_frac).round() as usize;
    let n_test = n_test.min(n.saturating_sub(1)).max(1.min(n));
    let split_at = n.saturating_sub(n_test).max(1);
    let train = sorted[..split_at].iter().map(|(id, _)| *id).collect();
    let test = sorted[split_at..].iter().map(|(id, _)| *id).collect();
    (train, test)
}

/// Uniform random sampling without replacement (legacy).
/// `scratch` is a reusable buffer for exclusion + chosen tracking.
pub(crate) fn sample_negatives_uniform(
    catalog: &[i64],
    excluded: &[i64],
    target: usize,
    scratch: &mut HashSet<i64>,
) -> Vec<i64> {
    scratch.clear();
    scratch.extend(excluded.iter().copied());
    if catalog.is_empty() {
        return Vec::new();
    }
    let mut rng = NEG_SAMPLE_SEED;
    let n = catalog.len();
    let mut out: Vec<i64> = Vec::with_capacity(target);
    // Reserve enough capacity so insert never rehashes mid-loop.
    scratch.reserve(target.saturating_sub(scratch.len()));
    let mut tries = 0usize;
    let max_tries = target.saturating_mul(10).max(100);
    while out.len() < target && tries < max_tries {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let idx = (rng as usize) % n;
        let id = catalog[idx];
        // `insert` returns true only when the ID wasn't already in the set
        // (neither excluded nor previously chosen).
        if scratch.insert(id) {
            out.push(id);
        }
        tries += 1;
    }
    out
}

/// Mixed hard-negatives: 40% uniform + 30% popularity-decile-matched +
/// 30% age-window-matched per held-out positive. Deterministic via
/// `NEG_SAMPLE_SEED ^ account_id`.
/// `scratch` is a reusable buffer for exclusion + chosen tracking.
pub(crate) fn sample_negatives_mixed(
    catalog: &CatalogIndex,
    excluded: &[i64],
    test_posts: &[Post],
    target: usize,
    account_id: i32,
    scratch: &mut HashSet<i64>,
) -> Vec<i64> {
    scratch.clear();
    scratch.extend(excluded.iter().copied());
    scratch.reserve(target);
    let n = catalog.ids.len();
    if n == 0 || test_posts.is_empty() {
        return Vec::new();
    }

    let target_uniform = (target as f32 * 0.40).round() as usize;
    let target_pop = (target as f32 * 0.30).round() as usize;
    let target_time = target.saturating_sub(target_uniform + target_pop);

    let mut rng: u64 = NEG_SAMPLE_SEED ^ (account_id as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut out: Vec<i64> = Vec::with_capacity(target);

    let next = |rng: &mut u64| -> usize {
        *rng ^= *rng << 13;
        *rng ^= *rng >> 7;
        *rng ^= *rng << 17;
        *rng as usize
    };

    // Uniform.
    {
        let mut tries = 0;
        let max_tries = target_uniform.saturating_mul(5).max(100);
        while out.len() < target_uniform && tries < max_tries {
            let idx = next(&mut rng) % n;
            let id = catalog.ids[idx];
            if scratch.insert(id) {
                out.push(id);
            }
            tries += 1;
        }
    }

    // Popularity-decile-matched.
    if !catalog.by_fav.is_empty() {
        let target_total = out.len() + target_pop;
        let mut tries = 0;
        let max_tries = target_pop.saturating_mul(5).max(100);
        while out.len() < target_total && tries < max_tries {
            let pivot: i32 =
                test_posts[next(&mut rng) % test_posts.len()].stats.fav_count.max(0) as i32;
            // 70%-150% of pivot fav_count (asymmetric: matched-or-slightly-higher).
            let lo = ((pivot as f32) * 0.7) as i32;
            let hi = ((pivot as f32) * 1.5).max((pivot + 50) as f32) as i32;
            let lo_idx = catalog
                .by_fav
                .partition_point(|&i| catalog.fav_counts[i as usize] < lo);
            let hi_idx = catalog
                .by_fav
                .partition_point(|&i| catalog.fav_counts[i as usize] <= hi);
            if hi_idx > lo_idx {
                let pick = catalog.by_fav[lo_idx + (next(&mut rng) % (hi_idx - lo_idx))] as usize;
                let id = catalog.ids[pick];
                if scratch.insert(id) {
                    out.push(id);
                }
            }
            tries += 1;
        }
    }

    // Age-window-matched (±30 days).
    if !catalog.by_age.is_empty() {
        const WINDOW_SECS: i64 = 30 * 24 * 3600;
        let target_total = out.len() + target_time;
        let mut tries = 0;
        let max_tries = target_time.saturating_mul(5).max(100);
        while out.len() < target_total && tries < max_tries {
            let pivot_post = &test_posts[next(&mut rng) % test_posts.len()];
            let pivot_epoch = pivot_post.created_at.timestamp();
            let lo = pivot_epoch - WINDOW_SECS;
            let hi = pivot_epoch + WINDOW_SECS;
            let lo_idx = catalog
                .by_age
                .partition_point(|&i| catalog.created_at_epoch[i as usize] < lo);
            let hi_idx = catalog
                .by_age
                .partition_point(|&i| catalog.created_at_epoch[i as usize] <= hi);
            if hi_idx > lo_idx {
                let pick = catalog.by_age[lo_idx + (next(&mut rng) % (hi_idx - lo_idx))] as usize;
                let id = catalog.ids[pick];
                if scratch.insert(id) {
                    out.push(id);
                }
            }
            tries += 1;
        }
    }

    // Top-up uniform if matching passes came up short.
    if out.len() < target {
        let max_tries = (target - out.len()).saturating_mul(5).max(50);
        let mut tries = 0;
        while out.len() < target && tries < max_tries {
            let idx = next(&mut rng) % n;
            let id = catalog.ids[idx];
            if scratch.insert(id) {
                out.push(id);
            }
            tries += 1;
        }
    }

    out
}

/// Sample a pool of candidate IDs for hard-negative mining.
/// Avoids IDs already in `scratch` (which should contain excluded train/test
/// IDs and any already-chosen negatives). Adds pool IDs to `scratch` to
/// prevent overlap with subsequent sampling passes.
/// Returns up to `pool_size` unique IDs.
pub(crate) fn sample_hard_pool(
    catalog: &[i64],
    pool_size: usize,
    account_id: i32,
    scratch: &mut HashSet<i64>,
) -> Vec<i64> {
    if catalog.is_empty() || pool_size == 0 {
        return Vec::new();
    }
    let mut rng: u64 = NEG_SAMPLE_SEED ^ (account_id as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let n = catalog.len();
    let mut out: Vec<i64> = Vec::with_capacity(pool_size);
    scratch.reserve(pool_size.saturating_sub(scratch.len()));
    let mut tries = 0usize;
    let max_tries = pool_size.saturating_mul(10).max(100);
    while out.len() < pool_size && tries < max_tries {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let idx = (rng as usize) % n;
        let id = catalog[idx];
        if scratch.insert(id) {
            out.push(id);
        }
        tries += 1;
    }
    out
}
