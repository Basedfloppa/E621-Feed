//! Criterion benchmarks for the `db_hydrate` phase of the recommendations
//! pipeline, run against a real migrated SQLite database.
//!
//! Measures the three SQL-backed components of `db_hydrate` in
//! `build_recommendations_shared` (see TODO §2.2a):
//!   1. `hydrate_posts_by_ids`        — post + tag hydration
//!   2. `collect_local_candidate_ids` — the three candidate SQL streams
//!      (`local_candidates_for_top_tags` ×2 + `local_candidates_recent_popular`),
//!      which full-scan `posts` and `ORDER BY RANDOM()`.
//!   3. `load_account_tag_relation`   — per-account co-occurrence graph build.
//!
//! Baseline (before) numbers are captured in
//! `docs/optimization-hydrate-diversify-results.md`.
//!
//! Run with:
//!   cargo bench --bench hydrate

use std::sync::OnceLock;

use chrono::Utc;
use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
use e621_account_parser_api::db;
use e621_account_parser_api::models::{TagCount, cfg};

// ---------------------------------------------------------------------------
// One-time setup: isolated temp DB + migrations + realistic seeding
// ---------------------------------------------------------------------------

const N_POSTS: i64 = 100_000;
const N_GENERAL_TAGS: i64 = 4_900;
const N_ARTIST_TAGS: i64 = 50;
const N_CHAR_TAGS: i64 = 50;

struct Setup {
    /// 400 candidate post ids to hydrate (mirrors prod local_candidate_limit).
    hydrate_ids: Vec<i64>,
    /// top tag counts for the account (from the seeded account_tag_counts).
    tag_counts: Vec<TagCount>,
}

static SETUP: OnceLock<Setup> = OnceLock::new();

fn setup() -> &'static Setup {
    SETUP.get_or_init(|| {
        install_isolated_db_config();
        db::ensure_sqlite().expect("run migrations");
        seed();
        let hydrate_ids: Vec<i64> = (1..=400).collect();
        let tag_counts = db::get_tag_counts(1).expect("load tag counts");
        Setup {
            hydrate_ids,
            tag_counts,
        }
    })
}

fn install_isolated_db_config() {
    use std::io::Write;
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let example = std::fs::read_to_string(manifest_dir.join("config.example.toml"))
        .expect("read config.example.toml");
    let db_path =
        std::env::temp_dir().join(format!("e621-bench-hydrate-{}.db", std::process::id()));
    let db_path = db_path.to_string_lossy().replace('\\', "\\\\");
    let config = example.replacen(
        "db_path = \"database.db\"",
        &format!("db_path = \"{db_path}\""),
        1,
    );
    assert_ne!(config, example, "config.example.toml is missing db_path");
    let mut file = tempfile::NamedTempFile::new().expect("create temporary test config");
    file.write_all(config.as_bytes())
        .expect("write temporary test config");
    file.flush().expect("flush temporary test config");
    e621_account_parser_api::models::reload_from(file.path()).expect("load bench config");
    std::mem::forget(file); // keep the NamedTempFile alive so the path stays valid
}

