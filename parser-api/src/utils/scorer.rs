use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use crate::models::{AccountPreferenceProfile, Post, ScoreBreakdown, ScoredPost, TagCount};
use crate::utils::idf::IdfIndex;
use crate::utils::tag_relation::{TagId, TagRelationGraph};

const GROUP_COUNT: usize = 7;
const DIVERSITY_INTERACTION_DAMP: f32 = 0.35;
const DIVERSITY_MAX_PENALTY: f32 = 0.45;
const DISCRETE_PREF_FLOOR: f32 = 0.05;
const FEEDBACK_NEUTRAL: f32 = 0.5;
/// At n_favorites = COLDSTART_N0 the personal signal hits 50% confidence.
const COLDSTART_N0: f32 = 50.0;
/// 95% one-sided Wilson lower-bound z-score.
const WILSON_Z: f32 = 1.96;
const BM25_K: f32 = 1.6;

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum Group {
    Artist = 0,
    Character = 1,
    Copyright = 2,
    Species = 3,
    General = 4,
    Lore = 5,
    Meta = 6,
}

/// Single source of truth for the group ↔ string mapping.
const GROUP_NAMES: [(Group, &str); GROUP_COUNT] = [
    (Group::Artist, "artist"),
    (Group::Character, "character"),
    (Group::Copyright, "copyright"),
    (Group::Species, "species"),
    (Group::General, "general"),
    (Group::Lore, "lore"),
    (Group::Meta, "meta"),
];

impl Group {
    pub const fn name(self) -> &'static str {
        GROUP_NAMES[self as usize].1
    }

    pub fn from_str(s: &str) -> Option<Self> {
        GROUP_NAMES.iter().find(|(_, n)| *n == s).map(|(g, _)| *g)
    }

    const ALL: [Group; GROUP_COUNT] = [
        Group::Artist,
        Group::Character,
        Group::Copyright,
        Group::Species,
        Group::General,
        Group::Lore,
        Group::Meta,
    ];

    pub const fn is_scoring(self) -> bool {
        !matches!(self, Group::Meta)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Priors {
    pub now: DateTime<Utc>,
    pub recency_tau_days: f32,
    pub quality_a: f32,
    pub quality_b: f32,
    pub mix_sim: f32,
    pub mix_quality: f32,
    pub mix_recency: f32,
    pub mix_rating: f32,
    pub mix_media: f32,
    pub mix_popularity: f32,
    pub mix_interaction: f32,
    #[serde(default = "default_mix_tag_relation")]
    pub mix_tag_relation: f32,
    pub idf_lambda: f32,
    pub idf_alpha: f32,
    pub freq_alpha: f32,

    pub quality_w_absolute: f32,
    pub quality_w_relative_score: f32,
    pub quality_w_relative_comments: f32,

    pub popularity_w_fav: f32,
    pub popularity_w_duration: f32,

    pub recency_w_global: f32,
    pub recency_w_personal: f32,

    pub diversity_window: usize,
    pub diversity_w_artist: f32,
    pub diversity_w_character: f32,
    pub diversity_w_general: f32,

    #[serde(default = "default_quality_log_bias")]
    pub quality_log_bias: f32,
    #[serde(default = "default_discrete_smoothing_alpha")]
    pub discrete_smoothing_alpha: f32,
    #[serde(default = "default_strong_negative_count")]
    pub strong_negative_count: i64,
    #[serde(default = "default_strong_negative_penalty")]
    pub strong_negative_penalty: f32,
    #[serde(default = "default_recency_personal_floor_frac")]
    pub recency_personal_floor_frac: f32,

    #[serde(default = "default_tag_relation_w_global")]
    pub tag_relation_w_global: f32,
    #[serde(default = "default_tag_relation_w_personal")]
    pub tag_relation_w_personal: f32,
    #[serde(default = "default_tag_relation_pmi_scale")]
    pub tag_relation_pmi_scale: f32,
    #[serde(default = "default_tag_relation_min_cooc")]
    pub tag_relation_min_cooc: i64,
    #[serde(default = "default_tag_relation_user_min_cooc")]
    pub tag_relation_user_min_cooc: i64,
    #[serde(default = "default_tag_relation_cooc_ref")]
    pub tag_relation_cooc_ref: f32,
    #[serde(default = "default_tag_relation_user_cooc_ref")]
    pub tag_relation_user_cooc_ref: f32,
    #[serde(default = "default_strong_negative_wilson_threshold")]
    pub strong_negative_wilson_threshold: f32,
    #[serde(default = "default_recency_log_personal")]
    pub recency_log_personal: bool,
    #[serde(default = "default_feedback_decay_half_life_days")]
    pub feedback_decay_half_life_days: f32,
    /// Meta is excluded from tag_similarity/tag_relation (would swamp content
    /// tags); this weight applies in the interaction channel only.
    #[serde(default = "default_meta_interaction_weight")]
    pub meta_interaction_weight: f32,
}

fn default_quality_log_bias() -> f32 { -3.0 }
fn default_discrete_smoothing_alpha() -> f32 { 1.0 }
fn default_strong_negative_count() -> i64 { 3 }
fn default_strong_negative_penalty() -> f32 { 0.40 }
fn default_recency_personal_floor_frac() -> f32 { 1.0 }
fn default_mix_tag_relation() -> f32 { 0.0 }
fn default_tag_relation_w_global() -> f32 { 0.4 }
fn default_tag_relation_w_personal() -> f32 { 0.6 }
fn default_tag_relation_pmi_scale() -> f32 { 5.0 }
fn default_tag_relation_min_cooc() -> i64 { 2 }
fn default_tag_relation_user_min_cooc() -> i64 { 1 }
fn default_tag_relation_cooc_ref() -> f32 { 20.0 }
fn default_tag_relation_user_cooc_ref() -> f32 { 5.0 }
fn default_strong_negative_wilson_threshold() -> f32 { 0.55 }
fn default_recency_log_personal() -> bool { true }
fn default_feedback_decay_half_life_days() -> f32 { 90.0 }
fn default_meta_interaction_weight() -> f32 { 0.3 }

#[inline]
fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

#[inline]
fn normalize_tag(t: &str) -> Cow<'_, str> {
    let trimmed = t.trim();
    if trimmed.bytes().any(|b| b.is_ascii_uppercase()) {
        Cow::Owned(trimmed.to_ascii_lowercase())
    } else {
        Cow::Borrowed(trimmed)
    }
}

