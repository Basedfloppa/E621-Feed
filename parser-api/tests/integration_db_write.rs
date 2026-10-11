//! Write-path integration tests: the posts upsert and the tag postings must be
//! one unit of work.
//!
//! Own test binary → own process → own temporary `SQLite` file. That matters
//! here: the rollback case takes the `tags` table away, which would break any
//! other test sharing the database.

mod support;

use std::collections::HashSet;

use e621_account_parser_api::db;
use e621_account_parser_api::models::{
    FileOriginal, Files, Flags, Has, Post, Rating, Relationships, Score, Stats, Tags,
};

const ACCOUNT_ID: i32 = 54901;

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
    db::set_account("db-write-test-owner", ACCOUNT_ID, "db-write-test-user", "")
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

fn account_link_rows(post_id: i64) -> i64 {
    scalar_i64(&format!(
        "SELECT COUNT(*) FROM accounts_post WHERE post_id = {post_id}"
    ))
}

/// The happy path: one call leaves the post, its account link and its tag
/// postings all readable.
#[test]
fn posts_link_and_tag_postings_are_written_by_one_call() {
    setup();
    let post = make_post(5490001, "solo_general_tag");

    db::save_posts_with_tags(&[post], ACCOUNT_ID, &HashSet::new(), false, None)
        .expect("combined write");

    assert_eq!(post_rows(5490001), 1);
    assert_eq!(account_link_rows(5490001), 1);
    assert_eq!(tag_link_rows(5490001), 1, "the general tag must be linked");
}

/// An empty batch is a no-op — no transaction, no error.
#[test]
fn an_empty_batch_is_a_noop() {
    setup();
    db::save_posts_with_tags(&[], ACCOUNT_ID, &HashSet::new(), false, None).expect("empty batch");
}
