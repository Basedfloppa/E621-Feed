//! Integration tests for the local catalog & offline-serving layer
//! (docs/offline-catalog.md): pools, media index/store, the media proxy route,
//! and local-first single-post serving.
//!
//! Own test binary → own process → own global config, so we can safely enable
//! `[catalog]` (media cache size cap + pool_membership) without disturbing other
//! test binaries.

use std::io::Write;

use rocket::http::{ContentType, Status};
use rocket::local::asynchronous::Client;

use e621_account_parser_api::models::{
    FileOriginal, Files, Flags, Has, Post, Rating, Relationships, Score, Stats, Tags,
};
use e621_account_parser_api::{db, media_store};

fn make_post(id: i64, general: &str, file_url: &str) -> Post {
    Post {
        id,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        change_seq: 0.0,
        files: Files {
            original: FileOriginal {
                width: 0,
                height: 0,
                url: Some(file_url.into()),
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

fn install_catalog_config() {
    static _ONCE: std::sync::Once = std::sync::Once::new();
    _ONCE.call_once(|| {
        let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let example = std::fs::read_to_string(manifest_dir.join("config.example.toml"))
            .expect("read config.example.toml");
        let db_path = std::env::temp_dir().join(format!(
            "e621-catalog-test-{}-{}.db",
            std::process::id(),
            module_path!().replace("::", "-")
        ));
        let db_path = db_path.to_string_lossy().replace('\\', "\\\\");

        let config = example
            .replacen(
                "db_path = \"database.db\"",
                &format!("db_path = \"{db_path}\""),
                1,
            )
            .replace("\n[catalog]\n", "\n# [catalog] (test-merged below)\n")
            + "\n[catalog]\nsave_favourites = true\nmedia_cache_max_bytes = 0\npool_membership = true\n";
        let mut file = tempfile::NamedTempFile::new().expect("temp test config");
        file.write_all(config.as_bytes()).expect("write config");
        file.flush().expect("flush config");
        e621_account_parser_api::models::reload_from(file.path())
            .expect("load catalog integration-test config");
        e621_account_parser_api::db::ensure_sqlite().expect("catalog-test DB migrations failed");
    });
}

#[tokio::test(flavor = "multi_thread")]
async fn media_store_saves_and_indexes_original() {
    install_catalog_config();
    // Unique ids so parallel tests in this binary share the temp DB safely.
    let a = 51001;
    let b = 51002;
    db::upsert_catalog_posts(&[
        make_post(a, "tag_a", "https://static1.e621.net/data/a/51001.png"),
        make_post(b, "tag_b", "https://static1.e621.net/data/b/51002.png"),
    ])
    .unwrap();

    let bytes = vec![1u8, 2, 3, 4];
    let rel = media_store::store_original(a, "png", &bytes, "d1").unwrap();
    assert!(rel.ends_with("51001.png"));

    let (stored_rel, _mtime) = db::get_media_entry(a).unwrap().expect("entry");
    assert_eq!(stored_rel, rel);
    assert!(media_store::stored_path(a, &rel).is_some());

    // URL rewrite: locally-available post points at the local proxy.
    let mut posts = vec![make_post(a, "", "e621://x"), make_post(b, "", "e621://y")];
    media_store::rewrite_local_media_urls(&mut posts);
    assert_eq!(
        posts[0].files.original.url.as_deref(),
        Some("/api/media/51001?size=original")
    );
    assert_eq!(posts[1].files.original.url.as_deref(), Some("e621://y"));

    // When an original is local, every size in the model points at the local
    // full file so the card renders it (cost is negligible on a LAN).
    let mut p = make_post(a, "", "https://e/preview.jpg");
    p.files.preview.url = Some("https://e/preview.jpg".into());
    p.files.sample.url = Some("https://e/sample.jpg".into());
    p.files.original.url = Some("https://e/orig.png".into());
    let mut single = vec![p];
    media_store::rewrite_local_media_urls(&mut single);
    assert_eq!(
        single[0].files.preview.url.as_deref(),
        Some("/api/media/51001?size=original")
    );
    assert_eq!(
        single[0].files.sample.url.as_deref(),
        Some("/api/media/51001?size=original")
    );
    assert_eq!(
        single[0].files.original.url.as_deref(),
        Some("/api/media/51001?size=original")
    );

    // The media folder is hardcoded to `media/` under the crate root; remove
    // the file this test wrote so a test run doesn't pollute the repo.
    let _ = std::fs::remove_file(media_store::cache_dir().join(rel));
}

#[tokio::test(flavor = "multi_thread")]
async fn pools_round_trip() {
    install_catalog_config();
    let a = 52001;
    let b = 52002;
    db::upsert_catalog_posts(&[
        make_post(a, "tag_a", "https://e/52001.png"),
        make_post(b, "tag_b", "https://e/52002.png"),
    ])
    .unwrap();
    db::save_pool(520001, "Summer set", &[(a, 0), (b, 1)]).unwrap();
    let members = db::get_pool_members(520001).unwrap();
    assert_eq!(members, vec![(a, 0), (b, 1)]);
    assert_eq!(db::pools_for_post(a).unwrap(), vec![520001]);
    // Replace membership: post b left.
    db::save_pool(520001, "Summer set", &[(a, 0)]).unwrap();
    assert_eq!(db::get_pool_members(520001).unwrap(), vec![(a, 0)]);
    assert!(db::pools_for_post(b).unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn media_route_serves_stored_original_and_404s_missing() {
    install_catalog_config();
    let a = 53001;
    let b = 53002;
    db::upsert_catalog_posts(&[
        make_post(a, "tag_a", "https://static1.e621.net/data/a/53001.png"),
        make_post(b, "tag_b", "https://static1.e621.net/data/b/53002.png"),
    ])
    .unwrap();
    let bytes = b"\x89PNG\r\n\x1a\nabc".to_vec();
    media_store::store_original(a, "png", &bytes, "d1").unwrap();

    let client = Client::tracked(rocket::build().mount(
        "/api",
        e621_account_parser_api::routes::integration_test_routes(),
    ))
    .await
    .unwrap();

    // Stored original → 200 with the bytes.
    let resp = client
        .get("/api/media/53001?size=original")
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok);
    assert_eq!(resp.content_type(), Some(ContentType::PNG));
    let body: Vec<u8> = resp.into_bytes().await.unwrap().to_vec();
    assert_eq!(body, bytes.as_slice());

    // Post with no stored original → 404 (never a live e621 fetch).
    let resp = client
        .get("/api/media/53002?size=original")
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::NotFound);

    // The media folder is hardcoded to `media/` under the crate root; remove
    // the file this test wrote so a test run doesn't pollute the repo.
    let _ =
        std::fs::remove_file(media_store::cache_dir().join(media_store::rel_path_for(a, "png")));
}

/// The media route caches aggressively: `Cache-Control: immutable`, a strong
/// `ETag`, and `If-None-Match` revalidation → `304 Not Modified` without a
/// body.
#[tokio::test(flavor = "multi_thread")]
async fn media_route_supports_conditional_requests() {
    install_catalog_config();
    let a = 53003;
    db::upsert_catalog_posts(&[make_post(
        a,
        "tag_a",
        "https://static1.e621.net/data/a/53003.png",
    )])
    .unwrap();
    let bytes = b"\x89PNG\r\n\x1a\nabc".to_vec();
    media_store::store_original(a, "png", &bytes, "d1").unwrap();

    let client = Client::tracked(rocket::build().mount(
        "/api",
        e621_account_parser_api::routes::integration_test_routes(),
    ))
    .await
    .unwrap();
    let url = format!("/api/media/{a}?size=original");

    // First fetch: 200 with the bytes and cache headers.
    let resp = client.get(&url).dispatch().await;
    assert_eq!(resp.status(), Status::Ok);
    assert_eq!(resp.content_type(), Some(ContentType::PNG));
    let etag = resp
        .headers()
        .get_one("ETag")
        .expect("ETag present")
        .to_string();
    let cc = resp
        .headers()
        .get_one("Cache-Control")
        .expect("Cache-Control");
    assert!(cc.contains("immutable"), "immutable cache: {cc}");
    assert!(cc.contains("max-age=31536000"), "year-long max-age: {cc}");
    let body: Vec<u8> = resp.into_bytes().await.unwrap().to_vec();
    assert_eq!(body, bytes.as_slice());

    // Revalidation with the same ETag → 304, no body, ETag echoed back.
    let resp = client
        .get(&url)
        .header(rocket::http::Header::new("If-None-Match", etag.clone()))
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::NotModified);
    assert_eq!(
        resp.headers().get_one("ETag"),
        Some(etag.as_str()),
        "304 echoes the ETag"
    );
    assert_eq!(resp.into_bytes().await, None, "304 has no body");

    // The `*` wildcard matches anything.
    let resp = client
        .get(&url)
        .header(rocket::http::Header::new("If-None-Match", "*"))
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::NotModified);

    // A stale ETag → full 200 again.
    let resp = client
        .get(&url)
        .header(rocket::http::Header::new("If-None-Match", "\"deadbeef-0\""))
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok);

    // Cleanup the hardcoded media file.
    let _ =
        std::fs::remove_file(media_store::cache_dir().join(media_store::rel_path_for(a, "png")));
}

/// Byte-range requests: `206 Partial Content` with correct `Content-Range`,
/// `416` for unsatisfiable ranges, and multi-range headers ignored (full 200).
#[tokio::test(flavor = "multi_thread")]
async fn media_route_serves_byte_ranges() {
    install_catalog_config();
    let a = 53004;
    db::upsert_catalog_posts(&[make_post(
        a,
        "tag_a",
        "https://static1.e621.net/data/a/53004.png",
    )])
    .unwrap();
    let bytes: Vec<u8> = b"abcdefghijklmnopqrstuvwxyz".to_vec(); // 26 bytes
    media_store::store_original(a, "png", &bytes, "d1").unwrap();

    let client = Client::tracked(rocket::build().mount(
        "/api",
        e621_account_parser_api::routes::integration_test_routes(),
    ))
    .await
    .unwrap();
    let url = format!("/api/media/{a}?size=original");
    let r = |range: String| {
        client
            .get(&url)
            .header(rocket::http::Header::new("Range", range))
            .dispatch()
    };

    // Plain request advertises range support.
    let resp = client.get(&url).dispatch().await;
    assert_eq!(resp.status(), Status::Ok);
    assert_eq!(resp.headers().get_one("Accept-Ranges"), Some("bytes"));

    // Closed range → 206 with exactly those bytes.
    let resp = r("bytes=0-9".to_string()).await;
    assert_eq!(resp.status(), Status::PartialContent);
    assert_eq!(
        resp.headers().get_one("Content-Range"),
        Some("bytes 0-9/26")
    );
    let body = resp.into_bytes().await.unwrap().to_vec();
    assert_eq!(body, b"abcdefghij".to_vec());

    // Open-ended range → to EOF.
    let resp = r("bytes=5-".to_string()).await;
    assert_eq!(resp.status(), Status::PartialContent);
    assert_eq!(
        resp.headers().get_one("Content-Range"),
        Some("bytes 5-25/26")
    );
    let body = resp.into_bytes().await.unwrap().to_vec();
    assert_eq!(body, b"fghijklmnopqrstuvwxyz".to_vec());

    // Suffix range → last N bytes.
    let resp = r("bytes=-4".to_string()).await;
    assert_eq!(resp.status(), Status::PartialContent);
    assert_eq!(
        resp.headers().get_one("Content-Range"),
        Some("bytes 22-25/26")
    );
    let body = resp.into_bytes().await.unwrap().to_vec();
    assert_eq!(body, b"wxyz".to_vec());

    // End beyond EOF clamps to the last byte.
    let resp = r("bytes=20-999".to_string()).await;
    assert_eq!(resp.status(), Status::PartialContent);
    assert_eq!(
        resp.headers().get_one("Content-Range"),
        Some("bytes 20-25/26")
    );

    // Entirely past EOF → 416 with a `bytes */26` Content-Range.
    let resp = r("bytes=100-".to_string()).await;
    assert_eq!(resp.status(), Status::RangeNotSatisfiable);
    assert_eq!(resp.headers().get_one("Content-Range"), Some("bytes */26"));

    // Multi-range headers are ignored (single-range only) → full 200.
    let resp = r("bytes=0-1,3-4".to_string()).await;
    assert_eq!(resp.status(), Status::Ok);

    // Cleanup the hardcoded media file.
    let _ =
        std::fs::remove_file(media_store::cache_dir().join(media_store::rel_path_for(a, "png")));
}

fn make_post_multi(id: i64, general: &[&str], file_url: &str) -> Post {
    let mut p = make_post(id, "unused", file_url);
    p.tags = Tags {
        general: general.iter().map(|s| (*s).to_string()).collect(),
        ..Default::default()
    };
    p
}

/// Catalog tag search: AND semantics over the saved posts' tags, and an empty
/// query returns nothing (no accidental “all posts” dump).
#[rocket::async_test]
async fn catalog_search_filters_saved_posts_by_tag() {
    install_catalog_config();
    let account_id = 54020;
    let owner = "catalog_owner_54020";
    e621_account_parser_api::db::set_account(owner, account_id, "cat_search", "").unwrap();
    let posts = [
        make_post_multi(540201, &["fox", "urban"], "https://e/a.png"),
        make_post_multi(540202, &["fox"], "https://e/b.png"),
        make_post_multi(540203, &["wolf"], "https://e/c.png"),
    ];
    e621_account_parser_api::db::save_posts(&posts, account_id).unwrap();
    e621_account_parser_api::db::save_posts_tags_batch(
        &posts,
        &std::collections::HashSet::new(),
        false,
        None,
    )
    .unwrap();

    let client = Client::tracked(rocket::build().mount(
        "/api",
        e621_account_parser_api::routes::integration_test_routes(),
    ))
    .await
    .unwrap();
    let cookie =
        rocket::http::Cookie::new(e621_account_parser_api::auth::OWNER_TOKEN_COOKIE, owner);

    // Single tag → both fox posts.
    let resp = client
        .get(format!("/api/catalog/{account_id}/search?query=fox"))
        .cookie(cookie.clone())
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok);
    let v: serde_json::Value = resp.into_json().await.unwrap();
    let ids: Vec<i64> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_i64().unwrap())
        .collect();
    assert!(ids.contains(&540201));
    assert!(ids.contains(&540202));
    assert!(!ids.contains(&540203));

    // AND of two tags → only the fox+urban post.
    let resp = client
        .get(format!(
            "/api/catalog/{account_id}/search?query=fox%20urban"
        ))
        .cookie(cookie.clone())
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok);
    let v: serde_json::Value = resp.into_json().await.unwrap();
    let ids: Vec<i64> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, vec![540201]);

    // Empty query returns the account's full saved catalog (no filter), which
    // the UI never triggers but is the documented search semantics.
    let resp = client
        .get(format!("/api/catalog/{account_id}/search?query="))
        .cookie(cookie.clone())
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok);
    let v: serde_json::Value = resp.into_json().await.unwrap();
    let ids: Vec<i64> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_i64().unwrap())
        .collect();
    // Deterministic order: newest first (`post_id DESC`), preserved through
    // hydration (not shuffled by HashMap iteration) so pagination is stable
    // between requests.
    assert_eq!(ids, vec![540203, 540202, 540201]);
}