#[inline]
fn one_sided_ratio(value: f32, baseline: f32) -> f32 {
    if baseline <= 1e-6 {
        return if value > 0.0 { 1.0 } else { FEEDBACK_NEUTRAL };
    }
    let r = value.max(0.0) / baseline;
    r.min(1.0).sqrt().clamp(0.0, 1.0)
}

#[inline]
fn discrete_preference_smooth(total: i64, matched: i64, k: usize, alpha: f32) -> f32 {
    if total <= 0 {
        return FEEDBACK_NEUTRAL;
    }
    let k = k.max(1) as f32;
    let a = alpha.max(0.0);
    let num = matched.max(0) as f32 + a;
    let den = total as f32 + a * k;
    (num / den).sqrt().clamp(DISCRETE_PREF_FLOOR, 1.0)
}

#[inline]
fn blend2(a: f32, wa: f32, b: f32, wb: f32) -> f32 {
    let sum = wa + wb;
    if sum <= 0.0 {
        return 0.0;
    }
    ((wa * a + wb * b) / sum).clamp(0.0, 1.0)
}

#[inline]
fn blend3(a: f32, wa: f32, b: f32, wb: f32, c: f32, wc: f32) -> f32 {
    let sum = wa + wb + wc;
    if sum <= 0.0 {
        return 0.0;
    }
    ((wa * a + wb * b + wc * c) / sum).clamp(0.0, 1.0)
}

/// `n / (n + n0)` — 0 at no data, asymptotic to 1.
#[inline]
fn confidence(n: f32, n0: f32) -> f32 {
    let nn = n.max(0.0);
    let nn0 = n0.max(1e-3);
    nn / (nn + nn0)
}

