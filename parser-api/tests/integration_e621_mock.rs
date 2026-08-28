//! End-to-end integration tests for the **per-user e621 API key flow**
//! (M2 ownership gate) and **read-only direct account sync**, driven through
//! the real HTTP routes against a local wiremock e621 upstream.
//!
//! These complement the offline `tests/integration.rs` key/sync suite: that
//! file covers the deterministic branches (missing/malformed key, unlinked
//! token, no-key sync trigger), while this file stands up a fake e621 and
//! exercises the *happy paths* that require upstream responses:
//!
//!   * `POST /api/account` claim with a **valid** key → linked + encrypted
//!     key stored at rest; with a **wrong** key → 403; with an upstream
//!     failure → 502.
//!   * `POST /account/<id>/key/test` against a configured key → valid/invalid.
//!   * `POST /account/<id>/sync` → favourites + private blacklist imported.
//!
//! `cfg()` is a process-wide `ArcSwap<Config>`, so tests that point
//! `posts_domain` at a mock serialize through `MOCK_LOCK` (same pattern as
//! `tests/integration_pipeline.rs`).

mod support;

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::OnceLock;

use e621_account_parser_api::db;
use e621_account_parser_api::{auth, crypto};
use rocket::http::{ContentType, Cookie, Status};
use rocket::local::asynchronous::Client;
use tokio::sync::Mutex;
use wiremock::matchers::{basic_auth, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ------------------------------------------------------------------
//  Mock-e621 constants
// ------------------------------------------------------------------

/// A key the mock accepts as "valid" (authenticates the account).
const VALID_KEY: &str = "valid_mock_key_12345678";
/// A key the mock rejects with 401 (→ `KeyValidation::Invalid`).
const INVALID_KEY: &str = "invalid_mock_99999999";
/// The e621 username every mock user answers to — must match the
/// `name` field of [`fake_user_json`], because `create_account` compares
/// the public lookup name against the claimed name, and the key-verify /
/// user-with-key calls present `basic_auth(<that name>, <key>)`.
const MOCK_NAME: &str = "test_user";
/// Admin credentials after [`install_mock_config`] — used by the public,
/// admin-authenticated lookup `api::get_user_by_id`.
const ADMIN_USER: &str = "test_admin";
const ADMIN_PASS: &str = "test_api_key";
/// The owner's real private blacklist the mock returns from `users/<id>.json`;
/// asserted to be imported by direct sync.
const MOCK_BLACKLIST: [&str; 2] = ["mock_private_tag", "-mock_excluded_tag"];

// ------------------------------------------------------------------
//  Setup helpers (mirror tests/integration_pipeline.rs)
// ------------------------------------------------------------------

fn mock_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn ensure_migrations() {
    support::install_isolated_db_config();
    db::ensure_sqlite().expect("DB migrations failed");
    // The create-account tests exhaust the shared per-IP `acct_create:ip`
    // bucket; tests serialize on `mock_lock` but all run in one process, so
    // clear buckets at the start of each test to avoid cross-test 429s.
    e621_account_parser_api::ratelimit::reset_for_tests();
}

/// Point the global config at a wiremock e621 with a fast, deterministic
/// network profile. Preserves the process-isolated DB installed by `support`
/// (the read pool caches its path, so we must keep pointing at the same file).
fn install_mock_config(mock_uri: &str) -> tempfile::NamedTempFile {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let example =
        std::fs::read_to_string(manifest.join("config.example.toml")).expect("read example.toml");

    let mut modified = swap_toml_field(
        &example,
        "db_path",
        &format!("\"{}\"", e621_account_parser_api::models::cfg().db_path),
    );
    modified = swap_toml_field(&modified, "posts_domain", &format!("\"{mock_uri}\""));
    modified = swap_toml_field(&modified, "posts_limit", "4");
    modified = swap_toml_field(&modified, "admin_user", &format!("\"{ADMIN_USER}\""));
    modified = swap_toml_field(&modified, "admin_api", &format!("\"{ADMIN_PASS}\""));
    // Sync persists favourites in the favourites-collection scope — enable
    // save_favourites so the sync flow tests persist favourites.
    modified = swap_toml_field(&modified, "save_favourites", "true");
    // No retries and no mandatory inter-request delay → 5xx tests fail fast
    // and the sync loop is not throttled by the per-attempt backoff.
    modified = swap_toml_field(&modified, "max_retries", "0");
    modified = swap_toml_field(&modified, "rps_delay_ms", "0");

    let mut file = tempfile::NamedTempFile::new().expect("temp file");
    use std::io::Write;
    file.write_all(modified.as_bytes())
        .expect("write temp config");
    file.flush().expect("flush temp config");
    e621_account_parser_api::models::reload_from(file.path()).expect("reload config");
    file
}

/// Replace `key = ...` line in a TOML doc with `key = new`. Naive — matches
/// only the first `^key\s*=`, which is all we need (one definition per key).
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
        "key '{key}' not found in config.example.toml — mock-test setup is broken"
    );
    out
}

