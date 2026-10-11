//! Rollback case for the combined posts+tags write: if the tag half fails, the
//! posts half must not survive.
//!
//! A separate test binary on purpose — it renames the `tags` table away, and
//! test binaries in the same process share one temporary database.

mod support;

use std::collections::HashSet;

use e621_account_parser_api::db;
use e621_account_parser_api::models::{
    FileOriginal, Files, Flags, Has, Post, Rating, Relationships, Score, Stats, Tags,
};

const ACCOUNT_ID: i32 = 54902;

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
    db::set_account(
        "db-write-rollback-owner",
        ACCOUNT_ID,
        "db-write-rollback-user",
        "",
    )
    .expect("create test account");
}

fn scalar_i64(sql: &str) -> i64 {
    db::with_write_tx(|tx| {
        tx.query_row(sql, [], |r| r.get(0))
            .map_err(|e| e.to_string())
    })
    .expect("scalar query")
}

fn post_rows(post_id: i64) -> i64 {
    scalar_i64(&format!("SELECT COUNT(*) FROM posts WHERE id = {post_id}"))
}

fn tag_link_rows(post_id: i64) -> i64 {
    scalar_i64(&format!(
        "SELECT COUNT(*) FROM tags_posts WHERE post_id = {post_id}"
    ))
}

/// The reason the combined primitive exists: if the tag half fails, the posts
/// half must not survive. Two separate transactions would leave the post
/// committed and tagless.
///
/// The failure is injected by renaming `tags` away, so the tags half fails with
/// "no such table" *after* the posts half has already run inside the same
/// transaction. The control at the end proves the posts upsert itself is fine —
/// without it, a broken posts writer would pass this test by accident.
#[test]
fn a_failed_tag_half_rolls_the_post_half_back() {
    setup();

    db::with_write_tx(|tx| {
        tx.execute_batch("ALTER TABLE tags RENAME TO tags_unavailable;")
            .map_err(|e| e.to_string())
    })
    .expect("hide the tags table");

    let err = db::save_posts_with_tags(
        &[make_post(5490002, "solo_general_tag")],
        ACCOUNT_ID,
        &HashSet::new(),
        false,
        None,
    )
    .expect_err("the tag half must fail while `tags` is missing");
    assert!(
        err.contains("no such table: tags"),
        "expected the tags half to fail, got: {err}"
    );

    assert_eq!(
        post_rows(5490002),
        0,
        "the post row must roll back with the failed tag half"
    );

    // Control: `save_posts` alone owns its transaction, so it still commits.
    db::save_posts(&[make_post(5490003, "solo_general_tag")], ACCOUNT_ID)
        .expect("standalone posts upsert still works");
    assert_eq!(post_rows(5490003), 1);
    assert_eq!(
        tag_link_rows(5490003),
        0,
        "the control post is deliberately tagless"
    );
}