/// Wilson lower bound on a binomial proportion (used for negative-feedback veto).
#[inline]
fn wilson_lower_bound(positives: f32, total: f32, z: f32) -> f32 {
    if total <= 0.0 {
        return 0.0;
    }
    let p_hat = positives / total;
    let z2 = z * z;
    let denom = 1.0 + z2 / total;
    let centre = p_hat + z2 / (2.0 * total);
    let margin = z * ((p_hat * (1.0 - p_hat) + z2 / (4.0 * total)) / total).sqrt();
    ((centre - margin) / denom).max(0.0)
}

/// CTR with Bayesian prior toward `p0` so low-interaction tags decay to the
/// user's own base rate, not neutral 0.5.
#[inline]
fn ctr_score(pos: f32, neg: f32, imp: f32, p0: f32, alpha: f32) -> f32 {
    let strong = pos + neg;
    let exposure = (imp - pos).max(0.0);
    let denom = strong + exposure + alpha;
    if denom <= 0.0 {
        return p0.clamp(0.0, 1.0);
    }
    ((pos + alpha * p0) / denom).clamp(0.0, 1.0)
}

#[derive(Default, Clone, Copy)]
struct CompactFeedback {
    positive: i64,
    negative: i64,
    impressions: i64,
}

#[derive(Default, Clone, Copy)]
struct MixWeights {
    sim: f32,
    quality: f32,
    recency: f32,
    rating: f32,
    media: f32,
    popularity: f32,
    interaction: f32,
    tag_relation: f32,
}

impl MixWeights {
    fn from_priors(p: &Priors) -> Self {
        let sum = p.mix_sim
            + p.mix_quality
            + p.mix_recency
            + p.mix_rating
            + p.mix_media
            + p.mix_popularity
            + p.mix_interaction
            + p.mix_tag_relation.max(0.0);
        if sum <= 0.0 {
            Self::default()
        } else {
            Self {
                sim: p.mix_sim / sum,
                quality: p.mix_quality / sum,
                recency: p.mix_recency / sum,
                rating: p.mix_rating / sum,
                media: p.mix_media / sum,
                popularity: p.mix_popularity / sum,
                interaction: p.mix_interaction / sum,
                tag_relation: p.mix_tag_relation.max(0.0) / sum,
            }
        }
    }
}

pub struct ScoringContext<'a> {
    priors: &'a Priors,
    profile: &'a AccountPreferenceProfile,
    idf: &'a IdfIndex,
    global_relation: &'a TagRelationGraph,
    user_relation: &'a TagRelationGraph,
    group_wts: [f32; GROUP_COUNT],
    user: [HashMap<String, f32>; GROUP_COUNT],
    u_norm: f32,
    feedback: [HashMap<String, CompactFeedback>; GROUP_COUNT],
    rating_total: i64,
    media_total: i64,
    /// 0 = no personal data → fall back to global signals.
    personal_confidence: f32,
    /// User's overall positive rate; Bayesian prior for per-tag CTR.
    user_base_positive_rate: f32,
    mix: MixWeights,
}

