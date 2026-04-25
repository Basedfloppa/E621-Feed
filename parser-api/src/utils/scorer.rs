use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};

use crate::models::{AccountPreferenceProfile, Post, ScoreBreakdown, ScoredPost, TagCount};
use crate::utils::idf::IdfIndex;
use crate::utils::tag_relation::TagRelationGraph;

const GROUP_COUNT: usize = 7;
const DIVERSITY_INTERACTION_DAMP: f32 = 0.35;
const DIVERSITY_MAX_PENALTY: f32 = 0.45;
const DISCRETE_PREF_FLOOR: f32 = 0.05;
const FEEDBACK_NEUTRAL: f32 = 0.5;

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

impl Group {
    const fn name(self) -> &'static str {
        match self {
            Group::Artist => "artist",
            Group::Character => "character",
            Group::Copyright => "copyright",
            Group::Species => "species",
            Group::General => "general",
            Group::Lore => "lore",
            Group::Meta => "meta",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "artist" => Group::Artist,
            "character" => Group::Character,
            "copyright" => Group::Copyright,
            "species" => Group::Species,
            "general" => Group::General,
            "lore" => Group::Lore,
            "meta" => Group::Meta,
            _ => return None,
        })
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
    #[serde(default = "default_strong_negative_ratio")]
    pub strong_negative_ratio: f32,
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
    #[serde(default = "default_tag_relation_user_smooth")]
    pub tag_relation_user_smooth: f32,
}

fn default_quality_log_bias() -> f32 {
    -3.0
}
fn default_discrete_smoothing_alpha() -> f32 {
    1.0
}
fn default_strong_negative_count() -> i64 {
    3
}
fn default_strong_negative_ratio() -> f32 {
    2.0
}
fn default_strong_negative_penalty() -> f32 {
    0.40
}
fn default_recency_personal_floor_frac() -> f32 {
    1.0
}
fn default_mix_tag_relation() -> f32 {
    0.0
}
fn default_tag_relation_w_global() -> f32 {
    0.4
}
fn default_tag_relation_w_personal() -> f32 {
    0.6
}
fn default_tag_relation_pmi_scale() -> f32 {
    5.0
}
fn default_tag_relation_min_cooc() -> i64 {
    2
}
fn default_tag_relation_user_smooth() -> f32 {
    1.0
}

#[inline]
fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