/// Build a Rocket test client mounting the same authenticated route set the
/// offline integration suite uses.
async fn test_client() -> Client {
    Client::tracked(rocket::build().mount(
        "/api",
        e621_account_parser_api::routes::integration_test_routes(),
    ))
    .await
    .unwrap()
}

fn owner_cookie(owner: &str) -> Cookie<'static> {
    Cookie::new(auth::OWNER_TOKEN_COOKIE, owner.to_string())
}

/// e621's canonical `POST` JSON for `/users/<id>.json` (matches `E621User`).
/// `favorite_count` drives how many favourites pages direct sync pulls; the
/// mock always includes the owner's private `blacklist`.
fn fake_user_json(id: i32, favorite_count: i32) -> serde_json::Value {
    fake_user_json_named(id, favorite_count, MOCK_NAME)
}

/// Like [`fake_user_json`] but with an explicit e621 username (used for the
/// admin_user account, whose name must match `ADMIN_USER` for admin_api sync).
fn fake_user_json_named(id: i32, favorite_count: i32, name: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "created_at": "2020-01-01T00:00:00.000-08:00",
        "name": name,
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
        "profile_about": "",
        "profile_artinfo": "",
        "is_verified": false,
        "has_cropped_avatar": false,
        "upload_slots": 10,
        "upload_karma": 0,
        "upload_karma_free": false,
        "blacklisted_tags": "mock_private_tag\n-mock_excluded_tag",
    })
}

/// One favourite `POST` in `favorites.json` (matches the parser's `Post`).
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

fn count_cooc_for_account(account_id: i32) -> i64 {
    let conn = db::open_db_for_calibration().expect("open DB");
    conn.query_row(
        "SELECT COUNT(*) FROM account_tag_cooccurrence WHERE account_id = ?1",
        rusqlite::params![account_id],
        |r| r.get(0),
    )
    .expect("count cooc")
}

