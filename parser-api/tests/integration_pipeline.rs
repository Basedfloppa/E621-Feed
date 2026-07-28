//! End-to-end pipeline tests against a mock e621.
//!
//! These tests stand up a wiremock server, point `cfg().posts_domain`
//! at it, then drive `pipeline::run_process` and `pipeline::strip_blacklisted_tags`
//! through the real DB layer. They cover the user stories that touch
//! the e621 client (account analysis / favourites download) — for the
//! pure-DB stories see `tests/integration.rs`.
//!
//! Concurrency: `cfg()` is a process-wide `ArcSwap<Config>`, so tests
//! that mutate it serialize through `PIPELINE_LOCK`. Tests run on a
//! single tokio runtime per `#[tokio::test]` invocation but the lock
//! is shared across all of them.

mod support;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::OnceLock;

use e621_account_parser_api::jobs::{self, ProcessJobPhase};
use e621_account_parser_api::pipeline;
use e621_account_parser_api::{api, db};
use rocket::http::{Cookie, Status};
use rocket::local::asynchronous::Client;
use tokio::sync::Mutex;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

// Pipeline tests mutate the process-wide config to point at a mock
// server. Serialize so two concurrent tests can't see each other's
// `posts_domain`.
fn pipeline_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Write a minimal toml config that points at the given e621 URL, then
/// reload it into the global `CONFIG`. Uses the project's own
/// `config.example.toml` as a base so every required field is present
/// — we only override `posts_domain`, `posts_limit`, and
/// `process_fetch_concurrency`.
fn install_mock_config(mock_uri: &str) -> tempfile::NamedTempFile {
    let example = std::fs::read_to_string(example_config_path()).expect("read config.example.toml");

    // Preserve the process-isolated database installed by `support` before
    // swapping in the mock e621 endpoint. The read pool is lazily created
    // after this helper, so reverting to config.example's database.db would
    // split reads from writes.
    let modified = swap_toml_field(
        &example,
        "db_path",
        &format!("\"{}\"", e621_account_parser_api::models::cfg().db_path),
    );
    // posts_domain
    let modified = swap_toml_field(&modified, "posts_domain", &format!("\"{mock_uri}\""));
    // posts_limit — keep it small so tests fabricate fewer posts.
    let modified = swap_toml_field(&modified, "posts_limit", "4");
    // process_fetch_concurrency — keep it 1 so ordering is deterministic.
    let modified = swap_toml_field(&modified, "process_fetch_concurrency", "1");
    // admin creds — placeholder values; mock server ignores auth.
    let modified = swap_toml_field(&modified, "admin_user", "\"test_admin\"");
    let modified = swap_toml_field(&modified, "admin_api", "\"test_api_key\"");

    let mut file = tempfile::NamedTempFile::new().expect("temp file");
    use std::io::Write;
    file.write_all(modified.as_bytes())
        .expect("write temp config");
    file.flush().expect("flush temp config");

    e621_account_parser_api::models::reload_from(file.path()).expect("reload config");
    file
}

/// Replace `key = ...` line in a TOML doc with `key = new`. Naive — only
/// matches the first occurrence of `^key\s*=`, which is all we need
/// because the example file has one definition per key.
fn swap_toml_field(toml: &str, key: &str, new_value: &str) -> String {
    let mut out = String::with_capacity(toml.len());
    let mut replaced = false;
    for line in toml.lines() {
        if !replaced {
            let trimmed = line.trim_start();
            if trimmed.starts_with(key) && trimmed[key.len()..].trim_start().starts_with('=') {
                out.push_str(&format!("{key} = {new_value}\n"));
                replaced = true;
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    assert!(
        replaced,
        "key '{key}' not found in config.example.toml — pipeline-test setup is broken"
    );
    out
}

fn example_config_path() -> PathBuf {
    // tests/ → up to crate root → config.example.toml
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config.example.toml")
}

fn ensure_migrations() {
    support::install_isolated_db_config();
    db::ensure_sqlite().expect("DB migrations failed");
}

/// Build a canonical-shape `Post` JSON value that e621 would return.
/// Only the fields the parser actually reads need real values; the
/// rest fill in safe defaults so deserialisation succeeds.
fn fake_post_json(id: i64, artist: &[&str], general: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "created_at": "2024-01-01T00:00:00.000-08:00",
        "updated_at": "2024-01-01T00:00:00.000-08:00",
        "files": {
            "meta": {
                "md5": "0".repeat(32), "ext": "jpg", "size": 12345,
                "duration": null, "has_sample": false,
            },
            "original": {
                "width": 800, "height": 600,
                "url": format!("https://e6.example/data/{id}.jpg"),
            },
            "preview": {
                "width": 150, "height": 100,
                "jpg": format!("https://e6.example/data/preview/{id}.jpg"),
                "webp": null,
            },
            "sample": {"width": 0, "height": 0, "jpg": null, "webp": null},
        },
        "change_seq": 0.0,
        "uploader_id": 42,
        "uploader_name": null,
        "approver_id": null,
        "stats": {
            "score": {"up": 10, "down": 0, "total": 10},
            "fav_count": 5, "is_favorited": false, "vote": 0, "comment_count": 0,
        },
        "flags": {
            "pending": false, "flagged": false, "note_locked": false,
            "status_locked": false, "rating_locked": false, "deleted": false,
        },
        "has": {
            "parent": false, "children": false, "active_children": false,
            "notes": false, "sample": false,
        },
        "relationships": {"parent_id": null, "children": []},
        "pools": [],
        "rating": "s",
        "locked_tags": [],
        "sources": [],
        "description": null,
        "tags": {
            "general": general,
            "artist": artist,
            "copyright": [],
            "character": [],
            "species": [],
            "invalid": [],
            "meta": [],
            "lore": [],
            "contributor": [],
        },
    })
}

fn fake_user_json(id: i32, favorite_count: i32) -> serde_json::Value {
    // Match `FullUser` shape — fields the parser doesn't read get
    // placeholder values that satisfy the deserialiser.
    serde_json::json!({
        "id": id,
        "created_at": "2020-01-01T00:00:00.000-08:00",
        "name": "test_user",
        "level": 20,
        "base_upload_limit": 10,
        "post_upload_count": 0,
        "post_update_count": 0,
        "note_update_count": 0,
        "is_banned": false,
        "can_approve_posts": false,
        "can_upload_free": false,
        "level_string": "Member",
        "avatar_id": null,
        "wiki_page_version_count": 0,
        "artist_version_count": 0,
        "pool_version_count": 0,
        "forum_post_count": 0,
        "comment_count": 0,
        "flag_count": 0,
        "favorite_count": favorite_count,
        "positive_feedback_count": 0,
        "neutral_feedback_count": 0,
        "negative_feedback_count": 0,
        "upload_limit": 10,
        "profile_about": "",
        "profile_artinfo": "",
    })
}

/// Wipe an account, its derived state, AND any catalog posts that were
/// linked to it. Tests share the production `posts` table, so without
/// the second step a pipeline test that imports post 60010 leaks that
/// post into every subsequent test reading from `posts` directly (e.g.
/// `find_similar_post_ids`, `get_trending_posts`).
fn wipe_account(id: i32) {
    // 1. Collect post_ids this account owned so we can drop their
    //    catalog rows after `drop_account_posts` clears the join table.
    let owned_post_ids: Vec<i64> = if let Ok(conn) = db::open_db_for_calibration() {
        let mut stmt = conn
            .prepare("SELECT post_id FROM accounts_post WHERE account_id = ?1")
            .unwrap();
        stmt.query_map(rusqlite::params![id], |r| r.get::<_, i64>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let _ = db::drop_account_posts(id);
    let _ = db::drop_account_cooccurrence_batched(id, 1024, |_, _| {});
    let _ = db::drop_account_feed_interactions_batched(id, 1024, |_, _| {});

    // Force-remove via direct SQL since delete_device_link refuses if no
    // device link exists for the given owner. Tests bypass owner checks.
    if let Ok(conn) = db::open_db_for_calibration() {
        for stmt in [
            "DELETE FROM account_device_links WHERE account_id = ?1",
            "DELETE FROM account_tag_counts WHERE account_id = ?1",
            "DELETE FROM account_rating_profile WHERE account_id = ?1",
            "DELETE FROM account_media_profile WHERE account_id = ?1",
            "DELETE FROM account_quality_profile WHERE account_id = ?1",
            "DELETE FROM account_tag_feedback WHERE account_id = ?1",
            "DELETE FROM feed_sessions WHERE account_id = ?1",
            "DELETE FROM feed_session_posts WHERE session_id IN (SELECT session_id FROM feed_sessions WHERE account_id = ?1)",
            "DELETE FROM accounts WHERE id = ?1",
        ] {
            let _ = conn.execute(stmt, rusqlite::params![id]);
        }
        // 2. Drop the catalog posts this account contributed. CASCADE
        //    handles `tags_posts` etc.
        for pid in &owned_post_ids {
            let _ = conn.execute("DELETE FROM posts WHERE id = ?1", rusqlite::params![pid]);
        }
    }
}

fn count_table_for_account(table: &str, account_id: i32) -> i64 {
    let conn = db::open_db_for_calibration().unwrap();
    let sql = format!("SELECT COUNT(*) FROM {table} WHERE account_id = ?1");
    conn.query_row(&sql, rusqlite::params![account_id], |r| r.get(0))
        .unwrap()
}

// ==================================================================
//  User story 1 — "Analyze my account": full /process pipeline
// ==================================================================

/// Happy path: e621 reports 6 favourites split across 2 pages of 4
/// (posts_limit=4). After `run_process` the catalog has the posts,
/// `accounts_post` links them all, tag-counts profile is populated,
/// `pages_total = pages_done = 2`, and the job state is `Done`.
#[tokio::test(flavor = "multi_thread")]
async fn analyze_account_happy_path() {
    let _guard = pipeline_lock().lock().await;
    ensure_migrations();
    let server = MockServer::start().await;
    let _cfg = install_mock_config(&server.uri());

    let account_id = 91001;
    wipe_account(account_id);

    // /users/<id>.json → 6 favourites total.
    Mock::given(method("GET"))
        .and(path(format!("/users/{account_id}.json")))
        .respond_with(ResponseTemplate::new(200).set_body_json(fake_user_json(account_id, 6)))
        .mount(&server)
        .await;

    // Page 1: 4 posts. Page 2: 2 posts.
    Mock::given(method("GET"))
        .and(path("/favorites.json"))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            fake_post_json(20001, &["artist_a"], &["fluffy", "outdoor"]),
            fake_post_json(20002, &["artist_a"], &["fluffy", "indoor"]),
            fake_post_json(20003, &["artist_b"], &["scaly", "outdoor"]),
            fake_post_json(20004, &["artist_b"], &["scaly", "indoor"]),
        ])))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/favorites.json"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            fake_post_json(20005, &["artist_c"], &["fluffy", "night"]),
            fake_post_json(20006, &["artist_c"], &["scaly", "night"]),
        ])))
        .mount(&server)
        .await;

    // Account must exist (linked) before `run_process` because
    // `get_account_by_id` is the auth gate.
    db::set_account("pipeline_owner", account_id, "test_user", "").unwrap();
    jobs::try_begin(account_id); // mark Running

    pipeline::run_process(account_id, "pipeline_owner".to_string())
        .await
        .expect("pipeline completes");
    jobs::finish(account_id, Ok(()));

    // ── Post-conditions ─────────────────────────────────────────
    let state = jobs::get_state(account_id).expect("job state recorded");
    assert_eq!(state.phase, ProcessJobPhase::Done);
    assert_eq!(state.pages_total, 2);
    assert_eq!(state.pages_done, 2, "every page must be marked done");
    assert!(
        state.error.is_none(),
        "no error on the happy path: {:?}",
        state.error
    );

    assert_eq!(
        count_table_for_account("accounts_post", account_id),
        6,
        "all 6 favourites linked"
    );
    assert!(
        count_table_for_account("account_tag_counts", account_id) > 0,
        "tag-counts profile must be populated"
    );
    assert!(
        count_table_for_account("account_tag_cooccurrence", account_id) > 0,
        "cooc must be built incrementally during fetch_and_save"
    );

    // Phase order recorded.
    let phase_names: Vec<&str> = state.phases.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        phase_names,
        vec![
            "init",
            "drop_old",
            "drop_cooc",
            "fetch_and_save",
            "profile_refresh"
        ]
    );

    wipe_account(account_id);
}