fn seed() {
    let path = cfg().db_path.clone();
    let conn = rusqlite::Connection::open(&path).expect("open seed db");
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=OFF; PRAGMA temp_store=MEMORY;",
    )
    .expect("seed pragmas");
    let tx = conn.unchecked_transaction().expect("seed tx");

    {
        // posts — all recent (within 30 days), preview_url set, not deleted,
        // fav_count above the account's baseline threshold so EVERY post
        // qualifies for local_candidates_recent_popular (full scan + RANDOM sort).
        let mut ins = tx
            .prepare(
                "INSERT INTO posts
                 (id, created_at, updated_at, score_total, score_up, score_down,
                  fav_count, rating, file_ext, preview_url, sample_url, file_url,
                  last_seen_at, uploader_id)
                 VALUES (?1,?2,?2,?3,?4,?5,?6,'q','jpg',?7,?7,?8,?2,?9)",
            )
            .expect("prep posts");
        let now = Utc::now();
        for i in 1..=N_POSTS {
            let created = (now - chrono::Duration::days(i % 29))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
            let fav = (i % 100) + 20; // all ≥ 24 > baseline 30*0.6=18
            let preview = format!("https://static2.example/preview/{i}.jpg");
            let file = format!("https://static1.example/data/{i}.jpg");
            ins.execute(rusqlite::params![
                i,
                created,
                i % 500,
                i % 400,
                i % 40,
                fav,
                preview,
                file,
                i % 500
            ])
            .expect("insert post");
        }
    }

    {
        // tags: N_ARTIST + N_CHAR + N_GENERAL, ids 1..=N_TAGS via AUTOINCREMENT.
        let mut ins = tx
            .prepare("INSERT INTO tags (name, group_type) VALUES (?1,?2)")
            .expect("prep tags");
        for i in 0..N_ARTIST_TAGS {
            ins.execute(rusqlite::params![format!("artist_{i}"), "artist"])
                .expect("insert artist tag");
        }
        for i in 0..N_CHAR_TAGS {
            ins.execute(rusqlite::params![format!("char_{i}"), "character"])
                .expect("insert char tag");
        }
        for i in 0..N_GENERAL_TAGS {
            ins.execute(rusqlite::params![format!("general_{i}"), "general"])
                .expect("insert general tag");
        }
        drop(ins);
        // tag ids: artist 1..=50, char 51..=100, general 101..=5000
    }

    {
        // tags_posts: each post -> 1 artist + 2 char + 5 general tags.
        // artist id = i%50 + 1 ; char ids = (i%40)+51, ((i*7)%50)+51 ; general ids = 101+((i*13+k*7)%4900)
        let mut ins = tx
            .prepare("INSERT INTO tags_posts (tag_id, post_id) VALUES (?1,?2)")
            .expect("prep tags_posts");
        let artist_tag = |i: i64| (i % N_ARTIST_TAGS) + 1;
        let char_tag = |i: i64, k: i64| (i * (1 + k) % N_CHAR_TAGS) + 1 + N_ARTIST_TAGS;
        let general_tag =
            |i: i64, k: i64| (i * 13 + k * 7) % N_GENERAL_TAGS + 1 + N_ARTIST_TAGS + N_CHAR_TAGS;
        for i in 1..=N_POSTS {
            let mut tag_ids: Vec<i64> = vec![
                artist_tag(i),
                char_tag(i, 1),
                char_tag(i, 2),
                general_tag(i, 1),
                general_tag(i, 2),
                general_tag(i, 3),
                general_tag(i, 4),
                general_tag(i, 5),
            ];
            tag_ids.sort_unstable();
            tag_ids.dedup();
            for tag in tag_ids {
                ins.execute(rusqlite::params![tag, i])
                    .expect("insert tags_post");
            }
        }
        drop(ins);
    }

    {
        // account 1 + owned posts (first 2000) + quality profile + top tag counts.
        tx.execute("INSERT INTO accounts (id, name) VALUES (1, 'bench')", [])
            .expect("insert account");
        tx.execute_batch("INSERT INTO accounts_post (post_id, account_id) SELECT id, 1 FROM posts WHERE id <= 100000;")
            .expect("insert accounts_post");
        tx.execute(
            "INSERT INTO account_quality_profile (account_id, avg_score_total, avg_fav_count, avg_comment_count, avg_duration)
             VALUES (1, 250, 50, 5, 0)",
            [],
        )
        .expect("insert quality profile");
        let mut ins = tx
            .prepare("INSERT INTO account_tag_counts (account_id, tag_name, group_type, count) VALUES (1,?1,?2,?3)")
            .expect("prep atc");
        for i in 0..20 {
            ins.execute(rusqlite::params![
                format!("artist_{i}"),
                "artist",
                1000 - i * 10
            ])
            .expect("insert artist atc");
            ins.execute(rusqlite::params![
                format!("char_{i}"),
                "character",
                800 - i * 10
            ])
            .expect("insert char atc");
            ins.execute(rusqlite::params![
                format!("general_{i}"),
                "general",
                600 - i * 5
            ])
            .expect("insert general atc");
        }
        drop(ins);
    }

    {
        // feed_interactions: 5000 recent seen posts for account 1.
        let mut ins = tx
            .prepare(
                "INSERT INTO feed_interactions (account_id, post_id, event_type, position, session_id, created_at)
                 VALUES (1,?1,'qualified_impression',0,?2,?3)",
            )
            .expect("prep feed_interactions");
        let now = Utc::now();
        for i in 1..=200_000i64 {
            let created = (now - chrono::Duration::days(i % 10))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
            ins.execute(rusqlite::params![
                (i % 100_000) + 1,
                format!("s{i}"),
                created
            ])
            .expect("insert feed_interaction");
        }
        drop(ins);
    }

    {
        // account_tag_cooccurrence: full upper-triangle of 400-artist graph (~80k pairs).
        let mut ins = tx
            .prepare(
                "INSERT INTO account_tag_cooccurrence (account_id, tag1_name, tag1_group, tag2_name, tag2_group, cooc_count)
                 VALUES (1,?1,?2,?3,?4,?5)",
            )
            .expect("prep cooc");
        // Full upper-triangle of the 400-artist graph (~80k unique canonical
        // pairs) — a realistic heavy-account co-occurrence table.
        for a in 0..400 {
            for b in (a + 1)..400 {
                ins.execute(rusqlite::params![
                    format!("artist_{a}"),
                    "artist",
                    format!("artist_{b}"),
                    "artist",
                    10 + (a % 20)
                ])
                .expect("insert cooc");
            }
        }
        drop(ins);
    }

    tx.commit().expect("commit seed");
}