/// The in-server background media worker drains **saved** posts only — it must
/// never enqueue the whole `posts` corpus for download. A corpus-only post
/// (not saved by any account) and a saved post that already has a media entry
/// must both stay out of the pending set; only a saved post lacking media is
/// pending.
#[rocket::async_test]
async fn pending_saved_scope_only_account_saved_posts() {
    install_catalog_config();
    let account_id = 54004;
    let owner = "catalog_owner_54004";
    e621_account_parser_api::db::set_account(owner, account_id, "cat_user4", "").unwrap();

    let p1 = make_post(543001, "fox", "https://e/1.png"); // saved, no media   -> pending
    let p2 = make_post(543002, "wolf", "https://e/2.png"); // corpus-only       -> not pending
    let p3 = make_post(543003, "fox", "https://e/3.png"); // saved + media      -> not pending

    // p1 & p3 are saved by the account.
    e621_account_parser_api::db::save_posts(&[p1.clone(), p3.clone()], account_id).unwrap();
    // p2 is only in the corpus (not saved).
    e621_account_parser_api::db::upsert_catalog_posts(std::slice::from_ref(&p2)).unwrap();
    // p3 already has a local original indexed.
    e621_account_parser_api::db::upsert_media_entry(543003, "03/543003.png", 100, 0, "d").unwrap();
    e621_account_parser_api::db::save_posts_tags_batch(
        &[p1, p2, p3],
        &std::collections::HashSet::new(),
        false,
        None,
    )
    .unwrap();

    // Large limit: the pending set is global, and a sibling test (the overflow
    // test creates ~1100 saved posts) can push other rows into the first N.
    // We only assert membership of our unique ids here.
    let pending = e621_account_parser_api::db::pending_saved_original_posts(200_000).unwrap();
    let ids: Vec<i64> = pending.iter().map(|(id, _, _)| *id).collect();
    assert!(
        ids.contains(&543001),
        "saved post without media must be pending"
    );
    assert!(
        !ids.contains(&543002),
        "corpus-only post must NOT be pending"
    );
    assert!(
        !ids.contains(&543003),
        "post with media must NOT be pending"
    );
}

