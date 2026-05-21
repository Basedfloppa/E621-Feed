//! Integration tests for the e621-account-parser DB layer.
//!
//! These tests exercise real SQLite reads/writes against the shared
//! `database.db`, so they MUST use unique account IDs and clean up
//! after themselves. Run with `cargo test --test integration`.
//!
//! Requires the database to be initialized (run the server once, or
//! let the first test trigger `ensure_sqlite`).

use std::collections::HashSet;

use e621_account_parser_api::db;
use e621_account_parser_api::models::{
    Flags, Post, Rating, Relationships, Score, Tags,
};

/// Helper: build a minimal Post for testing.
fn make_post(id: i64, tags: Tags, uploader_id: i64) -> Post {
    Post {
        id,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        file: None,
        preview: None,
        sample: None,
        score: Score { up: 10, down: 0, total: 10 },
        tags,
        locked_tags: None,
        change_seq: 0.0,
        flags: Flags::default(),
        rating: Rating::S,
        fav_count: 5,
        sources: vec![],
        pools: vec![],
        relationships: Relationships {
            parent_id: None,
            has_children: false,
            has_active_children: false,
            children: vec![],
        },
        approver_id: None,
        uploader_id,
        description: None,
        comment_count: 0,
        is_favorited: false,
        has_notes: false,
        duration: None,
    }
}

fn make_tags(artist: &[&str], character: &[&str], general: &[&str]) -> Tags {
    Tags {
        artist: artist.iter().map(|s| s.to_string()).collect(),
        character: character.iter().map(|s| s.to_string()).collect(),
        copyright: vec![],
        species: vec![],
        general: general.iter().map(|s| s.to_string()).collect(),
        lore: vec![],
        meta: vec![],
        invalid: vec![],
        contributor: vec![],
    }
}

/// Verify that `save_posts_tags_batch` with `account_id = Some(...)` 
/// incrementally populates `account_tag_cooccurrence`, and that the
/// entries survive `drop_account_posts` + re-save correctly.
#[test]
fn incremental_cooccurrence_roundtrip() {
    let account_id = 90001; // high ID to avoid collision
    let blacklist = HashSet::new();

    // Clean slate
    setup_test(account_id);

    // ---- Save batch 1: two posts sharing some tags ----
    let posts1 = vec![
        make_post(11001, make_tags(&["skeb"], &["cat"], &["furry"]), 42),
        make_post(11002, make_tags(&["skeb"], &["dog"], &["furry", "commission"]), 42),
    ];
    db::save_posts(&posts1, account_id).unwrap();
    db::save_posts_tags_batch(&posts1, &blacklist, true, Some(account_id)).unwrap();

    // Check account_tag_cooccurrence exists
    let count = count_account_cooc(account_id);
    assert!(count > 0, "expected cooc rows after save, got {count}");

    // ---- Save batch 2: overlapping tags should increment counts ----
    let posts2 = vec![
        make_post(11003, make_tags(&["skeb"], &["cat"], &["furry"]), 42),
    ];
    db::save_posts(&posts2, account_id).unwrap();
    db::save_posts_tags_batch(&posts2, &blacklist, true, Some(account_id)).unwrap();

    let count2 = count_account_cooc(account_id);
    assert!(count2 >= count, "cooc should persist across batches ({count} → {count2})");

    // ---- Drop + re-save: verify cooccurrence is rebuilt correctly ----
    db::drop_account_posts(account_id).unwrap();
    let count_after_drop = count_account_cooc(account_id);
    assert_eq!(count_after_drop, 0, "cooc should be empty after drop");

    let posts3 = vec![
        make_post(11001, make_tags(&["skeb"], &["cat"], &["furry"]), 42),
    ];
    db::save_posts(&posts3, account_id).unwrap();
    db::save_posts_tags_batch(&posts3, &blacklist, true, Some(account_id)).unwrap();
    let count3 = count_account_cooc(account_id);
    assert!(count3 > 0, "cooc should be rebuilt after re-save");

    // Clean up
    db::drop_account_posts(account_id).unwrap();
}