impl<'a> ScoringContext<'a> {
    pub fn new(
        account_tag_counts: &[TagCount],
        group_weights: &HashMap<String, f32>,
        priors: &'a Priors,
        idf: &'a IdfIndex,
        profile: &'a AccountPreferenceProfile,
        global_relation: &'a TagRelationGraph,
        user_relation: &'a TagRelationGraph,
    ) -> Self {
        let mut group_wts = [1.0f32; GROUP_COUNT];
        for &g in &Group::ALL {
            if let Some(&w) = group_weights.get(g.name()) {
                group_wts[g as usize] = w;
            }
        }
        for &g in &Group::ALL {
            if !g.is_scoring() {
                group_wts[g as usize] = 0.0;
            }
        }

        let mut user: [HashMap<String, f32>; GROUP_COUNT] = Default::default();
        let mut u_norm_sq = 0.0f32;
        let lambda = priors.idf_lambda;
        let alpha = priors.idf_alpha;

        for t in account_tag_counts {
            if t.count <= 0 {
                continue;
            }
            let Some(group) = Group::from_str(t.group_type.as_str()) else {
                continue;
            };
            let g = group_wts[group as usize];
            if g <= 0.0 {
                continue;
            }
            let tlc = normalize_tag(&t.name);
            let idf_w = idf.idf_tempered(&tlc, lambda, alpha);
            // BM25 saturation: a tag favoured 200× is not 200× the influence of one favoured 50×.
            let tf = (t.count as f32).max(0.0);
            let saturated = (tf * (BM25_K + 1.0)) / (tf + BM25_K);
            let w = saturated.powf(priors.freq_alpha) * g * idf_w;
            if w > 0.0 {
                *user[group as usize]
                    .entry(tlc.into_owned())
                    .or_insert(0.0) += w;
            }
        }
        for map in &user {
            for &w in map.values() {
                u_norm_sq += w * w;
            }
        }

        let mut total_pos = 0.0f32;
        let mut total_neg = 0.0f32;
        let mut feedback: [HashMap<String, CompactFeedback>; GROUP_COUNT] = Default::default();
        for fb in &profile.feedback {
            let Some(group) = Group::from_str(fb.group_type.as_str()) else {
                continue;
            };
            total_pos += fb.positive_count.max(0) as f32;
            total_neg += fb.negative_count.max(0) as f32;
            feedback[group as usize].insert(
                normalize_tag(&fb.tag_name).into_owned(),
                CompactFeedback {
                    positive: fb.positive_count,
                    negative: fb.negative_count,
                    impressions: fb.impression_count,
                },
            );
        }

        let rating_total: i64 = profile.rating.iter().map(|r| r.count.max(0)).sum();
        let media_total: i64 = profile.media.iter().map(|m| m.count.max(0)).sum();

        // n_favorites ≈ rating_total; media_total is a fallback if rating missing.
        let n_favorites = rating_total.max(media_total).max(0) as f32;
        let personal_confidence = confidence(n_favorites, COLDSTART_N0);

        let strong_total = total_pos + total_neg;
        let user_base_positive_rate = if strong_total > 0.0 {
            (total_pos / strong_total).clamp(0.05, 0.95)
        } else {
            0.5
        };

        Self {
            priors,
            profile,
            idf,
            global_relation,
            user_relation,
            group_wts,
            user,
            u_norm: u_norm_sq.sqrt(),
            feedback,
            rating_total,
            media_total,
            personal_confidence,
            user_base_positive_rate,
            mix: MixWeights::from_priors(priors),
        }
    }

    pub fn score(&self, post: &Post) -> (f32, ScoreBreakdown) {
        let sim = self.tag_similarity(post);
        let age_days = (self.priors.now - post.created_at).num_seconds() as f32 / 86_400.0;
        let quality = self.quality_fit(post);
        let popularity = self.popularity_fit(post);
        let rating = self.rating_fit(post);
        let media = self.media_fit(post);
        let (interaction, veto) = self.interaction_fit(post);
        let recency = self.recency_fit(age_days);
        let tag_relation = self.tag_relation_fit(post);

        let mix = self.mix;
        let raw = mix.sim * sim
            + mix.quality * quality
            + mix.recency * recency
            + mix.rating * rating
            + mix.media * media
            + mix.popularity * popularity
            + mix.interaction * interaction
            + mix.tag_relation * tag_relation;
        let mut score = raw.clamp(0.0, 1.0);
        if veto {
            score *= 1.0 - self.priors.strong_negative_penalty.clamp(0.0, 1.0);
        }

        let breakdown = ScoreBreakdown {
            tag_similarity: sim,
            quality_fit: quality,
            recency_fit: recency,
            rating_fit: rating,
            media_fit: media,
            popularity_fit: popularity,
            interaction_fit: interaction,
            tag_relation_fit: tag_relation,
        };

        (score.clamp(0.0, 1.0), breakdown)
    }

    fn tag_similarity(&self, post: &Post) -> f32 {
        let mut dot = 0.0f32;
        let mut p_norm_sq = 0.0f32;
        let lambda = self.priors.idf_lambda;
        let alpha = self.priors.idf_alpha;

        for (group, tags) in [
            (Group::Artist, &post.tags.artist),
            (Group::Character, &post.tags.character),
            (Group::Copyright, &post.tags.copyright),
            (Group::General, &post.tags.general),
            (Group::Lore, &post.tags.lore),
            (Group::Meta, &post.tags.meta),
            (Group::Species, &post.tags.species),
        ] {
            let g = self.group_wts[group as usize];
            if g <= 0.0 {
                continue;
            }
            let user_map = &self.user[group as usize];
            for t in tags {
                if t.is_empty() {
                    continue;
                }
                let tlc = normalize_tag(t);
                let idf_w = self.idf.idf_tempered(&tlc, lambda, alpha);
                let pw = g * idf_w;
                p_norm_sq += pw * pw;
                if let Some(&uw) = user_map.get(tlc.as_ref()) {
                    dot += uw * pw;
                }
            }
        }

        if self.u_norm <= 0.0 || p_norm_sq <= 0.0 {
            0.0
        } else {
            (dot / (self.u_norm * p_norm_sq.sqrt())).clamp(0.0, 1.0)
        }
    }

