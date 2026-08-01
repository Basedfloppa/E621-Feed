//! Criterion benchmarks for the scoring hot path (`ScoringContext::score`).
//!
//! Measures throughput of a single `score()` call and of batched
//! `score_cached()` calls against a realistic `ScoringContext` with
//! a full-sized IDF index, tag-relation graph, and synthetic posts
//! matching a typical account profile.
//!
//! Run with:
//!   cargo bench --bench scoring

use std::collections::HashMap;

use chrono::{TimeZone, Utc};
use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
use e621_account_parser_api::models::{
    AccountPreferenceProfile, AccountQualityProfile, AccountRatingStat, AccountRecencyProfile,
    Flags, Has, Post, PreferredTag, Rating, Relationships, ScoreBreakdown, Stats, TagCount, Tags,
};
use e621_account_parser_api::utils::{IdfIndex, Priors, ScoringContext, TagRelationGraph};

// ---------------------------------------------------------------------------
// Helpers — build a realistic-but-synthetic scenario
// ---------------------------------------------------------------------------

/// Build a `Priors` struct close to production defaults (see config.example.toml).
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
        diversity_semantic_blend: 0.05,
        diversity_pmi_threshold: 0.5,
        diversity_semantic_max_tags: 10,
        diversity_user_pmi_weight: 1.0,
        artist_discovery_n0: 3.0,
        artist_discovery_novelty_bonus: 0.2,
    }
}

/// Build an `IdfIndex` with ~5 000 tags (≈ typical catalog size).
fn realistic_idf() -> IdfIndex {
    let n_tags = 5_000;
    let n_docs = 200_000;
    let mut df = HashMap::with_capacity(n_tags);
    for i in 0..n_tags {
        let tag = format!("tag_{i}");
        // Zipf-like distribution: a few very common, most rare.
        let freq = (n_docs as f64 / (1.0 + i as f64 * 0.1)) as i64;
        df.insert(tag, freq);
    }
    IdfIndex::from_df(&df, n_docs)
}

/// Build a reasonably dense `TagRelationGraph`.
fn realistic_graph() -> TagRelationGraph {
    let mut g = TagRelationGraph::with_posts(200_000);
    // Distribute 5 000 tags across 7 group slots (0-6).
    let groups: [u8; 7] = [0, 1, 2, 3, 4, 5, 6];
    for i in 0..5_000_i64 {
        let tag = format!("tag_{i}");
        let (group, local_idx) = ((i % 7) as u8, (i / 7) as usize);
        let group_count = 200_000 / (1 + local_idx as i64);
        g.set_marginal(group, &tag, group_count);
    }
    // Connect neighbouring tags within the same group.
    for group in &groups {
        for local_i in 0..714_i64 {
            let i = local_i * 7 + *group as i64;
            for offset in 1..5 {
                let j = i + offset * 7;
                if j < 5_000 {
                    let pair_count = (100_000 / offset).max(1);
                    g.insert_pair(
                        *group,
                        &format!("tag_{i}"),
                        *group,
                        &format!("tag_{j}"),
                        pair_count,
                    );
                }
            }
        }
    }
    g
}

/// A minimal profile with some ratings and a bit of feedback.
fn realistic_profile() -> AccountPreferenceProfile {
    AccountPreferenceProfile {
        rating: vec![
            AccountRatingStat {
                rating: "s".to_string(),
                count: 60,
            },
            AccountRatingStat {
                rating: "q".to_string(),
                count: 30,
            },
            AccountRatingStat {
                rating: "e".to_string(),
                count: 10,
            },
        ],
        media: vec![],
        feedback: vec![],
        quality: AccountQualityProfile::default(),
        recency: AccountRecencyProfile::default(),
        uploaders: vec![],
        last_refreshed_at: None,
        preferred_tags: vec![
            PreferredTag {
                tag: "tag_1".into(),
                group: "general".into(),
                weight: 1.0,
            },
            PreferredTag {
                tag: "tag_10".into(),
                group: "general".into(),
                weight: 1.0,
            },
            PreferredTag {
                tag: "tag_100".into(),
                group: "general".into(),
                weight: 1.0,
            },
        ],
    }
}