// ---------------------------------------------------------------------------
// Bench functions (each against the same seeded DB)
// ---------------------------------------------------------------------------

fn bench_hydrate_posts(c: &mut Criterion) {
    let ids = setup().hydrate_ids.clone();
    let mut group = c.benchmark_group("db_hydrate");
    group.sample_size(100);
    group.throughput(Throughput::Elements(ids.len() as u64));
    group.bench_function("hydrate_posts_by_ids (400 posts + tags)", |b| {
        b.iter(|| {
            let posts = black_box(db::hydrate_posts_by_ids(&ids).expect("hydrate"));
            black_box(posts.len());
        });
    });
    group.finish();
}

fn bench_collect_candidates(c: &mut Criterion) {
    setup();
    let mut group = c.benchmark_group("db_hydrate");
    group.sample_size(50);
    group.throughput(Throughput::Elements(400));
    group.bench_function("collect_local_candidate_ids (limit=400)", |b| {
        b.iter(|| {
            let ids =
                black_box(db::collect_local_candidate_ids(1, 400)).expect("collect candidates");
            black_box(ids.len());
        });
    });
    group.finish();
}

fn bench_load_user_relation(c: &mut Criterion) {
    let setup = setup();
    let tag_counts = setup.tag_counts.clone();
    let mut group = c.benchmark_group("db_hydrate");
    group.sample_size(50);
    group.bench_function("load_account_tag_relation (200k pairs)", |b| {
        b.iter(|| {
            let g =
                black_box(db::load_account_tag_relation(1, &tag_counts).expect("load relation"));
            black_box(g.n_posts());
        });
    });
    group.finish();
}

fn bench_seen_owned(c: &mut Criterion) {
    setup();
    let mut group = c.benchmark_group("db_hydrate");
    group.sample_size(100);
    group.bench_function("get_recently_seen_post_ids (200k interactions)", |b| {
        b.iter(|| {
            let s = black_box(db::get_recently_seen_post_ids(1, 14).expect("recent seen"));
            black_box(s.len());
        });
    });
    group.bench_function("get_owned_post_ids (100k owned)", |b| {
        b.iter(|| {
            let s = black_box(db::get_owned_post_ids(1).expect("owned"));
            black_box(s.len());
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_hydrate_posts,
    bench_collect_candidates,
    bench_load_user_relation,
    bench_seen_owned
);
criterion_main!(benches);
