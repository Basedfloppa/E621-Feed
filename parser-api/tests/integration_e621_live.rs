//! **Live** integration tests for the per-user e621 API key flow / direct
//! sync, exercised against the REAL e621 API.
//!
//! These are the opt-in counterpart to the hermetic wiremock suite in
//! `tests/integration_e621_mock.rs`. They are `#[ignore]`-gated AND require
//! `E621_LIVE_TESTS=1`, so they never run in a plain `cargo test` and can
//! never fire a stray request at e621 by accident.
//!
//! ```text
//! E621_LIVE_TESTS=1 cargo test --test integration_e621_live -- --ignored --test-threads=1
//! ```
//!
//! They load the *real* `config.toml` (real `posts_domain`, real admin
//! credentials), swapping only `db_path` to a process-isolated temp DB so the
//! production database is never touched. The admin account ("zorolin" in the
//! ship config) doubles as the test subject: its e621 API key is used both as
//! the admin key (public user lookup) and as the per-user ownership/sync key.
//! The key is read from config at runtime and never printed.
//!
//! WARNING: these hit real e621 endpoints and may pull a real account's
//! favourites. Run only when you intend to use the live API. Keep
//! `--test-threads=1`: every test shares the same real account id, so they
//! must not race on the single per-account encrypted key.

use e621_account_parser_api::db;
use e621_account_parser_api::models::{self, UserApiResponse};
use e621_account_parser_api::{api, auth, crypto};
use rocket::http::{ContentType, Cookie, Status};
use rocket::local::asynchronous::Client;
use std::path::PathBuf;
use std::sync::OnceLock;

// ------------------------------------------------------------------
//  Opt-in guard + isolated-live config
// ------------------------------------------------------------------

fn require_live_opt_in() {
    assert_eq!(
        std::env::var("E621_LIVE_TESTS").as_deref(),
        Ok("1"),
        "real-e621 tests are disabled by default. Enable with:\n  \
         E621_LIVE_TESTS=1 cargo test --test integration_e621_live -- --ignored --test-threads=1"
    );
}

static LIVE_SETUP: OnceLock<()> = OnceLock::new();

/// Load the real `config.toml` (real domain + admin creds) but point the DB at
/// a process-isolated temp file, then run migrations once.
fn ensure_live() {
    LIVE_SETUP.get_or_init(|| {
        require_live_opt_in();
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let real = std::fs::read_to_string(manifest.join("config.toml"))
            .expect("read real config.toml (opt-in guard means this is intended)");
        assert!(
            real.contains("e621.net"),
            "refusing to run live tests: posts_domain does not point at real e621.net"
        );

        let db_path = std::env::temp_dir().join(format!(
            "e621-account-parser-live-{}.db",
            std::process::id()
        ));
        let db_path = db_path.to_string_lossy().replace('\\', "\\\\");
        let modified = build_live_config(&real, &db_path);

        let mut file = tempfile::NamedTempFile::new().expect("temp file");
        use std::io::Write;
        file.write_all(modified.as_bytes())
            .expect("write live config");
        file.flush().expect("flush live config");
        e621_account_parser_api::models::reload_from(file.path()).expect("reload real config");

        // Sanity: we actually have real credentials to work with.
        let cfg = models::cfg();
        assert!(
            !cfg.admin_api.is_empty() && cfg.admin_api != "api_key",
            "real admin_api required for live tests"
        );
        assert!(
            !cfg.admin_user.is_empty(),
            "real admin_user required for live tests"
        );

        db::ensure_sqlite().expect("DB migrations failed");
    });
}

/// Insert/overwrite a top-level `db_path` in the real config, pointing at an
/// isolated temp DB. The real `config.toml` defines no `db_path` (it has a
/// default) and every real credential/domain must be preserved, so we place
/// the key before the first `[section]` — a top-level TOML key cannot come
/// after a table.
fn build_live_config(real: &str, db_path: &str) -> String {
    let mut out = String::with_capacity(real.len() + 64);
    let mut placed = false;
    for line in real.lines() {
        let trimmed = line.trim_start();
        // Overwrite an existing top-level db_path if the real config has one.
        if !placed && trimmed.starts_with("db_path") && trimmed[7..].trim_start().starts_with('=') {
            out.push_str(&format!("db_path = \"{db_path}\"\n"));
            placed = true;
            continue;
        }
        // Otherwise insert before the first `[section]` table.
        if !placed && trimmed.starts_with('[') {
            out.push_str(&format!("db_path = \"{db_path}\"\n"));
            placed = true;
        }
        out.push_str(line);
        out.push('\n');
    }
    assert!(
        placed,
        "could not place db_path in live config — config format changed"
    );
    out
}