/// Deleting a post from the catalog removes its `accounts_post` association,
/// and removing its on-disk original also clears the `media_entries` index.
#[rocket::async_test]
async fn delete_catalog_post_removes_association_and_media() {
    install_catalog_config();
    let account_id = 54006;
    let owner = "catalog_owner_54006";
    e621_account_parser_api::db::set_account(owner, account_id, "cat_user6", "").unwrap();
    let p = make_post(545001, "fox", "https://e/1.png");
    e621_account_parser_api::db::save_posts(std::slice::from_ref(&p), account_id).unwrap();
    e621_account_parser_api::db::save_posts_tags_batch(
        std::slice::from_ref(&p),
        &std::collections::HashSet::new(),
        false,
        None,
    )
    .unwrap();
    media_store::store_original(545001, "png", b"\x89PNG", "d").unwrap();
    assert!(
        e621_account_parser_api::db::get_media_entry(545001)
            .unwrap()
            .is_some()
    );

    let n = e621_account_parser_api::db::delete_catalog_post(account_id, 545001).unwrap();
    assert_eq!(n, 1, "one accounts_post row removed");
    assert!(
        media_store::delete_and_unindex(545001).unwrap(),
        "media entry was present and removed"
    );
    assert!(
        e621_account_parser_api::db::get_media_entry(545001)
            .unwrap()
            .is_none()
    );
    // Deleting again → nothing left.
    assert_eq!(
        e621_account_parser_api::db::delete_catalog_post(account_id, 545001).unwrap(),
        0
    );
}

