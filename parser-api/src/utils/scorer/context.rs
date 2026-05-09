//! `ScoringContext`: per-account state cached at construction. The
//! per-channel implementations live in [`super::channels`].

use std::collections::HashMap;

use crate::models::{AccountPreferenceProfile, Post, ScoreBreakdown, TagCount};
use crate::utils::idf::IdfIndex;
use crate::utils::tag_relation::TagRelationGraph;

use super::priors::Priors;
use super::util::{
    confidence, normalize_tag, sigmoid, CompactFeedback, MixWeights, PairAggregator,
};
use super::{Group, GROUP_COUNT};

pub struct ScoringContext<'a> {
    pub(super) priors: &'a Priors,
    pub(super) profile: &'a AccountPreferenceProfile,
    pub(super) idf: &'a IdfIndex,
    pub(super) global_relation: &'a TagRelationGraph,
    pub(super) user_relation: &'a TagRelationGraph,
    pub(super) group_wts: [f32; GROUP_COUNT],
    pub(super) user: [HashMap<String, f32>; GROUP_COUNT],
    pub(super) user_tag_count: u32,
    pub(super) pair_aggregator: PairAggregator,
    pub(super) u_norm: f32,
    pub(super) feedback: [HashMap<String, CompactFeedback>; GROUP_COUNT],
    pub(super) rating_total: i64,
    pub(super) media_total: i64,
    pub(super) personal_confidence: f32,
    pub(super) user_base_positive_rate: f32,
    pub(super) mix: MixWeights,
}

impl<'a> ScoringContext<'a> {
    pub fn new(
        account_tag_counts: &[TagCount],
        priors: &'a Priors,
        idf: &'a IdfIndex,
        profile: &'a AccountPreferenceProfile,
        global_relation: &'a TagRelationGraph,
        user_relation: &'a TagRelationGraph,
    ) -> Self {
        // Group weights live in Priors (Class B v5.3); meta is pinned 0.
        let mut group_wts = [0.0f32; GROUP_COUNT];
        group_wts[Group::Artist as usize] = priors.group_w_artist;
        group_wts[Group::Character as usize] = priors.group_w_character;
        group_wts[Group::Copyright as usize] = priors.group_w_copyright;
        group_wts[Group::Species as usize] = priors.group_w_species;
        group_wts[Group::General as usize] = priors.group_w_general;
        group_wts[Group::Lore as usize] = priors.group_w_lore;
        group_wts[Group::Meta as usize] = 0.0;

        let mut user: [HashMap<String, f32>; GROUP_COUNT] = Default::default();
        let mut u_norm_sq = 0.0f32;
        let lambda = priors.idf_lambda;
        let alpha = priors.idf_alpha;
        let df_floor = priors.df_floor;
        let idf_max = priors.idf_max;
        let rsj = priors.idf_rsj_smoothing;
        let bm25_k = priors.bm25_k.max(1e-3);

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
            let idf_w = idf.idf_tempered(&tlc, df_floor, idf_max, rsj, lambda, alpha);
            // BM25 saturation on per-tag fav frequency.
            let tf = (t.count as f32).max(0.0);
            let saturated = (tf * (bm25_k + 1.0)) / (tf + bm25_k);
            let w = saturated.powf(priors.freq_alpha) * g * idf_w;
            if w > 0.0 {
                *user[group as usize].entry(tlc.into_owned()).or_insert(0.0) += w;
            }
        }
        let mut user_tag_count: u32 = 0;
        for map in &user {
            for &w in map.values() {
                u_norm_sq += w * w;
            }
            user_tag_count += map.len() as u32;
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

        let n_favorites = rating_total.max(media_total).max(0) as f32;
        let personal_confidence = confidence(
            n_favorites,
            priors.coldstart_n0.max(1.0),
            priors.confidence_steepness,
        );

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
            user_tag_count,
            pair_aggregator: PairAggregator::from_str(&priors.tag_relation_pair_aggregator),
            u_norm: u_norm_sq.sqrt(),
            feedback,
            rating_total,
            media_total,
            personal_confidence,
            user_base_positive_rate,
            mix: MixWeights::from_priors(priors),
        }
    }

    /// Apply the final mix+temperature+veto blend to a set of per-channel
    /// scores. Used by the calibrate cache path: channels can be reused
    /// across probes that don't invalidate them, and only this final step
    /// is rerun under the new mix/temperature/penalty priors.
    pub fn final_blend(
        &self,
        sim: f32,
        quality: f32,
        recency: f32,
        rating: f32,
        media: f32,
        popularity: f32,
        interaction: f32,
        tag_relation: f32,
        veto: bool,
    ) -> f32 {
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
        let temp = self.priors.score_temperature;
        if temp > 1e-3 {
            score = sigmoid((score - 0.5) * temp).clamp(0.0, 1.0);
        }
        if veto {
            score *= 1.0 - self.priors.strong_negative_penalty.clamp(0.0, 1.0);
        }
        score.clamp(0.0, 1.0)
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
        // Class C v5.3: optional sigmoid sharpening on the final blend.
        let temp = self.priors.score_temperature;
        if temp > 1e-3 {
            score = sigmoid((score - 0.5) * temp).clamp(0.0, 1.0);
        }
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
}
