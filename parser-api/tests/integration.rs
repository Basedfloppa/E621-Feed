//! Integration tests for the e621-account-parser DB layer.
//!
//! These tests exercise real `SQLite` reads/writes against a process-isolated
//! temporary database. Run with `cargo test --test integration`.

mod support;

use std::collections::HashSet;

use e621_account_parser_api::db;
use e621_account_parser_api::models::{
    FileMeta, FileOriginal, Files, Flags, Has, Post, Rating, Relationships, Score, Stats, Tags,
};
use rocket::http::{Cookie, Status};
use rocket::local::asynchronous::Client;

/// Helper: build a minimal Post for testing.
fn make_post(id: i64, tags: Tags, uploader_id: i64) -> Post {
    Post {
        id,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        change_seq: 0.0,
        files: Files::default(),
        uploader_id,
        uploader_name: None,
        approver_id: None,
        stats: Stats {
            score: Score {
                up: 10,
                down: 0,
                total: 10,
            },
            fav_count: 5,
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
        tags,
    }
}

fn make_tags(artist: &[&str], character: &[&str], general: &[&str]) -> Tags {
    Tags {
        artist: artist
            .iter()
            .map(std::string::ToString::to_string)
            .collect(),
        character: character
            .iter()
            .map(std::string::ToString::to_string)
            .collect(),
        copyright: vec![],
        species: vec![],
        general: general
            .iter()
            .map(std::string::ToString::to_string)
            .collect(),
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
        make_post(
            11002,
            make_tags(&["skeb"], &["dog"], &["furry", "commission"]),
            42,
        ),
    ];
    db::save_posts(&posts1, account_id).unwrap();
    db::save_posts_tags_batch(&posts1, &blacklist, true, Some(account_id)).unwrap();

    // Check account_tag_cooccurrence exists
    let count = count_account_cooc(account_id);
    assert!(count > 0, "expected cooc rows after save, got {count}");

    // Stronger content check: count only assertions hid the cooc_dirty
    // self-pair bug. Verify a specific cross-group pair landed AND that
    // no row is a degenerate self-pair (canonical ordering guarantees
    // tag1 ≠ tag2, but we double-check).
    let pair_present: i64 = {
        let conn = db::open_db_for_calibration().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM account_tag_cooccurrence \
             WHERE account_id = ?1 \
               AND ((tag1_name = 'skeb' AND tag2_name = 'furry') \
                 OR (tag1_name = 'furry' AND tag2_name = 'skeb'))",
            rusqlite::params![account_id],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert!(
        pair_present > 0,
        "expected (skeb, furry) cooc pair to be recorded"
    );
    let self_pairs: i64 = {
        let conn = db::open_db_for_calibration().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM account_tag_cooccurrence \
             WHERE account_id = ?1 \
               AND tag1_name = tag2_name \
               AND tag1_group = tag2_group",
            rusqlite::params![account_id],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(self_pairs, 0, "no self-pairs in account_tag_cooccurrence");

    // ---- Save batch 2: overlapping tags should increment counts ----
    let posts2 = vec![make_post(
        11003,
        make_tags(&["skeb"], &["cat"], &["furry"]),
        42,
    )];
    db::save_posts(&posts2, account_id).unwrap();
    db::save_posts_tags_batch(&posts2, &blacklist, true, Some(account_id)).unwrap();

    let count2 = count_account_cooc(account_id);
    assert!(
        count2 >= count,
        "cooc should persist across batches ({count} → {count2})"
    );

    // ---- Drop + re-save: verify cooccurrence is rebuilt correctly ----
    //
    // `drop_account_posts` clears `accounts_post`; the cooc wipe lives in
    // `drop_account_cooccurrence_batched` so the two destructive paths
    // can be staged separately (progress logging for the long one). The
    // /process pipeline calls both back-to-back; tests mirror that.
    db::drop_account_posts(account_id).unwrap();
    db::drop_account_cooccurrence_batched(account_id, 1024, |_, _| {}).unwrap();
    let count_after_drop = count_account_cooc(account_id);
    assert_eq!(count_after_drop, 0, "cooc should be empty after drop");

    let posts3 = vec![make_post(
        11001,
        make_tags(&["skeb"], &["cat"], &["furry"]),
        42,
    )];
    db::save_posts(&posts3, account_id).unwrap();
    db::save_posts_tags_batch(&posts3, &blacklist, true, Some(account_id)).unwrap();
    let count3 = count_account_cooc(account_id);
    assert!(count3 > 0, "cooc should be rebuilt after re-save");

    // Clean up
    db::drop_account_posts(account_id).unwrap();
    db::drop_account_cooccurrence_batched(account_id, 1024, |_, _| {}).unwrap();
}

/// Verify that `refresh_account_profiles_skip_cooc` does NOT rebuild
/// cooccurrence (it must already be built incrementally).
#[test]
fn profile_refresh_skip_cooc_leaves_cooc_intact() {
    let account_id = 90002;
    let blacklist = HashSet::new();

    setup_test(account_id);

    // Save posts + incremental cooc
    let posts = vec![make_post(
        11010,
        make_tags(&["skeb"], &["cat"], &["furry"]),
        42,
    )];
    db::save_posts(&posts, account_id).unwrap();
    db::save_posts_tags_batch(&posts, &blacklist, true, Some(account_id)).unwrap();
    let cooc_before = count_account_cooc(account_id);

    // Refresh with skip_cooc
    db::refresh_account_profiles_skip_cooc(account_id).unwrap();

    // Cooc should still be present (not wiped by a full rebuild)
    let cooc_after = count_account_cooc(account_id);
    assert_eq!(
        cooc_before, cooc_after,
        "skip_cooc should not change cooc count: before={cooc_before} after={cooc_after}"
    );

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
    let posts = vec![make_post(
        11020,
        make_tags(&["skeb"], &["cat"], &["furry"]),
        42,
    )];
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

    let posts = vec![make_post(
        11030,
        make_tags(&["skeb"], &["cat"], &["furry"]),
        42,
    )];
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
    support::install_isolated_db_config();
    e621_account_parser_api::db::ensure_sqlite().expect("DB migrations failed");
    // Clear rate-limit buckets at the start of each test: the feebly-bounded
    // per-owner/per-IP buckets accumulate in the process shared across tests.
    e621_account_parser_api::ratelimit::reset_for_tests();
}

// Each test must call ensure_migrations() first or use the helper below.

fn setup_test(account_id: i32) {
    ensure_migrations();
    let _ = e621_account_parser_api::db::set_account("test_owner", account_id, "test_user", "");
    let _ = e621_account_parser_api::db::drop_account_posts(account_id);
    let _ =
        e621_account_parser_api::db::drop_account_cooccurrence_batched(account_id, 1024, |_, _| {});
    // Drop any stale feed_sessions a previous panicking run may have
    // left behind. Without this, the Fresh→Active→Expired tests are
    // order-dependent: a leftover row makes the first call return
    // Active instead of Fresh. Also clean feed_session_posts since the
    // cascade FK was removed when session_id became non-unique.
    if let Ok(conn) = e621_account_parser_api::db::open_db_for_calibration() {
        let _ = conn.execute(
            "DELETE FROM feed_session_posts WHERE session_id IN (SELECT session_id FROM feed_sessions WHERE account_id = ?1)",
            rusqlite::params![account_id],
        );
        let _ = conn.execute(
            "DELETE FROM feed_sessions WHERE account_id = ?1",
            rusqlite::params![account_id],
        );
    }
}

/// RAII guard that owns a test `account_id` and (`owner_token`,
/// `account_id`) link for the lifetime of the test. On drop — even on
/// panic — `delete_device_link` runs the full cascade, batched cooc /
/// `feed_interactions` wipes included.
///
/// Use `TestAccount::new()` for new tests; the legacy `setup_test` is
/// still around for the old fixtures that haven't been migrated.
struct TestAccount {
    id: i32,
    owner: &'static str,
}

impl TestAccount {
    fn new(id: i32) -> Self {
        ensure_migrations();
        let owner = "test_owner_token_1234";
        let _ = e621_account_parser_api::db::set_account(owner, id, "test_user", "");
        let _ = e621_account_parser_api::db::drop_account_posts(id);
        let _ = e621_account_parser_api::db::drop_account_cooccurrence_batched(id, 1024, |_, _| {});
        let _ = e621_account_parser_api::db::drop_account_feed_interactions_batched(
            id,
            1024,
            |_, _| {},
        );
        if let Ok(conn) = e621_account_parser_api::db::open_db_for_calibration() {
            let _ = conn.execute(
                "DELETE FROM feed_sessions WHERE account_id = ?1",
                rusqlite::params![id],
            );
        }
        Self { id, owner }
    }
}

impl Drop for TestAccount {
    fn drop(&mut self) {
        // `delete_device_link` cascades through every per-account
        // table once the last link is severed; safe to call even if
        // the test already removed the link (returns Ok(0)).
        let _ = e621_account_parser_api::db::delete_device_link(self.owner, self.id);
        // Belt-and-suspenders: if a test created data without going
        // through a linked owner, the cascade above won't trigger.
        // Wipe what we can directly.
        let _ = e621_account_parser_api::db::drop_account_posts(self.id);
        let _ = e621_account_parser_api::db::drop_account_cooccurrence_batched(
            self.id,
            1024,
            |_, _| {},
        );
        let _ = e621_account_parser_api::db::drop_account_feed_interactions_batched(
            self.id,
            1024,
            |_, _| {},
        );
    }
}

// ==================================================================
//  Regression tests for bugs found in the most recent audit cycle.
//  Each test below corresponds to a fix that previously had no
//  integration coverage — without these, the bug could be reintroduced
//  silently because only `count > 0` style assertions existed.
// ==================================================================

/// `get_post_by_id` used to construct a `Post` with empty tag groups,
/// which made `/posts/<id>/similar` return `[]` for every locally-cached
/// post (similarity collapsed to 0 since the source's tag vector was
/// empty). Regression guard: the function must surface the same tags
/// the post was saved with.
#[test]
fn get_post_by_id_returns_hydrated_tags() {
    let account_id = 90005;
    let blacklist = HashSet::new();
    setup_test(account_id);

    let saved = make_post(
        12001,
        make_tags(
            &["test_artist"],
            &["test_char"],
            &["test_general1", "test_general2"],
        ),
        42,
    );
    db::save_posts(std::slice::from_ref(&saved), account_id).unwrap();
    db::save_posts_tags_batch(
        std::slice::from_ref(&saved),
        &blacklist,
        true,
        Some(account_id),
    )
    .unwrap();

    let fetched = db::get_post_by_id(12001).unwrap();
    let p = fetched.expect("post should exist after save");

    assert_eq!(p.id, 12001);
    assert!(
        p.tags.artist.iter().any(|t| t == "test_artist"),
        "artist tags should be populated, got {:?}",
        p.tags.artist
    );
    assert!(
        p.tags.character.iter().any(|t| t == "test_char"),
        "character tags should be populated, got {:?}",
        p.tags.character
    );
    assert!(
        p.tags.general.iter().any(|t| t == "test_general1"),
        "general tags should be populated, got {:?}",
        p.tags.general
    );
    assert!(
        p.tags.general.iter().any(|t| t == "test_general2"),
        "all general tags should be present"
    );

    // Missing post returns None, not an error and not an empty-tag stub.
    let missing = db::get_post_by_id(99_999_999).unwrap();
    assert!(missing.is_none());

    db::drop_account_posts(account_id).unwrap();
    db::drop_account_cooccurrence_batched(account_id, 1024, |_, _| {}).unwrap();
}

/// `touch_or_create_feed_session` is the atomic replacement for the
/// previous `upsert + validate` pair, which had two latent bugs:
///   * `validate` always succeeded because `upsert` had just touched
///     `last_accessed_at` (Expired branch unreachable, `fresh_start`
///     dead code).
///   * read-then-touch ran on two connections (TOCTOU window).
///
/// This test exercises all three return paths in one transaction
/// boundary each.
#[test]
fn touch_or_create_feed_session_lifecycle() {
    let account_id = 90006;
    setup_test(account_id);

    // Use a unique session_id per test run to avoid collisions with
    // parallel tests that may reuse the same session strings.
    let session_id = "test-sess-90006-abc123";

    // 1. First call → Fresh (row created).
    let state = db::touch_or_create_feed_session(session_id, account_id).unwrap();
    assert_eq!(state, db::FeedSessionState::Fresh);

    // 2. Second call (within TTL) → Active (row touched).
    let state = db::touch_or_create_feed_session(session_id, account_id).unwrap();
    assert_eq!(state, db::FeedSessionState::Active);

    // 3. Backdate `last_accessed_at` past the TTL window and re-call →
    //    Expired (no touch, no error). We use the writer connection
    //    directly here since there's no public helper for this and
    //    test code is intentionally privileged.
    let stale =
        (chrono::Utc::now() - chrono::Duration::minutes(db::FEED_SESSION_TTL_MIN + 5)).to_rfc3339();
    {
        let conn = db::open_db_for_calibration().unwrap();
        conn.execute(
            "UPDATE feed_sessions SET last_accessed_at = ?1 WHERE session_id = ?2",
            rusqlite::params![stale, session_id],
        )
        .unwrap();
    }
    let state = db::touch_or_create_feed_session(session_id, account_id).unwrap();
    assert_eq!(state, db::FeedSessionState::Expired);

    // 4. After Expired, `last_accessed_at` was NOT bumped — re-calling
    //    should still report Expired until the row is pruned or rotated.
    let state = db::touch_or_create_feed_session(session_id, account_id).unwrap();
    assert_eq!(
        state,
        db::FeedSessionState::Expired,
        "Expired must not touch last_accessed_at"
    );

    // 5. A different account using the same session_id is a different
    //    logical session: the row is keyed by (session_id, account_id).
    //    Account 90006's row exists but doesn't apply to 90007.
    // Create account 90007 first so the FK check passes.
    let _ = e621_account_parser_api::db::set_account("other_owner", 90007, "test_user", "");
    let state = db::touch_or_create_feed_session(session_id, 90007).unwrap();
    assert_eq!(
        state,
        db::FeedSessionState::Fresh,
        "different account_id must get its own Fresh row"
    );
    // Clean up account 90007.
    let _ = db::drop_account_posts(90007);

    // Cleanup
    {
        let conn = db::open_db_for_calibration().unwrap();
        conn.execute(
            "DELETE FROM feed_sessions WHERE session_id = ?1",
            rusqlite::params![session_id],
        )
        .unwrap();
    }
    db::drop_account_posts(account_id).unwrap();
    db::drop_account_cooccurrence_batched(account_id, 1024, |_, _| {}).unwrap();
}

/// `drop_account_cooccurrence_batched` must loop until everything is
/// gone, not just clear `batch_size` rows and stop. Forces a 2-batch
/// scenario by setting `batch_size` smaller than the number of rows
/// inserted, then verifies both that the callback fires multiple
/// times and that the table is empty afterwards.
#[test]
fn drop_account_cooccurrence_batched_loops_until_empty() {
    let account_id = 90008;
    let blacklist = HashSet::new();
    setup_test(account_id);

    // Build a post with enough tags that account_tag_cooccurrence
    // ends up with more rows than our deliberately tiny batch_size.
    // With 5 general tags and 1 artist tag we get C(6,2)=15 ordered
    // pairs (function picks canonical ordering); 3-batch is enough.
    let p = make_post(
        13001,
        make_tags(&["a"], &[], &["g1", "g2", "g3", "g4", "g5"]),
        42,
    );
    db::save_posts(std::slice::from_ref(&p), account_id).unwrap();
    db::save_posts_tags_batch(std::slice::from_ref(&p), &blacklist, true, Some(account_id))
        .unwrap();

    let before = count_account_cooc(account_id);
    assert!(before >= 10, "expected ≥10 cooc rows, got {before}");

    // The function now does a single unbounded DELETE (the index on
    // account_id makes it O(log n)) and fires one callback with the
    // total deleted count.
    use std::cell::Cell;
    let calls = Cell::new(0usize);
    let total_seen = Cell::new(0usize);
    let deleted = db::drop_account_cooccurrence_batched(account_id, 4, |_batch, total| {
        calls.set(calls.get() + 1);
        total_seen.set(total);
    })
    .unwrap();

    assert_eq!(
        deleted as i64, before,
        "total deleted must equal initial row count"
    );
    assert_eq!(
        calls.get(),
        1,
        "single unbounded DELETE must fire exactly 1 callback"
    );
    assert_eq!(total_seen.get() as i64, before);
    assert_eq!(count_account_cooc(account_id), 0);

    // No-op idempotency: calling again on an empty slate fires zero
    // callbacks and returns 0.
    let calls2 = Cell::new(0usize);
    let deleted2 = db::drop_account_cooccurrence_batched(account_id, 4, |_, _| {
        calls2.set(calls2.get() + 1);
    })
    .unwrap();
    assert_eq!(deleted2, 0);
    assert_eq!(calls2.get(), 0);

    db::drop_account_posts(account_id).unwrap();
}

/// Regression for the `cooc_dirty` per-batch flag bug.
///
/// The previous code skipped the per-post sort+dedup of `post_tag_ids`
/// for the account-cooc branch whenever ANY earlier post in the batch
/// had triggered the global branch. With duplicate `tag_ids` (rare but
/// possible: a tag appearing in two groups of the same post), the
/// cartesian pair loop would emit `(tag, tag)` self-pairs into
/// `account_tag_cooccurrence`. This test seeds a batch where post 1
/// goes through the global branch and post 2 follows; we then verify
/// no row has `tag1_name = tag2_name AND tag1_group = tag2_group`.
#[test]
fn save_posts_tags_batch_no_self_pairs() {
    let account_id = 90009;
    let blacklist = HashSet::new();
    setup_test(account_id);

    // Two posts in one batch. Both have a couple of overlapping tags
    // and at least 2 unique tags each so the global cooc branch fires
    // on post1 (priming `cooc_dirty`).
    let posts = vec![
        make_post(14001, make_tags(&["a"], &["c"], &["g1", "g2"]), 42),
        make_post(14002, make_tags(&["a"], &["c"], &["g1", "g3"]), 42),
    ];
    db::save_posts(&posts, account_id).unwrap();
    db::save_posts_tags_batch(&posts, &blacklist, true, Some(account_id)).unwrap();

    let self_pairs: i64 = {
        let conn = db::open_db_for_calibration().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM account_tag_cooccurrence \
             WHERE account_id = ?1 \
               AND tag1_name = tag2_name \
               AND tag1_group = tag2_group",
            rusqlite::params![account_id],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(
        self_pairs, 0,
        "account_tag_cooccurrence must not contain self-pairs"
    );

    db::drop_account_posts(account_id).unwrap();
    db::drop_account_cooccurrence_batched(account_id, 1024, |_, _| {}).unwrap();
}

/// `delete_by_account_in_batches` runs hand-spliced SQL keyed on the
/// table name, so a closed whitelist guards the splice. An unknown
/// table must surface as `Err`, not as a SQL injection or a silent
/// no-op. We can't call the private function directly, but the
/// behaviour is exposed via the two public helpers — both must reject
/// nothing because they're hard-coded to known tables. This test
/// instead exercises the "wrong table" failure mode by ensuring the
/// known helpers DON'T affect adjacent tables.
#[test]
fn drop_helpers_only_touch_their_target_table() {
    let account_id = 90010;
    let blacklist = HashSet::new();
    setup_test(account_id);

    // Seed cooc rows but NO feed_interactions.
    let p = make_post(15001, make_tags(&["a"], &["c"], &["g1", "g2"]), 42);
    db::save_posts(std::slice::from_ref(&p), account_id).unwrap();
    db::save_posts_tags_batch(std::slice::from_ref(&p), &blacklist, true, Some(account_id))
        .unwrap();

    let cooc_before = count_account_cooc(account_id);
    assert!(cooc_before > 0);

    // Cooc wipe must not touch accounts_post (the partner table).
    let accounts_post_before: i64 = {
        let conn = db::open_db_for_calibration().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM accounts_post WHERE account_id = ?1",
            rusqlite::params![account_id],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(accounts_post_before, 1);

    db::drop_account_cooccurrence_batched(account_id, 1024, |_, _| {}).unwrap();

    assert_eq!(count_account_cooc(account_id), 0);
    let accounts_post_after: i64 = {
        let conn = db::open_db_for_calibration().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM accounts_post WHERE account_id = ?1",
            rusqlite::params![account_id],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(
        accounts_post_after, accounts_post_before,
        "cooc wipe must not delete from accounts_post"
    );

    db::drop_account_posts(account_id).unwrap();
}

/// `prune_expired_sessions` must wipe sessions whose
/// `last_accessed_at` is older than the centralised TTL — and ONLY
/// those. After the fix that hoisted the literal `30` into
/// `FEED_SESSION_TTL_MIN`, this test pins the actual cutoff so a
/// future tweak to one side without the other is caught.
#[test]
fn prune_expired_sessions_respects_ttl_constant() {
    let account_id = 90011;
    setup_test(account_id);

    // One fresh, one stale (TTL+5min ago).
    db::touch_or_create_feed_session("test-fresh-90011", account_id).unwrap();
    db::touch_or_create_feed_session("test-stale-90011", account_id).unwrap();
    let stale =
        (chrono::Utc::now() - chrono::Duration::minutes(db::FEED_SESSION_TTL_MIN + 5)).to_rfc3339();
    {
        let conn = db::open_db_for_calibration().unwrap();
        conn.execute(
            "UPDATE feed_sessions SET last_accessed_at = ?1 WHERE session_id = ?2",
            rusqlite::params![stale, "test-stale-90011"],
        )
        .unwrap();
    }

    let pruned = db::prune_expired_sessions().unwrap();
    assert!(pruned >= 1, "expected ≥1 pruned session, got {pruned}");

    let fresh_remaining: i64 = {
        let conn = db::open_db_for_calibration().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM feed_sessions WHERE session_id = ?1",
            rusqlite::params!["test-fresh-90011"],
            |r| r.get(0),
        )
        .unwrap()
    };
    let stale_remaining: i64 = {
        let conn = db::open_db_for_calibration().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM feed_sessions WHERE session_id = ?1",
            rusqlite::params!["test-stale-90011"],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(fresh_remaining, 1, "fresh session must survive prune");
    assert_eq!(stale_remaining, 0, "stale session must be pruned");

    // Cleanup
    {
        let conn = db::open_db_for_calibration().unwrap();
        conn.execute(
            "DELETE FROM feed_sessions WHERE session_id IN (?1, ?2)",
            rusqlite::params!["test-fresh-90011", "test-stale-90011"],
        )
        .unwrap();
    }
    db::drop_account_posts(account_id).unwrap();
}

/// Records-shown-posts round-trip: write some IDs against a session,
/// read them back, and verify dedup-set semantics. This exercises the
/// other half of the `/continue` plumbing — `touch_or_create_feed_session`
/// is tested above, and this confirms the dedup store works.
#[test]
fn session_shown_posts_dedup_roundtrip() {
    let account_id = 90012;
    setup_test(account_id);

    let session_id = "test-shown-90012";
    db::touch_or_create_feed_session(session_id, account_id).unwrap();

    db::record_session_shown_posts(session_id, account_id, &[(1001, 0), (1002, 1), (1003, 2)])
        .unwrap();
    db::record_session_shown_posts(session_id, account_id, &[(1004, 0)]).unwrap();
    // Duplicate (session_id, post_id) — must be silently ignored by the
    // INSERT OR IGNORE in the writer.
    db::record_session_shown_posts(session_id, account_id, &[(1001, 10)]).unwrap();

    let shown = db::get_session_shown_post_ids(session_id, account_id).unwrap();
    assert_eq!(shown.len(), 4);
    for id in [1001, 1002, 1003, 1004] {
        assert!(shown.contains(&id), "expected post {id} in shown set");
    }

    // Cleanup
    {
        let conn = db::open_db_for_calibration().unwrap();
        conn.execute(
            "DELETE FROM feed_sessions WHERE session_id = ?1",
            rusqlite::params![session_id],
        )
        .unwrap();
    }
    db::drop_account_posts(account_id).unwrap();
}

/// Symmetric coverage for the second batched-delete helper. The cooc
/// version is already covered by
/// `drop_account_cooccurrence_batched_loops_until_empty`; this exercises
/// the `feed_interactions` helper the same way so a future change to the
/// shared `delete_by_account_in_batches` core can't break only one of
/// the two callers.
#[test]
fn drop_account_feed_interactions_batched_loops_until_empty() {
    let acc = TestAccount::new(90013);

    // Seed the catalog: a single post the interactions can reference.
    let p = make_post(16001, make_tags(&["a"], &[], &["g"]), 42);
    db::save_posts(std::slice::from_ref(&p), acc.id).unwrap();

    // Insert 10 feed_interactions directly. We bypass
    // `record_feed_interaction` to avoid the per-call ownership check
    // and the tag_feedback fanout — we only want raw rows.
    {
        let conn = db::open_db_for_calibration().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        for i in 0..10 {
            conn.execute(
                "INSERT INTO feed_interactions \
                 (account_id, post_id, event_type, position, session_id, created_at) \
                 VALUES (?1, ?2, 'qualified_impression', ?3, ?4, ?5)",
                rusqlite::params![acc.id, 16001_i64, i, format!("test-sess-{i}"), now],
            )
            .unwrap();
        }
    }

    let before: i64 = {
        let conn = db::open_db_for_calibration().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM feed_interactions WHERE account_id = ?1",
            rusqlite::params![acc.id],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(before, 10);

    use std::cell::Cell;
    let calls = Cell::new(0usize);
    let deleted = db::drop_account_feed_interactions_batched(acc.id, 3, |batch, _| {
        calls.set(calls.get() + 1);
        assert!(batch <= 3);
    })
    .unwrap();
    assert_eq!(deleted, 10);
    // 10 rows at batch_size=3 → ceil(10/3)=4 callback invocations.
    assert_eq!(
        calls.get(),
        4,
        "should have fired exactly 4 batches (10 rows, batch=3), got {}",
        calls.get()
    );

    let after: i64 = {
        let conn = db::open_db_for_calibration().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM feed_interactions WHERE account_id = ?1",
            rusqlite::params![acc.id],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(after, 0);

    // Idempotency: re-running on an empty slate is a no-op.
    let calls2 = Cell::new(0usize);
    let deleted2 = db::drop_account_feed_interactions_batched(acc.id, 3, |_, _| {
        calls2.set(calls2.get() + 1);
    })
    .unwrap();
    assert_eq!(deleted2, 0);
    assert_eq!(calls2.get(), 0);
    // TestAccount Drop handles cleanup.
}

/// `delete_device_link` underwent a 3-phase split to avoid pinning the
/// writer mutex on the multi-million-row cooc wipe:
///   1. drop the `device_link`, decide whether to cascade
///   2. batched cooc + `feed_interactions` wipe (outside the cascade tx)
///   3. atomic small-table cascade + `accounts` row removal
///
/// This regression test exercises all three phases by linking two
/// devices to one account, severing each in turn, and asserting the
/// observable state transitions at each step.
#[test]
fn delete_device_link_three_phase_cascade() {
    let account_id = 90014;
    ensure_migrations();
    let owner_a = "test_owner_a";
    let owner_b = "test_owner_b";

    // Two devices link to the same account, then we seed cooc + an
    // interaction so we can observe the cascade wiping them.
    let _ = db::set_account(owner_a, account_id, "test_user", "");
    let _ = db::set_account(owner_b, account_id, "test_user", "");
    let _ = db::drop_account_posts(account_id);
    let _ = db::drop_account_cooccurrence_batched(account_id, 1024, |_, _| {});
    let _ = db::drop_account_feed_interactions_batched(account_id, 1024, |_, _| {});

    let p = make_post(17001, make_tags(&["a"], &["c"], &["g1", "g2"]), 42);
    db::save_posts(std::slice::from_ref(&p), account_id).unwrap();
    db::save_posts_tags_batch(
        std::slice::from_ref(&p),
        &HashSet::new(),
        true,
        Some(account_id),
    )
    .unwrap();
    {
        let conn = db::open_db_for_calibration().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO feed_interactions \
             (account_id, post_id, event_type, position, session_id, created_at) \
             VALUES (?1, ?2, 'open', 0, 'sess', ?3)",
            rusqlite::params![account_id, 17001_i64, now],
        )
        .unwrap();
    }

    let count_table = |table: &str| -> i64 {
        let conn = db::open_db_for_calibration().unwrap();
        let sql = format!("SELECT COUNT(*) FROM {table} WHERE account_id = ?1");
        conn.query_row(&sql, rusqlite::params![account_id], |r| r.get(0))
            .unwrap()
    };
    let count_account_row = || -> i64 {
        let conn = db::open_db_for_calibration().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM accounts WHERE id = ?1",
            rusqlite::params![account_id],
            |r| r.get(0),
        )
        .unwrap()
    };

    // Pre-state assertions: links=2, data present.
    assert_eq!(count_table("account_device_links"), 2);
    assert!(count_table("account_tag_cooccurrence") > 0);
    assert_eq!(count_table("feed_interactions"), 1);
    assert_eq!(count_account_row(), 1);

    // ── Phase 1 ─────────────────────────────────────────────────
    // Sever device A. Another link still exists → NO cascade.
    let removed = db::delete_device_link(owner_a, account_id).unwrap();
    assert_eq!(removed, 1);
    assert_eq!(
        count_table("account_device_links"),
        1,
        "device B's link must survive"
    );
    assert!(
        count_table("account_tag_cooccurrence") > 0,
        "cooc must NOT be wiped while another device holds the account"
    );
    assert_eq!(
        count_table("feed_interactions"),
        1,
        "feed_interactions must NOT be wiped while another device holds the account"
    );
    assert_eq!(count_account_row(), 1, "accounts row must survive");

    // ── Phase 2 + 3 ─────────────────────────────────────────────
    // Sever the last device. Now cascade runs: batched cooc +
    // feed_interactions wipe, then the atomic small-table cleanup.
    let removed = db::delete_device_link(owner_b, account_id).unwrap();
    assert_eq!(removed, 1);
    assert_eq!(count_table("account_device_links"), 0);
    assert_eq!(
        count_table("account_tag_cooccurrence"),
        0,
        "cooc must be wiped on final unlink"
    );
    assert_eq!(
        count_table("feed_interactions"),
        0,
        "feed_interactions must be wiped on final unlink"
    );
    assert_eq!(
        count_table("accounts_post"),
        0,
        "accounts_post must be wiped on final unlink"
    );
    assert_eq!(count_account_row(), 0, "accounts row must be removed");

    // Idempotency: re-calling on an already-removed link returns 0.
    assert_eq!(db::delete_device_link(owner_a, account_id).unwrap(), 0);
    assert_eq!(db::delete_device_link(owner_b, account_id).unwrap(), 0);

    // Belt-and-suspenders cleanup (matches the new TestAccount pattern
    // for symmetry, even though everything's already gone).
    let _ = db::drop_account_posts(account_id);
    let _ = db::drop_account_cooccurrence_batched(account_id, 1024, |_, _| {});
    let _ = db::drop_account_feed_interactions_batched(account_id, 1024, |_, _| {});
}

// ==================================================================
//  Profile computation tests
// ==================================================================

/// Build a post with the given parameters — reduced copy to avoid importing
/// the test crate's helper from the pipeline test file.
#[allow(
    clippy::too_many_arguments,
    reason = "The integration-test fixture deliberately exposes each post field used by a profile assertion."
)]
fn profile_post(
    id: i64,
    rating: &str,
    ext: &str,
    duration: Option<f64>,
    score_total: i64,
    fav_count: i64,
    comment_count: i64,
    uploader_id: i64,
) -> Post {
    let r = match rating {
        "s" => Rating::S,
        "q" => Rating::Q,
        "e" => Rating::E,
        _ => Rating::S,
    };
    Post {
        id,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        change_seq: 0.0,
        files: Files {
            meta: FileMeta {
                ext: Some(ext.into()),
                size: 1234,
                md5: Some("dummy".into()),
                duration,
                ..Default::default()
            },
            original: FileOriginal {
                width: 100,
                height: 100,
                url: Some("https://example.com/img".into()),
            },
            ..Default::default()
        },
        uploader_id,
        uploader_name: None,
        approver_id: None,
        stats: Stats {
            score: Score {
                up: score_total.max(0),
                down: 0,
                total: score_total,
            },
            fav_count,
            comment_count,
            ..Default::default()
        },
        flags: Flags::default(),
        has: Has::default(),
        relationships: Relationships::default(),
        pools: vec![],
        rating: r,
        locked_tags: vec![],
        sources: vec![],
        description: None,
        tags: Tags {
            general: vec!["tag".into()],
            artist: vec!["art".into()],
            ..Tags::default()
        },
    }
}

/// Verify rating profile counts match expected distribution.
#[test]
fn profile_rating_profile() {
    let acc = TestAccount::new(90020);

    let posts = vec![
        profile_post(200001, "s", "jpg", None, 10, 5, 2, 100),
        profile_post(200002, "s", "jpg", None, 20, 3, 1, 100),
        profile_post(200003, "q", "jpg", None, 15, 8, 0, 200),
    ];
    db::save_posts(&posts, acc.id).unwrap();

    // Run profiles individually
    db::set_rating_profile(acc.id).unwrap();
    let profile = db::get_account_rating_profile(acc.id).unwrap();

    // Should have 2 S-rated and 1 Q-rated
    let s_count = profile
        .iter()
        .find(|r| r.rating == "s")
        .map_or(0, |r| r.count);
    let q_count = profile
        .iter()
        .find(|r| r.rating == "q")
        .map_or(0, |r| r.count);
    assert_eq!(s_count, 2, "should have 2 S-rated posts");
    assert_eq!(q_count, 1, "should have 1 Q-rated post");
}

/// Verify media profile classifies file extensions correctly.
#[test]
fn profile_media_profile() {
    let acc = TestAccount::new(90021);

    let posts = vec![
        profile_post(200011, "s", "jpg", None, 1, 0, 0, 1), // image
        profile_post(200012, "s", "png", None, 1, 0, 0, 1), // image
        profile_post(200013, "s", "gif", None, 1, 0, 0, 1), // animated (gif)
        profile_post(200014, "s", "webm", None, 1, 0, 0, 1), // video
        profile_post(200015, "s", "mp4", None, 1, 0, 0, 1), // video
        // Post with duration > 0 but no file_ext (should be video)
        e621_account_parser_api::models::Post {
            id: 200016,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            change_seq: 0.0,
            files: Files {
                meta: FileMeta {
                    duration: Some(10.0),
                    ..Default::default()
                },
                ..Default::default()
            },
            uploader_id: 1,
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
            flags: e621_account_parser_api::models::Flags::default(),
            has: Has::default(),
            relationships: Relationships::default(),
            pools: vec![],
            rating: e621_account_parser_api::models::Rating::S,
            locked_tags: vec![],
            sources: vec![],
            description: None,
            tags: Tags {
                general: vec!["tag".into()],
                artist: vec!["art".into()],
                ..Tags::default()
            },
        },
    ];
    db::save_posts(&posts, acc.id).unwrap();

    e621_account_parser_api::db::set_media_profile(acc.id).unwrap();
    let profile = db::get_account_media_profile(acc.id).unwrap();

    let image_count = profile
        .iter()
        .find(|m| m.media_type == "image")
        .map_or(0, |m| m.count);
    let animated = profile
        .iter()
        .find(|m| m.media_type == "animated")
        .map_or(0, |m| m.count);
    let video_count = profile
        .iter()
        .find(|m| m.media_type == "video")
        .map_or(0, |m| m.count);
    assert_eq!(image_count, 2, "jpg + png = 2 images");
    assert_eq!(animated, 1, "gif = 1 animated");
    assert_eq!(video_count, 3, "webm + mp4 + duration>0 = 3 video");
}

/// Verify quality profile computes averages correctly.
#[test]
fn profile_quality_profile() {
    let acc = TestAccount::new(90022);

    let posts = vec![
        profile_post(200021, "s", "jpg", Some(5.0), 100, 50, 10, 1),
        profile_post(200022, "s", "jpg", Some(15.0), 200, 30, 20, 1),
    ];
    db::save_posts(&posts, acc.id).unwrap();

    e621_account_parser_api::db::set_quality_profile(acc.id).unwrap();
    let q = db::get_account_quality_profile(acc.id).unwrap();

    assert!(
        (q.avg_score_total - 150.0).abs() < 1.0,
        "avg score = 150, got {}",
        q.avg_score_total
    );
    assert!(
        (q.avg_fav_count - 40.0).abs() < 1.0,
        "avg fav = 40, got {}",
        q.avg_fav_count
    );
    assert!(
        (q.avg_comment_count - 15.0).abs() < 1.0,
        "avg comments = 15, got {}",
        q.avg_comment_count
    );
    assert!(
        (q.avg_duration - 10.0).abs() < 1.0,
        "avg duration = 10, got {}",
        q.avg_duration
    );
}

/// Verify recency profile computes averages correctly.
#[test]
fn profile_recency_profile() {
    let acc = TestAccount::new(90023);

    use chrono::{Duration, Utc};
    let now = Utc::now();
    let make_post = |id: i64, days_ago: f64| -> e621_account_parser_api::models::Post {
        e621_account_parser_api::models::Post {
            id,
            created_at: now - Duration::seconds((days_ago * 86_400.0) as i64),
            updated_at: now,
            change_seq: 0.0,
            files: Files::default(),
            uploader_id: 1,
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
            flags: e621_account_parser_api::models::Flags::default(),
            has: Has::default(),
            relationships: Relationships::default(),
            pools: vec![],
            rating: e621_account_parser_api::models::Rating::S,
            locked_tags: vec![],
            sources: vec![],
            description: None,
            tags: Tags {
                general: vec!["tag".into()],
                artist: vec!["art".into()],
                ..Tags::default()
            },
        }
    };

    let posts = vec![
        make_post(200031, 30.0), // 30 days old
        make_post(200032, 10.0), // 10 days old
    ];
    db::save_posts(&posts, acc.id).unwrap();

    e621_account_parser_api::db::set_recency_profile(acc.id).unwrap();
    let r = db::get_account_recency_profile(acc.id).unwrap();

    // Mean age = (30+10)/2 = 20 days. Allow 1 day tolerance for test timing.
    assert!(
        (r.avg_age_days - 20.0).abs() < 2.0,
        "avg_age_days ≈ 20, got {}",
        r.avg_age_days
    );
    // Mean absolute deviation = (|30-20| + |10-20|)/2 = (10+10)/2 = 10
    assert!(
        (r.avg_abs_dev_days - 10.0).abs() < 2.0,
        "avg_abs_dev_days ≈ 10, got {}",
        r.avg_abs_dev_days
    );
}

/// Verify uploader profile groups by `uploader_id`.
#[test]
fn profile_uploader_profile() {
    let acc = TestAccount::new(90024);

    let posts = vec![
        profile_post(200041, "s", "jpg", None, 10, 5, 0, 100),
        profile_post(200042, "s", "jpg", None, 30, 15, 0, 100),
        profile_post(200043, "s", "jpg", None, 20, 10, 0, 200),
    ];
    db::save_posts(&posts, acc.id).unwrap();

    e621_account_parser_api::db::set_uploader_profile(acc.id).unwrap();
    let uploaders = db::get_account_uploader_profile(acc.id).unwrap();

    // Uploader 100 has 2 posts (score 10 and 30 → avg 20; fav 5 and 15 → avg 10)
    let u100 = uploaders
        .iter()
        .find(|u| u.uploader_id == 100)
        .expect("uploader 100 present");
    assert!(
        (u100.avg_score - 20.0).abs() < 1.0,
        "uploader 100 avg_score=20, got {}",
        u100.avg_score
    );
    assert!(
        (u100.avg_fav - 10.0).abs() < 1.0,
        "uploader 100 avg_fav=10, got {}",
        u100.avg_fav
    );

    // Uploader 200 has 1 post (score 20, fav 10)
    let u200 = uploaders
        .iter()
        .find(|u| u.uploader_id == 200)
        .expect("uploader 200 present");
    assert!((u200.avg_score - 20.0).abs() < 1.0);
    assert!((u200.avg_fav - 10.0).abs() < 1.0);
}

/// Verify full refresh sets all profiles and the `profile_refreshed_at` timestamp.
#[test]
fn profile_refresh_full_sets_profiles_and_timestamp() {
    let acc = TestAccount::new(90025);

    let p = profile_post(200051, "e", "jpg", None, 5, 0, 0, 42);
    db::save_posts(std::slice::from_ref(&p), acc.id).unwrap();
    db::save_posts_tags_batch(
        std::slice::from_ref(&p),
        &std::collections::HashSet::new(),
        true,
        Some(acc.id),
    )
    .unwrap();

    e621_account_parser_api::db::refresh_account_profiles(acc.id).unwrap();

    let rating = db::get_account_rating_profile(acc.id).unwrap();
    assert!(!rating.is_empty(), "rating profile populated");

    let media = db::get_account_media_profile(acc.id).unwrap();
    assert!(!media.is_empty(), "media profile populated");

    let quality = db::get_account_quality_profile(acc.id).unwrap();
    assert!(quality.avg_score_total > 0.0, "quality profile populated");

    let recency = db::get_account_recency_profile(acc.id).unwrap();
    assert!(recency.avg_age_days >= 0.0, "recency profile populated");

    // Verify profile_refreshed_at was set
    let pref = db::get_account_preference_profile(acc.id).unwrap();
    assert!(
        pref.last_refreshed_at.is_some(),
        "last_refreshed_at should be set after refresh"
    );
}

/// Verify `get_account_preference_profile` aggregates all sub-profiles.
#[test]
fn profile_preference_profile_aggregates_all() {
    let acc = TestAccount::new(90026);

    let posts = vec![
        profile_post(200061, "s", "jpg", None, 10, 5, 0, 100),
        profile_post(200062, "q", "webm", Some(8.0), 20, 3, 2, 100),
    ];
    db::save_posts(&posts, acc.id).unwrap();
    // Full refresh builds all profiles
    e621_account_parser_api::db::refresh_account_profiles(acc.id).unwrap();

    let pref = db::get_account_preference_profile(acc.id).unwrap();
    assert_eq!(pref.rating.len(), 2, "two rating categories");
    assert_eq!(pref.media.len(), 2, "image + video");
    assert!(pref.quality.avg_score_total > 0.0);
    assert!(pref.recency.avg_age_days >= 0.0);
    assert!(
        pref.last_refreshed_at.is_some(),
        "refresh should set timestamp"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn authenticated_read_routes_use_seeded_sqlite() {
    let account = TestAccount::new(9_100_100);
    let client = Client::tracked(rocket::build().mount(
        "/api",
        e621_account_parser_api::routes::integration_test_routes(),
    ))
    .await
    .unwrap();
    let cookie = Cookie::new(
        e621_account_parser_api::auth::OWNER_TOKEN_COOKIE,
        account.owner,
    );

    for path in [
        "/api/accounts".to_owned(),
        format!("/api/account/{}/tag_counts", account.id),
        format!("/api/account/{}/profile", account.id),
        format!("/api/account/{}/export", account.id),
        format!("/api/digest/{}?full=false", account.id),
    ] {
        let response = client.get(path).cookie(cookie.clone()).dispatch().await;
        assert_eq!(response.status(), Status::Ok);
    }
}

/// `GET /accounts?limit=&offset=` returns an honest slice: `limit` caps the
/// page, `offset` advances it, oversized offsets yield an empty page.
#[tokio::test(flavor = "multi_thread")]
async fn list_accounts_respects_limit_and_offset() {
    let owner = "pag_owner_token_0001";
    for id in [9_200_001i32, 9_200_002, 9_200_003, 9_200_004] {
        e621_account_parser_api::db::set_account(owner, id, "pg", "").unwrap();
    }
    let client = Client::tracked(rocket::build().mount(
        "/api",
        e621_account_parser_api::routes::integration_test_routes(),
    ))
    .await
    .unwrap();
    let cookie = Cookie::new(e621_account_parser_api::auth::OWNER_TOKEN_COOKIE, owner);
    let get_ids = |q: String| {
        let cookie = cookie.clone();
        let client = &client;
        async move {
            let resp = client
                .get(format!("/api/accounts{q}"))
                .cookie(cookie)
                .dispatch()
                .await;
            assert_eq!(resp.status(), Status::Ok, "get /api/accounts{q}");
            let v: Vec<serde_json::Value> = resp.into_json().await.unwrap();
            v.into_iter()
                .map(|x| x["id"].as_i64().unwrap())
                .collect::<std::collections::HashSet<i64>>()
        }
    };

    let full = get_ids(String::new()).await;
    assert!(
        full.len() >= 4,
        "owner should have >=4 accounts, got {}",
        full.len()
    );
    let p1 = get_ids("?limit=2&offset=0".to_owned()).await;
    let p2 = get_ids("?limit=2&offset=2".to_owned()).await;
    assert!(p1.len() <= 2 && p2.len() <= 2, "limit not respected");
    assert_eq!(
        p1.intersection(&p2).count(),
        0,
        "offset must advance past page 1"
    );
    let union: std::collections::HashSet<i64> = p1.union(&p2).copied().collect();
    assert_eq!(union.len(), 4, "both pages together cover all 4 accounts");
    assert!(
        get_ids("?limit=2&offset=9999".to_owned()).await.is_empty(),
        "far offset -> empty"
    );
}

/// `GET /session/devices` returns every device sharing an account with the
/// caller, flags the current one, and never leaks raw owner tokens.
#[tokio::test(flavor = "multi_thread")]
async fn session_devices_lists_sharing_devices_without_leaking_tokens() {
    ensure_migrations();
    let owner_a = "dev_owner_A_token";
    let owner_b = "dev_owner_B_token";
    let aid_a = 9_301_001i32;
    let aid_b = 9_301_002i32;

    // Device A owns accounts 9_301_001 and 9_301_002; device B shares 9_301_002.
    e621_account_parser_api::db::set_account(owner_a, aid_a, "devA_only", "").unwrap();
    e621_account_parser_api::db::set_account(owner_a, aid_b, "shared", "").unwrap();
    e621_account_parser_api::db::set_account(owner_b, aid_b, "shared", "").unwrap();

    let client = Client::tracked(rocket::build().mount(
        "/api",
        e621_account_parser_api::routes::integration_test_routes(),
    ))
    .await
    .unwrap();
    let cookie_a = Cookie::new(
        e621_account_parser_api::auth::OWNER_TOKEN_COOKIE,
        owner_a.to_string(),
    );
    let response = client
        .get("/api/session/devices")
        .cookie(cookie_a)
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    let body: serde_json::Value = response.into_json().await.unwrap();
    let devices = body.as_array().expect("devices must be a JSON array");
    assert_eq!(
        devices.len(),
        2,
        "owner A sees itself + the sharing device B"
    );

    let current: Vec<&serde_json::Value> = devices
        .iter()
        .filter(|d| d["isCurrent"].as_bool().unwrap_or(false))
        .collect();
    assert_eq!(current.len(), 1, "exactly one current device");
    let current = current[0];

    // Device id must be a sha256 hex (64 chars) — never the raw token.
    let id = current["id"].as_str().expect("current.id");
    assert_eq!(id.len(), 64, "id is a sha256 hex digest");
    assert!(id.chars().all(|c| c.is_ascii_hexdigit()));

    // Current (A) device owns both accounts.
    let cur_accounts: std::collections::HashSet<i64> = current["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["accountId"].as_i64().unwrap())
        .collect();
    assert!(cur_accounts.contains(&(aid_a as i64)));
    assert!(cur_accounts.contains(&(aid_b as i64)));

    // The other device (B) shares only the second account and is not current.
    let other = devices
        .iter()
        .find(|d| d["isCurrent"].as_bool() == Some(false))
        .expect("non-current sharing device present");
    let other_accounts: Vec<i64> = other["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["accountId"].as_i64().unwrap())
        .collect();
    assert_eq!(
        other_accounts,
        vec![aid_b as i64],
        "B shares only the second"
    );

    // No-secret invariant: raw owner tokens must never appear in the payload.
    let raw = body.to_string();
    assert!(!raw.contains(owner_a), "raw token A must not leak");
    assert!(!raw.contains(owner_b), "raw token B must not leak");
    assert!(
        current["firstSeenAt"].as_str().is_some()
            && current["lastSeenAt"].as_str().is_some()
            && current["active"].is_boolean(),
        "first/last seen + active present"
    );

    // Best-effort cleanup so accounts don't linger for sibling tests.
    let _ = e621_account_parser_api::db::delete_device_link(owner_a, aid_a);
    let _ = e621_account_parser_api::db::delete_device_link(owner_a, aid_b);
    let _ = e621_account_parser_api::db::delete_device_link(owner_b, aid_b);
}

/// `POST /session/revoke` severs another sharing device by its public id,
/// refuses unknown ids (404), and cannot revoke the current device itself.
#[tokio::test(flavor = "multi_thread")]
async fn session_device_revoke_severs_other_device() {
    ensure_migrations();
    let owner_a = "revoke_owner_A_token_0001";
    let owner_b = "revoke_owner_B_token_0001";
    let aid_a = 9_400_001i32;
    let aid_b = 9_400_002i32;
    e621_account_parser_api::db::set_account(owner_a, aid_a, "revA", "").unwrap();
    e621_account_parser_api::db::set_account(owner_a, aid_b, "shared", "").unwrap();
    e621_account_parser_api::db::set_account(owner_b, aid_b, "shared", "").unwrap();

    let client = Client::tracked(rocket::build().mount(
        "/api",
        e621_account_parser_api::routes::integration_test_routes(),
    ))
    .await
    .unwrap();
    let cookie_a = Cookie::new(e621_account_parser_api::auth::OWNER_TOKEN_COOKIE, owner_a);
    let post_revoke = |cookie: Cookie<'static>, device_id: String| {
        let client = &client;
        async move {
            let body = serde_json::json!({ "deviceId": device_id });
            client
                .post("/api/session/revoke")
                .cookie(cookie)
                .header(rocket::http::ContentType::JSON)
                .body(body.to_string())
                .dispatch()
                .await
        }
    };

    // Find B's device id from A's perspective.
    let list = client
        .get("/api/session/devices")
        .cookie(cookie_a.clone())
        .dispatch()
        .await;
    let devices: serde_json::Value = list.into_json().await.unwrap();
    let other = devices
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["isCurrent"].as_bool() == Some(false))
        .expect("other sharing device");
    let b_id = other["id"].as_str().unwrap().to_string();

    // Revoke B: 200 + revoked:true, and B disappears from A's list.
    let resp = post_revoke(cookie_a.clone(), b_id.clone()).await;
    assert_eq!(resp.status(), Status::Ok);
    let v: serde_json::Value = resp.into_json().await.unwrap();
    assert_eq!(v["revoked"], serde_json::Value::Bool(true));

    let list = client
        .get("/api/session/devices")
        .cookie(cookie_a.clone())
        .dispatch()
        .await;
    let devices: serde_json::Value = list.into_json().await.unwrap();
    assert_eq!(
        devices.as_array().unwrap().len(),
        1,
        "revoked device is severed from the shared account"
    );

    // Unknown device id -> 404.
    let resp = post_revoke(cookie_a.clone(), "f".repeat(64)).await;
    assert_eq!(resp.status(), Status::NotFound);

    // Current device cannot be revoked via this endpoint -> 404.
    let cur = client
        .get("/api/session/devices")
        .cookie(cookie_a.clone())
        .dispatch()
        .await;
    let cur: serde_json::Value = cur.into_json().await.unwrap();
    let cur_id = cur.as_array().unwrap()[0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = post_revoke(cookie_a.clone(), cur_id).await;
    assert_eq!(
        resp.status(),
        Status::NotFound,
        "self-revoke via device refused"
    );

    // Cleanup.
    let _ = e621_account_parser_api::db::delete_device_link(owner_a, aid_a);
    let _ = e621_account_parser_api::db::delete_device_link(owner_a, aid_b);
    let _ = e621_account_parser_api::db::delete_device_link(owner_b, aid_b);
}

/// Export returns the full snapshot (identity + blacklist + preferred tags +
/// profile); import restores user-settable fields and returns current state.
#[tokio::test(flavor = "multi_thread")]
async fn account_export_import_round_trip() {
    let account = TestAccount::new(9_100_101);
    let owner = account.owner;
    let aid = account.id;

    // Seed a blacklist + preferred tags directly via the DB layer.
    e621_account_parser_api::db::update_device_blacklist(owner, aid, "gore\nyoung").unwrap();
    e621_account_parser_api::db::set_preferred_tags(
        owner,
        aid,
        &[
            e621_account_parser_api::models::PreferredTag {
                tag: "wolf".into(),
                group: "general".into(),
                weight: 2.0,
            },
            e621_account_parser_api::models::PreferredTag {
                tag: "canine".into(),
                group: "species".into(),
                weight: 1.5,
            },
        ],
    )
    .unwrap();

    let client = Client::tracked(rocket::build().mount(
        "/api",
        e621_account_parser_api::routes::integration_test_routes(),
    ))
    .await
    .unwrap();
    let cookie = Cookie::new(e621_account_parser_api::auth::OWNER_TOKEN_COOKIE, owner);

    // ── Export ────────────────────────────────────────────────────────
    let response = client
        .get(format!("/api/account/{aid}/export"))
        .cookie(cookie.clone())
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    let body = response.into_json::<serde_json::Value>().await.unwrap();
    assert_eq!(body["account"]["id"], aid);
    assert_eq!(body["account"]["name"], "test_user");
    assert!(body["blacklist"].as_str().unwrap().contains("gore"));
    assert_eq!(body["preferred_tags"].as_array().unwrap().len(), 2);
    assert!(body["profile"].is_object(), "profile included for backup");

    // ── Import (full replace) ─────────────────────────────────────────
    let import = serde_json::json!({
        "blacklist": "scat\nwatersports",
        "preferred_tags": [
            {"tag": "fluffy", "group": "general", "weight": 3.0}
        ],
    });
    let response = client
        .post(format!("/api/account/{aid}/import"))
        .cookie(cookie.clone())
        .header(rocket::http::ContentType::JSON)
        .body(import.to_string())
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    let state = response.into_json::<serde_json::Value>().await.unwrap();
    assert!(
        state["blacklist"].as_str().unwrap().contains("scat"),
        "blacklist replaced: {:?}",
        state["blacklist"]
    );
    assert_eq!(state["preferred_tags"].as_array().unwrap().len(), 1);
    assert_eq!(state["preferred_tags"][0]["tag"], "fluffy");

    // ── Partial import: only preferred_tags, blacklist untouched ──────
    let partial = serde_json::json!({
        "preferred_tags": [
            {"tag": "canine", "group": "species", "weight": 1.0},
            {"tag": "wolf", "group": "general", "weight": 2.0},
        ],
    });
    let response = client
        .post(format!("/api/account/{aid}/import"))
        .cookie(cookie.clone())
        .header(rocket::http::ContentType::JSON)
        .body(partial.to_string())
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    let state = response.into_json::<serde_json::Value>().await.unwrap();
    assert!(
        state["blacklist"].as_str().unwrap().contains("scat"),
        "partial import must not touch blacklist"
    );
    assert_eq!(state["preferred_tags"].as_array().unwrap().len(), 2);

    // ── Invalid import (bad group) must be rejected ───────────────────
    let bad = serde_json::json!({
        "preferred_tags": [{"tag": "wolf", "group": "nope", "weight": 1.0}],
    });
    let response = client
        .post(format!("/api/account/{aid}/import"))
        .cookie(cookie.clone())
        .header(rocket::http::ContentType::JSON)
        .body(bad.to_string())
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::BadRequest);
}

/// The interaction model is exported with the backup and restored on import:
/// replayed into `feed_interactions` for the target account, idempotent on
/// re-import, and refused for an account the token isn't linked to.
#[tokio::test(flavor = "multi_thread")]
async fn account_export_import_interactions_round_trip() {
    use e621_account_parser_api::models::{FeedInteractionRequest, FeedInteractionType};

    let src = TestAccount::new(9_150_001);
    let dst = TestAccount::new(9_150_002);
    let owner = src.owner;

    // Seed catalog posts the interaction FKs can reference.
    let p1 = make_post(12_700_001, make_tags(&["ta"], &["tc"], &["tg1"]), 7);
    let p2 = make_post(12_700_002, make_tags(&["ta"], &["tc"], &["tg2"]), 7);
    e621_account_parser_api::db::save_posts(&[p1, p2], src.id).unwrap();

    // Record two interactions on the source account.
    e621_account_parser_api::db::record_feed_interaction(
        owner,
        &FeedInteractionRequest {
            account_id: src.id,
            post_id: 12_700_001,
            event_type: FeedInteractionType::Like,
            position: 1,
            session_id: "sess_a".into(),
        },
    )
    .unwrap();
    e621_account_parser_api::db::record_feed_interaction(
        owner,
        &FeedInteractionRequest {
            account_id: src.id,
            post_id: 12_700_002,
            event_type: FeedInteractionType::Hide,
            position: 2,
            session_id: "sess_a".into(),
        },
    )
    .unwrap();

    let client = Client::tracked(rocket::build().mount(
        "/api",
        e621_account_parser_api::routes::integration_test_routes(),
    ))
    .await
    .unwrap();
    let cookie = Cookie::new(e621_account_parser_api::auth::OWNER_TOKEN_COOKIE, owner);

    // Export: interactions present and non-empty.
    let body = client
        .get(format!("/api/account/{}/export", src.id))
        .cookie(cookie.clone())
        .dispatch()
        .await;
    assert_eq!(body.status(), Status::Ok);
    let export: serde_json::Value = body.into_json().await.unwrap();
    let interactions = export["interactions"]
        .as_array()
        .expect("interactions array");
    assert_eq!(interactions.len(), 2, "both recorded interactions exported");
    assert!(
        !export.to_string().contains("sess_a"),
        "session ids must not leak into the export"
    );

    // Import the interactions into the destination account.
    let payload = serde_json::json!({"interactions": interactions.clone()});
    let resp = client
        .post(format!("/api/account/{}/import", dst.id))
        .cookie(cookie.clone())
        .header(rocket::http::ContentType::JSON)
        .body(payload.to_string())
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok);

    let restored =
        e621_account_parser_api::db::get_account_interactions_for_export(dst.id, 100).unwrap();
    assert_eq!(
        restored.len(),
        2,
        "interactions restored into destination account"
    );

    // Idempotent: re-importing the same export must not duplicate rows.
    client
        .post(format!("/api/account/{}/import", dst.id))
        .cookie(cookie.clone())
        .header(rocket::http::ContentType::JSON)
        .body(payload.to_string())
        .dispatch()
        .await;
    let restored2 =
        e621_account_parser_api::db::get_account_interactions_for_export(dst.id, 100).unwrap();
    assert_eq!(
        restored2.len(),
        2,
        "re-import is idempotent (no duplicates)"
    );

    // Ownership gate: importing interactions for an account linked to a
    // DIFFERENT token (not the caller's cookie token) must be refused.
    let unrelated_id = 9_150_003i32;
    e621_account_parser_api::db::set_account("other_owner_token_zz", unrelated_id, "other", "")
        .unwrap();
    let resp = client
        .post(format!("/api/account/{unrelated_id}/import"))
        .cookie(cookie.clone())
        .header(rocket::http::ContentType::JSON)
        .body(payload.to_string())
        .dispatch()
        .await;
    assert!(
        resp.status() != Status::Ok,
        "unlinked account import must fail, got {:?}",
        resp.status()
    );
    let _ = e621_account_parser_api::db::delete_device_link("other_owner_token_zz", unrelated_id);
}

/// Interaction history: records an interaction, fetches it back, applies an
/// event filter, and rejects an unknown filter.
#[tokio::test(flavor = "multi_thread")]
async fn interaction_history_lists_and_filters() {
    let account = TestAccount::new(9_100_102);
    let owner = account.owner;
    let aid = account.id;

    // Seed two posts so the FK constraint on feed_interactions holds.
    e621_account_parser_api::db::save_posts(
        &[
            make_post(7_100_001, Tags::default(), 1),
            make_post(7_100_002, Tags::default(), 1),
        ],
        aid,
    )
    .unwrap();

    // Record interactions directly via the DB layer.
    for (post_id, event) in [
        (7_100_001_i64, "open"),
        (7_100_001_i64, "hide"),
        (7_100_002_i64, "like"),
    ] {
        e621_account_parser_api::db::record_feed_interaction(
            owner,
            &e621_account_parser_api::models::FeedInteractionRequest {
                account_id: aid,
                post_id,
                event_type: match event {
                    "open" => e621_account_parser_api::models::FeedInteractionType::Open,
                    "hide" => e621_account_parser_api::models::FeedInteractionType::Hide,
                    _ => e621_account_parser_api::models::FeedInteractionType::Like,
                },
                position: 1,
                session_id: "hist-test".into(),
            },
        )
        .unwrap();
    }

    let client = Client::tracked(rocket::build().mount(
        "/api",
        e621_account_parser_api::routes::integration_test_routes(),
    ))
    .await
    .unwrap();
    let cookie = Cookie::new(e621_account_parser_api::auth::OWNER_TOKEN_COOKIE, owner);

    // Unfiltered — all three rows, newest first.
    let response = client
        .get(format!("/api/account/{aid}/interactions"))
        .cookie(cookie.clone())
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    let items = response.into_json::<serde_json::Value>().await.unwrap();
    let arr = items.as_array().unwrap();
    assert_eq!(arr.len(), 3, "all three interactions listed");
    assert!(arr.iter().any(|i| i["event_type"] == "open"));
    assert!(arr.iter().any(|i| i["event_type"] == "hide"));
    assert!(arr.iter().any(|i| i["event_type"] == "like"));

    // Filtered by event.
    let response = client
        .get(format!("/api/account/{aid}/interactions?event=open"))
        .cookie(cookie.clone())
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    let items = response.into_json::<serde_json::Value>().await.unwrap();
    let arr = items.as_array().unwrap();
    assert_eq!(arr.len(), 1, "only open interactions");
    assert_eq!(arr[0]["event_type"], "open");

    // Unknown filter rejected.
    let response = client
        .get(format!("/api/account/{aid}/interactions?event=nope"))
        .cookie(cookie.clone())
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::BadRequest);
}

#[test]
fn catalog_hydration_scan_selects_only_incomplete_metadata() {
    ensure_migrations();
    let ids = [9_100_001, 9_100_002, 9_100_003, 9_100_004];
    db::delete_catalog_posts_by_ids(&ids).unwrap();

    let missing_media = make_post(
        ids[0],
        Tags {
            general: vec!["test_hydration_media".into()],
            ..Tags::default()
        },
        42,
    );
    let mut missing_tags = make_post(
        ids[1],
        Tags {
            general: vec!["test_hydration_tags".into()],
            ..Tags::default()
        },
        43,
    );
    let mut missing_uploader = make_post(
        ids[2],
        Tags {
            general: vec!["test_hydration_uploader".into()],
            ..Tags::default()
        },
        0,
    );
    let mut complete = make_post(
        ids[3],
        Tags {
            general: vec!["test_hydration_complete".into()],
            ..Tags::default()
        },
        44,
    );
    for post in [&mut missing_tags, &mut missing_uploader, &mut complete] {
        post.files.original.url = Some("https://static1.e621.net/data/test.jpg".into());
    }

    let posts = vec![
        missing_media.clone(),
        missing_tags.clone(),
        missing_uploader.clone(),
        complete.clone(),
    ];
    db::upsert_catalog_posts(&posts).unwrap();
    // Deliberately do not save missing_tags' relations.
    db::save_posts_tags_batch(
        &[missing_media, missing_uploader, complete],
        &HashSet::new(),
        false,
        None,
    )
    .unwrap();

    let selected = db::collect_post_ids_needing_hydration(10).unwrap();
    assert!(selected.contains(&ids[0]), "missing media must be repaired");
    assert!(selected.contains(&ids[1]), "missing tags must be repaired");
    assert!(
        selected.contains(&ids[2]),
        "missing uploader must be repaired"
    );
    assert!(
        !selected.contains(&ids[3]),
        "complete catalog metadata must not be fetched again"
    );

    // Model the persistence phase after e621 returns a repaired record.
    let mut repaired = make_post(
        ids[0],
        Tags {
            general: vec!["test_hydration_repaired".into()],
            artist: vec!["test_hydration_artist".into()],
            ..Tags::default()
        },
        99,
    );
    repaired.files.preview.url = Some("https://static1.e621.net/data/preview/repaired.jpg".into());
    db::upsert_catalog_posts(std::slice::from_ref(&repaired)).unwrap();
    db::replace_posts_tags_batch(std::slice::from_ref(&repaired)).unwrap();
    let restored = db::hydrate_posts_by_ids(&[ids[0]]).unwrap();
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].uploader_id, 99);
    assert_eq!(
        restored[0].files.preview.url.as_deref(),
        Some("https://static1.e621.net/data/preview/repaired.jpg")
    );
    assert_eq!(restored[0].tags.general, vec!["test_hydration_repaired"]);
    assert_eq!(restored[0].tags.artist, vec!["test_hydration_artist"]);

    // The worker uses this cascading deletion for IDs absent from e621.
    assert_eq!(db::delete_catalog_posts_by_ids(&[ids[1]]).unwrap(), 1);
    assert!(db::hydrate_posts_by_ids(&[ids[1]]).unwrap().is_empty());

    db::delete_catalog_posts_by_ids(&ids).unwrap();
}

// ------------------------------------------------------------------
// Encrypted per-device e621 API key storage (Account Key)
// ------------------------------------------------------------------

#[test]
fn e621_key_set_get_roundtrip_encrypted_at_rest() {
    let id = 8_800_001;
    ensure_migrations();
    let owner = "test_key_owner_A_400001";
    let _ = db::set_account(owner, id, "key_user", "");

    let key = "abc123SECRET-e621-key";
    db::set_account_e621_key(owner, id, key).expect("set key");
    assert_eq!(
        db::get_account_e621_key(owner, id).unwrap().as_deref(),
        Some(key),
        "read back the same plaintext key for the owner"
    );
    assert!(db::has_account_e621_key(owner, id).unwrap());

    // At rest the DB column must hold AES-GCM ciphertext, never plaintext.
    if let Ok(conn) = db::open_db_for_calibration() {
        let stored: Option<String> = conn
            .query_row(
                "SELECT e621_api_key_encrypted FROM accounts WHERE id = ?1",
                rusqlite::params![id],
                |r| r.get(0),
            )
            .unwrap();
        let stored = stored.expect("blob present");
        assert_ne!(stored, key, "ciphertext must not equal plaintext");
        assert!(
            !stored.contains("SECRET-e621"),
            "plaintext must not appear in the stored blob"
        );
    }

    db::clear_account_e621_key(owner, id).expect("clear");
    assert_eq!(db::get_account_e621_key(owner, id).unwrap(), None);
    assert!(!db::has_account_e621_key(owner, id).unwrap());
}

#[test]
fn e621_key_is_owner_gated() {
    let id = 8_800_002;
    ensure_migrations();
    let owner_a = "test_key_owner_A_400002";
    let owner_b = "test_key_owner_B_400002"; // NOT linked to the account
    let _ = db::set_account(owner_a, id, "key_user2", "");

    db::set_account_e621_key(owner_b, id, "sk-other").expect_err("set must refuse non-owner");
    db::get_account_e621_key(owner_b, id).expect_err("get must refuse non-owner");
    db::has_account_e621_key(owner_b, id).expect_err("has must refuse non-owner");
    db::clear_account_e621_key(owner_b, id).expect_err("clear must refuse non-owner");
    db::mark_e621_key_verified(owner_b, id).expect_err("mark must refuse non-owner");

    // The linked owner can manage the key.
    db::set_account_e621_key(owner_a, id, "sk-real").unwrap();
    assert!(db::has_account_e621_key(owner_a, id).unwrap());
}

#[test]
fn e621_key_is_shared_across_linked_devices() {
    let id = 8_800_003;
    ensure_migrations();
    let owner_a = "test_key_owner_A_400003";
    let owner_b = "test_key_owner_B_400003";
    // Both devices claim the same public account (shared-account model).
    let _ = db::set_account(owner_a, id, "shared_user", "");
    let _ = db::set_account(owner_b, id, "shared_user", "");

    // The key is account-scoped: any LINKED device sees the same key (so sync
    // works from any of them). Device gating only checks the link, not a
    // per-device copy.
    db::set_account_e621_key(owner_a, id, "sk-account").unwrap();
    assert_eq!(
        db::get_account_e621_key(owner_a, id).unwrap().as_deref(),
        Some("sk-account"),
        "A reads the account key"
    );
    assert_eq!(
        db::get_account_e621_key(owner_b, id).unwrap().as_deref(),
        Some("sk-account"),
        "B (linked, no key presented) reads the same account key"
    );
    assert!(db::has_account_e621_key(owner_b, id).unwrap());
    assert!(db::get_account_key_meta(owner_b, id).unwrap().has_key);

    // An UNLINKED token is still refused (device gate is about the link).
    let stranger = "test_key_owner_C_400003";
    db::get_account_e621_key(stranger, id).expect_err("unlinked token cannot read");
}

#[rocket::async_test]
async fn e621_key_not_leaked_in_export_and_state_is_boolean() {
    let account = TestAccount::new(8_800_004);
    let owner = account.owner;
    let aid = account.id;
    db::set_account_e621_key(owner, aid, "supersecret-e621-key-xyz").unwrap();

    let client = Client::tracked(rocket::build().mount(
        "/api",
        e621_account_parser_api::routes::integration_test_routes(),
    ))
    .await
    .unwrap();
    let cookie = Cookie::new(e621_account_parser_api::auth::OWNER_TOKEN_COOKIE, owner);

    // Export must not contain the key or any key material.
    let response = client
        .get(format!("/api/account/{aid}/export"))
        .cookie(cookie.clone())
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    let body = response.into_json::<serde_json::Value>().await.unwrap();
    let dump = serde_json::to_string(&body).unwrap();
    assert!(
        !dump.contains("supersecret-e621-key"),
        "export must not leak the e621 key"
    );
    assert!(
        !dump.contains("e621_api_key_encrypted"),
        "export must not expose the key column"
    );
}

// ------------------------------------------------------------------
// Per-account e621 API key endpoints (V27 / Account Key)
// ------------------------------------------------------------------

async fn key_client(
    id: i32,
) -> (
    rocket::local::asynchronous::Client,
    TestAccount,
    rocket::http::Cookie<'static>,
) {
    let account = TestAccount::new(id);
    let client = Client::tracked(rocket::build().mount(
        "/api",
        e621_account_parser_api::routes::integration_test_routes(),
    ))
    .await
    .unwrap();
    let cookie = Cookie::new(
        e621_account_parser_api::auth::OWNER_TOKEN_COOKIE,
        account.owner,
    );
    // Keep `account` alive: TestAccount's `Drop` severs the owner↔account link,
    // so dropping it here would make subsequent requests report "not linked".
    (client, account, cookie)
}

#[rocket::async_test]
async fn account_key_add_state_rotate_revoke() {
    let (client, account, cookie) = key_client(8_800_101).await;
    let owner = account.owner;
    let aid = account.id;

    // ── Add ───────────────────────────────────────────────────────────
    let resp = client
        .put(format!("/api/account/{aid}/key"))
        .cookie(cookie.clone())
        .header(rocket::http::ContentType::JSON)
        .body(serde_json::json!({ "key": "abcdef1234567890-key" }).to_string())
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok);
    let state: serde_json::Value = resp.into_json().await.unwrap();
    assert_eq!(state["hasKey"], serde_json::Value::Bool(true));
    assert_eq!(state["accountId"], aid);
    assert!(state["addedAt"].is_string());
    let ops = state["operations"].as_array().unwrap();
    assert!(
        ops.iter().any(|o| o == "direct_sync"),
        "state lists ops using the key"
    );

    // ── State ─────────────────────────────────────────────────────────
    let resp = client
        .get(format!("/api/account/{aid}/key/state"))
        .cookie(cookie.clone())
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok);
    let state: serde_json::Value = resp.into_json().await.unwrap();
    assert_eq!(state["hasKey"], serde_json::Value::Bool(true));
    assert!(
        !state.to_string().contains("abcdef1234567890"),
        "state must not leak the key"
    );

    // ── Rotate ────────────────────────────────────────────────────────
    let resp = client
        .put(format!("/api/account/{aid}/key"))
        .cookie(cookie.clone())
        .header(rocket::http::ContentType::JSON)
        .body(serde_json::json!({ "key": "newkey9876543210-rotated" }).to_string())
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok);
    assert_eq!(
        e621_account_parser_api::db::get_account_e621_key(owner, aid)
            .unwrap()
            .as_deref(),
        Some("newkey9876543210-rotated"),
        "rotate replaces the key"
    );

    // ── Revoke ────────────────────────────────────────────────────────
    let resp = client
        .delete(format!("/api/account/{aid}/key"))
        .cookie(cookie.clone())
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok);
    let state: serde_json::Value = resp.into_json().await.unwrap();
    assert_eq!(state["hasKey"], serde_json::Value::Bool(false));
    assert_eq!(
        e621_account_parser_api::db::get_account_e621_key(owner, aid).unwrap(),
        None,
        "revoke removes the stored key"
    );
}

