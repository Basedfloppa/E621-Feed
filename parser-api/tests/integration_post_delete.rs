//! Deleting posts must not take a user's library with it, and must keep the
//! tag document frequencies (and the in-memory IDF index) in step.
//!
//! Both are regressions that are invisible without a catalog: the cascade from
//! `posts` into `accounts_post` silently drops favourites, and a stale
//! `tags.df` skews every IDF-scored feed until a full rebuild happens to run.

mod support;

use std::collections::HashSet;

use e621_account_parser_api::db;
use e621_account_parser_api::models::{
    FileOriginal, Files, Flags, Has, Post, Rating, Relationships, Score, Stats, Tags,
};

const ACCOUNT_ID: i32 = 73111;

fn make_post(id: i64, tag: &str, deleted: bool) -> Post {
    Post {
        id,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        change_seq: 0.0,
        files: Files {
            original: FileOriginal {
                width: 10,
                height: 10,
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
        flags: Flags {
            deleted,
            ..Default::default()
        },
        has: Has::default(),
        relationships: Relationships::default(),
        pools: vec![],
        rating: Rating::S,
        locked_tags: vec![],
        sources: vec![],
        description: None,
        tags: Tags {
            general: vec![tag.into()],
            ..Default::default()
        },
    }
}

fn setup() {
    support::install_isolated_db_config();
    db::ensure_sqlite().expect("DB migrations failed");
    db::set_account("post-delete-owner", ACCOUNT_ID, "post-delete-user", "")
        .expect("create test account");
}

fn post_exists(id: i64) -> bool {
    let conn = db::open_db_for_calibration().expect("open db");
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM posts WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .expect("count post");
    count == 1
}

fn linked_to_account(id: i64) -> bool {
    let conn = db::open_db_for_calibration().expect("open db");
    conn.query_row(
        "SELECT COUNT(*) FROM accounts_post WHERE post_id = ?1 AND account_id = ?2",
        rusqlite::params![id, ACCOUNT_ID],
        |r| r.get::<_, i64>(0),
    )
    .expect("count link")
        == 1
}

fn stored_df(tag: &str) -> i64 {
    let conn = db::open_db_for_calibration().expect("open db");
    conn.query_row("SELECT df FROM tags WHERE name = ?1", [tag], |r| r.get(0))
        .expect("read df")
}

/// The media worker sees `is_deleted = 1` after e621 hides a post. Purging it
/// must not cascade into `accounts_post`: the row is the owner's library entry,
/// and the file is still on disk.
#[test]
fn purge_keeps_a_deleted_post_that_is_still_in_a_library() {
    setup();
    let favourited = make_post(7_311_001, "purge_favourited", true);
    let orphan = make_post(7_311_002, "purge_orphan", true);

    db::save_posts_with_tags(&[favourited], ACCOUNT_ID, &HashSet::new(), false, None)
        .expect("link deleted favourited post");
    db::upsert_catalog_posts_with_tags(&[orphan]).expect("catalog-only deleted post");

    let removed = db::purge_deleted_catalog_posts(500).expect("purge");

    assert_eq!(removed, 1, "only the unowned post is purgeable");
    assert!(post_exists(7_311_001), "favourited post must survive");
    assert!(linked_to_account(7_311_001), "library link must survive");
    assert!(!post_exists(7_311_002), "unowned deleted post is purged");
}

/// A hard delete (`delete_catalog_posts_by_ids`, the authoritative "gone"
/// signal) cascades into `tags_posts`. `tags.df` and the in-memory IDF index
/// have to follow in the same transaction, without waiting for a full rebuild.
#[test]
fn deleting_an_unowned_post_updates_df_and_the_in_memory_index() {
    setup();
    let id = 7_311_020;
    let tag = "df_after_delete";
    db::upsert_catalog_posts_with_tags(&[make_post(id, tag, false)]).expect("catalog post");
    assert_eq!(stored_df(tag), 1);
    assert_eq!(e621_account_parser_api::utils::current_idf().df_for(tag), 1);

    let removed = db::delete_catalog_posts_by_ids(&[id]).expect("delete");

    assert_eq!(removed, 1);
    assert!(!post_exists(id));
    assert_eq!(stored_df(tag), 0, "tags.df must be recomputed in the tx");
    assert_eq!(
        e621_account_parser_api::utils::current_idf().df_for(tag),
        0,
        "the in-memory index must follow the delete without a full rebuild"
    );
}