    fn quality_fit(&self, post: &Post) -> f32 {
        let p = self.priors;
        let absolute = sigmoid(
            p.quality_a * (post.score.total.max(0) as f32).ln_1p()
                + p.quality_b * (post.fav_count.max(0) as f32).ln_1p()
                + p.quality_log_bias,
        );
        let rel_score = one_sided_ratio(
            post.score.total.max(0) as f32,
            self.profile.quality.avg_score_total,
        );
        let rel_comments = one_sided_ratio(
            post.comment_count.max(0) as f32,
            self.profile.quality.avg_comment_count,
        );
        blend3(
            absolute,
            p.quality_w_absolute,
            rel_score,
            p.quality_w_relative_score,
            rel_comments,
            p.quality_w_relative_comments,
        )
    }

    fn popularity_fit(&self, post: &Post) -> f32 {
        let p = self.priors;
        let fav_fit = one_sided_ratio(
            post.fav_count.max(0) as f32,
            self.profile.quality.avg_fav_count,
        );
        let dur_val = post.duration.unwrap_or(0.0) as f32;
        let dur_base = self.profile.quality.avg_duration;
        let duration_fit = if dur_val > 0.0 || dur_base > 0.0 {
            one_sided_ratio(dur_val, dur_base)
        } else {
            1.0
        };
        blend2(
            fav_fit,
            p.popularity_w_fav,
            duration_fit,
            p.popularity_w_duration,
        )
    }

    fn rating_fit(&self, post: &Post) -> f32 {
        let rating = post.rating.to_string();
        let matched = self
            .profile
            .rating
            .iter()
            .find(|s| s.rating == rating)
            .map(|s| s.count.max(0))
            .unwrap_or(0);
        let k = self.profile.rating.len().max(3);
        // Expand smoothing on small profiles so rare ratings aren't nuked by tiny samples.
        let alpha = self.priors.discrete_smoothing_alpha * (1.0 + (1.0 - self.personal_confidence) * 2.0);
        discrete_preference_smooth(self.rating_total, matched, k, alpha)
    }

    fn media_fit(&self, post: &Post) -> f32 {
        let media = post.media_type();
        let matched = self
            .profile
            .media
            .iter()
            .find(|s| s.media_type == media)
            .map(|s| s.count.max(0))
            .unwrap_or(0);
        let k = self.profile.media.len().max(3);
        let alpha = self.priors.discrete_smoothing_alpha * (1.0 + (1.0 - self.personal_confidence) * 2.0);
        discrete_preference_smooth(self.media_total, matched, k, alpha)
    }