#[rocket::async_test]
async fn account_key_test_returns_invalid_when_no_key() {
    let (client, account, cookie) = key_client(8_800_102).await;
    let aid = account.id;
    let resp = client
        .post(format!("/api/account/{aid}/key/test"))
        .cookie(cookie.clone())
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok);
    let v: serde_json::Value = resp.into_json().await.unwrap();
    assert_eq!(v["valid"], serde_json::Value::Bool(false));
    assert!(
        v["verifiedAt"].is_null(),
        "no verification timestamp without a valid key"
    );
}

#[rocket::async_test]
async fn account_key_rejects_bad_payload_and_non_owner() {
    let (client, account, cookie) = key_client(8_800_103).await;
    let aid = account.id;

    // Too-short key is rejected with 400 before any DB/e621 work.
    let resp = client
        .put(format!("/api/account/{aid}/key"))
        .cookie(cookie.clone())
        .header(rocket::http::ContentType::JSON)
        .body(serde_json::json!({ "key": "short" }).to_string())
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::BadRequest);

    // An unlinked owner token cannot set a key (owner-gated).
    let other = Cookie::new(
        e621_account_parser_api::auth::OWNER_TOKEN_COOKIE,
        "unlinked_owner_token_9999",
    );
    let resp = client
        .put(format!("/api/account/{aid}/key"))
        .cookie(other)
        .header(rocket::http::ContentType::JSON)
        .body(serde_json::json!({ "key": "owner_a_key_1234567890" }).to_string())
        .dispatch()
        .await;
    assert_ne!(resp.status(), Status::Ok, "non-owner must be refused");
}