/// e621 returns 500 for the user lookup. Pipeline must surface the
/// error (no partial state), and the caller can mark `Failed` —
/// importantly, the previous teardown phases (drop_old, drop_cooc)
/// should NOT have run.
#[tokio::test(flavor = "multi_thread")]
async fn analyze_account_e621_user_error_returns_err() {
    let _guard = pipeline_lock().lock().await;
    ensure_migrations();
    let server = MockServer::start().await;
    let _cfg = install_mock_config(&server.uri());

    let account_id = 91002;
    wipe_account(account_id);

    Mock::given(method("GET"))
        .and(path(format!("/users/{account_id}.json")))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;

    db::set_account("pipeline_owner", account_id, "test_user", "").unwrap();

    // Seed cooc so we can assert teardown DID NOT run.
    let p = e621_account_parser_api::models::Post {
        id: 30001,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        change_seq: 0.0,
        files: e621_account_parser_api::models::Files::default(),
        uploader_id: 42,
        uploader_name: None,
        approver_id: None,
        stats: e621_account_parser_api::models::Stats {
            score: e621_account_parser_api::models::Score {
                up: 1,
                down: 0,
                total: 1,
            },
            ..Default::default()
        },
        flags: e621_account_parser_api::models::Flags::default(),
        has: e621_account_parser_api::models::Has::default(),
        relationships: e621_account_parser_api::models::Relationships::default(),
        pools: vec![],
        rating: e621_account_parser_api::models::Rating::S,
        locked_tags: vec![],
        sources: vec![],
        description: None,
        tags: e621_account_parser_api::models::Tags {
            general: vec!["a".into(), "b".into()],
            ..Default::default()
        },
    };
    db::save_posts(std::slice::from_ref(&p), account_id).unwrap();
    db::save_posts_tags_batch(
        std::slice::from_ref(&p),
        &std::collections::HashSet::new(),
        true,
        Some(account_id),
    )
    .unwrap();
    let cooc_before = count_table_for_account("account_tag_cooccurrence", account_id);
    assert!(cooc_before > 0);

    let result = pipeline::run_process(account_id, "pipeline_owner".to_string()).await;
    assert!(result.is_err(), "should propagate e621 5xx as Err");
    let err = result.unwrap_err();
    assert!(
        err.contains("account request") || err.contains("500"),
        "error should mention the upstream failure, got: {err}"
    );

    // Teardown phases come AFTER get_account, so the pre-existing
    // cooc must still be present.
    assert_eq!(
        count_table_for_account("account_tag_cooccurrence", account_id),
        cooc_before,
        "cooc must NOT be wiped when /process aborts before drop_old"
    );

    wipe_account(account_id);
}