    fn interaction_fit(&self, post: &Post) -> (f32, bool) {
        let mut total_weight = 0.0f32;
        let mut weighted = 0.0f32;
        let mut strong_neg = false;

        let strong_min = self.priors.strong_negative_count.max(1) as f32;
        let wilson_threshold = self.priors.strong_negative_wilson_threshold.clamp(0.05, 0.99);
        let p0 = self.user_base_positive_rate;
        let meta_w = self.priors.meta_interaction_weight.max(0.0);

        // Meta is excluded from tag_similarity / tag_relation but contributes
        // here under its own weight (preferences for monochrome, absurd_res, …).
        let groups: [(Group, &Vec<String>, f32); 7] = [
            (Group::Artist, &post.tags.artist, self.group_wts[Group::Artist as usize]),
            (Group::Character, &post.tags.character, self.group_wts[Group::Character as usize]),
            (Group::Copyright, &post.tags.copyright, self.group_wts[Group::Copyright as usize]),
            (Group::Species, &post.tags.species, self.group_wts[Group::Species as usize]),
            (Group::General, &post.tags.general, self.group_wts[Group::General as usize]),
            (Group::Lore, &post.tags.lore, self.group_wts[Group::Lore as usize]),
            (Group::Meta, &post.tags.meta, meta_w),
        ];

        for (group, tags, group_weight) in groups {
            if group_weight <= 0.0 {
                continue;
            }
            let group_feedback = &self.feedback[group as usize];
            for tag in tags {
                if tag.is_empty() {
                    continue;
                }
                let tlc = normalize_tag(tag);
                if let Some(fb) = group_feedback.get(tlc.as_ref()) {
                    let pos = fb.positive.max(0) as f32;
                    let neg = fb.negative.max(0) as f32;
                    let imp = fb.impressions.max(0) as f32;
                    // log1p of sample size: confident tags dominate, no signal contributes 0.
                    let confidence = (pos + neg + imp).ln_1p();
                    if confidence <= 0.0 {
                        continue;
                    }
                    let w = group_weight * confidence;
                    total_weight += w;
                    weighted += w * ctr_score(pos, neg, imp, p0, 4.0);

                    // Veto only when both volume (>= strong_min) and statistical confidence agree.
                    if neg >= strong_min {
                        let neg_lcb = wilson_lower_bound(neg, pos + neg, WILSON_Z);
                        if neg_lcb >= wilson_threshold {
                            strong_neg = true;
                        }
                    }
                }
            }
        }

        let score = if total_weight <= 0.0 {
            FEEDBACK_NEUTRAL
        } else {
            (weighted / total_weight).clamp(0.0, 1.0)
        };
        (score, strong_neg)
    }