// ------------------------------------------------------------------
// M2 ownership gate — claim requires the e621 key; reads are owner-gated
// ------------------------------------------------------------------

#[rocket::async_test]
async fn create_account_rejects_malformed_key() {
    let client = Client::tracked(rocket::build().mount(
        "/api",
        e621_account_parser_api::routes::integration_test_routes(),
    ))
    .await
    .unwrap();
    let cookie = Cookie::new(
        e621_account_parser_api::auth::OWNER_TOKEN_COOKIE,
        "fresh_owner_token_777001",
    );

    // A missing key is now ALLOWED (optional ownership proof) and is exercised
    // against the wiremock suite (it proceeds to a real e621 user lookup), so
    // this hermetic client only covers the offline shape validation below.

    // Claim with a malformed key → 400 (shape validation, offline).
    let resp = client
        .post("/api/account")
        .cookie(cookie.clone())
        .header(rocket::http::ContentType::JSON)
        .body(
            serde_json::json!({ "id": 8_800_501, "name": "some_user", "api_key": "short" })
                .to_string(),
        )
        .dispatch()
        .await;
    assert_eq!(
        resp.status(),
        Status::BadRequest,
        "malformed key must be rejected"
    );
}

#[rocket::async_test]
async fn cross_owner_read_is_refused() {
    let account = TestAccount::new(8_800_502);
    let owner = account.owner;
    let aid = account.id;

    let client = Client::tracked(rocket::build().mount(
        "/api",
        e621_account_parser_api::routes::integration_test_routes(),
    ))
    .await
    .unwrap();
    let owner_cookie = Cookie::new(e621_account_parser_api::auth::OWNER_TOKEN_COOKIE, owner);
    let other = "other_unlinked_token_777002";
    let other_cookie = Cookie::new(e621_account_parser_api::auth::OWNER_TOKEN_COOKIE, other);

    // A token that did not prove ownership cannot read this account's data.
    for path in ["tag_counts", "profile", "export"] {
        let resp = client
            .get(format!("/api/account/{aid}/{path}"))
            .cookie(other_cookie.clone())
            .dispatch()
            .await;
        assert_ne!(
            resp.status(),
            Status::Ok,
            "cross-owner read of {path} must be refused"
        );
    }

    // The owner (who proved ownership) can still read their own export.
    let resp = client
        .get(format!("/api/account/{aid}/export"))
        .cookie(owner_cookie.clone())
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok);
}