/// A successful HTTP status with an unexpected response shape must not be
/// interpreted as an empty favourites page. Otherwise a schema change or an
/// upstream JSON error envelope can silently truncate a full/incremental import.
#[tokio::test(flavor = "multi_thread")]
async fn get_favorites_malformed_200_returns_err() {
    let _guard = pipeline_lock().lock().await;
    ensure_migrations();
    let server = MockServer::start().await;
    let _cfg = install_mock_config(&server.uri());

    let account_id = 91007;
    wipe_account(account_id);

    Mock::given(method("GET"))
        .and(path(format!("/users/{account_id}.json")))
        .respond_with(ResponseTemplate::new(200).set_body_json(fake_user_json(account_id, 1)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/favorites.json"))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "posts": []
        })))
        .mount(&server)
        .await;

    let account = db::set_account("pipeline_owner", account_id, "test_user", "").unwrap();
    let error = match api::get_favorites(&account, 1).await {
        Ok(_) => panic!("malformed 200 response must be surfaced as an error"),
        Err(error) => error.to_string(),
    };

    assert!(
        error.contains("favorites page 1"),
        "missing page context: {error}"
    );
    assert!(
        error.contains("malformed 200 response"),
        "wrong error: {error}"
    );

    jobs::try_begin(account_id);
    let pipeline_error = pipeline::run_process(account_id, "pipeline_owner".to_string())
        .await
        .expect_err("one malformed page must abort the pipeline immediately");
    assert!(
        pipeline_error.contains("aborted on malformed favourites response"),
        "wrong pipeline error: {pipeline_error}"
    );
    let state = jobs::get_state(account_id).expect("job state recorded");
    assert_eq!(
        state.pages_done, 0,
        "malformed page must not be marked done"
    );

    jobs::finish(account_id, Err(pipeline_error));
    wipe_account(account_id);
}

/// Regression for the "20-minute hang" report: when several favourite
/// pages in a row fail (timeouts / 5xx exhausting retries), the
/// pipeline must abort with a clear error and NOT silently complete
/// with dropped favourites. The previous code returned `Vec::new()` on
/// fetch error, so the user got a "Done" job with a half-built profile.
#[tokio::test(flavor = "multi_thread")]
async fn analyze_account_aborts_on_consecutive_page_failures() {
    let _guard = pipeline_lock().lock().await;
    ensure_migrations();
    let server = MockServer::start().await;
    let _cfg = install_mock_config(&server.uri());

    let account_id = 91006;
    wipe_account(account_id);

    // User has 12 favs → 3 pages at posts_limit=4.
    Mock::given(method("GET"))
        .and(path(format!("/users/{account_id}.json")))
        .respond_with(ResponseTemplate::new(200).set_body_json(fake_user_json(account_id, 12)))
        .mount(&server)
        .await;

    // Page 1 succeeds with one post.
    Mock::given(method("GET"))
        .and(path("/favorites.json"))
        .and(query_param("page", "1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!([fake_post_json(
                60010,
                &["a"],
                &["x"]
            )])),
        )
        .mount(&server)
        .await;
    // Pages 2 and 3 both 500 — `send_with_retry` will exhaust the retry
    // budget on each, producing back-to-back hard failures.
    Mock::given(method("GET"))
        .and(path("/favorites.json"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream blew up"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/favorites.json"))
        .and(query_param("page", "3"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream blew up"))
        .mount(&server)
        .await;

    db::set_account("pipeline_owner", account_id, "test_user", "").unwrap();
    jobs::try_begin(account_id);
    let result = pipeline::run_process(account_id, "pipeline_owner".to_string()).await;

    assert!(
        result.is_err(),
        "two consecutive page failures must abort, got Ok"
    );
    let err = result.unwrap_err();
    assert!(
        err.contains("consecutive page fetch failures"),
        "error must mention the abort reason, got: {err}"
    );

    // The successful page 1 should still have been persisted before the
    // pipeline noticed the failure pattern.
    assert!(
        count_table_for_account("accounts_post", account_id) >= 1,
        "page 1's post should have been saved before the abort"
    );

    jobs::finish(account_id, Err(err));
    wipe_account(account_id);
}

/// `try_begin` is the gate that protects /process from being driven
/// twice in parallel. Second call returns the existing Running state
/// without spawning a duplicate job — the pipeline itself is never
/// reached again.
#[tokio::test(flavor = "multi_thread")]
async fn analyze_account_double_start_returns_already_running() {
    let _guard = pipeline_lock().lock().await;
    ensure_migrations();
    let account_id = 91003;
    wipe_account(account_id);

    let first = jobs::try_begin(account_id);
    let started_state = match first {
        jobs::BeginResult::Started(s) => s,
        jobs::BeginResult::AlreadyRunning(_) => panic!("first try_begin must be Started"),
    };
    assert_eq!(started_state.phase, ProcessJobPhase::Running);

    let second = jobs::try_begin(account_id);
    match second {
        jobs::BeginResult::AlreadyRunning(s) => {
            assert_eq!(s.phase, ProcessJobPhase::Running);
            assert_eq!(s.started_at, started_state.started_at);
        }
        jobs::BeginResult::Started(_) => {
            panic!("second try_begin must return AlreadyRunning while phase=Running")
        }
    }

    jobs::finish(account_id, Ok(()));
    // After finish, try_begin yields a fresh Started.
    let third = jobs::try_begin(account_id);
    matches!(third, jobs::BeginResult::Started(_));
    jobs::finish(account_id, Ok(()));

    wipe_account(account_id);
}

/// Re-analysing the same account swaps out the old favourite set
/// without doubling counts. Pre-condition: account already has posts
/// 20001/20002 (with tags x). Mock returns a different set (20003/
/// 20004 with tags y). After /process the linked posts are exactly
/// the new set, and the tag-counts profile reflects only y.
#[tokio::test(flavor = "multi_thread")]
async fn analyze_account_re_analyze_replaces_state() {
    let _guard = pipeline_lock().lock().await;
    ensure_migrations();
    let server = MockServer::start().await;
    let _cfg = install_mock_config(&server.uri());

    let account_id = 91004;
    wipe_account(account_id);

    // Seed prior state: two posts with tag "old".
    let make_p = |id: i64, general_tag: &str| e621_account_parser_api::models::Post {
        id,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        change_seq: 0.0,
        files: e621_account_parser_api::models::Files::default(),
        uploader_id: 42,
        uploader_name: None,
        approver_id: None,
        stats: e621_account_parser_api::models::Stats {
            score: e621_account_parser_api::models::Score {
                up: 1,
                down: 0,
                total: 1,
            },
            ..Default::default()
        },
        flags: e621_account_parser_api::models::Flags::default(),
        has: e621_account_parser_api::models::Has::default(),
        relationships: e621_account_parser_api::models::Relationships::default(),
        pools: vec![],
        rating: e621_account_parser_api::models::Rating::S,
        locked_tags: vec![],
        sources: vec![],
        description: None,
        tags: e621_account_parser_api::models::Tags {
            general: vec![general_tag.into()],
            ..Default::default()
        },
    };
    db::set_account("pipeline_owner", account_id, "test_user", "").unwrap();
    let old_posts = vec![make_p(40001, "old_tag"), make_p(40002, "old_tag")];
    db::save_posts(&old_posts, account_id).unwrap();
    db::save_posts_tags_batch(
        &old_posts,
        &std::collections::HashSet::new(),
        true,
        Some(account_id),
    )
    .unwrap();

    // Mock the NEW set — posts 40003/40004 with tag "new_tag".
    Mock::given(method("GET"))
        .and(path(format!("/users/{account_id}.json")))
        .respond_with(ResponseTemplate::new(200).set_body_json(fake_user_json(account_id, 2)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/favorites.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            fake_post_json(40003, &[], &["new_tag"]),
            fake_post_json(40004, &[], &["new_tag"]),
        ])))
        .mount(&server)
        .await;

    jobs::try_begin(account_id);
    // Explicit Full mode: Auto would pick Incremental when local_count (2)
    // >= remote_count (2), which only adds new posts without dropping old
    // ones — this test asserts the old set is replaced entirely.
    pipeline::run_process_with_mode(
        account_id,
        "pipeline_owner".to_string(),
        pipeline::ProcessMode::Full,
    )
    .await
    .expect("re-analyze completes");
    jobs::finish(account_id, Ok(()));

    // Linked set is exactly the new posts.
    let linked: Vec<i64> = {
        let conn = db::open_db_for_calibration().unwrap();
        let mut stmt = conn
            .prepare("SELECT post_id FROM accounts_post WHERE account_id = ?1 ORDER BY post_id")
            .unwrap();
        let rows = stmt
            .query_map(rusqlite::params![account_id], |r| r.get::<_, i64>(0))
            .unwrap();
        rows.collect::<Result<Vec<_>, _>>().unwrap()
    };
    assert_eq!(linked, vec![40003, 40004]);

    // Tag-counts profile reflects only the new tag.
    let has_new: i64 = {
        let conn = db::open_db_for_calibration().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM account_tag_counts \
             WHERE account_id = ?1 AND tag_name = 'new_tag'",
            rusqlite::params![account_id],
            |r| r.get(0),
        )
        .unwrap()
    };
    let has_old: i64 = {
        let conn = db::open_db_for_calibration().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM account_tag_counts \
             WHERE account_id = ?1 AND tag_name = 'old_tag'",
            rusqlite::params![account_id],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(has_new, 1, "new_tag must appear in profile");
    assert_eq!(
        has_old, 0,
        "old_tag must be wiped by drop_old + profile refresh"
    );

    wipe_account(account_id);
}

