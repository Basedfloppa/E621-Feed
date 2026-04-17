use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::collections::HashMap;

use crate::models::{AccountPreferenceProfile, Post, ScoreBreakdown, TagCount};
use crate::utils::idf::IdfIndex;

#[derive(Debug, Clone, Deserialize)]
pub struct Priors {
    pub now: DateTime<Utc>,
    pub recency_tau_days: f32,
    pub quality_a: f32,
    pub quality_b: f32,
    pub mix_sim: f32,
    pub mix_quality: f32,
    pub mix_recency: f32,
    pub mix_rating: Option<f32>,
    pub mix_media: Option<f32>,
    pub mix_popularity: Option<f32>,
    pub idf_lambda: Option<f32>,
    pub idf_alpha:  Option<f32>,
    pub freq_alpha: f32,
}

#[inline]
fn sigmoid(x: f32) -> f32 { 1.0 / (1.0 + (-x).exp()) }

#[inline]
fn gw<'a>(group_wts: &HashMap<&'a str, f32>, group: &'a str) -> f32 {
    *group_wts.get(group).unwrap_or(&1.0)
}

#[inline]
fn safe_ratio(value: f32, baseline: f32) -> f32 {
    if value <= 0.0 && baseline <= 0.0 {
        return 1.0;
    }

    let lhs = value.max(0.0).ln_1p();
    let rhs = baseline.max(0.0).ln_1p();
    (1.0 / (1.0 + (lhs - rhs).abs())).clamp(0.0, 1.0)
}

fn discrete_preference<'a>(items: impl Iterator<Item = (&'a str, i64)>, key: &str) -> f32 {
    let counts: Vec<(&str, i64)> = items.collect();
    let total: i64 = counts.iter().map(|(_, count)| *count).sum();
    if total <= 0 {
        return 0.5;
    }

    let matched = counts
        .iter()
        .find(|(candidate, _)| *candidate == key)
        .map(|(_, count)| *count)
        .unwrap_or_default();

    ((matched as f32 / total as f32).sqrt()).clamp(0.05, 1.0)
}

fn recency_fit(age_days: f32, priors: &Priors, profile: &AccountPreferenceProfile) -> f32 {
    let global = (-age_days / priors.recency_tau_days.max(1e-3)).exp().clamp(0.0, 1.0);

    if profile.recency.avg_age_days <= 0.0 {
        return global;
    }

    let spread = profile
        .recency
        .avg_abs_dev_days
        .max(priors.recency_tau_days * 0.25)
        .max(1.0);
    let personal = (-(age_days - profile.recency.avg_age_days).abs() / spread)
        .exp()
        .clamp(0.0, 1.0);

    (0.4 * global + 0.6 * personal).clamp(0.0, 1.0)
}

fn quality_fit(origin_post: &Post, priors: &Priors, profile: &AccountPreferenceProfile) -> f32 {
    let absolute = sigmoid(
        priors.quality_a * origin_post.score.total as f32
            + priors.quality_b * origin_post.fav_count as f32,
    );

    let relative_score = safe_ratio(
        origin_post.score.total as f32,
        profile.quality.avg_score_total,
    );
    let relative_comments = safe_ratio(
        origin_post.comment_count as f32,
        profile.quality.avg_comment_count,
    );

    (0.55 * absolute + 0.30 * relative_score + 0.15 * relative_comments).clamp(0.0, 1.0)
}

fn popularity_fit(origin_post: &Post, profile: &AccountPreferenceProfile) -> f32 {
    let fav_fit = safe_ratio(origin_post.fav_count as f32, profile.quality.avg_fav_count);
    let duration_fit = if origin_post.duration.unwrap_or(0.0) > 0.0 || profile.quality.avg_duration > 0.0
    {
        safe_ratio(
            origin_post.duration.unwrap_or(0.0) as f32,
            profile.quality.avg_duration,
        )
    } else {
        1.0
    };

    (0.8 * fav_fit + 0.2 * duration_fit).clamp(0.0, 1.0)
}

fn rating_fit(origin_post: &Post, profile: &AccountPreferenceProfile) -> f32 {
    let rating = origin_post.rating.to_string();
    discrete_preference(
        profile.rating.iter().map(|stat| (stat.rating.as_str(), stat.count)),
        &rating,
    )
}

fn media_fit(origin_post: &Post, profile: &AccountPreferenceProfile) -> f32 {
    let media = origin_post.media_type();
    discrete_preference(
        profile
            .media
            .iter()
            .map(|stat| (stat.media_type.as_str(), stat.count)),
        media,
    )
}