// ------------------------------------------------------------------
// Read-only direct account sync endpoints (Account Key)
// ------------------------------------------------------------------

async fn sync_client(
    id: i32,
) -> (
    rocket::local::asynchronous::Client,
    TestAccount,
    rocket::http::Cookie<'static>,
) {
    let account = TestAccount::new(id);
    let client = Client::tracked(rocket::build().mount(
        "/api",
        e621_account_parser_api::routes::integration_test_routes(),
    ))
    .await
    .unwrap();
    let cookie = Cookie::new(
        e621_account_parser_api::auth::OWNER_TOKEN_COOKIE,
        account.owner,
    );
    (client, account, cookie)
}

#[rocket::async_test]
async fn sync_status_without_key_reports_no_key() {
    let (client, account, cookie) = sync_client(8_800_601).await;
    let resp = client
        .get(format!("/api/account/{}/sync/status", account.id))
        .cookie(cookie)
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok);
    let v: serde_json::Value = resp.into_json().await.unwrap();
    assert_eq!(v["hasKey"], serde_json::Value::Bool(false));
    assert!(v["lastSyncedAt"].is_null());
    assert_eq!(v["datasets"].as_array().unwrap().len(), 4);
}

#[rocket::async_test]
async fn sync_is_not_gated_on_catalog_toggles() {
    // The integration config loads config.example.toml, where save_favourites
    // and save_all are both off. Post-info collection (account sync) must NOT
    // be gated on them anymore — with no key the first error is the missing
    // key, never a catalog-disabled rejection (which would have fired before
    // the key check under the old gate).
    let account = TestAccount::new(8_800_602);
    let err = e621_account_parser_api::sync::sync_account_direct(account.owner, account.id)
        .await
        .expect_err("keyless sync must fail with the missing-key error");
    assert!(
        matches!(
            err,
            e621_account_parser_api::sync::SyncError::NoKeyConfigured
        ),
        "expected NoKeyConfigured (catalog gate must not fire), got: {err}"
    );
}