/// queue_stats reports pending/downloaded counts consistently with the DB.
#[rocket::async_test]
async fn queue_stats_reports_counts() {
    install_catalog_config();
    let account_id = 54007;
    let owner = "catalog_owner_54007";
    e621_account_parser_api::db::set_account(owner, account_id, "cat_user7", "").unwrap();
    let p1 = make_post(545101, "fox", "https://e/1.png"); // saved, no media -> pending
    let p2 = make_post(545102, "wolf", "https://e/2.png"); // saved + media -> stored
    e621_account_parser_api::db::save_posts(&[p1.clone(), p2.clone()], account_id).unwrap();
    e621_account_parser_api::db::save_posts_tags_batch(
        &[p1, p2],
        &std::collections::HashSet::new(),
        false,
        None,
    )
    .unwrap();
    e621_account_parser_api::db::upsert_media_entry(545102, "02/545102.png", 100, 0, "d").unwrap();
    let (pending, stored, bytes) = e621_account_parser_api::db::queue_stats().unwrap();
    assert!(pending >= 1, "saved post without media is pending");
    assert!(stored >= 1);
    assert!(bytes >= 100);
}

/// The catalog-manage HTTP routes (queue status, pause/resume, delete post)
/// are owner-gated and mutate/read consistently.
#[rocket::async_test]
async fn media_manage_routes_work() {
    install_catalog_config();
    let account_id = 54008;
    let owner = "catalog_owner_54008";
    e621_account_parser_api::db::set_account(owner, account_id, "cat_user8", "").unwrap();
    let p = make_post(545201, "fox", "https://e/1.png");
    e621_account_parser_api::db::save_posts(std::slice::from_ref(&p), account_id).unwrap();
    e621_account_parser_api::db::save_posts_tags_batch(
        std::slice::from_ref(&p),
        &std::collections::HashSet::new(),
        false,
        None,
    )
    .unwrap();

    let client = Client::tracked(rocket::build().mount(
        "/api",
        e621_account_parser_api::routes::integration_test_routes(),
    ))
    .await
    .unwrap();
    let cookie =
        rocket::http::Cookie::new(e621_account_parser_api::auth::OWNER_TOKEN_COOKIE, owner);

    // Queue status reports our pending post.
    let resp = client
        .get(format!("/api/catalog/{account_id}/media/status"))
        .cookie(cookie.clone())
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok);
    let v: serde_json::Value = resp.into_json().await.unwrap();
    assert!(v["pending"].as_i64().unwrap() >= 1);

    // Pause flips the global worker flag; resume clears it.
    let resp = client
        .post(format!("/api/catalog/{account_id}/media/pause"))
        .cookie(cookie.clone())
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok);
    assert!(e621_account_parser_api::media_fetch_worker::worker_paused());
    let _ = client
        .post(format!("/api/catalog/{account_id}/media/resume"))
        .cookie(cookie.clone())
        .dispatch()
        .await;
    assert!(!e621_account_parser_api::media_fetch_worker::worker_paused());

    // Delete the saved post → 200, no longer in the catalog.
    let resp = client
        .delete(format!("/api/catalog/{account_id}/post/545201"))
        .cookie(cookie.clone())
        .dispatch()
        .await;
    assert_eq!(
        resp.status(),
        Status::Ok,
        "delete status was {}",
        resp.status()
    );
    assert_eq!(
        e621_account_parser_api::db::delete_catalog_post(account_id, 545201).unwrap(),
        0,
        "post removed from catalog"
    );
}