/// Server-side blacklist filtering — `cfg.tag_blacklist` is applied to
/// e621 posts before they hit the DB. With "fluffy" globally
/// blacklisted, posts whose ONLY general tag is fluffy lose that tag
/// and shouldn't pull it into the tag-counts profile.
#[tokio::test(flavor = "multi_thread")]
async fn analyze_account_global_blacklist_strips_tags() {
    let _guard = pipeline_lock().lock().await;
    ensure_migrations();
    let server = MockServer::start().await;
    // Override the blacklist for this test.
    let cfg_file = install_mock_config(&server.uri());
    {
        let toml = std::fs::read_to_string(cfg_file.path()).unwrap();
        let patched = swap_toml_field(&toml, "tag_blacklist", "[\"fluffy\", \"sound_warning\"]");
        std::fs::write(cfg_file.path(), patched).unwrap();
        e621_account_parser_api::models::reload_from(cfg_file.path()).unwrap();
    }

    let account_id = 91005;
    wipe_account(account_id);

    Mock::given(method("GET"))
        .and(path(format!("/users/{account_id}.json")))
        .respond_with(ResponseTemplate::new(200).set_body_json(fake_user_json(account_id, 1)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/favorites.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            fake_post_json(50001, &["artist_x"], &["fluffy", "kept_tag"])
        ])))
        .mount(&server)
        .await;

    db::set_account("pipeline_owner", account_id, "test_user", "").unwrap();
    jobs::try_begin(account_id);
    pipeline::run_process(account_id, "pipeline_owner".to_string())
        .await
        .expect("pipeline completes");
    jobs::finish(account_id, Ok(()));

    let has_fluffy: i64 = {
        let conn = db::open_db_for_calibration().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM account_tag_counts \
             WHERE account_id = ?1 AND tag_name = 'fluffy'",
            rusqlite::params![account_id],
            |r| r.get(0),
        )
        .unwrap()
    };
    let has_kept: i64 = {
        let conn = db::open_db_for_calibration().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM account_tag_counts \
             WHERE account_id = ?1 AND tag_name = 'kept_tag'",
            rusqlite::params![account_id],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(has_fluffy, 0, "global blacklist must strip 'fluffy'");
    assert_eq!(has_kept, 1, "non-blacklisted tags must survive");

    wipe_account(account_id);
}

// ==================================================================
//  Pure helper coverage
// ==================================================================

/// `strip_blacklisted_tags` was hoisted into `pipeline` so callers
/// (tests, seed, prefetch) can use the same filter as the live ingest.
/// Verify it filters every group and is case-insensitive.
#[test]
fn strip_blacklisted_tags_is_case_insensitive_and_per_group() {
    use std::collections::HashSet;
    let blacklist: HashSet<String> = ["bad", "alsobad", "trim_me"]
        .into_iter()
        .map(String::from)
        .collect();

    let post = e621_account_parser_api::models::Post {
        id: 1,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        change_seq: 0.0,
        files: e621_account_parser_api::models::Files::default(),
        uploader_id: 0,
        uploader_name: None,
        approver_id: None,
        stats: e621_account_parser_api::models::Stats {
            score: e621_account_parser_api::models::Score {
                up: 1,
                down: 0,
                total: 1,
            },
            ..Default::default()
        },
        flags: e621_account_parser_api::models::Flags::default(),
        has: e621_account_parser_api::models::Has::default(),
        relationships: e621_account_parser_api::models::Relationships::default(),
        pools: vec![],
        rating: e621_account_parser_api::models::Rating::S,
        locked_tags: vec![],
        sources: vec![],
        description: None,
        tags: e621_account_parser_api::models::Tags {
            general: vec![
                "keep".into(),
                "BAD".into(),
                "alsobad".into(),
                "  trim_me  ".into(),
            ],
            artist: vec!["keep_artist".into(), "bad".into()],
            character: vec!["bad".into()],
            copyright: vec![],
            species: vec!["bad".into()],
            invalid: vec![],
            meta: vec!["bad".into()],
            lore: vec!["bad".into()],
            contributor: vec![],
        },
    };

    let p = pipeline::strip_blacklisted_tags(post, &blacklist);

    // Case-insensitive: "BAD" / "alsobad" gone, "keep" stays.
    assert_eq!(p.tags.general, vec!["keep".to_string()]);
    assert_eq!(p.tags.artist, vec!["keep_artist".to_string()]);
    // Every tag group filtered.
    assert!(p.tags.character.is_empty());
    assert!(p.tags.species.is_empty());
    assert!(p.tags.meta.is_empty());
    assert!(p.tags.lore.is_empty());
}

// ==================================================================
//  User story 2 — "Get me posts (similar / digest)" — pure DB layer
// ==================================================================
//
// These don't need a mock e621: they read the local catalog populated
// by /process. We seed the catalog directly and exercise the DB-side
// helpers the /posts/<id>/similar and /digest routes consume.

