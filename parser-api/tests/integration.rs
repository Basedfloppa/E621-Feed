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
    let posts2 = vec![
        make_post(11003, make_tags(&["skeb"], &["cat"], &["furry"]), 42),
    ];
    db::save_posts(&posts2, account_id).unwrap();
    db::save_posts_tags_batch(&posts2, &blacklist, true, Some(account_id)).unwrap();

    let count2 = count_account_cooc(account_id);
    assert!(count2 >= count, "cooc should persist across batches ({count} → {count2})");

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

    let posts3 = vec![
        make_post(11001, make_tags(&["skeb"], &["cat"], &["furry"]), 42),
    ];
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
    let _ = e621_account_parser_api::db::drop_account_cooccurrence_batched(
        account_id,
        1024,
        |_, _| {},
    );
    // Drop any stale feed_sessions a previous panicking run may have
    // left behind. Without this, the Fresh→Active→Expired tests are
    // order-dependent: a leftover row makes the first call return
    // Active instead of Fresh.
    if let Ok(conn) = e621_account_parser_api::db::open_db_for_calibration() {
        let _ = conn.execute(
            "DELETE FROM feed_sessions WHERE account_id = ?1",
            rusqlite::params![account_id],
        );
    }
}

/// RAII guard that owns a test account_id and (`owner_token`,
/// `account_id`) link for the lifetime of the test. On drop — even on
/// panic — `delete_device_link` runs the full cascade, batched cooc /
/// feed_interactions wipes included.
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
        let owner = "test_owner";
        let _ = e621_account_parser_api::db::set_account(owner, id, "test_user", "");
        let _ = e621_account_parser_api::db::drop_account_posts(id);
        let _ = e621_account_parser_api::db::drop_account_cooccurrence_batched(
            id,
            1024,
            |_, _| {},
        );
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
        make_tags(&["test_artist"], &["test_char"], &["test_general1", "test_general2"]),
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
    let state = db::touch_or_create_feed_session(session_id, 90007).unwrap();
    assert_eq!(
        state,
        db::FeedSessionState::Fresh,
        "different account_id must get its own Fresh row"
    );

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
        make_tags(
            &["a"],
            &[],
            &["g1", "g2", "g3", "g4", "g5"],
        ),
        42,
    );
    db::save_posts(std::slice::from_ref(&p), account_id).unwrap();
    db::save_posts_tags_batch(
        std::slice::from_ref(&p),
        &blacklist,
        true,
        Some(account_id),
    )
    .unwrap();

    let before = count_account_cooc(account_id);
    assert!(before >= 10, "expected ≥10 cooc rows, got {before}");

    // Drive batching by forcing batch_size=4 → at least 3 iterations.
    use std::cell::Cell;
    let calls = Cell::new(0usize);
    let total_seen = Cell::new(0usize);
    let deleted = db::drop_account_cooccurrence_batched(
        account_id,
        4,
        |batch, total| {
            calls.set(calls.get() + 1);
            total_seen.set(total);
            assert!(batch <= 4, "batch must respect batch_size, got {batch}");
        },
    )
    .unwrap();

    assert_eq!(
        deleted as i64, before,
        "total deleted must equal initial row count"
    );
    assert!(
        calls.get() >= 2,
        "should have fired ≥2 batches for {before} rows at batch_size=4, got {}",
        calls.get()
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
/// had triggered the global branch. With duplicate tag_ids (rare but
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
        make_post(
            14001,
            make_tags(&["a"], &["c"], &["g1", "g2"]),
            42,
        ),
        make_post(
            14002,
            make_tags(&["a"], &["c"], &["g1", "g3"]),
            42,
        ),
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
    db::save_posts_tags_batch(
        std::slice::from_ref(&p),
        &blacklist,
        true,
        Some(account_id),
    )
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

    db::record_session_shown_posts(session_id, &[(1001, 0), (1002, 1), (1003, 2)]).unwrap();
    db::record_session_shown_posts(session_id, &[(1004, 0)]).unwrap();
    // Duplicate (session_id, post_id) — must be silently ignored by the
    // INSERT OR IGNORE in the writer.
    db::record_session_shown_posts(session_id, &[(1001, 10)]).unwrap();

    let shown = db::get_session_shown_post_ids(session_id).unwrap();
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
/// the feed_interactions helper the same way so a future change to the
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
                rusqlite::params![
                    acc.id,
                    16001_i64,
                    i,
                    format!("test-sess-{i}"),
                    now
                ],
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
///   1. drop the device_link, decide whether to cascade
///   2. batched cooc + feed_interactions wipe (outside the cascade tx)
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
