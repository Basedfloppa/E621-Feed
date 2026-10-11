//! The in-memory follow-ups of a tag batch — the IDF bump and the
//! relation-graph dirty flag — are applied by the caller, after its
//! transaction has committed.
//!
//! Only the committed direction is covered here, deliberately: the other
//! direction needs a COMMIT that fails while the batch body has already run,
//! and nothing in the public DB API lets a test make that happen (the deferred
//! foreign-key pragma resets at every COMMIT, so it cannot be armed from
//! outside the batch's own transaction). What this file guards is that the
//! rename did not quietly drop the bump; the ordering itself rests on the
//! `with_write_tx(...)?` that every caller goes through before
//! `TagBatchEffects::apply`.

mod support;

use std::collections::HashSet;

use e621_account_parser_api::db;
use e621_account_parser_api::models::{
    FileOriginal, Files, Flags, Has, Post, Rating, Relationships, Score, Stats, Tags,
};
use e621_account_parser_api::utils;

const ACCOUNT_ID: i32 = 54911;

fn make_post(id: i64, general: &str) -> Post {
    Post {
        id,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        change_seq: 0.0,
        files: Files {
            original: FileOriginal {
                width: 0,
                height: 0,
                url: Some(format!("https://example.test/{id}.png")),
            },
            ..Default::default()
        },
        uploader_id: 42,
        uploader_name: None,
        approver_id: None,
        stats: Stats {
            score: Score {
                up: 1,
                down: 0,
                total: 1,
            },
            ..Default::default()
        },
        flags: Flags::default(),
        has: Has::default(),
        relationships: Relationships::default(),
        pools: vec![],
        rating: Rating::S,
        locked_tags: vec![],
        sources: vec![],
        description: None,
        tags: Tags {
            general: vec![general.into()],
            ..Default::default()
        },
    }
}

fn setup() {
    support::install_isolated_db_config();
    db::ensure_sqlite().expect("DB migrations failed");
    db::set_account("tag-effects-owner", ACCOUNT_ID, "tag-effects-user", "")
        .expect("create test account");
}

/// A batch that commits does reach the in-memory index. Without this, the
/// effects could be dropped in the refactor and nothing would notice.
#[test]
fn a_committed_batch_bumps_the_idf_index() {
    setup();
    let tag = "idf_effect_committed";
    db::save_posts(&[make_post(5491101, tag)], ACCOUNT_ID).expect("post row");

    assert_eq!(utils::current_idf().df_for(tag), 0, "fresh tag");

    db::save_posts_tags_batch(&[make_post(5491101, tag)], &HashSet::new(), false, None)
        .expect("committed batch");

    assert_eq!(
        utils::current_idf().df_for(tag),
        1,
        "a committed posting must raise the in-memory document frequency"
    );
}

/// The combined write reaches the same index — it carries the same effects.
#[test]
fn the_combined_write_bumps_the_idf_index_too() {
    setup();
    let tag = "idf_effect_combined";
    db::save_posts_with_tags(
        &[make_post(5491102, tag)],
        ACCOUNT_ID,
        &HashSet::new(),
        false,
        None,
    )
    .expect("combined write");

    assert_eq!(utils::current_idf().df_for(tag), 1);
}