/// `find_similar_post_ids` ranks candidates by tag-overlap DESC,
/// excludes owned + interacted posts, and respects `min_overlap`.
#[test]
fn find_similar_post_ids_ranks_by_overlap_and_filters() {
    ensure_migrations();
    let account_id = 91010;
    wipe_account(account_id);
    let fixture_ids = [60001, 60002, 60003, 60004, 60005, 60006];
    // Remove remnants from previous runs before inserting: `upsert` adds tag
    // relations but intentionally does not replace historical associations.
    db::delete_catalog_posts_by_ids(&fixture_ids).unwrap();
    db::set_account("similar_owner", account_id, "test_user", "").unwrap();

    let blacklist = std::collections::HashSet::new();
    // The integration suite shares a catalog with the developer database. Use
    // namespaced tags so real e621 posts cannot enter this ranking assertion.
    let make_post = |id: i64, general: Vec<&str>| e621_account_parser_api::models::Post {
        id,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        change_seq: 0.0,
        files: e621_account_parser_api::models::Files {
            preview: e621_account_parser_api::models::FilePreview {
                width: 1,
                height: 1,
                jpg: Some("p".into()),
                ..Default::default()
            },
            ..Default::default()
        },
        uploader_id: 0,
        uploader_name: None,
        approver_id: None,
        stats: e621_account_parser_api::models::Stats {
            score: e621_account_parser_api::models::Score {
                up: 1,
                down: 0,
                total: 1,
            },
            ..Default::default()
        },
        flags: e621_account_parser_api::models::Flags::default(),
        has: e621_account_parser_api::models::Has::default(),
        relationships: e621_account_parser_api::models::Relationships::default(),
        pools: vec![],
        rating: e621_account_parser_api::models::Rating::S,
        locked_tags: vec![],
        sources: vec![],
        description: None,
        tags: e621_account_parser_api::models::Tags {
            general: general.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        },
    };

    // Source post + three candidates, plus one "owned" and one "interacted"
    // that must be filtered.
    let source = make_post(
        60001,
        vec![
            "test_similar_91010_x",
            "test_similar_91010_y",
            "test_similar_91010_z",
        ],
    );
    let high_overlap = make_post(
        60002,
        vec![
            "test_similar_91010_x",
            "test_similar_91010_y",
            "test_similar_91010_z",
            "test_similar_91010_w",
        ],
    ); // 3 overlap
    let mid_overlap = make_post(
        60003,
        vec![
            "test_similar_91010_x",
            "test_similar_91010_y",
            "test_similar_91010_q",
        ],
    ); // 2 overlap
    let low_overlap = make_post(60004, vec!["test_similar_91010_x", "test_similar_91010_p"]); // 1 overlap
    let owned = make_post(
        60005,
        vec![
            "test_similar_91010_x",
            "test_similar_91010_y",
            "test_similar_91010_z",
        ],
    ); // owned
    let interacted = make_post(
        60006,
        vec![
            "test_similar_91010_x",
            "test_similar_91010_y",
            "test_similar_91010_z",
        ],
    ); // interacted

    let posts = vec![
        source.clone(),
        high_overlap,
        mid_overlap,
        low_overlap,
        owned.clone(),
        interacted.clone(),
    ];
    // Insert into catalog without linking any to the account.
    db::upsert_catalog_posts(&posts).unwrap();
    db::save_posts_tags_batch(&posts, &blacklist, false, None).unwrap();

    // Link `owned` to the account.
    db::save_posts(std::slice::from_ref(&owned), account_id).unwrap();

    // Mark `interacted` as opened by this account.
    {
        let conn = db::open_db_for_calibration().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO feed_interactions \
             (account_id, post_id, event_type, position, session_id, created_at) \
             VALUES (?1, ?2, 'open', 0, 's', ?3)",
            rusqlite::params![account_id, 60006_i64, now],
        )
        .unwrap();
    }

    // min_overlap = 2 → low_overlap (1 shared tag) drops out.
    let ranked = db::find_similar_post_ids(60001, account_id, 2, 10, 1).unwrap();
    assert_eq!(
        ranked,
        vec![60002, 60003],
        "expected high-then-mid, with low excluded by min_overlap=2 and owned/interacted filtered"
    );

    // min_overlap = 1 brings low_overlap back; owned + interacted still filtered.
    let ranked = db::find_similar_post_ids(60001, account_id, 1, 10, 1).unwrap();
    assert_eq!(ranked, vec![60002, 60003, 60004]);

    wipe_account(account_id);
    db::delete_catalog_posts_by_ids(&fixture_ids).unwrap();
}