// ------------------------------------------------------------------
//  M2 ownership gate — claiming with a key (against a mock e621)
// ------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn create_account_with_valid_key_links_and_stores_encrypted() {
    let _guard = mock_lock().lock().await;
    ensure_migrations();
    let server = MockServer::start().await;
    let _cfg = install_mock_config(&server.uri());

    let account_id = 8_810_001;
    let owner = "mock_owner_8810001";

    // Public, admin-authenticated lookup (`api::get_user_by_id`) → 200.
    Mock::given(method("GET"))
        .and(path(format!("/users/{account_id}.json")))
        .and(basic_auth(ADMIN_USER, ADMIN_PASS))
        .respond_with(ResponseTemplate::new(200).set_body_json(fake_user_json(account_id, 2)))
        .mount(&server)
        .await;
    // Ownership verify with a valid key → 200 user whose id matches → Valid.
    Mock::given(method("GET"))
        .and(path(format!("/users/{account_id}.json")))
        .and(basic_auth(MOCK_NAME, VALID_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(fake_user_json(account_id, 2)))
        .mount(&server)
        .await;

    let client = test_client().await;
    let cookie = owner_cookie(owner);
    let resp = client
        .post("/api/account")
        .cookie(cookie.clone())
        .header(ContentType::JSON)
        .body(
            serde_json::json!({ "id": account_id, "name": MOCK_NAME, "api_key": VALID_KEY })
                .to_string(),
        )
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok, "valid-key claim must succeed");

    // Ownership was proven → the owner can now read the account's data.
    let resp = client
        .get(format!("/api/account/{account_id}/export"))
        .cookie(cookie.clone())
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok, "owner reads own account");

    // The key round-trips through storage and is encrypted at rest.
    assert_eq!(
        db::get_account_e621_key(owner, account_id)
            .unwrap()
            .as_deref(),
        Some(VALID_KEY),
        "key round-trips through encrypted storage"
    );
    let raw: String = {
        let conn = db::open_db_for_calibration().unwrap();
        conn.query_row(
            "SELECT e621_api_key_encrypted FROM accounts WHERE id = ?1",
            rusqlite::params![account_id],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_ne!(raw, VALID_KEY, "raw column must not hold the plaintext");
    assert!(
        !raw.contains(VALID_KEY),
        "no plaintext substring in the blob"
    );
    assert_eq!(
        crypto::decrypt(&raw).unwrap(),
        VALID_KEY.as_bytes(),
        "stored blob decrypts to the original key"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn create_account_without_key_links_but_stores_no_key() {
    let _guard = mock_lock().lock().await;
    ensure_migrations();
    let server = MockServer::start().await;
    let _cfg = install_mock_config(&server.uri());

    let account_id = 8_810_009;
    let owner = "mock_owner_8810009";

    // Public, admin-authenticated lookup ONLY — no ownership-verify is needed
    // because no key was presented.
    Mock::given(method("GET"))
        .and(path(format!("/users/{account_id}.json")))
        .and(basic_auth(ADMIN_USER, ADMIN_PASS))
        .respond_with(ResponseTemplate::new(200).set_body_json(fake_user_json(account_id, 2)))
        .mount(&server)
        .await;

    let client = test_client().await;
    let cookie = owner_cookie(owner);
    let resp = client
        .post("/api/account")
        .cookie(cookie.clone())
        .header(ContentType::JSON)
        .body(
            serde_json::json!({ "id": account_id, "name": MOCK_NAME, "blacklist": "" }).to_string(),
        )
        .dispatch()
        .await;
    assert_eq!(
        resp.status(),
        Status::Ok,
        "no-key claim must succeed (key is optional)"
    );

    // The account is linked (the owner can read it)…
    let resp = client
        .get(format!("/api/account/{account_id}/export"))
        .cookie(cookie.clone())
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok, "owner reads own account");

    // …but no key is stored.
    assert_eq!(
        db::get_account_e621_key(owner, account_id).unwrap(),
        None,
        "no key provided → no encrypted key stored"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_key_is_shared_across_linked_devices() {
    let _guard = mock_lock().lock().await;
    ensure_migrations();
    let server = MockServer::start().await;
    let _cfg = install_mock_config(&server.uri());

    let account_id = 8_810_010;
    let owner_a = "mock_owner_dev_8810010a";
    let owner_b = "mock_owner_dev_8810010b";

    // Admin lookup + Device A's ownership verify both answer as the account.
    Mock::given(method("GET"))
        .and(path(format!("/users/{account_id}.json")))
        .and(basic_auth(ADMIN_USER, ADMIN_PASS))
        .respond_with(ResponseTemplate::new(200).set_body_json(fake_user_json(account_id, 2)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/users/{account_id}.json")))
        .and(basic_auth(MOCK_NAME, VALID_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(fake_user_json(account_id, 2)))
        .mount(&server)
        .await;

    let client = test_client().await;
    let cookie_a = owner_cookie(owner_a);
    let cookie_b = owner_cookie(owner_b);

    // Device A claims the account WITH its key.
    let resp = client
        .post("/api/account")
        .cookie(cookie_a.clone())
        .header(ContentType::JSON)
        .body(
            serde_json::json!({ "id": account_id, "name": MOCK_NAME, "api_key": VALID_KEY })
                .to_string(),
        )
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok, "device A claims with its key");

    // Device B (= a random person) links the SAME public account WITHOUT a key.
    let resp = client
        .post("/api/account")
        .cookie(cookie_b.clone())
        .header(ContentType::JSON)
        .body(
            serde_json::json!({ "id": account_id, "name": MOCK_NAME, "blacklist": "" }).to_string(),
        )
        .dispatch()
        .await;
    assert_eq!(
        resp.status(),
        Status::Ok,
        "device B links the same account without a key"
    );

    // The key is account-scoped: any LINKED device sees the same key, so sync
    // works from any of them. Device gating is about the link, not a
    // per-device copy.
    assert!(db::has_account_e621_key(owner_a, account_id).unwrap());
    assert_eq!(
        db::get_account_e621_key(owner_a, account_id)
            .unwrap()
            .as_deref(),
        Some(VALID_KEY),
        "A reads the account key"
    );
    assert_eq!(
        db::get_account_e621_key(owner_b, account_id)
            .unwrap()
            .as_deref(),
        Some(VALID_KEY),
        "B (linked, no key presented) reads the same account key"
    );
    assert!(db::has_account_e621_key(owner_b, account_id).unwrap());
    assert!(
        db::get_account_key_meta(owner_b, account_id)
            .unwrap()
            .has_key
    );

    // An UNLINKED token cannot touch the key (device gate = the link).
    let stranger = owner_b.to_string() + "_stranger";
    db::get_account_e621_key(&stranger, account_id)
        .expect_err("unlinked token cannot read the account key");
}

#[tokio::test(flavor = "multi_thread")]
async fn create_account_with_wrong_key_is_forbidden() {
    let _guard = mock_lock().lock().await;
    ensure_migrations();
    let server = MockServer::start().await;
    let _cfg = install_mock_config(&server.uri());

    let account_id = 8_810_002;
    let owner = "mock_owner_8810002";

    // Public admin lookup succeeds (so the rejection is about ownership).
    Mock::given(method("GET"))
        .and(path(format!("/users/{account_id}.json")))
        .and(basic_auth(ADMIN_USER, ADMIN_PASS))
        .respond_with(ResponseTemplate::new(200).set_body_json(fake_user_json(account_id, 2)))
        .mount(&server)
        .await;
    // The presented (wrong) key → 401 → not an owner.
    Mock::given(method("GET"))
        .and(path(format!("/users/{account_id}.json")))
        .and(basic_auth(MOCK_NAME, INVALID_KEY))
        .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
        .mount(&server)
        .await;

    let client = test_client().await;
    let cookie = owner_cookie(owner);
    let resp = client
        .post("/api/account")
        .cookie(cookie.clone())
        .header(ContentType::JSON)
        .body(
            serde_json::json!({ "id": account_id, "name": MOCK_NAME, "api_key": INVALID_KEY })
                .to_string(),
        )
        .dispatch()
        .await;
    assert_eq!(
        resp.status(),
        Status::Forbidden,
        "wrong key must be refused"
    );

    // No link was created: the owner token cannot reach the account's data.
    assert!(
        db::get_account_e621_key(owner, account_id).is_err(),
        "a refused claim must not link the token to the account"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn create_account_with_upstream_failure_is_502() {
    let _guard = mock_lock().lock().await;
    ensure_migrations();
    let server = MockServer::start().await;
    let _cfg = install_mock_config(&server.uri());

    let account_id = 8_810_003;
    let owner = "mock_owner_8810003";

    // Public lookup succeeds; the ownership verify hits an upstream error.
    Mock::given(method("GET"))
        .and(path(format!("/users/{account_id}.json")))
        .and(basic_auth(ADMIN_USER, ADMIN_PASS))
        .respond_with(ResponseTemplate::new(200).set_body_json(fake_user_json(account_id, 2)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/users/{account_id}.json")))
        .and(basic_auth(MOCK_NAME, VALID_KEY))
        .respond_with(ResponseTemplate::new(503).set_body_string("upstream unavailable"))
        .mount(&server)
        .await;

    let client = test_client().await;
    let cookie = owner_cookie(owner);
    let resp = client
        .post("/api/account")
        .cookie(cookie)
        .header(ContentType::JSON)
        .body(
            serde_json::json!({ "id": account_id, "name": MOCK_NAME, "api_key": VALID_KEY })
                .to_string(),
        )
        .dispatch()
        .await;
    assert_eq!(
        resp.status(),
        Status::ServiceUnavailable,
        "upstream failure during ownership verify → 502"
    );
}

// ------------------------------------------------------------------
//  /account/<id>/key/test — against a configured key
// ------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn key_test_with_valid_configured_key_reports_valid() {
    let _guard = mock_lock().lock().await;
    ensure_migrations();
    let server = MockServer::start().await;
    let _cfg = install_mock_config(&server.uri());

    let account_id = 8_810_004;
    let owner = "mock_owner_8810004";
    db::set_account(owner, account_id, MOCK_NAME, "").unwrap();
    db::set_account_e621_key(owner, account_id, VALID_KEY).unwrap();

    Mock::given(method("GET"))
        .and(path(format!("/users/{account_id}.json")))
        .and(basic_auth(MOCK_NAME, VALID_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(fake_user_json(account_id, 2)))
        .mount(&server)
        .await;

    let client = test_client().await;
    let resp = client
        .post(format!("/api/account/{account_id}/key/test"))
        .cookie(owner_cookie(owner))
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok);
    let v: serde_json::Value = resp.into_json().await.unwrap();
    assert_eq!(v["valid"], serde_json::Value::Bool(true));
    assert_eq!(v["name"], serde_json::json!(MOCK_NAME));
    assert!(
        v["verifiedAt"].is_string(),
        "a successful verify records a timestamp"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn key_test_with_invalid_configured_key_reports_invalid() {
    let _guard = mock_lock().lock().await;
    ensure_migrations();
    let server = MockServer::start().await;
    let _cfg = install_mock_config(&server.uri());

    let account_id = 8_810_005;
    let owner = "mock_owner_8810005";
    db::set_account(owner, account_id, MOCK_NAME, "").unwrap();
    db::set_account_e621_key(owner, account_id, INVALID_KEY).unwrap();

    Mock::given(method("GET"))
        .and(path(format!("/users/{account_id}.json")))
        .and(basic_auth(MOCK_NAME, INVALID_KEY))
        .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
        .mount(&server)
        .await;

    let client = test_client().await;
    let resp = client
        .post(format!("/api/account/{account_id}/key/test"))
        .cookie(owner_cookie(owner))
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok);
    let v: serde_json::Value = resp.into_json().await.unwrap();
    assert_eq!(v["valid"], serde_json::Value::Bool(false));
    assert!(
        v["verifiedAt"].is_null(),
        "an invalid key must carry no verification timestamp"
    );
}

// ------------------------------------------------------------------
//  Read-only direct sync — favourites + private blacklist import
// ------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn sync_trigger_without_key_is_rejected() {
    let _guard = mock_lock().lock().await;
    ensure_migrations();
    let server = MockServer::start().await;
    let _cfg = install_mock_config(&server.uri());

    let account_id = 8_810_012;
    let owner = "mock_owner_8810012";
    db::set_account(owner, account_id, MOCK_NAME, "").unwrap();
    // No e621 key stored. Catalog persistence is enabled in the mock config,
    // so the key check is what must reject (not the catalog gate).

    let client = test_client().await;
    let cookie = owner_cookie(owner);
    let resp = client
        .post(format!("/api/account/{account_id}/sync"))
        .cookie(cookie)
        .dispatch()
        .await;
    assert_eq!(
        resp.status(),
        Status::BadRequest,
        "sync without a key must be rejected clearly, got: {}",
        resp.status()
    );
    let v: serde_json::Value = resp.into_json().await.unwrap();
    let msg = v["error"].as_str().unwrap_or_default().to_lowercase();
    assert!(
        msg.contains("key"),
        "rejection should mention the missing key, got: {msg}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_imports_favorites_and_private_blacklist() {
    let _guard = mock_lock().lock().await;
    ensure_migrations();
    let server = MockServer::start().await;
    let _cfg = install_mock_config(&server.uri());

    let account_id = 8_810_006;
    let owner = "mock_owner_8810006";
    db::set_account(owner, account_id, MOCK_NAME, "").unwrap();
    db::set_account_e621_key(owner, account_id, VALID_KEY).unwrap();

    // get_user_with_key → the owner profile: 2 favourites + private blacklist.
    Mock::given(method("GET"))
        .and(path(format!("/users/{account_id}.json")))
        .and(basic_auth(MOCK_NAME, VALID_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(fake_user_json(account_id, 2)))
        .mount(&server)
        .await;
    // favourites page 1 → two posts (share an artist so co-occurrence accrues).
    Mock::given(method("GET"))
        .and(path("/favorites.json"))
        .and(query_param("user_id", account_id.to_string()))
        .and(query_param("page", "1"))
        .and(basic_auth(MOCK_NAME, VALID_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            fake_post_json(5_550_011, &["artist_a"], &["fluffy", "outdoor"]),
            fake_post_json(5_550_012, &["artist_a"], &["fluffy", "indoor"]),
        ])))
        .with_priority(1) // beats the catch-all favorites mock regardless of registration order
        .mount(&server)
        .await;
    // Any later page → empty (sync stops).
    Mock::given(method("GET"))
        .and(path("/favorites.json"))
        .and(basic_auth(MOCK_NAME, VALID_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;

    let client = test_client().await;
    let cookie = owner_cookie(owner);

    let resp = client
        .post(format!("/api/account/{account_id}/sync"))
        .cookie(cookie.clone())
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok);
    let v: serde_json::Value = resp.into_json().await.unwrap();
    assert_eq!(v["synced"], serde_json::Value::Bool(true));
    assert_eq!(v["favoritesPersisted"], serde_json::json!(2));
    assert_eq!(v["blacklistImported"], serde_json::Value::Bool(true));
    assert!(v["syncedAt"].is_string());

    // Sync status now reports a key and a last-sync timestamp.
    let resp = client
        .get(format!("/api/account/{account_id}/sync/status"))
        .cookie(cookie.clone())
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok);
    let s: serde_json::Value = resp.into_json().await.unwrap();
    assert_eq!(s["hasKey"], serde_json::Value::Bool(true));
    assert!(
        s["lastSyncedAt"].is_string(),
        "last-sync timestamp recorded"
    );

    // The owner's real private blacklist was written back to the account.
    let acc = db::get_account_by_id(owner, account_id).unwrap();
    for tag in MOCK_BLACKLIST {
        assert!(
            acc.blacklist.split_whitespace().any(|t| t == tag),
            "blacklist must contain mock entry {tag:?}; got {:?}",
            acc.blacklist
        );
    }

    // Favourites were persisted through the pipeline writer → co-occurrence
    // tag feedback was built for the comment pair sharing `artist_a`.
    assert!(
        count_cooc_for_account(account_id) > 0,
        "imported favourites must populate account_tag_cooccurrence"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn admin_account_syncs_with_admin_api_without_stored_key() {
    let _guard = mock_lock().lock().await;
    ensure_migrations();
    let server = MockServer::start().await;
    let _cfg = install_mock_config(&server.uri());

    // The admin_user account has NO stored key — sync must use the shared
    // admin_api to pull its tags/blacklist.
    let account_id = 8_810_011;
    let owner = "mock_owner_admin_8810011";
    db::set_account(owner, account_id, ADMIN_USER, "").unwrap();
    assert!(
        !db::has_account_e621_key(owner, account_id).unwrap(),
        "admin account starts with no stored key"
    );

    // admin_api-authenticated endpoints: user lookup (tags/blacklist) + favourites.
    Mock::given(method("GET"))
        .and(path(format!("/users/{account_id}.json")))
        .and(basic_auth(ADMIN_USER, ADMIN_PASS))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(fake_user_json_named(account_id, 1, ADMIN_USER)),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/favorites.json"))
        .and(basic_auth(ADMIN_USER, ADMIN_PASS))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            fake_post_json(5_550_021, &["artist_a"], &["fluffy"]),
        ])))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/favorites.json"))
        .and(basic_auth(ADMIN_USER, ADMIN_PASS))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;

    let client = test_client().await;
    let resp = client
        .post(format!("/api/account/{account_id}/sync"))
        .cookie(owner_cookie(owner))
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok);
    let v: serde_json::Value = resp.into_json().await.unwrap();
    assert_eq!(v["synced"], serde_json::Value::Bool(true));
    assert_eq!(v["blacklistImported"], serde_json::Value::Bool(true));

    // The admin account's blacklist was pulled even though it holds no key.
    let acc = db::get_account_by_id(owner, account_id).unwrap();
    for tag in MOCK_BLACKLIST {
        assert!(
            acc.blacklist.split_whitespace().any(|t| t == tag),
            "admin blacklist must contain {tag:?}; got {:?}",
            acc.blacklist
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_with_upstream_error_returns_502() {
    let _guard = mock_lock().lock().await;
    ensure_migrations();
    let server = MockServer::start().await;
    let _cfg = install_mock_config(&server.uri());

    let account_id = 8_810_007;
    let owner = "mock_owner_8810007";
    db::set_account(owner, account_id, MOCK_NAME, "").unwrap();
    db::set_account_e621_key(owner, account_id, VALID_KEY).unwrap();

    Mock::given(method("GET"))
        .and(path(format!("/users/{account_id}.json")))
        .and(basic_auth(MOCK_NAME, VALID_KEY))
        .respond_with(ResponseTemplate::new(503).set_body_string("upstream unavailable"))
        .mount(&server)
        .await;

    let client = test_client().await;
    let resp = client
        .post(format!("/api/account/{account_id}/sync"))
        .cookie(owner_cookie(owner))
        .dispatch()
        .await;
    assert_eq!(
        resp.status(),
        Status::ServiceUnavailable,
        "sync surfaces an upstream failure as 502 (read-only, no partial state)"
    );
}

// ------------------------------------------------------------------
//  Ownership / route hygiene sanity
// ------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn key_endpoints_refuse_unlinked_token() {
    let _guard = mock_lock().lock().await;
    ensure_migrations();
    let server = MockServer::start().await;
    let _cfg = install_mock_config(&server.uri());

    // No account linked to this token: every key/sync route must refuse.
    let account_id = 8_810_008;
    let stranger = owner_cookie("stranger_token_8810008");
    let client = test_client().await;

    for (method, path) in [
        ("GET", format!("/api/account/{account_id}/key/state")),
        ("PUT", format!("/api/account/{account_id}/key")),
        ("DELETE", format!("/api/account/{account_id}/key")),
        ("POST", format!("/api/account/{account_id}/key/test")),
        ("POST", format!("/api/account/{account_id}/sync")),
        ("GET", format!("/api/account/{account_id}/sync/status")),
    ] {
        let uri = path.clone();
        let req = client.req(rocket::http::Method::from_str(method).unwrap(), uri);
        let req = req.cookie(stranger.clone());
        let req = if method == "PUT" {
            req.header(ContentType::JSON)
                .body(serde_json::json!({ "key": VALID_KEY }).to_string())
        } else {
            req
        };
        let resp = req.dispatch().await;
        assert_ne!(
            resp.status(),
            Status::Ok,
            "{method} {path} must refuse an unlinked token"
        );
    }
}