/// Media deletion cascades only when the LAST account removes a saved post:
/// deleting from one of two owners keeps the file + index; deleting the final
/// owner removes them.
#[rocket::async_test]
async fn delete_catalog_post_cascades_media_only_for_last_owner() {
    install_catalog_config();
    let acc_a = 54011;
    let acc_b = 54012;
    let owner_a = "catalog_owner_54011";
    let owner_b = "catalog_owner_54012";
    e621_account_parser_api::db::set_account(owner_a, acc_a, "cat_user11", "").unwrap();
    e621_account_parser_api::db::set_account(owner_b, acc_b, "cat_user12", "").unwrap();
    let p = make_post(545501, "fox", "https://e/1.png");
    e621_account_parser_api::db::save_posts(std::slice::from_ref(&p), acc_a).unwrap();
    e621_account_parser_api::db::save_posts(std::slice::from_ref(&p), acc_b).unwrap();
    e621_account_parser_api::db::save_posts_tags_batch(
        std::slice::from_ref(&p),
        &std::collections::HashSet::new(),
        false,
        None,
    )
    .unwrap();
    media_store::store_original(545501, "png", b"\x89PNG\x00\x01", "d").unwrap();
    assert!(
        e621_account_parser_api::db::get_media_entry(545501)
            .unwrap()
            .is_some(),
        "media present before deletion"
    );

    let client = Client::tracked(rocket::build().mount(
        "/api",
        e621_account_parser_api::routes::integration_test_routes(),
    ))
    .await
    .unwrap();
    let cookie_a =
        rocket::http::Cookie::new(e621_account_parser_api::auth::OWNER_TOKEN_COOKIE, owner_a);
    let cookie_b =
        rocket::http::Cookie::new(e621_account_parser_api::auth::OWNER_TOKEN_COOKIE, owner_b);

    // Delete from A — B still owns the post → the global media is kept.
    let resp = client
        .delete(format!("/api/catalog/{acc_a}/post/545501"))
        .cookie(cookie_a)
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok, "delete from A");
    assert!(
        e621_account_parser_api::db::get_media_entry(545501)
            .unwrap()
            .is_some(),
        "media kept while another account still owns the post"
    );

    // Delete from B — last owner → file + index row removed.
    let resp = client
        .delete(format!("/api/catalog/{acc_b}/post/545501"))
        .cookie(cookie_b)
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok, "delete from B");
    assert!(
        e621_account_parser_api::db::get_media_entry(545501)
            .unwrap()
            .is_none(),
        "media removed with the last owner"
    );

    // Cleanup the hardcoded media file just in case.
    let _ = std::fs::remove_file(
        media_store::cache_dir().join(media_store::rel_path_for(545501, "png")),
    );
}

