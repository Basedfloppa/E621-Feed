//! Synthetic `EvalDataset` builders for the calibrate unit tests.
//!
//! Test-only: `main.rs` declares this module under `#[cfg(test)]`, so it never
//! enters the shipped binary. Hydrating the real dataset needs PostgreSQL and
//! ~60s, but the tests that verify cache plumbing (the blend fit's re-scoring,
//! the cache-file round trip) only need the *shape* the scorer consumes — so
//! they build a handful of accounts in memory.
//!
//! The builders deliberately leave the profile empty and the tag graphs empty:
//! that keeps the channels reproducible (recency and popularity are the only
//! ones with real spread) and free of hidden global state.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use e621_account_parser_api::models::{AccountPreferenceProfile, Rating};
use e621_account_parser_api::utils::{CachedPostFeatures, CachedTag, IdfIndex, TagRelationGraph};

use crate::dataset::{AccountFixture, EvalDataset};

/// An empty IDF index. `n_posts` must be > 0; 1 is the smallest value the
/// index accepts and every lookup falls back to the floor.
pub(crate) fn empty_idf() -> IdfIndex {
    IdfIndex::from_df(&HashMap::new(), 1)
}

/// One candidate with an explicit `created_at` (recency) and `fav_count`
/// (popularity) so a test can control which channels carry spread.
pub(crate) fn post(
    id: i64,
    created_at: DateTime<Utc>,
    fav_count: i64,
    tags: &[(u8, &str)],
) -> CachedPostFeatures {
    CachedPostFeatures {
        id,
        uploader_id: 0,
        created_at,
        score_total: 0,
        score_up: 0,
        score_down: 0,
        fav_count,
        comment_count: 0,
        duration: 0.0,
        rating: Rating::S,
        media_type: "image",
        tags: tags
            .iter()
            .map(|&(group, name)| CachedTag {
                group,
                lc: name.into(),
                df_raw: 1,
                global_tid: None,
                user_tid: None,
            })
            .collect(),
    }
}

/// One account: `test` are positives (relevance 1), `neg` negatives.
/// `test_count` is set from `test.len()`, which is what production does
/// (`AccountFixture::test_count` and `test_features.len()` are 1:1).
pub(crate) fn account(
    test: Vec<CachedPostFeatures>,
    neg: Vec<CachedPostFeatures>,
) -> AccountFixture {
    let test_count = test.len();
    AccountFixture {
        profile: AccountPreferenceProfile::default(),
        tags: Vec::new(),
        test_features: test,
        neg_features: neg,
        diversity_features: Vec::new(),
        user_relation: TagRelationGraph::empty(),
        test_count,
    }
}

pub(crate) fn dataset(accounts: Vec<AccountFixture>) -> EvalDataset {
    EvalDataset {
        idf: empty_idf(),
        global_relation: TagRelationGraph::empty(),
        accounts,
    }
}
