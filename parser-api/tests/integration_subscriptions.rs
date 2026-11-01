//! The watchlist must report what arrived *after* the subscription was
//! created, and "seen" must be a one-way mark.
//!
//! Both are invisible without a catalog: seeding the mark at the newest post
//! that already carries the tag is the difference between "one new post" and
//! "the whole tag's history is new", and a mark that can go backwards replays
//! the same posts on every visit.

mod support;

use std::collections::HashSet;

use e621_account_parser_api::db;
use e621_account_parser_api::models::{
    FileOriginal, Files, Flags, Has, Post, Rating, Relationships, Score, Stats, Tags,
};

const ACCOUNT_A: i32 = 424242;
const ACCOUNT_B: i32 = 424243;
const ACCOUNT_C: i32 = 424244;
const ACCOUNT_D: i32 = 424245;

fn make_post(id: i64, tag: &str) -> Post {
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
        flags: Flags::default(),
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

fn seed_account(account_id: i32) {
    support::install_isolated_db_config();
    db::ensure_sqlite().expect("migrations");
    let conn = db::open_db_for_calibration().expect("open db");
    conn.execute(
        "INSERT OR IGNORE INTO accounts (id, name) VALUES (?1, 'watchlist')",
        [account_id],
    )
    .expect("insert account");
}

fn save(account_id: i32, posts: &[Post]) {
    db::save_posts_with_tags(posts, account_id, &HashSet::new(), false, None).expect("save posts");
}

#[test]
fn a_new_subscription_reports_only_posts_that_arrive_later() {
    seed_account(ACCOUNT_A);
    // Two posts already carry the tag: subscribing now must not count them.
    save(ACCOUNT_A, &[make_post(100, "wolf"), make_post(101, "wolf")]);

    let sub = db::add_subscription(ACCOUNT_A, "Wolf").expect("subscribe");
    assert_eq!(sub.tag_name, "wolf", "the name is normalised");
    assert_eq!(sub.group_type, "general");
    assert_eq!(
        sub.last_seen_post_id, 101,
        "the mark starts at the newest post"
    );
    assert_eq!(sub.new_count, 0);

    save(ACCOUNT_A, &[make_post(102, "wolf"), make_post(103, "fox")]);

    let list = db::list_subscriptions(ACCOUNT_A).expect("list");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].new_count, 1, "only the post above the mark is new");

    let new = db::new_subscription_posts(ACCOUNT_A, 10).expect("new posts");
    assert_eq!(
        new.iter().map(|p| p.post.id).collect::<Vec<_>>(),
        vec![102],
        "the fox post is not on the watchlist"
    );

    db::mark_seen(ACCOUNT_A, None).expect("mark seen");
    assert_eq!(
        db::list_subscriptions(ACCOUNT_A).expect("list")[0].new_count,
        0
    );
    assert!(
        db::new_subscription_posts(ACCOUNT_A, 10)
            .expect("new")
            .is_empty()
    );
}

#[test]
fn subscribing_twice_keeps_the_existing_mark() {
    seed_account(ACCOUNT_B);
    save(ACCOUNT_B, &[make_post(200, "otter")]);
    let first = db::add_subscription(ACCOUNT_B, "otter").expect("first");
    save(ACCOUNT_B, &[make_post(201, "otter")]);
    let second = db::add_subscription(ACCOUNT_B, "otter").expect("second");

    assert_eq!(first.last_seen_post_id, 200);
    assert_eq!(
        second.last_seen_post_id, 200,
        "re-adding a tag must not replay its history"
    );
    assert_eq!(second.new_count, 1);
}

#[test]
fn an_unknown_tag_is_rejected_with_the_reason() {
    seed_account(ACCOUNT_C);
    let err = db::add_subscription(ACCOUNT_C, "definitely_not_a_tag").expect_err("unknown tag");
    assert!(
        err.contains("not in the local catalog"),
        "error should say why: {err}"
    );
}

#[test]
fn removing_one_subscription_keeps_the_others() {
    seed_account(ACCOUNT_D);
    save(ACCOUNT_D, &[make_post(300, "deer"), make_post(301, "bear")]);
    let deer = db::add_subscription(ACCOUNT_D, "deer").expect("deer");
    db::add_subscription(ACCOUNT_D, "bear").expect("bear");

    assert!(db::remove_subscription(ACCOUNT_D, deer.tag_id).expect("remove"));
    let list = db::list_subscriptions(ACCOUNT_D).expect("list");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].tag_name, "bear");
    assert!(
        !db::remove_subscription(ACCOUNT_D, deer.tag_id).expect("remove again"),
        "removing a missing subscription reports false, not an error"
    );
}