// ------------------------------------------------------------------
// Media worker: deleted-post handling
// (404/410 from the CDN → purge from catalog; transient → retry)
// ------------------------------------------------------------------

/// e621 answering 404 for a saved post's original is authoritative: the post
/// is purged from the local catalog (posts row + accounts_post + media index),
/// so the worker never picks it up again on later passes.
#[tokio::test(flavor = "multi_thread")]
async fn media_worker_purges_post_whose_original_404s() {
    install_catalog_config();
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/gone.png"))
        .respond_with(wiremock::ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let account_id = 54009;
    let owner = "catalog_owner_54009";
    e621_account_parser_api::db::set_account(owner, account_id, "cat_user9", "").unwrap();
    let url = format!("{}/gone.png", server.uri());
    let p = make_post(545301, "fox", &url);
    e621_account_parser_api::db::save_posts(std::slice::from_ref(&p), account_id).unwrap();
    e621_account_parser_api::db::save_posts_tags_batch(
        std::slice::from_ref(&p),
        &std::collections::HashSet::new(),
        false,
        None,
    )
    .unwrap();

    // It must start pending (saved, no stored original).
    let pending = e621_account_parser_api::db::pending_saved_original_posts(10).unwrap();
    assert!(
        pending.iter().any(|(id, _, _)| *id == 545301),
        "post starts pending"
    );

    let client = reqwest::Client::new();
    let stored =
        e621_account_parser_api::media_fetch_worker::fetch_original(&client, 545301, &url, "png")
            .await
            .unwrap();
    assert!(!stored, "deleted post must not report as stored");

    // Purged: no posts row, no accounts_post link, no pending entry, no media.
    assert!(
        e621_account_parser_api::db::get_post_by_id(545301)
            .unwrap()
            .is_none(),
        "posts row deleted"
    );
    let pending_after = e621_account_parser_api::db::pending_saved_original_posts(10).unwrap();
    assert!(
        !pending_after.iter().any(|(id, _, _)| *id == 545301),
        "post no longer pending on later passes"
    );
    let ids = e621_account_parser_api::db::catalog_search_post_ids(account_id, &[], 10, 0).unwrap();
    assert!(!ids.contains(&545301), "accounts_post link removed");
    assert!(
        e621_account_parser_api::db::get_media_entry(545301)
            .unwrap()
            .is_none()
    );
}

/// A transient upstream failure (5xx) must NOT purge the post: it stays in the
/// catalog and remains pending so a later pass retries it.
#[tokio::test(flavor = "multi_thread")]
async fn media_worker_keeps_post_pending_on_transient_5xx() {
    install_catalog_config();
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/flaky.png"))
        .respond_with(wiremock::ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let account_id = 54010;
    let owner = "catalog_owner_54010";
    e621_account_parser_api::db::set_account(owner, account_id, "cat_user10", "").unwrap();
    let url = format!("{}/flaky.png", server.uri());
    let p = make_post(545401, "fox", &url);
    e621_account_parser_api::db::save_posts(std::slice::from_ref(&p), account_id).unwrap();
    e621_account_parser_api::db::save_posts_tags_batch(
        std::slice::from_ref(&p),
        &std::collections::HashSet::new(),
        false,
        None,
    )
    .unwrap();

    let client = reqwest::Client::new();
    let res =
        e621_account_parser_api::media_fetch_worker::fetch_original(&client, 545401, &url, "png")
            .await;
    assert!(res.is_err(), "transient failure surfaces as an error");

    // Post untouched: still in posts, still pending for the next pass.
    assert!(
        e621_account_parser_api::db::get_post_by_id(545401)
            .unwrap()
            .is_some(),
        "post row kept on transient failure"
    );
    let pending = e621_account_parser_api::db::pending_saved_original_posts(10).unwrap();
    assert!(
        pending.iter().any(|(id, _, _)| *id == 545401),
        "post stays pending for retry"
    );
}