    fn tag_relation_fit(&self, post: &Post) -> f32 {
        let w_g_cfg = self.priors.tag_relation_w_global.max(0.0);
        let w_u_cfg = self.priors.tag_relation_w_personal.max(0.0);
        if w_g_cfg + w_u_cfg <= 0.0 {
            return FEEDBACK_NEUTRAL;
        }

        // Cold-start: shrink personal weight by confidence, re-route freed weight to global.
        let conf = self.personal_confidence;
        let w_u = w_u_cfg * conf;
        let w_g = w_g_cfg + w_u_cfg * (1.0 - conf);

        // Resolve tags to interned ids once; pair loop is then alloc-free.
        let mut entries: Vec<(f32, Option<TagId>, Option<TagId>)> = Vec::with_capacity(24);
        for (group, group_tags) in [
            (Group::Artist, &post.tags.artist),
            (Group::Character, &post.tags.character),
            (Group::Copyright, &post.tags.copyright),
            (Group::Species, &post.tags.species),
            (Group::General, &post.tags.general),
            (Group::Lore, &post.tags.lore),
        ] {
            let gw = self.group_wts[group as usize];
            if gw <= 0.0 {
                continue;
            }
            for t in group_tags {
                if t.is_empty() {
                    continue;
                }
                let tlc = normalize_tag(t);
                let g_id = self.global_relation.tag_id(group as u8, tlc.as_ref());
                let u_id = self.user_relation.tag_id(group as u8, tlc.as_ref());
                entries.push((gw, g_id, u_id));
            }
        }
        if entries.len() < 2 {
            return FEEDBACK_NEUTRAL;
        }

        let ng = self.global_relation.n_posts().max(1) as f32;
        let nu = self.user_relation.n_posts().max(0) as f32;
        let pmi_scale = self.priors.tag_relation_pmi_scale.max(1e-3);
        let min_cooc_global = self.priors.tag_relation_min_cooc.max(1);
        let min_cooc_user = self.priors.tag_relation_user_min_cooc.max(1);
        let cooc_ref = self.priors.tag_relation_cooc_ref.max(1.0);
        let user_cooc_ref = self.priors.tag_relation_user_cooc_ref.max(1.0);
        let cooc_ref_log = (cooc_ref + 1.0).ln().max(1e-3);
        let user_cooc_ref_log = (user_cooc_ref + 1.0).ln().max(1e-3);

        let mut num = 0.0f32;
        let mut den = 0.0f32;

        for (i, entry_i) in entries.iter().enumerate() {
            let (gi_w, gi_global, gi_user) = *entry_i;
            let gi_df = gi_global
                .map(|id| self.global_relation.marginal_by_id(id).max(0) as f32)
                .unwrap_or(0.0);
            let gi_um = gi_user
                .map(|id| self.user_relation.marginal_by_id(id).max(0) as f32)
                .unwrap_or(0.0);

            for entry_j in &entries[i + 1..] {
                let (gj_w, gj_global, gj_user) = *entry_j;
                let pair_w = (gi_w * gj_w).sqrt();
                if pair_w <= 0.0 {
                    continue;
                }

                // Global channel: PMI in [0, 1]. Boost-only — anti-association
                // at the catalog level isn't a personal preference statement.
                let (global_score, global_has_signal) = match (gi_global, gj_global) {
                    (Some(a), Some(b)) => {
                        let c = self.global_relation.cooc_by_id(a, b);
                        let gj_df = self.global_relation.marginal_by_id(b).max(0) as f32;
                        if c >= min_cooc_global && gi_df > 0.0 && gj_df > 0.0 {
                            let denom = gi_df * gj_df / ng;
                            if denom > 0.0 {
                                let lift = (c as f32) / denom;
                                let raw_pmi =
                                    (lift.max(1e-6).ln() / pmi_scale).clamp(0.0, 1.0);
                                let conf_pmi =
                                    ((c as f32 + 1.0).ln() / cooc_ref_log).clamp(0.0, 1.0);
                                (raw_pmi * conf_pmi, true)
                            } else {
                                (0.0, false)
                            }
                        } else {
                            (0.0, false)
                        }
                    }
                    _ => (0.0, false),
                };

                // Personal channel: signed PMI mapped to [0, 1]. Anti-correlated
                // pairs push the score below 0.5, not merely zero out.
                let (user_score, user_has_signal) = match (gi_user, gj_user) {
                    (Some(a), Some(b)) if nu > 0.0 => {
                        let c = self.user_relation.cooc_by_id(a, b);
                        let gj_um = self.user_relation.marginal_by_id(b).max(0) as f32;
                        if c >= min_cooc_user && gi_um > 0.0 && gj_um > 0.0 {
                            let denom = gi_um * gj_um / nu;
                            if denom > 0.0 {
                                let lift = (c as f32) / denom;
                                let signed_pmi =
                                    (lift.max(1e-6).ln() / pmi_scale).clamp(-1.0, 1.0);
                                let conf_user = ((c as f32 + 1.0).ln() / user_cooc_ref_log)
                                    .clamp(0.0, 1.0);
                                let mapped = (signed_pmi * conf_user + 1.0) * 0.5;
                                (mapped.clamp(0.0, 1.0), true)
                            } else {
                                (0.0, false)
                            }
                        } else {
                            (0.0, false)
                        }
                    }
                    _ => (0.0, false),
                };

                // Drop missing channels from the blend so present ones aren't diluted to zero.
                let active_g = if global_has_signal { w_g } else { 0.0 };
                let active_u = if user_has_signal { w_u } else { 0.0 };
                let active_sum = active_g + active_u;
                if active_sum <= 0.0 {
                    continue;
                }
                let pair_score = (active_g * global_score + active_u * user_score) / active_sum;

                num += pair_w * pair_score;
                den += pair_w;
            }
        }

        if den <= 0.0 {
            FEEDBACK_NEUTRAL
        } else {
            (num / den).clamp(0.0, 1.0)
        }
    }

    fn recency_fit(&self, age_days: f32) -> f32 {
        let p = self.priors;
        let tau = p.recency_tau_days.max(1e-3);
        let global = (-age_days.max(0.0) / tau).exp().clamp(0.0, 1.0);
        let avg_age = self.profile.recency.avg_age_days;
        if avg_age <= 0.0 {
            return global;
        }
        let floor = tau * p.recency_personal_floor_frac.max(0.0);
        let spread = self.profile.recency.avg_abs_dev_days.max(floor).max(1.0);

        // Log-age makes the kernel scale-free: 30→60d and 300→600d count equally.
        let personal = if p.recency_log_personal {
            let log_age = age_days.max(0.0).ln_1p();
            let log_avg = avg_age.max(0.0).ln_1p();
            let log_spread = (spread / avg_age.max(1.0)).max(0.05);
            (-((log_age - log_avg).abs()) / log_spread).exp().clamp(0.0, 1.0)
        } else {
            (-((age_days - avg_age).abs()) / spread).exp().clamp(0.0, 1.0)
        };

        let conf = self.personal_confidence;
        let w_p = p.recency_w_personal * conf;
        let w_g = p.recency_w_global + p.recency_w_personal * (1.0 - conf);
        blend2(global, w_g, personal, w_p)
    }
}

