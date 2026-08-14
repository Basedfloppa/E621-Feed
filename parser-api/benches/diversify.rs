//! Criterion benchmarks for the MMR diversification hot path
//! (`diversify_scored_posts` → `diversify_indices`).
//!
//! This is the `diversify_post` phase of the recommendations pipeline
//! (see TODO §2.2a): prod trace shows it at ~8.5 s of an ~18 s request.
//! The dominant cost is the O(N² × window × max_tags²) PMI recomputation
//! when `diversity_semantic_blend > 0` (prod default 0.05).
//!
//! Run with:
//!   cargo bench --bench diversify
//!
//! Baseline (before) numbers are captured in
//! `docs/optimization-hydrate-diversify-results.md`.

use chrono::{TimeZone, Utc};
use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
use e621_account_parser_api::models::{
    Flags, Has, Post, Rating, Relationships, ScoreBreakdown, ScoredPost, Stats, Tags,
};
use e621_account_parser_api::utils::{Priors, TagRelationGraph, diversify_scored_posts};

// ---------------------------------------------------------------------------
// Scenario helpers (mirror benches/scoring.rs so both benches share shape)
// ---------------------------------------------------------------------------

fn realistic_priors() -> Priors {
    Priors {
        now: Utc::now(),
        recency_tau_days: 10.0,
        quality_a: 0.50,
        quality_b: 0.20,
        quality_log_bias: -3.0,
        mix_sim: 0.603,
        mix_quality: 0.017,
        mix_recency: 0.017,
        mix_rating: 0.034,
        mix_media: 0.042,
        mix_popularity: 0.017,
        mix_interaction: 0.084,
        mix_tag_relation: 0.067,
        mix_uploader: 0.05,
        mix_exclusivity: 0.02,
        mix_novelty: 0.02,
        mix_artist_discovery: 0.03,
        idf_lambda: 1.0,
        idf_alpha: 1.05,
        freq_alpha: 0.95,
        quality_w_absolute: 0.55,
        quality_w_relative_score: 0.30,
        quality_w_relative_comments: 0.15,
        quality_c: 0.3,
        popularity_w_fav: 0.80,
        popularity_w_duration: 0.20,
        recency_w_global: 0.40,
        recency_w_personal: 0.60,
        tag_relation_w_global: 0.4,
        tag_relation_w_personal: 0.6,
        tag_relation_pmi_scale: 3.5,
        tag_relation_min_cooc: 2,
        tag_relation_user_min_cooc: 1,
        tag_relation_cooc_ref: 16.0,
        tag_relation_user_cooc_ref: 5.0,
        tag_relation_max_tags: 20,
        tag_relation_pair_aggregator: "mean".to_string(),
        diversity_window: 32,
        diversity_w_artist: 0.22,
        diversity_w_character: 0.16,
        diversity_w_copyright: 1.8,
        diversity_w_species: 1.5,
        diversity_w_general: 0.08,
        discrete_smoothing_alpha: 1.0,
        strong_negative_count: 3,
        strong_negative_penalty: 0.40,
        strong_negative_wilson_threshold: 0.55,
        recency_personal_floor_frac: 1.0,
        recency_log_personal: true,
        feedback_decay_half_life_days: 90.0,
        meta_interaction_weight: 0.3,
        coldstart_n0: 25.0,
        discrete_pref_floor: 0.05,
        diversity_max_penalty: 0.45,
        diversity_interaction_damp: 0.35,
        df_floor: 0.40,
        idf_max: 100.0,
        bm25_k: 2.25,
        one_sided_ratio_exp: 0.5,
        coldstart_smoothing_boost: 2.0,
        interaction_ctr_prior_alpha: 4.0,
        idf_rsj_smoothing: 0.35,
        group_w_artist: 2.40,
        group_w_character: 2.00,
        group_w_copyright: 1.45,
        group_w_species: 1.30,
        group_w_general: 0.60,
        group_w_lore: 0.40,
        score_temperature: 0.0,
        confidence_steepness: 1.0,
        mmr_redundancy_exp: 1.0,
        tag_sim_jaccard_blend: 0.0,
        idf_lambda_meta: f32::NAN,
        tag_relation_pmi_scale_user: f32::NAN,
        recency_tau_recent: f32::NAN,
        recency_split_age_days: 30.0,
        recency_tau_hot: f32::NAN,
        recency_split_age_hours: 24.0,
        exploration_epsilon: 0.0,
        uploader_n0: 5.0,
        uploader_w_avg_score: 0.6,
        uploader_w_avg_fav: 0.4,
        min_exclusivity_cooc: 2,
        exclusivity_scale: 0.5,
        exclusivity_max_tags: 15,
        exclusivity_cross_group_weight: 0.5,
        novelty_n0: 3.0,
        novelty_use_feedback: true,
        diversity_semantic_blend: 0.05, // prod default (PMI soft-match active)
        diversity_pmi_threshold: 0.5,
        diversity_semantic_max_tags: 10,
        diversity_user_pmi_weight: 1.0,
        artist_discovery_n0: 3.0,
        artist_discovery_novelty_bonus: 0.2,
    }
}

/// Dense relation graph so PMI lookups return non-trivial co-occurrence.
fn realistic_graph() -> TagRelationGraph {
    let mut g = TagRelationGraph::with_posts(200_000);
    for i in 0..5_000_i64 {
        let tag = format!("tag_{i}");
        let group = (i % 7) as u8;
        let local_idx = (i / 7) as usize;
        let group_count = 200_000 / (1 + local_idx as i64);
        g.set_marginal(group, &tag, group_count);
    }
    for group in 0u8..7 {
        for local_i in 0..714_i64 {
            let i = local_i * 7 + group as i64;
            for offset in 1..5 {
                let j = i + offset * 7;
                if j < 5_000 {
                    let pair_count = (100_000 / offset).max(1);
                    g.insert_pair(
                        group,
                        &format!("tag_{i}"),
                        group,
                        &format!("tag_{j}"),
                        pair_count,
                    );
                }
            }
        }
    }
    g
}