#[inline]
fn normalize_tag(t: &str) -> String {
    let trimmed = t.trim();
    if trimmed.bytes().any(|b| b.is_ascii_uppercase()) {
        trimmed.to_ascii_lowercase()
    } else {
        trimmed.to_owned()
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

#[derive(Default, Clone, Copy)]
struct CompactFeedback {
    score: f32,
    positive: i64,
    negative: i64,
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
    feedback: HashMap<(u8, String), CompactFeedback>,
    rating_total: i64,
    media_total: i64,
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
            let w = (t.count as f32).powf(priors.freq_alpha) * g * idf_w;
            if w > 0.0 {
                *user[group as usize].entry(tlc).or_insert(0.0) += w;
            }
        }
        for map in &user {
            for &w in map.values() {
                u_norm_sq += w * w;
            }
        }

        let mut feedback: HashMap<(u8, String), CompactFeedback> =
            HashMap::with_capacity(profile.feedback.len());
        for fb in &profile.feedback {
            let Some(group) = Group::from_str(fb.group_type.as_str()) else {
                continue;
            };
            feedback.insert(
                (group as u8, normalize_tag(&fb.tag_name)),
                CompactFeedback {
                    score: fb.interaction_score(),
                    positive: fb.positive_count,
                    negative: fb.negative_count,
                },
            );
        }

        let rating_total: i64 = profile.rating.iter().map(|r| r.count.max(0)).sum();
        let media_total: i64 = profile.media.iter().map(|m| m.count.max(0)).sum();

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
                if let Some(&uw) = user_map.get(&tlc) {
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
        discrete_preference_smooth(
            self.rating_total,
            matched,
            k,
            self.priors.discrete_smoothing_alpha,
        )
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
        discrete_preference_smooth(
            self.media_total,
            matched,
            k,
            self.priors.discrete_smoothing_alpha,
        )
    }

    fn interaction_fit(&self, post: &Post) -> (f32, bool) {
        let mut total_weight = 0.0f32;
        let mut weighted = 0.0f32;
        let mut strong_neg = false;

        let strong_min = self.priors.strong_negative_count.max(1);
        let strong_ratio = self.priors.strong_negative_ratio.max(1.0);

        for (group, tags) in [
            (Group::Artist, &post.tags.artist),
            (Group::Character, &post.tags.character),
            (Group::Copyright, &post.tags.copyright),
            (Group::Species, &post.tags.species),
            (Group::General, &post.tags.general),
            (Group::Lore, &post.tags.lore),
        ] {
            let group_weight = self.group_wts[group as usize];
            if group_weight <= 0.0 {
                continue;
            }
            for tag in tags {
                if tag.is_empty() {
                    continue;
                }
                let key = (group as u8, normalize_tag(tag));
                if let Some(fb) = self.feedback.get(&key) {
                    total_weight += group_weight;
                    weighted += group_weight * fb.score;
                    if fb.negative >= strong_min
                        && fb.negative as f32 > (fb.positive as f32 + 1.0) * strong_ratio
                    {
                        strong_neg = true;
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
        let w_g = self.priors.tag_relation_w_global.max(0.0);
        let w_u = self.priors.tag_relation_w_personal.max(0.0);
        if w_g + w_u <= 0.0 {
            return FEEDBACK_NEUTRAL;
        }

        let mut tags: Vec<(u8, String)> = Vec::with_capacity(24);
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
                tags.push((group as u8, normalize_tag(t)));
            }
        }
        if tags.len() < 2 {
            return FEEDBACK_NEUTRAL;
        }

        let global_marg: Vec<i64> = tags
            .iter()
            .map(|(g, t)| self.global_relation.marginal(*g, t))
            .collect();
        let user_marg: Vec<i64> = tags
            .iter()
            .map(|(g, t)| self.user_relation.marginal(*g, t))
            .collect();

        let ng = self.global_relation.n_posts().max(1) as f32;
        let pmi_scale = self.priors.tag_relation_pmi_scale.max(1e-3);
        let min_cooc_global = self.priors.tag_relation_min_cooc.max(1);
        let user_smooth = self.priors.tag_relation_user_smooth.max(0.0);
        let pair_wsum = (w_g + w_u).max(1e-6);

        let mut num = 0.0f32;
        let mut den = 0.0f32;

        for i in 0..tags.len() {
            let (gi, ti) = &tags[i];
            let gi_w = self.group_wts[*gi as usize];
            let gi_df = global_marg[i].max(0) as f32;
            let gi_um = user_marg[i].max(0) as f32;
            for j in (i + 1)..tags.len() {
                let (gj, tj) = &tags[j];
                let gj_w = self.group_wts[*gj as usize];

                let pair_w = (gi_w * gj_w).sqrt();
                if pair_w <= 0.0 {
                    continue;
                }

                let global_score = {
                    let c = self.global_relation.cooc(*gi, ti, *gj, tj);
                    let gj_df = global_marg[j].max(0) as f32;
                    if c >= min_cooc_global && gi_df > 0.0 && gj_df > 0.0 {
                        let denom = gi_df * gj_df / ng;
                        if denom > 0.0 {
                            let lift = (c as f32) / denom;
                            (lift.max(1e-6).ln() / pmi_scale).clamp(0.0, 1.0)
                        } else {
                            0.0
                        }
                    } else {
                        0.0
                    }
                };

                let user_score = {
                    let c = self.user_relation.cooc(*gi, ti, *gj, tj) as f32;
                    let gj_um = user_marg[j].max(0) as f32;
                    if gi_um + gj_um > 0.0 {
                        let denom = gi_um.min(gj_um).max(0.0) + user_smooth;
                        if denom > 0.0 {
                            (c / denom).clamp(0.0, 1.0)
                        } else {
                            0.0
                        }
                    } else {
                        0.0
                    }
                };

                let pair_score = (w_g * global_score + w_u * user_score) / pair_wsum;

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
        let global = (-age_days / tau).exp().clamp(0.0, 1.0);
        let avg_age = self.profile.recency.avg_age_days;
        if avg_age <= 0.0 {
            return global;
        }
        let floor = tau * p.recency_personal_floor_frac.max(0.0);
        let spread = self.profile.recency.avg_abs_dev_days.max(floor).max(1.0);
        let personal = (-((age_days - avg_age).abs()) / spread)
            .exp()
            .clamp(0.0, 1.0);
        blend2(global, p.recency_w_global, personal, p.recency_w_personal)
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
            artist: p.tags.artist.iter().map(|t| normalize_tag(t)).collect(),
            character: p.tags.character.iter().map(|t| normalize_tag(t)).collect(),
            general: p.tags.general.iter().map(|t| normalize_tag(t)).collect(),
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

fn diversity_penalty(
    cand: &PostFeatures,
    cand_interaction_fit: f32,
    selected: &[PostFeatures],
    priors: &Priors,
) -> f32 {
    let mut penalty = 0.0f32;
    let window = priors.diversity_window.max(1);
    for chosen in selected.iter().rev().take(window) {
        penalty += jaccard(&cand.artist, &chosen.artist) * priors.diversity_w_artist;
        penalty += jaccard(&cand.character, &chosen.character) * priors.diversity_w_character;
        penalty += jaccard(&cand.general, &chosen.general) * priors.diversity_w_general;
    }
    (penalty * (1.0 - DIVERSITY_INTERACTION_DAMP * cand_interaction_fit))
        .clamp(0.0, DIVERSITY_MAX_PENALTY)
}

pub fn diversify_scored_posts(mut posts: Vec<ScoredPost>, priors: &Priors) -> Vec<ScoredPost> {
    let mut features: Vec<PostFeatures> = posts
        .iter()
        .map(|sp| PostFeatures::from_post(&sp.post))
        .collect();
    let mut selected: Vec<ScoredPost> = Vec::with_capacity(posts.len());
    let mut selected_feats: Vec<PostFeatures> = Vec::with_capacity(posts.len());

    while !posts.is_empty() {
        let mut best_idx = 0usize;
        let mut best_value = f32::MIN;
        let mut best_id = i64::MAX;

        for idx in 0..posts.len() {
            let interaction_fit = posts[idx]
                .breakdown
                .as_ref()
                .map(|b| b.interaction_fit)
                .unwrap_or(FEEDBACK_NEUTRAL);
            let penalty =
                diversity_penalty(&features[idx], interaction_fit, &selected_feats, priors);
            let adj = posts[idx].score - penalty;
            let id = posts[idx].post.id;
            if adj > best_value || (adj == best_value && id < best_id) {
                best_value = adj;
                best_idx = idx;
                best_id = id;
            }
        }

        selected.push(posts.swap_remove(best_idx));
        selected_feats.push(features.swap_remove(best_idx));
    }

    selected
}