struct PostFeatures {
    artist: HashSet<String>,
    character: HashSet<String>,
    general: HashSet<String>,
}

impl PostFeatures {
    fn from_post(p: &Post) -> Self {
        Self {
            artist: p
                .tags
                .artist
                .iter()
                .map(|t| normalize_tag(t).into_owned())
                .collect(),
            character: p
                .tags
                .character
                .iter()
                .map(|t| normalize_tag(t).into_owned())
                .collect(),
            general: p
                .tags
                .general
                .iter()
                .map(|t| normalize_tag(t).into_owned())
                .collect(),
        }
    }
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f32;
    let union = (a.len() + b.len()) as f32 - inter;
    if union <= 0.0 { 0.0 } else { inter / union }
}

/// MMR-style: max similarity to any selected post in the window, not a sum.
fn max_redundancy(cand: &PostFeatures, selected: &[PostFeatures], priors: &Priors) -> f32 {
    let window = priors.diversity_window.max(1);
    let mut max_sim = 0.0f32;
    for chosen in selected.iter().rev().take(window) {
        let sim = jaccard(&cand.artist, &chosen.artist) * priors.diversity_w_artist
            + jaccard(&cand.character, &chosen.character) * priors.diversity_w_character
            + jaccard(&cand.general, &chosen.general) * priors.diversity_w_general;
        if sim > max_sim {
            max_sim = sim;
        }
    }
    max_sim
}

pub fn diversify_scored_posts(mut posts: Vec<ScoredPost>, priors: &Priors) -> Vec<ScoredPost> {
    let mut features: Vec<PostFeatures> = posts
        .iter()
        .map(|sp| PostFeatures::from_post(&sp.post))
        .collect();
    let mut selected: Vec<ScoredPost> = Vec::with_capacity(posts.len());
    let mut selected_feats: Vec<PostFeatures> = Vec::with_capacity(posts.len());

    // Cache top_score across iterations; only re-scan when the just-picked
    // item carried the current maximum (otherwise removing it can't change
    // `max(remaining.score)`). Drops the per-iteration full scan and turns
    // the loop's overhead into roughly O(N + #ties).
    let mut top_score = posts
        .iter()
        .map(|p| p.score)
        .fold(f32::MIN, f32::max);

    while !posts.is_empty() {
        // MMR: penalty = redundancy * gap-from-top. Perfect score → gap≈0 → minimal penalty.
        let mut best_idx = 0usize;
        let mut best_value = f32::MIN;
        let mut best_id = i64::MAX;

        for idx in 0..posts.len() {
            let interaction_fit = posts[idx]
                .breakdown
                .as_ref()
                .map(|b| b.interaction_fit)
                .unwrap_or(FEEDBACK_NEUTRAL);
            let redundancy = max_redundancy(&features[idx], &selected_feats, priors);
            let gap = (top_score - posts[idx].score).max(0.0);
            let penalty = (redundancy
                * gap
                * (1.0 - DIVERSITY_INTERACTION_DAMP * interaction_fit))
                .clamp(0.0, DIVERSITY_MAX_PENALTY);
            let adj = posts[idx].score - penalty;
            let id = posts[idx].post.id;
            if adj > best_value || (adj == best_value && id < best_id) {
                best_value = adj;
                best_idx = idx;
                best_id = id;
            }
        }

        let removed_was_top = (posts[best_idx].score - top_score).abs() < 1e-6;
        selected.push(posts.swap_remove(best_idx));
        selected_feats.push(features.swap_remove(best_idx));
        if removed_was_top && !posts.is_empty() {
            top_score = posts
                .iter()
                .map(|p| p.score)
                .fold(f32::MIN, f32::max);
        }
    }

    selected
}