/// Resolve the real owner account (the admin user) against live e621: its id
/// and canonical username. Uses only the admin key (public lookup).
async fn resolve_real_owner() -> (i32, String) {
    let cfg = models::cfg();
    let name = cfg.admin_user.clone();
    match api::get_user_by_name(&name).await {
        Ok(UserApiResponse::FullUser(u)) => (u.id, u.name.clone()),
        other => panic!(
            "could not resolve real owner {name:?} on e621: {:?}",
            other.map(|_| ())
        ),
    }
}

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

// ------------------------------------------------------------------
//  Live tests
// ------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
#[ignore = "hits the real e621 API; enable with E621_LIVE_TESTS=1 -- --ignored"]
async fn live_resolves_real_user_with_admin_key() {
    ensure_live();
    let (id, name) = resolve_real_owner().await;
    assert!(id > 0, "real e621 user id must be positive");
    assert!(!name.is_empty(), "real e621 user must have a name");
    eprintln!("[live] resolved real e621 owner: id={id} name={name:?}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "hits the real e621 API; enable with E621_LIVE_TESTS=1 -- --ignored"]
async fn live_key_test_with_real_key_reports_valid() {
    ensure_live();
    let (id, name) = resolve_real_owner().await;
    let owner = "live_owner_keytest_valid";

    let cfg = models::cfg();
    db::set_account(owner, id, &name, "").unwrap();
    db::set_account_e621_key(owner, id, &cfg.admin_api).unwrap();

    let client = test_client().await;
    let resp = client
        .post(format!("/api/account/{id}/key/test"))
        .cookie(owner_cookie(owner))
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok);
    let v: serde_json::Value = resp.into_json().await.unwrap();
    assert_eq!(
        v["valid"],
        serde_json::Value::Bool(true),
        "the real admin key must verify as the owner: {v}"
    );
    assert!(
        v["verifiedAt"].is_string(),
        "a successful live verify records verifiedAt"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "hits the real e621 API; enable with E621_LIVE_TESTS=1 -- --ignored"]
async fn live_key_test_with_wrong_key_reports_invalid() {
    ensure_live();
    let (id, name) = resolve_real_owner().await;
    let owner = "live_owner_keytest_wrong";

    db::set_account(owner, id, &name, "").unwrap();
    // A syntactically valid but wrong key — e621 must reject it with 401.
    db::set_account_e621_key(owner, id, "live_wrong_key_0011223344").unwrap();

    let client = test_client().await;
    let resp = client
        .post(format!("/api/account/{id}/key/test"))
        .cookie(owner_cookie(owner))
        .dispatch()
        .await;
    // A rejected credential is `KeyValidation::Invalid` → HTTP 200 {valid:false}.
    let v: serde_json::Value = resp.into_json().await.unwrap();
    assert_eq!(
        v["valid"],
        serde_json::Value::Bool(false),
        "a wrong key must NOT verify (got {v})"
    );
    assert!(v["verifiedAt"].is_null());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "hits the real e621 API; enable with E621_LIVE_TESTS=1 -- --ignored"]
async fn live_create_account_with_real_key_claims_and_stores_encrypted() {
    ensure_live();
    let (id, name) = resolve_real_owner().await;
    let owner = "live_owner_create_account";
    let cfg = models::cfg();

    let client = test_client().await;
    let cookie = owner_cookie(owner);
    let resp = client
        .post("/api/account")
        .cookie(cookie.clone())
        .header(ContentType::JSON)
        .body(serde_json::json!({ "id": id, "name": name, "api_key": cfg.admin_api }).to_string())
        .dispatch()
        .await;
    assert_eq!(
        resp.status(),
        Status::Ok,
        "claiming the owner account with the real key must succeed"
    );

    // Ownership proven → owner can read the account's export.
    let resp = client
        .get(format!("/api/account/{id}/export"))
        .cookie(cookie.clone())
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok, "owner reads own account");

    // Key round-trips and is encrypted at rest. Use `assert!` on boolean
    // conditions (not `assert_eq!`) so a failure message can never echo the
    // real key back into test output.
    let stored = db::get_account_e621_key(owner, id).unwrap();
    assert!(
        stored.as_deref() == Some(cfg.admin_api.as_str()),
        "stored key must round-trip through encrypted storage"
    );
    let raw: String = {
        let conn = db::open_db_for_calibration().unwrap();
        conn.query_row(
            "SELECT e621_api_key_encrypted FROM accounts WHERE id = ?1",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert!(
        raw != cfg.admin_api.as_str(),
        "raw column must not hold the plaintext"
    );
    assert!(!raw.contains(&cfg.admin_api), "no plaintext substring");
    let decrypted = crypto::decrypt(&raw).unwrap();
    assert!(
        decrypted == cfg.admin_api.as_bytes(),
        "stored blob must decrypt to the real key"
    );
}

/// Diagnostic probe: fetch the owner's real profile by key (same API path
/// the sync blacklist import uses — `api::get_user_with_key`) and report what
/// private blacklist e621 actually returned. Informational; asserts only that
/// the key-authenticated private fetch succeeded.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "hits the real e621 API; enable with E621_LIVE_TESTS=1 -- --ignored"]
async fn live_pulls_real_blacklist_by_key() {
    ensure_live();
    let (id, name) = resolve_real_owner().await;
    let cfg = models::cfg();
    let user = api::get_user_with_key(id, &name, &cfg.admin_api)
        .await
        .expect("key-authenticated private profile fetch must succeed");
    match &user.blacklisted_tags {
        Some(s) if !s.trim().is_empty() => {
            let lines: Vec<&str> = s.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
            eprintln!(
                "[live] REAL blacklist pulled via key: {} entr{}:",
                lines.len(),
                if lines.len() == 1 { "y" } else { "ies" }
            );
            for e in lines {
                eprintln!("  - {e}");
            }
        }
        other => {
            eprintln!("[live] blacklisted_tags field from e621: {other:?} (empty/none)");
        }
    }
    eprintln!(
        "[live] owner id={id} name={name:?} favorite_count={}",
        user.favorite_count
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "hits the real e621 API; enable with E621_LIVE_TESTS=1 -- --ignored"]
async fn live_sync_imports_real_favorites_and_blacklist() {
    ensure_live();
    let (id, name) = resolve_real_owner().await;
    let owner = "live_owner_sync_test";
    let cfg = models::cfg();

    db::set_account(owner, id, &name, "").unwrap();
    db::set_account_e621_key(owner, id, &cfg.admin_api).unwrap();

    let client = test_client().await;
    let cookie = owner_cookie(owner);
    let resp = client
        .post(format!("/api/account/{id}/sync"))
        .cookie(cookie.clone())
        .dispatch()
        .await;
    assert_eq!(
        resp.status(),
        Status::Ok,
        "direct sync against the real owner account must succeed"
    );
    let v: serde_json::Value = resp.into_json().await.unwrap();
    assert_eq!(v["synced"], serde_json::Value::Bool(true));
    let persisted = v["favoritesPersisted"].as_i64().unwrap_or(0);
    assert!(
        persisted > 0,
        "sync should import at least one real favourite (got {persisted})"
    );
    assert!(v["syncedAt"].is_string());

    // The owner's real private blacklist was imported (e621 field
    // `blacklisted_tags`).
    assert_eq!(
        v["blacklistImported"],
        serde_json::Value::Bool(true),
        "sync must report the real blacklist import: {v}"
    );
    let acc = db::get_account_by_id(owner, id).unwrap();
    assert!(
        acc.blacklist.split_whitespace().any(|t| t == "gore"),
        "stored blacklist must contain a real entry (e.g. 'gore'); got {:?}",
        acc.blacklist
    );

    // Sync status reflects the live run.
    let resp = client
        .get(format!("/api/account/{id}/sync/status"))
        .cookie(cookie)
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok);
    let s: serde_json::Value = resp.into_json().await.unwrap();
    assert_eq!(s["hasKey"], serde_json::Value::Bool(true));
    assert!(
        s["lastSyncedAt"].is_string(),
        "last-sync timestamp recorded"
    );

    // Imported favourites built tag-feedback co-occurrence for this account.
    let count: i64 = {
        let conn = db::open_db_for_calibration().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM account_tag_cooccurrence WHERE account_id = ?1",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert!(count > 0, "real favourites must populate tag co-occurrence");
}