/// Digest "generic" path (cold account, no visit history) — must
/// return a mix of trending / popular-new / random, capped at 20.
#[tokio::test(flavor = "multi_thread")]
async fn digest_generic_returns_mixed_posts() {
    let _guard = pipeline_lock().lock().await;
    ensure_migrations();
    let account_id = 91011;
    wipe_account(account_id);
    db::set_account("digest_owner", account_id, "test_user", "").unwrap();

    // Seed catalog with a handful of posts; the digest reads from
    // `posts` directly for trending/random.
    let blacklist = std::collections::HashSet::new();
    let posts: Vec<_> = (0..10)
        .map(|i| e621_account_parser_api::models::Post {
            id: 70000 + i,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            change_seq: 0.0,
            files: e621_account_parser_api::models::Files {
                preview: e621_account_parser_api::models::FilePreview {
                    width: 1,
                    height: 1,
                    jpg: Some("p".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
            uploader_id: 42,
            uploader_name: None,
            approver_id: None,
            stats: e621_account_parser_api::models::Stats {
                score: e621_account_parser_api::models::Score {
                    up: 10 + i,
                    down: 0,
                    total: 10 + i,
                },
                fav_count: 5,
                ..Default::default()
            },
            flags: e621_account_parser_api::models::Flags::default(),
            has: e621_account_parser_api::models::Has::default(),
            relationships: e621_account_parser_api::models::Relationships::default(),
            pools: vec![],
            rating: e621_account_parser_api::models::Rating::S,
            locked_tags: vec![],
            sources: vec![],
            description: None,
            tags: e621_account_parser_api::models::Tags {
                general: vec!["tag".into()],
                ..Default::default()
            },
        })
        .collect();
    db::upsert_catalog_posts(&posts).unwrap();
    db::save_posts_tags_batch(&posts, &blacklist, false, None).unwrap();

    let trending = db::get_trending_posts(30, 5).unwrap();
    assert!(!trending.is_empty(), "trending must surface seeded posts");
    let random = db::get_random_posts(5).unwrap();
    assert!(!random.is_empty(), "random must surface seeded posts");

    wipe_account(account_id);
}

// ==================================================================
//  User story 3 — "Log my interactions"
// ==================================================================

/// `record_feed_interaction` requires a linked owner_token and writes
/// to BOTH feed_interactions AND account_tag_feedback (via the tag fan-out).
#[test]
fn feed_interaction_round_trip_writes_both_tables() {
    ensure_migrations();
    let account_id = 91020;
    wipe_account(account_id);
    db::set_account("interaction_owner", account_id, "test_user", "").unwrap();

    // Seed a post with tags so the feedback fan-out has something to count.
    let p = e621_account_parser_api::models::Post {
        id: 80001,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        change_seq: 0.0,
        files: e621_account_parser_api::models::Files::default(),
        uploader_id: 0,
        uploader_name: None,
        approver_id: None,
        stats: e621_account_parser_api::models::Stats {
            score: e621_account_parser_api::models::Score {
                up: 1,
                down: 0,
                total: 1,
            },
            ..Default::default()
        },
        flags: e621_account_parser_api::models::Flags::default(),
        has: e621_account_parser_api::models::Has::default(),
        relationships: e621_account_parser_api::models::Relationships::default(),
        pools: vec![],
        rating: e621_account_parser_api::models::Rating::S,
        locked_tags: vec![],
        sources: vec![],
        description: None,
        tags: e621_account_parser_api::models::Tags {
            general: vec!["g1".into(), "g2".into()],
            ..Default::default()
        },
    };
    db::save_posts(std::slice::from_ref(&p), account_id).unwrap();
    db::save_posts_tags_batch(
        std::slice::from_ref(&p),
        &std::collections::HashSet::new(),
        false,
        None,
    )
    .unwrap();

    let req = e621_account_parser_api::models::FeedInteractionRequest {
        account_id,
        post_id: 80001,
        event_type: e621_account_parser_api::models::FeedInteractionType::Open,
        position: 0,
        session_id: "test-sess".into(),
    };
    db::record_feed_interaction("interaction_owner", &req).unwrap();

    let fi: i64 = count_table_for_account("feed_interactions", account_id);
    assert_eq!(fi, 1, "feed_interactions row written");
    let fb: i64 = count_table_for_account("account_tag_feedback", account_id);
    assert!(fb >= 2, "tag-feedback fan-out should record both g1 and g2");

    let pos: i64 = {
        let conn = db::open_db_for_calibration().unwrap();
        conn.query_row(
            "SELECT positive_count FROM account_tag_feedback \
             WHERE account_id = ?1 AND tag_name = 'g1'",
            rusqlite::params![account_id],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(pos, 1, "Open event must increment positive_count");

    wipe_account(account_id);
}

/// Wrong owner_token → record refuses. This is the auth gate for the
/// public POST /interaction handler.
#[test]
fn feed_interaction_rejects_unlinked_owner() {
    ensure_migrations();
    let account_id = 91021;
    wipe_account(account_id);
    db::set_account("real_owner", account_id, "test_user", "").unwrap();

    let req = e621_account_parser_api::models::FeedInteractionRequest {
        account_id,
        post_id: 80002,
        event_type: e621_account_parser_api::models::FeedInteractionType::Open,
        position: 0,
        session_id: "test-sess".into(),
    };
    let r = db::record_feed_interaction("fake_owner", &req);
    assert!(r.is_err(), "wrong owner_token must error");

    wipe_account(account_id);
}

/// Batch interactions: duplicate (account, post, event, session) is a
/// no-op the SECOND time through `INSERT OR IGNORE`.
#[test]
fn feed_interactions_batch_dedups_duplicates() {
    ensure_migrations();
    let account_id = 91022;
    wipe_account(account_id);
    db::set_account("batch_owner", account_id, "test_user", "").unwrap();

    let p = e621_account_parser_api::models::Post {
        id: 80003,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        change_seq: 0.0,
        files: e621_account_parser_api::models::Files::default(),
        uploader_id: 0,
        uploader_name: None,
        approver_id: None,
        stats: e621_account_parser_api::models::Stats {
            score: e621_account_parser_api::models::Score {
                up: 1,
                down: 0,
                total: 1,
            },
            ..Default::default()
        },
        flags: e621_account_parser_api::models::Flags::default(),
        has: e621_account_parser_api::models::Has::default(),
        relationships: e621_account_parser_api::models::Relationships::default(),
        pools: vec![],
        rating: e621_account_parser_api::models::Rating::S,
        locked_tags: vec![],
        sources: vec![],
        description: None,
        tags: e621_account_parser_api::models::Tags {
            general: vec!["g".into()],
            artist: vec![],
            copyright: vec![],
            character: vec![],
            species: vec![],
            invalid: vec![],
            meta: vec![],
            lore: vec![],
            contributor: vec![],
        },
    };
    db::save_posts(std::slice::from_ref(&p), account_id).unwrap();
    db::save_posts_tags_batch(
        std::slice::from_ref(&p),
        &std::collections::HashSet::new(),
        false,
        None,
    )
    .unwrap();

    let make_req = || e621_account_parser_api::models::FeedInteractionRequest {
        account_id,
        post_id: 80003,
        event_type: e621_account_parser_api::models::FeedInteractionType::Open,
        position: 0,
        session_id: "batch-sess".into(),
    };

    // Three identical interactions in one batch — `INSERT OR IGNORE`
    // collapses them to exactly one stored row.
    db::record_feed_interactions_batch("batch_owner", &[make_req(), make_req(), make_req()])
        .unwrap();
    assert_eq!(count_table_for_account("feed_interactions", account_id), 1);

    // Same batch again across a second call — still 1 row.
    db::record_feed_interactions_batch("batch_owner", &[make_req()]).unwrap();
    assert_eq!(count_table_for_account("feed_interactions", account_id), 1);

    wipe_account(account_id);
}

// ==================================================================
//  User story 4 — "Manage preferred tags"
// ==================================================================

/// Set → get round-trip for preferred tags. Verifies the persistence
/// layer matches what the validator + handler produce.
#[test]
fn preferred_tags_set_get_round_trip() {
    use e621_account_parser_api::models::PreferredTag;

    ensure_migrations();
    let account_id = 91030;
    wipe_account(account_id);
    db::set_account("pref_owner", account_id, "test_user", "").unwrap();

    let tags = vec![
        PreferredTag {
            tag: "fluffy".into(),
            group: "general".into(),
            weight: 2.0,
        },
        PreferredTag {
            tag: "wolf".into(),
            group: "species".into(),
            weight: 1.5,
        },
    ];
    db::set_preferred_tags("pref_owner", account_id, &tags).unwrap();

    let mut got = db::get_account_preferred_tags(account_id).unwrap();
    got.sort_by(|a, b| a.tag.cmp(&b.tag));
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].tag, "fluffy");
    assert!((got[0].weight - 2.0).abs() < 1e-6);
    assert_eq!(got[1].tag, "wolf");
    assert!((got[1].weight - 1.5).abs() < 1e-6);

    // Re-setting replaces, not appends.
    db::set_preferred_tags(
        "pref_owner",
        account_id,
        &[PreferredTag {
            tag: "cat".into(),
            group: "species".into(),
            weight: 1.0,
        }],
    )
    .unwrap();
    let got = db::get_account_preferred_tags(account_id).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].tag, "cat");

    wipe_account(account_id);
}

// ==================================================================
//  User story 5 — "Don't recommend posts I already saved"
// ==================================================================

/// Dedup invariant: `get_owned_post_ids` returns every post linked to
/// the account via `accounts_post`, and `/recommendations` (both the
/// main route and the `/continue` helper) filters those ids out of
/// both the local-candidate pool AND the live e621 page before
/// scoring. This test verifies the contract at the DB layer — if it
/// breaks, the bug is in `get_owned_post_ids`, not the filter.
#[test]
fn recommendations_owned_dedup_invariant() {
    ensure_migrations();
    let account_id = 91040;
    wipe_account(account_id);
    db::set_account("dedup_owner", account_id, "test_user", "").unwrap();

    let make = |id: i64| e621_account_parser_api::models::Post {
        id,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        change_seq: 0.0,
        files: e621_account_parser_api::models::Files {
            preview: e621_account_parser_api::models::FilePreview {
                width: 1,
                height: 1,
                jpg: Some("p".into()),
                ..Default::default()
            },
            ..Default::default()
        },
        uploader_id: 0,
        uploader_name: None,
        approver_id: None,
        stats: e621_account_parser_api::models::Stats {
            score: e621_account_parser_api::models::Score {
                up: 1,
                down: 0,
                total: 1,
            },
            ..Default::default()
        },
        flags: e621_account_parser_api::models::Flags::default(),
        has: e621_account_parser_api::models::Has::default(),
        relationships: e621_account_parser_api::models::Relationships::default(),
        pools: vec![],
        rating: e621_account_parser_api::models::Rating::S,
        locked_tags: vec![],
        sources: vec![],
        description: None,
        tags: e621_account_parser_api::models::Tags {
            general: vec!["shared_tag".into()],
            ..Default::default()
        },
    };

    let owned_post = make(90001);
    let catalog_post = make(90002); // in catalog but not owned

    // Owned path: link via save_posts (this is what /process does).
    db::save_posts(std::slice::from_ref(&owned_post), account_id).unwrap();
    // Catalog path: in posts table but no accounts_post row for this user.
    db::upsert_catalog_posts(std::slice::from_ref(&catalog_post)).unwrap();

    let owned = db::get_owned_post_ids(account_id).unwrap();
    assert!(
        owned.contains(&90001),
        "owned post 90001 must be returned by get_owned_post_ids, got {:?}",
        owned
    );
    assert!(
        !owned.contains(&90002),
        "catalog-only post 90002 must NOT be in the owned set"
    );

    // The /recommendations dedup pipeline applies `!owned_ids.contains(id)` to
    // BOTH the local candidate pool AND the live e621 page. We simulate the
    // same filter here to lock the contract: any id that came out of
    // `get_owned_post_ids` is dropped from a candidate list.
    let candidates: Vec<i64> = vec![90001, 90002, 90003];
    let after_dedup: Vec<i64> = candidates
        .into_iter()
        .filter(|id| !owned.contains(id))
        .collect();
    assert_eq!(
        after_dedup,
        vec![90002, 90003],
        "dedup must drop 90001 (owned), keep 90002 (catalog-only) and 90003 (unknown)"
    );

    wipe_account(account_id);
}

/// /process imports favourites → owned-set grows → those ids are
/// excluded from subsequent recommendation queries. This is the
/// end-to-end contract from a user's POV: "I just imported my favs,
/// the feed shouldn't keep showing them to me".
#[tokio::test(flavor = "multi_thread")]
async fn process_then_owned_dedup_excludes_imported_favs() {
    let _guard = pipeline_lock().lock().await;
    ensure_migrations();
    let server = MockServer::start().await;
    let _cfg = install_mock_config(&server.uri());

    let account_id = 91041;
    wipe_account(account_id);

    Mock::given(method("GET"))
        .and(path(format!("/users/{account_id}.json")))
        .respond_with(ResponseTemplate::new(200).set_body_json(fake_user_json(account_id, 3)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/favorites.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            fake_post_json(95001, &["a"], &["t"]),
            fake_post_json(95002, &["a"], &["t"]),
            fake_post_json(95003, &["a"], &["t"]),
        ])))
        .mount(&server)
        .await;

    db::set_account("dedup_pipeline_owner", account_id, "test_user", "").unwrap();
    jobs::try_begin(account_id);
    pipeline::run_process(account_id, "dedup_pipeline_owner".to_string())
        .await
        .unwrap();
    jobs::finish(account_id, Ok(()));

    let owned = db::get_owned_post_ids(account_id).unwrap();
    for id in [95001_i64, 95002, 95003] {
        assert!(
            owned.contains(&id),
            "post {id} imported by /process must be in owned set, got {:?}",
            owned
        );
    }

    // Same filter the recommendations pipeline applies:
    let candidate_pool: Vec<i64> = vec![95001, 95002, 95003, 99999];
    let recommendable: Vec<i64> = candidate_pool
        .into_iter()
        .filter(|id| !owned.contains(id))
        .collect();
    assert_eq!(
        recommendable,
        vec![99999],
        "all imported favourites must be excluded from the recommendation pool"
    );

    wipe_account(account_id);
}