/// Tag counts for an account that has interacted with ~200 tags.
fn realistic_tag_counts() -> Vec<TagCount> {
    let mut counts = Vec::with_capacity(200);
    for i in 0..200 {
        let tag = format!("tag_{}", i * 5);
        counts.push(TagCount {
            name: tag,
            group_type: "general".to_string(),
            count: (20 - (i as u32 / 10)).max(1) as i64,
        });
    }
    counts
}

/// Create a synthetic `Post` with a realistic tag shape (10-25 tags).
fn make_post(id: i64, base_artist: u32, age_days: f64) -> Post {
    let artist = format!("artist_{}", base_artist % 200);
    let mut general: Vec<String> = (0..12)
        .map(|j| {
            format!(
                "tag_{}",
                (base_artist as usize * 7 + j as usize * 13) % 5_000
            )
        })
        .collect();
    // Add some overlap with the profile's preferred tags.
    general.push("tag_1".to_string());
    general.push("tag_10".to_string());
    general.push("tag_100".to_string());
    let created = Utc
        .timestamp_opt(
            (Utc::now().timestamp() as f64 - age_days * 86_400.0) as i64,
            0,
        )
        .unwrap();

    Post {
        id,
        created_at: created,
        updated_at: created,
        change_seq: 1_000_000.0 + id as f64,
        files: e621_account_parser_api::models::Files::default(),
        uploader_id: (base_artist % 500) as i64,
        uploader_name: None,
        approver_id: None,
        stats: Stats {
            score: e621_account_parser_api::models::Score {
                up: (100 - base_artist as i64 % 50).max(1),
                down: (base_artist as i64 % 10),
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
        rating: match base_artist % 3 {
            0 => Rating::S,
            1 => Rating::Q,
            _ => Rating::E,
        },
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

// ---------------------------------------------------------------------------
// Benchmarks
// ---------------------------------------------------------------------------

fn bench_score_single(c: &mut Criterion) {
    let priors = realistic_priors();
    let idf = realistic_idf();
    let tag_counts = realistic_tag_counts();
    let profile = realistic_profile();
    let global_graph = realistic_graph();
    let user_graph = TagRelationGraph::with_posts(0); // empty user graph

    let ctx = ScoringContext::new_with_blacklist(
        &tag_counts,
        &priors,
        &idf,
        &profile,
        &global_graph,
        &user_graph,
        [].into(), // empty blacklist
    );

    let post = make_post(1_000_001, 42, 3.0);

    let mut group = c.benchmark_group("score_single");
    group.throughput(Throughput::Elements(1));
    group.bench_function("ScoringContext::score", |b| {
        b.iter(|| {
            let (_score, _breakdown): (f32, ScoreBreakdown) =
                black_box(&ctx).score(black_box(&post));
            black_box(_score);
        });
    });
    group.finish();
}

fn bench_score_batch(c: &mut Criterion) {
    let priors = realistic_priors();
    let idf = realistic_idf();
    let tag_counts = realistic_tag_counts();
    let profile = realistic_profile();
    let global_graph = realistic_graph();
    let user_graph = TagRelationGraph::with_posts(0);

    let ctx = ScoringContext::new_with_blacklist(
        &tag_counts,
        &priors,
        &idf,
        &profile,
        &global_graph,
        &user_graph,
        [].into(),
    );

    let n_posts = 200;
    let posts: Vec<Post> = (0..n_posts)
        .map(|i| {
            make_post(
                1_000_001 + i as i64,
                i as u32 * 3,
                (i as f64 * 0.5).max(0.5),
            )
        })
        .collect();

    let mut group = c.benchmark_group("score_batch");
    group.throughput(Throughput::Elements(n_posts as u64));
    group.bench_function("ScoringContext::score × 200", |b| {
        b.iter(|| {
            for post in &posts {
                let (_score, _breakdown) = black_box(&ctx).score(black_box(post));
                black_box(_score);
            }
        });
    });
    group.finish();
}

criterion_group!(benches, bench_score_single, bench_score_batch);
criterion_main!(benches);