/// Smaller personalized graph with the same id namespace so PMI resolves.
fn user_graph() -> TagRelationGraph {
    let mut g = TagRelationGraph::with_posts(20_000);
    for i in 0..5_000_i64 {
        let tag = format!("tag_{i}");
        let group = (i % 7) as u8;
        let group_count = 20_000 / (1 + (i / 7));
        g.set_marginal(group, &tag, group_count);
    }
    for group in 0u8..7 {
        for local_i in 0..714_i64 {
            let i = local_i * 7 + group as i64;
            for offset in 1..5 {
                let j = i + offset * 7;
                if j < 5_000 {
                    g.insert_pair(
                        group,
                        &format!("tag_{i}"),
                        group,
                        &format!("tag_{j}"),
                        (10_000 / offset).max(1),
                    );
                }
            }
        }
    }
    g
}

fn make_post(id: i64, base_artist: u32) -> Post {
    let artist = format!("artist_{}", base_artist % 200);
    let mut general: Vec<String> = (0..12)
        .map(|j| {
            format!(
                "tag_{}",
                (base_artist as usize * 7 + j as usize * 13) % 5_000
            )
        })
        .collect();
    general.push("tag_1".to_string());
    general.push("tag_10".to_string());
    let created = Utc
        .timestamp_opt(
            (Utc::now().timestamp() as f64 - 3600.0 * 24.0 * 5.0) as i64,
            0,
        )
        .unwrap();
    Post {
        id,
        created_at: created,
        updated_at: created,
        change_seq: 1_000_000.0 + id as f64,
        files: Default::default(),
        uploader_id: (base_artist % 500) as i64,
        uploader_name: None,
        approver_id: None,
        stats: Stats {
            score: e621_account_parser_api::models::Score {
                up: 100,
                down: 0,
                total: 100,
            },
            fav_count: 50,
            is_favorited: false,
            vote: 0,
            comment_count: 10,
        },
        flags: Flags::default(),
        has: Has::default(),
        relationships: Relationships::default(),
        pools: vec![],
        rating: Rating::Q,
        locked_tags: vec![],
        sources: vec![],
        description: None,
        tags: Tags {
            artist: vec![artist],
            character: vec![],
            copyright: vec![],
            species: vec![],
            general,
            invalid: vec![],
            meta: vec!["benchmark".to_string()],
            lore: vec![],
            contributor: vec![],
        },
    }
}

fn scored_post(id: i64) -> ScoredPost {
    ScoredPost {
        post: make_post(id, (id as u32) % 2000),
        score: ((id as f32) / 1_000_000.0).fract().abs(),
        breakdown: Some(ScoreBreakdown {
            tag_similarity: 0.5,
            quality_fit: 0.1,
            recency_fit: 0.1,
            rating_fit: 0.1,
            media_fit: 0.1,
            popularity_fit: 0.1,
            interaction_fit: 0.1,
            tag_relation_fit: 0.1,
            uploader_fit: 0.1,
            exclusivity_fit: 0.1,
            novelty_fit: 0.1,
            artist_discovery_fit: 0.1,
        }),
        reasons: Vec::new(),
    }
}

fn scored_pool(n: usize) -> Vec<ScoredPost> {
    (0..n).map(|i| scored_post(1_000_001 + i as i64)).collect()
}

// ---------------------------------------------------------------------------
// Bench functions
// ---------------------------------------------------------------------------

fn bench_diversify(c: &mut Criterion) {
    let priors = realistic_priors();
    let global_graph = realistic_graph();
    let user_graph = user_graph();

    let cases: [(usize, &str); 3] = [
        (100, "n=100"),
        (400, "n=400 (prod local_candidate_limit)"),
        (1000, "n=1000 (stress)"),
    ];

    let mut group = c.benchmark_group("diversify_post");
    group.sample_size(20); // each iteration is heavy (up to seconds at n=1000)
    for (n, label) in cases {
        let pool = scored_pool(n);
        group.throughput(Throughput::Elements(n as u64));
        group.bench_function(label, |b| {
            b.iter(|| {
                let out = black_box(diversify_scored_posts(
                    pool.clone(),
                    &global_graph,
                    Some(&user_graph),
                    &priors,
                ));
                black_box(out.len());
            });
        });
    }
    group.finish();
}

/// isolate the PMI overhead: same n=400 but with blend=0 (pure Jaccard).
fn bench_diversify_jaccard_only(c: &mut Criterion) {
    let mut priors = realistic_priors();
    priors.diversity_semantic_blend = 0.0; // fast path — no PMI
    let global_graph = realistic_graph();
    let user_graph = user_graph();
    let pool = scored_pool(400);

    let mut group = c.benchmark_group("diversify_post_jaccard_only");
    group.sample_size(30);
    group.throughput(Throughput::Elements(400));
    group.bench_function("n=400 blend=0", |b| {
        b.iter(|| {
            let out = black_box(diversify_scored_posts(
                pool.clone(),
                &global_graph,
                Some(&user_graph),
                &priors,
            ));
            black_box(out.len());
        });
    });
    group.finish();
}

criterion_group!(benches, bench_diversify, bench_diversify_jaccard_only);
criterion_main!(benches);