// ==================================================================
//  Cross-page dedup integration — /recommendations route
// ==================================================================

/// The `get_recommendations` route applies cross-page dedup when
/// `session` is provided. Posts already recorded in `feed_session_posts`
/// must be filtered out of the scored list. Rather than running the HTTP
/// route directly (which requires the full Rocket app), this test verifies
/// the core dedup contract at the pipeline + DB layer: when a session has
/// prior shown-IDs, `build_recommendations_shared` filters owned+seen
/// posts, and the feed route applies the additional session-based filter.
#[tokio::test(flavor = "multi_thread")]
async fn recommendations_cross_page_dedup_filters_shown_posts() {
    let _guard = pipeline_lock().lock().await;
    ensure_migrations();
    let server = MockServer::start().await;
    let _cfg = install_mock_config(&server.uri());

    let account_id = 91050;
    wipe_account(account_id);

    Mock::given(method("GET"))
        .and(path(format!("/users/{account_id}.json")))
        .respond_with(ResponseTemplate::new(200).set_body_json(fake_user_json(account_id, 6)))
        .mount(&server)
        .await;

    // Return posts 101-106 on page 1.
    Mock::given(method("GET"))
        .and(path("/favorites.json"))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            fake_post_json(101, &["artist_a"], &["t1"]),
            fake_post_json(102, &["artist_a"], &["t1"]),
            fake_post_json(103, &["artist_b"], &["t2"]),
            fake_post_json(104, &["artist_b"], &["t2"]),
            fake_post_json(105, &["artist_c"], &["t3"]),
            fake_post_json(106, &["artist_c"], &["t3"]),
        ])))
        .mount(&server)
        .await;

    db::set_account("dedup_owner", account_id, "test_user", "").unwrap();

    // Simulate page 1 already shown: manually insert posts into the
    // session shown set so subsequent recommendation queries can
    // reference them. Clean up first in case of a previous run.
    let session_id = "cross-page-test-91050";
    {
        let conn = db::open_db_for_calibration().unwrap();
        conn.execute(
            "DELETE FROM feed_session_posts WHERE session_id = ?1",
            rusqlite::params![session_id],
        )
        .unwrap();
        conn.execute(
            "DELETE FROM feed_sessions WHERE session_id = ?1",
            rusqlite::params![session_id],
        )
        .unwrap();
        // Create the session row (Fresh).
        conn.execute(
            "INSERT OR IGNORE INTO feed_sessions (session_id, account_id, created_at, last_accessed_at) \
             VALUES (?1, ?2, ?3, ?3)",
            rusqlite::params![session_id, account_id, chrono::Utc::now().to_rfc3339()],
        ).unwrap();
        // Mark posts 101 and 103 as already shown on page 1.
        conn.execute(
            "INSERT OR IGNORE INTO feed_session_posts (session_id, post_id, position, shown_at)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![session_id, 101_i64, 1, chrono::Utc::now().to_rfc3339()],
        )
        .unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO feed_session_posts (session_id, post_id, position, shown_at)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![session_id, 103_i64, 2, chrono::Utc::now().to_rfc3339()],
        )
        .unwrap();
    }

    // Verify the DB-level dedup set matches what the route would load.
    let shown_ids = db::get_session_shown_post_ids(session_id).unwrap();
    assert!(
        shown_ids.contains(&101) && shown_ids.contains(&103),
        "dedup set should contain page-1 posts 101 and 103, got {:?}",
        shown_ids
    );
    assert_eq!(shown_ids.len(), 2, "should have exactly 2 shown posts");

    // Simulate what `get_recommendations` does: filter a candidate list
    // against the shown-ids set, then record the new page's posts.
    // We use the same candidate pool the route would build from e621.
    let candidate_ids: Vec<i64> = vec![101, 102, 103, 104, 105, 106];
    let after_dedup: Vec<i64> = candidate_ids
        .into_iter()
        .filter(|id| !shown_ids.contains(id))
        .collect();
    assert_eq!(
        after_dedup,
        vec![102, 104, 105, 106],
        "page 2 candidates must exclude shown posts 101 and 103"
    );

    // Verify that recording the page-2 posts works correctly.
    let posts_to_record: Vec<(i64, i32)> = after_dedup
        .iter()
        .enumerate()
        .map(|(i, id)| (*id, (i + 1) as i32))
        .collect();
    db::record_session_shown_posts(session_id, &posts_to_record).unwrap();

    // All 6 posts should now be in the shown set.
    let shown_after: HashSet<i64> = db::get_session_shown_post_ids(session_id).unwrap();
    assert!(
        shown_after.len() == 6,
        "after page-2 recording, shown set should have 6 posts, got {}",
        shown_after.len()
    );
    for id in [101, 102, 103, 104, 105, 106] {
        assert!(
            shown_after.contains(&id),
            "post {id} should be in shown set"
        );
    }

    wipe_account(account_id);
}

// ==================================================================
//  Prefetch target selection integration
// ==================================================================

