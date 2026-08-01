//! Memory profiling for the main in-memory data structures.
//!
//! Measures RSS before/after building each structure at realistic production
//! sizes so we know where the 1.3–1.4 GB RSS comes from.
//!
//! Run with:
//!   cargo run --release --features jemalloc --example memory_profile
//!
//! Builds:
//!   1. IDF index with realistic tag count + Zipf distribution
//!   2. TagRelationGraph: Hot (HashMap) form vs Frozen (Vec) form
//!   3. ScoringContext + batch of posts, warm-channel scoring

use std::fs;

use chrono::{TimeZone, Utc};
use e621_account_parser_api::models::{
    AccountPreferenceProfile, AccountQualityProfile, AccountRecencyProfile, Flags, Has, Post,
    Rating, Relationships, Stats, Tags,
};
use e621_account_parser_api::utils::{IdfIndex, TagRelationGraph};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn rss_mb() -> f64 {
    if let Ok(statm) = fs::read_to_string("/proc/self/statm")
        && let Some(val) = statm.split_whitespace().nth(1)
        && let Ok(pages) = val.parse::<f64>()
    {
        return pages * 4096.0 / 1024.0 / 1024.0;
    }
    0.0
}

fn print_stage(label: &str, prev: &mut f64) {
    let rss = rss_mb();
    let delta = rss - *prev;
    println!("  {:>28}  {:>10.1}  {:>+10.1}", label, rss, delta);
    *prev = rss;
}

fn print_header(label: &str) {
    println!("\n═══ {label} ═══");
    println!("  {:>28}  {:>10}  {:>10}", "Stage", "RSS (MB)", "Δ RSS");
}

/// Build IDF with N tags, Zipf-like distribution, N documents.
fn build_idf(n_tags: usize, n_docs: i64) -> IdfIndex {
    let mut df = std::collections::HashMap::with_capacity((n_tags as f64 / 0.75).ceil() as usize);
    for i in 0..n_tags {
        let tag = format!("tag_{i}");
        let freq = (n_docs as f64 / (1.0 + i as f64 * 0.15)) as i64;
        df.insert(tag, freq);
    }
    IdfIndex::from_df(&df, n_docs)
}

/// Build tag-relation graph with n_tags tags and dense pair connections.
/// Returns (graph, pair_count) so the caller can print size info.
fn build_graph(n_tags: usize, pairs_per_tag: usize) -> (TagRelationGraph, usize) {
    let mut g = TagRelationGraph::with_posts(200_000);
    let mut pair_count = 0usize;

    for i in 0..n_tags as i64 {
        let tag = format!("tag_{i}");
        let group = (i % 7) as u8;
        let count = (200_000 / (1 + i / 7)).max(1);
        g.set_marginal(group, &tag, count);
    }

    for group in 0..7_u8 {
        for local_i in 0..(n_tags / 7) as i64 {
            let a = local_i * 7 + group as i64;
            for offset in 1..=pairs_per_tag as i64 {
                let b = a + offset * 7;
                if b < n_tags as i64 {
                    let count = (100_000 / offset).max(1);
                    g.insert_pair(
                        group,
                        &format!("tag_{a}"),
                        group,
                        &format!("tag_{b}"),
                        count,
                    );
                    pair_count += 1;
                }
            }
        }
    }
    (g, pair_count)
}

fn make_post(id: i64, n_tags: usize) -> Post {
    let general: Vec<String> = (0..20)
        .map(|j| format!("tag_{}", (id as usize * 7 + j * 13) % n_tags))
        .collect();
    let created = Utc
        .timestamp_opt(Utc::now().timestamp() - 86400 * 30, 0)
        .unwrap();
    Post {
        id,
        created_at: created,
        updated_at: created,
        change_seq: 1_000_000.0 + id as f64,
        files: e621_account_parser_api::models::Files::default(),
        uploader_id: id % 500,
        uploader_name: None,
        approver_id: None,
        stats: Stats {
            score: e621_account_parser_api::models::Score {
                up: 50,
                down: 5,
                total: 55,
            },
            fav_count: 20,
            is_favorited: false,
            vote: 0,
            comment_count: 5,
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
            artist: vec!["artist_a".into()],
            character: vec![],
            copyright: vec![],
            species: vec![],
            general,
            invalid: vec![],
            meta: vec![],
            lore: vec![],
            contributor: vec![],
        },
    }
}

fn main() {
    println!("Memory profile — E621 Account Parser (synthetic)");
    println!("  Page size: 4 KiB (assumed)");
    println!(
        "  Approximating prod: 200k tags / 2M docs / ~2M pairs (real DB: 198k tags, 307k posts, 2.7M cooc)\n"
    );

    let mut prev: f64;

    // ── 1. Baseline ────────────────────────────────────────────────────
    print_header("Baseline");
    prev = rss_mb();
    print_stage("idle", &mut prev);

    // ── 2. IDF index (realistic: 50k tags, 2M docs) ──────────────────
    print_header("IDF index (200k tags, 2M docs)");
    let idf = build_idf(200_000, 2_000_000);
    print_stage("after build", &mut prev);
    println!("  └─ n_tags: {}", idf.n_tags());

    // ── 3. Tag-relation graph, HOT form ────────────────────────────────
    print_header("TagRelationGraph — HOT (HashMap)");
    let (mut graph_hot, pair_count) = build_graph(200_000, 10);
    print_stage("after build (hot)", &mut prev);
    println!(
        "  └─ pairs: {pair_count} ({} MB @ ~40B/pair est.)",
        (pair_count * 40) / 1024 / 1024
    );

    // ── 4. Freeze — compacts to Vec<(u32,u32,u32)> ─────────────────────
    print_header("TagRelationGraph — FROZEN (Vec)");
    graph_hot.freeze(2);
    print_stage("after freeze", &mut prev);
    println!(
        "  └─ pairs: {pair_count} ({} MB @ 12B/pair est.)",
        (pair_count * 12) / 1024 / 1024
    );

    // ── 5. Scoring (10k posts) ────────────────────────────────────────
    print_header("Scoring (10k posts, frozen graph)");
    let priors = e621_account_parser_api::models::cfg().priors.clone();
    let profile = AccountPreferenceProfile {
        rating: vec![],
        media: vec![],
        feedback: vec![],
        quality: AccountQualityProfile::default(),
        recency: AccountRecencyProfile::default(),
        uploaders: vec![],
        last_refreshed_at: None,
        preferred_tags: vec![],
    };
    let empty_graph = TagRelationGraph::with_posts(0);
    let ctx = e621_account_parser_api::utils::ScoringContext::new(
        &[],
        &priors,
        &idf,
        &profile,
        &graph_hot,
        &empty_graph,
    );

    let posts: Vec<Post> = (0..10_000)
        .map(|i| make_post(100_000 + i, 200_000))
        .collect();
    print_stage("ctx + posts built", &mut prev);

    let mut sum = 0.0_f32;
    for p in &posts {
        let (s, _bd) = ctx.score(p);
        sum += s;
    }
    print_stage("after score (warm)", &mut prev);
    println!("  └─ score sum: {sum:.1}");

    // ── 6. After drop ──────────────────────────────────────────────────
    print_header("After drop");
    drop(ctx); // borrows idf/graph
    drop(posts);
    drop((idf, graph_hot, empty_graph));
    print_stage("dropped all", &mut prev);

    println!("\n─── Summary ───");
    println!("Peak RSS observed during the run: see per-stage numbers above.");
}