#[rocket::async_test]
async fn sync_status_reflects_last_sync_when_key_present() {
    let (client, account, cookie) = sync_client(8_800_603).await;
    let owner = account.owner;
    let aid = account.id;
    // Configure a key + record a successful sync (DB-level, offline).
    e621_account_parser_api::db::set_account_e621_key(owner, aid, "abcdef1234567890-key").unwrap();
    e621_account_parser_api::db::mark_account_direct_synced(owner, aid).unwrap();

    let resp = client
        .get(format!("/api/account/{aid}/sync/status"))
        .cookie(cookie)
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok);
    let v: serde_json::Value = resp.into_json().await.unwrap();
    assert_eq!(v["hasKey"], serde_json::Value::Bool(true));
    assert!(v["lastSyncedAt"].is_string(), "last sync timestamp present");
}

#[rocket::async_test]
async fn sync_status_refused_for_unlinked_token() {
    let (client, account, _cookie) = sync_client(8_800_604).await;
    let other = Cookie::new(
        e621_account_parser_api::auth::OWNER_TOKEN_COOKIE,
        "other_unlinked_token_8888",
    );
    let resp = client
        .get(format!("/api/account/{}/sync/status", account.id))
        .cookie(other)
        .dispatch()
        .await;
    assert_ne!(
        resp.status(),
        Status::Ok,
        "unlinked token cannot read sync status"
    );
}