/// Verify that `refresh_account_profiles_skip_cooc` does NOT rebuild
/// cooccurrence (it must already be built incrementally).
#[test]
fn profile_refresh_skip_cooc_leaves_cooc_intact() {
    let account_id = 90002;
    let blacklist = HashSet::new();

    setup_test(account_id);

    // Save posts + incremental cooc
    let posts = vec![
        make_post(11010, make_tags(&["skeb"], &["cat"], &["furry"]), 42),
    ];
    db::save_posts(&posts, account_id).unwrap();
    db::save_posts_tags_batch(&posts, &blacklist, true, Some(account_id)).unwrap();
    let cooc_before = count_account_cooc(account_id);

    // Refresh with skip_cooc
    db::refresh_account_profiles_skip_cooc(account_id).unwrap();

    // Cooc should still be present (not wiped by a full rebuild)
    let cooc_after = count_account_cooc(account_id);
    assert_eq!(cooc_before, cooc_after,
        "skip_cooc should not change cooc count: before={cooc_before} after={cooc_after}");

    db::drop_account_posts(account_id).unwrap();
}

/// Verify that the full `refresh_account_profiles` DOES rebuild
/// cooccurrence (regression check — the non-skip path still works).
#[test]
fn profile_refresh_full_rebuilds_cooc() {
    let account_id = 90003;
    let blacklist = HashSet::new();

    setup_test(account_id);

    // Save posts WITHOUT incremental cooc (track_cooccurrence=false)
    let posts = vec![
        make_post(11020, make_tags(&["skeb"], &["cat"], &["furry"]), 42),
    ];
    db::save_posts(&posts, account_id).unwrap();
    db::save_posts_tags_batch(&posts, &blacklist, false, None).unwrap();

    // Full refresh should build cooc from scratch
    db::refresh_account_profiles(account_id).unwrap();
    let cooc = count_account_cooc(account_id);
    assert!(cooc > 0, "full refresh should build cooc, got {cooc}");

    db::drop_account_posts(account_id).unwrap();
}

/// Test `save_posts_tags_batch` with `account_id=None` does not
/// write to `account_tag_cooccurrence` (catalog save path).
#[test]
fn catalog_save_does_not_write_account_cooc() {
    let account_id = 90004;
    let blacklist = HashSet::new();

    setup_test(account_id);

    let posts = vec![
        make_post(11030, make_tags(&["skeb"], &["cat"], &["furry"]), 42),
    ];
    db::save_posts(&posts, account_id).unwrap();
    // account_id=None simulates the catalog path (feed.rs / prefetch.rs)
    db::save_posts_tags_batch(&posts, &blacklist, false, None).unwrap();

    let cooc = count_account_cooc(account_id);
    assert_eq!(cooc, 0, "catalog save should not write account cooc");

    db::drop_account_posts(account_id).unwrap();
}

// ------------------------------------------------------------------
//  Helpers
// ------------------------------------------------------------------

fn count_account_cooc(account_id: i32) -> i64 {
    let conn = db::open_db_for_calibration().unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM account_tag_cooccurrence WHERE account_id = ?1",
        rusqlite::params![account_id],
        |r| r.get(0),
    )
    .unwrap()
}

// ------------------------------------------------------------------
//  Test harness — runs pending DB migrations before any test.
//  The Rocket fairing normally does this at startup; integration
//  tests bypass the fairing so we trigger it manually.
// ------------------------------------------------------------------

fn ensure_migrations() {
    e621_account_parser_api::db::ensure_sqlite().expect("DB migrations failed");
}

// Each test must call ensure_migrations() first or use the helper below.

fn setup_test(account_id: i32) {
    ensure_migrations();
    let _ = e621_account_parser_api::db::set_account("test_owner", account_id, "test_user", "");
    let _ = e621_account_parser_api::db::drop_account_posts(account_id);
}