pub fn score_post(
    account_tag_counts: &[TagCount],
    origin_post: &Post,
    group_wts: &HashMap<String, f32>,
    priors: &Priors,
    idf: &IdfIndex,
    profile: &AccountPreferenceProfile,
) -> (f32, ScoreBreakdown) {
    let group_wts_hash: HashMap<&str, f32> =
        group_wts.iter().map(|(k, v)| (k.as_str(), *v)).collect();

    let lambda = priors.idf_lambda.unwrap_or(0.4);
    let alpha  = priors.idf_alpha.unwrap_or(0.5);

    let mut user: HashMap<String, f32> = HashMap::default();
    let mut u_norm_sq = 0.0f32;

    for t in account_tag_counts {
        if t.count <= 0 { continue; }
        let g = *group_wts_hash.get(t.group_type.as_str()).unwrap_or(&1.0);
        let tlc = t.name.to_lowercase();
        let idf_w = idf.idf_tempered(&tlc, lambda, alpha);
        let w = (t.count as f32).powf(priors.freq_alpha) * g * idf_w;
        if w > 0.0 {
            let key = format!("{}|{}", t.group_type, tlc);
            let e = user.entry(key).or_insert(0.0);
            *e += w;
        }
    }
    for &uw in user.values() { u_norm_sq += uw * uw; }

    let mut dot = 0.0f32;
    let mut p_norm_sq = 0.0f32;

    let mut acc = |tags: &Vec<String>, group: &'static str| {
        let g = gw(&group_wts_hash, group);
        for t in tags {
            if t.is_empty() { continue; }
            let tlc = t.to_lowercase();
            let idf_w = idf.idf_tempered(&tlc, lambda, alpha);
            let pw = g * idf_w;
            p_norm_sq += pw * pw;

            let key = {
                let mut s = String::with_capacity(group.len() + 1 + tlc.len());
                s.push_str(group);
                s.push('|');
                s.push_str(&tlc);
                s
            };
            if let Some(&uw) = user.get(&key) {
                dot += uw * pw;
            }
        }
    };

    acc(&origin_post.tags.artist,    "artist");
    acc(&origin_post.tags.character, "character");
    acc(&origin_post.tags.copyright, "copyright");
    acc(&origin_post.tags.general,   "general");
    acc(&origin_post.tags.lore,      "lore");
    acc(&origin_post.tags.meta,      "meta");
    acc(&origin_post.tags.species,   "species");

    let sim = if u_norm_sq == 0.0 || p_norm_sq == 0.0 {
        0.0
    } else {
        dot / (u_norm_sq.sqrt() * p_norm_sq.sqrt())
    };

    let age_days = (priors.now - origin_post.created_at).num_seconds() as f32 / 86_400.0;
    let quality = quality_fit(origin_post, priors, profile);
    let popularity = popularity_fit(origin_post, profile);
    let rating = rating_fit(origin_post, profile);
    let media = media_fit(origin_post, profile);
    let recency = recency_fit(age_days, priors, profile);

    let mix_rating = priors.mix_rating.unwrap_or(0.12);
    let mix_media = priors.mix_media.unwrap_or(0.08);
    let mix_popularity = priors.mix_popularity.unwrap_or(0.15);
    let sum = priors.mix_sim
        + priors.mix_quality
        + priors.mix_recency
        + mix_rating
        + mix_media
        + mix_popularity;
    let (ms, mq, mr, mrat, mmed, mpop) = if sum > 0.0 {
        (
            priors.mix_sim / sum,
            priors.mix_quality / sum,
            priors.mix_recency / sum,
            mix_rating / sum,
            mix_media / sum,
            mix_popularity / sum,
        )
    } else {
        (0.0, 0.0, 0.0, 0.0, 0.0, 0.0)
    };

    let breakdown = ScoreBreakdown {
        tag_similarity: sim,
        quality_fit: quality,
        recency_fit: recency,
        rating_fit: rating,
        media_fit: media,
        popularity_fit: popularity,
    };

    let score = (
        ms * breakdown.tag_similarity
            + mq * breakdown.quality_fit
            + mr * breakdown.recency_fit
            + mrat * breakdown.rating_fit
            + mmed * breakdown.media_fit
            + mpop * breakdown.popularity_fit
    )
    .clamp(0.0, 1.0);

    (score, breakdown)
}