/// The prefetch worker picks targets based on recent feed interactions.
/// Accounts with no interactions, or that were prefetched recently,
/// should be excluded. This tests the DB setup that the prefetch
/// worker depends on (interactions + tag_counts). The actual
/// `pick_prefetch_targets()` function is private but the DB schema
/// it reads from is exercised here.
#[tokio::test(flavor = "multi_thread")]
async fn prefetch_target_selection_respects_cooldown() {
    let _guard = pipeline_lock().lock().await;
    ensure_migrations();
    let server = MockServer::start().await;
    let _cfg = install_mock_config(&server.uri());

    let account_id = 91060;
    wipe_account(account_id);
    db::set_account("prefetch_owner", account_id, "test_user", "").unwrap();

    // Seed a post so tag_counts will be populated.
    let p = e621_account_parser_api::models::Post {
        id: 50001,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        change_seq: 0.0,
        files: e621_account_parser_api::models::Files::default(),
        uploader_id: 42,
        uploader_name: None,
        approver_id: None,
        stats: e621_account_parser_api::models::Stats {
            score: e621_account_parser_api::models::Score {
                up: 10,
                down: 0,
                total: 10,
            },
            fav_count: 5,
            ..Default::default()
        },
        flags: e621_account_parser_api::models::Flags::default(),
        has: e621_account_parser_api::models::Has::default(),
        relationships: e621_account_parser_api::models::Relationships::default(),
        pools: vec![],
        rating: e621_account_parser_api::models::Rating::S,
        locked_tags: vec![],
        sources: vec![],
        description: None,
        tags: e621_account_parser_api::models::Tags {
            general: vec!["artist_a".into()],
            artist: vec!["artist_a".into()],
            ..Default::default()
        },
    };
    db::save_posts(std::slice::from_ref(&p), account_id).unwrap();

    // Insert a feed_interaction so this account appears in the
    // prefetch candidate set.
    {
        let conn = db::open_db_for_calibration().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO feed_interactions (account_id, post_id, event_type, position, session_id, created_at)
             VALUES (?1, ?2, 'qualified_impression', 0, 'prefetch-test-sess', ?3)",
            rusqlite::params![account_id, 50001_i64, now],
        ).unwrap();
    }

    // Seed tag_counts so the prefetch query returns results.
    {
        let conn = db::open_db_for_calibration().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO account_tag_counts (account_id, tag_name, group_type, count)
             VALUES (?1, 'artist_a', 'artist', 3)",
            rusqlite::params![account_id],
        )
        .unwrap();
    }

    let targets = e621_account_parser_api::prefetch::pick_prefetch_targets().unwrap();
    assert!(
        targets.iter().any(|target| target.account_id == account_id),
        "the production selector must choose the active account"
    );

    let conn = db::open_db_for_calibration().unwrap();
    conn.execute(
        "UPDATE accounts SET last_prefetched_at = ?1 WHERE id = ?2",
        rusqlite::params![chrono::Utc::now().to_rfc3339(), account_id],
    )
    .unwrap();
    let targets = e621_account_parser_api::prefetch::pick_prefetch_targets().unwrap();
    assert!(
        !targets.iter().any(|target| target.account_id == account_id),
        "the production selector must honor cooldown"
    );

    wipe_account(account_id);
}

/// Verify that `wipe_account` properly cleans up prefetch-related
/// DB state (account, interactions, tag counts) so a fresh test
/// starts with no residual data.
#[test]
fn wipe_account_cleans_prefetch_state() {
    ensure_migrations();
    let account_id = 91070;

    db::set_account("wipe_test", account_id, "test_user", "").unwrap();

    // Seed a post so FK constraints are satisfied.
    {
        let conn = db::open_db_for_calibration().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO posts (id, created_at, score_total, fav_count, rating, last_seen_at)
             VALUES (?1, ?2, 0, 0, 's', ?3)",
            rusqlite::params![1_i64, chrono::Utc::now().to_rfc3339(), chrono::Utc::now().to_rfc3339()],
        ).unwrap();
    }

    // Seed prefetch-relevant state.
    {
        let conn = db::open_db_for_calibration().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO feed_interactions (account_id, post_id, event_type, position, session_id, created_at)
             VALUES (?1, 1, 'open', 0, 'sess', ?2)",
            rusqlite::params![account_id, now],
        ).unwrap();
        conn.execute(
            "INSERT INTO account_tag_counts (account_id, tag_name, group_type, count)
             VALUES (?1, 'test_tag', 'artist', 5)",
            rusqlite::params![account_id],
        )
        .unwrap();
    }

    let interactions_before: i64 = {
        let conn = db::open_db_for_calibration().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM feed_interactions WHERE account_id = ?1",
            rusqlite::params![account_id],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(
        interactions_before, 1,
        "should have 1 interaction before wipe"
    );

    // Wipe
    wipe_account(account_id);

    // All prefetch-related state should be cleared.
    let interactions_after: i64 = {
        let conn = db::open_db_for_calibration().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM feed_interactions WHERE account_id = ?1",
            rusqlite::params![account_id],
            |r| r.get(0),
        )
        .unwrap_or(0)
    };
    let tag_counts_after: i64 = {
        let conn = db::open_db_for_calibration().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM account_tag_counts WHERE account_id = ?1",
            rusqlite::params![account_id],
            |r| r.get(0),
        )
        .unwrap_or(0)
    };
    let account_count: i64 = {
        let conn = db::open_db_for_calibration().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM accounts WHERE id = ?1",
            rusqlite::params![account_id],
            |r| r.get(0),
        )
        .unwrap_or(0)
    };

    assert_eq!(interactions_after, 0, "interactions should be wiped");
    assert_eq!(tag_counts_after, 0, "tag_counts should be wiped");
    assert_eq!(account_count, 0, "account row should be wiped");
}

#[tokio::test(flavor = "multi_thread")]
async fn media_hydrator_repairs_returned_posts_and_purges_absent_ids() {
    let _guard = pipeline_lock().lock().await;
    ensure_migrations();
    let server = MockServer::start().await;
    let _cfg = install_mock_config(&server.uri());
    let ids = [9_200_001, 9_200_002];
    db::delete_catalog_posts_by_ids(&ids).unwrap();

    let returned: e621_account_parser_api::models::Post = serde_json::from_value(fake_post_json(
        ids[0],
        &["hydration_artist"],
        &["hydration_tag"],
    ))
    .unwrap();
    let mut stale = returned.clone();
    stale.uploader_id = 0;
    stale.files = Default::default();
    let mut absent = stale.clone();
    absent.id = ids[1];
    db::upsert_catalog_posts(&[stale, absent]).unwrap();

    Mock::given(method("GET"))
        .and(path("/posts.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(vec![returned]))
        .mount(&server)
        .await;
    e621_account_parser_api::media_hydrator::hydrate_catalog_once().await;

    let repaired = db::hydrate_posts_by_ids(&[ids[0]]).unwrap();
    assert_eq!(repaired[0].uploader_id, 42);
    assert_eq!(repaired[0].tags.general, vec!["hydration_tag"]);
    assert!(db::hydrate_posts_by_ids(&[ids[1]]).unwrap().is_empty());
    db::delete_catalog_posts_by_ids(&ids).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn authenticated_browse_and_recommendation_routes_use_mock_e621() {
    let _guard = pipeline_lock().lock().await;
    ensure_migrations();
    let server = MockServer::start().await;
    let _cfg = install_mock_config(&server.uri());
    let account_id = 9_200_100;
    let owner = "route_owner_token_9200100";
    wipe_account(account_id);
    db::set_account(owner, account_id, "test_user", "").unwrap();
    Mock::given(method("GET"))
        .and(path("/posts.json"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(vec![fake_post_json(
                9_200_101,
                &["route_artist"],
                &["route_tag"],
            )]),
        )
        .mount(&server)
        .await;
    let client = Client::tracked(rocket::build().mount(
        "/api",
        e621_account_parser_api::routes::integration_test_routes(),
    ))
    .await
    .unwrap();
    let cookie = Cookie::new(e621_account_parser_api::auth::OWNER_TOKEN_COOKIE, owner);
    for path in [
        format!("/api/recommendations/{account_id}?page=1"),
        format!("/api/browse/trending/{account_id}?page=1"),
        format!("/api/browse/favorites/{account_id}?page=1"),
    ] {
        let response = client.get(path).cookie(cookie.clone()).dispatch().await;
        assert_eq!(response.status(), Status::Ok);
    }
    wipe_account(account_id);
}
