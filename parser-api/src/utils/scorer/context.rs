//! `ScoringContext`: per-account state cached at construction. The
//! per-channel implementations live in [`super::channels`].

use std::collections::{HashMap, HashSet};

use crate::models::{
    AccountPreferenceProfile, AccountUploaderStat, Post, ScoreBreakdown, TagCount,
};
use crate::utils::idf::IdfIndex;
use crate::utils::tag_relation::TagRelationGraph;

use super::priors::Priors;
use super::util::{
    confidence, normalize_tag, sigmoid, CompactFeedback, MixWeights, PairAggregator,
};
use super::{Group, GROUP_COUNT};
use crate::db::parse_db_datetime;

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
    /// Per-uploader quality stats (uploader_id -> stats), built from profile.
    pub(super) uploader_map: HashMap<i64, AccountUploaderStat>,
    /// Simple tag names (no e621 search syntax) that should be ignored during
    /// tag-similarity computation. Acts as an IDF prior: blacklisted tags
    /// contribute 0 to both the post vector and the dot product, reducing the
    /// similarity score for posts that contain them.
    pub(super) blacklisted_tags: HashSet<String>,
}

/// Pre-computed per-account data whose construction is expensive (HashMap
/// builds, per-tag IDF computations, BM25 saturation). Cached across grid
/// probes by the calibrate harness so probes that don't touch IDF params
/// or group weights can skip rebuilding the `user` / `feedback` maps.
///
/// `fingerprint` is a hash of the priors fields that affect the base:
/// IDF parameters, group weights, `freq_alpha`, `coldstart_n0`, and
/// `confidence_steepness`. If the fingerprint matches the current probe's
/// priors, the base can be reused; otherwise it must be rebuilt.
///
/// All fields are `pub(super)` — the calibrate binary accesses the struct
/// through [`ScoringContext::from_base`] which moves the fields out.
#[derive(Clone)]
pub struct ContextBase {
    pub(super) user: [HashMap<String, f32>; GROUP_COUNT],
    pub(super) user_tag_count: u32,
    pub(super) u_norm: f32,
    pub(super) feedback: [HashMap<String, CompactFeedback>; GROUP_COUNT],
    pub(super) rating_total: i64,
    pub(super) media_total: i64,
    pub(super) personal_confidence: f32,
    pub(super) user_base_positive_rate: f32,
    pub(super) fingerprint: u64,
    pub(super) uploader_map: HashMap<i64, AccountUploaderStat>,
}

/// Hash of the priors fields that affect [`ContextBase`] construction.
/// Used by the calibrate grid to decide whether a cached base is still
/// fresh for the current probe's priors.
pub fn context_fingerprint(p: &Priors) -> u64 {
    let mut h: u64 = 0;
    // Fold each relevant f32 field via its raw bits.
    macro_rules! mix {
        ($val:expr) => {
            h = h.wrapping_mul(0x9E37_79B9_7F4A_7C15);
            h ^= ($val as u32) as u64;
        };
    }
    // IDF params (M_SIM)
    mix!(p.idf_lambda.to_bits());
    mix!(p.idf_alpha.to_bits());
    mix!(if p.idf_lambda_meta.is_nan() {
        p.idf_lambda.to_bits()
    } else {
        p.idf_lambda_meta.to_bits()
    });
    mix!(p.df_floor.to_bits());
    mix!(p.idf_max.to_bits());
    mix!(p.idf_rsj_smoothing.to_bits());
    mix!(p.bm25_k.to_bits());
    mix!(p.freq_alpha.to_bits());
    // Group weights (M_GROUP_W)
    mix!(p.group_w_artist.to_bits());
    mix!(p.group_w_character.to_bits());
    mix!(p.group_w_copyright.to_bits());
    mix!(p.group_w_species.to_bits());
    mix!(p.group_w_general.to_bits());
    mix!(p.group_w_lore.to_bits());
    // Confidence params (M_CONFIDENCE_DERIVED)
    mix!(p.coldstart_n0.to_bits());
    mix!(p.confidence_steepness.to_bits());
    h
}

impl ContextBase {
    /// Build a new ContextBase from account profile data + current priors.
    /// This is the expensive path: per-tag `normalize_tag`, IDF computation,
    /// BM25 saturation, `powf(freq_alpha)`, and HashMap insertion for every
    /// tag in the account's tag-counts and feedback profile.
    pub fn new(
        account_tag_counts: &[TagCount],
        priors: &Priors,
        idf: &IdfIndex,
        profile: &AccountPreferenceProfile,
    ) -> Self {
        // Group weights affect the `user` map (multiply into per-tag weight).
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

        // Feedback maps (from profile — never changes between probes).
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
                    last_interaction_secs: parse_db_datetime(&fb.last_interaction_at)
                        .ok()
                        .map(|dt| dt.timestamp() as f64),
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

        let fingerprint = context_fingerprint(priors);

        let uploader_map: HashMap<i64, AccountUploaderStat> = profile
            .uploaders
            .iter()
            .map(|u| (u.uploader_id, u.clone()))
            .collect();

        // Option 2 (Positive preferences): synthetic entries for preferred
        // tags that aren't already in the user map. Each gets a synthetic
        // count computed from coldstart_smoothing_boost so they contribute
        // to tag_similarity even without post history.
        for pt in &profile.preferred_tags {
            let tlc = normalize_tag(&pt.tag);
            let Some(group) = Group::from_str(pt.group.as_str()) else {
                continue;
            };
            let g = group_wts[group as usize];
            if g <= 0.0 {
                continue;
            }
            // Skip if the user already has this tag in their map.
            if user[group as usize].contains_key(tlc.as_ref()) {
                continue;
            }
            let idf_w = idf.idf_tempered(&tlc, df_floor, idf_max, rsj, lambda, alpha);
            // Synthetic tf: coldstart_smoothing_boost so the tag registers
            // but doesn't dominate real preference signals.
            let tf = priors.coldstart_smoothing_boost.max(0.1);
            let saturated = (tf * (bm25_k + 1.0)) / (tf + bm25_k);
            let w = saturated.powf(priors.freq_alpha) * g * idf_w * pt.weight;
            if w > 0.0 {
                *user[group as usize].entry(tlc.into_owned()).or_insert(0.0) += w;
                u_norm_sq += w * w;
                user_tag_count += 1;
            }
        }

        Self {
            user,
            user_tag_count,
            u_norm: u_norm_sq.sqrt(),
            feedback,
            rating_total,
            media_total,
            personal_confidence,
            user_base_positive_rate,
            fingerprint,
            uploader_map,
        }
    }

    /// The fingerprint hash this base was built from.
    pub fn fingerprint(&self) -> u64 {
        self.fingerprint
    }
}

impl<'a> ScoringContext<'a> {
    /// Full constructor — builds the expensive ContextBase internally.
    /// Prefer [`Self::from_base`] when a cached base is available.
    /// Construct with an empty blacklist set. See [`Self::new_with_blacklist`]
    /// for the version that accepts blacklisted tags.
    pub fn new(
        account_tag_counts: &[TagCount],
        priors: &'a Priors,
        idf: &'a IdfIndex,
        profile: &'a AccountPreferenceProfile,
        global_relation: &'a TagRelationGraph,
        user_relation: &'a TagRelationGraph,
    ) -> Self {
        Self::new_with_blacklist(
            account_tag_counts,
            priors,
            idf,
            profile,
            global_relation,
            user_relation,
            HashSet::new(),
        )
    }

    /// Like [`Self::new`] but accepts a set of simple tag names that should
    /// be ignored during tag-similarity computation (IDF prior for account
    /// blacklist). Pass an empty set for the same behaviour as [`Self::new`].
    pub fn new_with_blacklist(
        account_tag_counts: &[TagCount],
        priors: &'a Priors,
        idf: &'a IdfIndex,
        profile: &'a AccountPreferenceProfile,
        global_relation: &'a TagRelationGraph,
        user_relation: &'a TagRelationGraph,
        blacklisted_tags: HashSet<String>,
    ) -> Self {
        let base = ContextBase::new(account_tag_counts, priors, idf, profile);
        let mut ctx = Self::from_base(base, priors, idf, profile, global_relation, user_relation);
        ctx.blacklisted_tags = blacklisted_tags;
        ctx
    }

    /// Fast-path constructor: takes ownership of a pre-built [`ContextBase`]
    /// and wraps it into a full ScoringContext. Only the cheap fields
    /// (`group_wts`, `pair_aggregator`, `mix`) are recomputed from the
    /// current priors — the expensive `user` / `feedback` HashMaps are
    /// moved in from the base.
    pub fn from_base(
        base: ContextBase,
        priors: &'a Priors,
        idf: &'a IdfIndex,
        profile: &'a AccountPreferenceProfile,
        global_relation: &'a TagRelationGraph,
        user_relation: &'a TagRelationGraph,
    ) -> Self {
        // Group weights are cheap to recompute (7 assignments).
        let mut group_wts = [0.0f32; GROUP_COUNT];
        group_wts[Group::Artist as usize] = priors.group_w_artist;
        group_wts[Group::Character as usize] = priors.group_w_character;
        group_wts[Group::Copyright as usize] = priors.group_w_copyright;
        group_wts[Group::Species as usize] = priors.group_w_species;
        group_wts[Group::General as usize] = priors.group_w_general;
        group_wts[Group::Lore as usize] = priors.group_w_lore;
        group_wts[Group::Meta as usize] = 0.0;

        Self {
            priors,
            profile,
            idf,
            global_relation,
            user_relation,
            group_wts,
            pair_aggregator: PairAggregator::from_str(&priors.tag_relation_pair_aggregator),
            mix: MixWeights::from_priors(priors),
            user: base.user,
            user_tag_count: base.user_tag_count,
            u_norm: base.u_norm,
            feedback: base.feedback,
            rating_total: base.rating_total,
            media_total: base.media_total,
            personal_confidence: base.personal_confidence,
            user_base_positive_rate: base.user_base_positive_rate,
            uploader_map: base.uploader_map,
            blacklisted_tags: HashSet::new(),
        }
    }

    /// Apply the final mix+temperature+veto blend to a set of per-channel
    /// scores. Used by the calibrate cache path: channels can be reused
    /// across probes that don't invalidate them, and only this final step
    /// is rerun under the new mix/temperature/penalty priors.
    #[allow(clippy::too_many_arguments)]
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
        uploader: f32,
        exclusivity: f32,
        novelty: f32,
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
            + mix.tag_relation * tag_relation
            + mix.uploader * uploader
            + mix.exclusivity * exclusivity
            + mix.novelty * novelty;
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
        let uploader = self.uploader_fit(post);
        let exclusivity = self.exclusivity_fit(post);
        let novelty = self.novelty_fit(post);

        let mix = self.mix;
        let raw = mix.sim * sim
            + mix.quality * quality
            + mix.recency * recency
            + mix.rating * rating
            + mix.media * media
            + mix.popularity * popularity
            + mix.interaction * interaction
            + mix.tag_relation * tag_relation
            + mix.uploader * uploader
            + mix.exclusivity * exclusivity
            + mix.novelty * novelty;
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
            uploader_fit: uploader,
            exclusivity_fit: exclusivity,
            novelty_fit: novelty,
        };
        (score.clamp(0.0, 1.0), breakdown)
    }

    /// Cached counterpart of [`Self::score`]: takes pre-resolved
    /// [`super::cached::CachedPostFeatures`] and skips all the
    /// `IdfIndex::df_for` / `TagRelationGraph::tag_id` HashMap-by-string
    /// lookups in the tag-keyed channels. Lets callers (calibrate `eval`
    /// + `grid`) drop the original `Post` from the dataset entirely.
    pub fn score_cached(
        &self,
        features: &super::cached::CachedPostFeatures,
    ) -> (f32, ScoreBreakdown) {
        let sim = self.tag_similarity_cached(features);
        let age_days = (self.priors.now - features.created_at).num_seconds() as f32 / 86_400.0;
        let quality = self.quality_fit_cached(features);
        let popularity = self.popularity_fit_cached(features);
        let rating = self.rating_fit_cached(features);
        let media = self.media_fit_cached(features);
        let (interaction, veto) = self.interaction_fit_cached(features);
        let recency = self.recency_fit(age_days);
        let tag_relation = self.tag_relation_fit_cached(features);
        let uploader = self.uploader_fit_cached(features);
        let exclusivity = self.exclusivity_fit_cached(features);
        let novelty = self.novelty_fit_cached(features);

        let score = self.final_blend(
            sim,
            quality,
            recency,
            rating,
            media,
            popularity,
            interaction,
            tag_relation,
            uploader,
            exclusivity,
            novelty,
            veto,
        );

        let breakdown = ScoreBreakdown {
            tag_similarity: sim,
            quality_fit: quality,
            recency_fit: recency,
            rating_fit: rating,
            media_fit: media,
            popularity_fit: popularity,
            interaction_fit: interaction,
            tag_relation_fit: tag_relation,
            uploader_fit: uploader,
            exclusivity_fit: exclusivity,
            novelty_fit: novelty,
        };
        (score, breakdown)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        AccountMediaStat, AccountPreferenceProfile, AccountQualityProfile, AccountRatingStat,
        AccountRecencyProfile, AccountTagFeedback, TagCount,
    };
    use crate::utils::idf::IdfIndex;
    use crate::utils::scorer::priors::Priors;
    use crate::utils::tag_relation::TagRelationGraph;
    use chrono::Utc;
    use std::collections::HashMap;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    fn default_priors() -> Priors {
        Priors {
            now: Utc::now(),
            recency_tau_days: 60.0,
            quality_a: 1.0,
            quality_b: 1.0,
            mix_sim: 0.0,
            mix_quality: 1.0,
            mix_recency: 0.0,
            mix_rating: 0.0,
            mix_media: 0.0,
            mix_popularity: 0.0,
            mix_interaction: 0.0,
            mix_tag_relation: 0.0,
            idf_lambda: 1.0,
            idf_alpha: 1.0,
            freq_alpha: 1.0,
            quality_w_absolute: 1.0,
            quality_w_relative_score: 0.0,
            quality_w_relative_comments: 0.0,
            popularity_w_fav: 1.0,
            popularity_w_duration: 0.0,
            recency_w_global: 1.0,
            recency_w_personal: 0.0,
            diversity_window: 100,
            diversity_w_artist: 1.0,
            diversity_w_character: 1.0,
            diversity_w_general: 1.0,
            quality_log_bias: -3.0,
            discrete_smoothing_alpha: 1.0,
            strong_negative_count: 3,
            strong_negative_penalty: 0.40,
            recency_personal_floor_frac: 1.0,
            tag_relation_w_global: 0.0,
            tag_relation_w_personal: 0.0,
            tag_relation_pmi_scale: 3.5,
            tag_relation_min_cooc: 2,
            tag_relation_user_min_cooc: 1,
            tag_relation_cooc_ref: 16.0,
            tag_relation_user_cooc_ref: 5.0,
            strong_negative_wilson_threshold: 0.55,
            recency_log_personal: true,
            feedback_decay_half_life_days: 90.0,
            meta_interaction_weight: 0.3,
            coldstart_n0: 25.0,
            discrete_pref_floor: 0.05,
            diversity_max_penalty: 0.45,
            diversity_interaction_damp: 0.35,
            df_floor: 0.4,
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
            group_w_general: 0.70,
            group_w_lore: 0.40,
            score_temperature: 0.0,
            confidence_steepness: 1.0,
            mmr_redundancy_exp: 1.0,
            tag_sim_jaccard_blend: 0.0,
            idf_lambda_meta: f32::NAN,
            tag_relation_pmi_scale_user: f32::NAN,
            recency_tau_recent: f32::NAN,
            recency_split_age_days: 30.0,
            tag_relation_pair_aggregator: "mean".to_string(),
            quality_c: 0.0,
            recency_tau_hot: f32::NAN,
            recency_split_age_hours: 24.0,
            diversity_w_copyright: 1.8,
            diversity_w_species: 1.5,
            exploration_epsilon: 0.0,
            tag_relation_max_tags: 20,
            mix_uploader: 0.0,
            uploader_n0: 5.0,
            uploader_w_avg_score: 0.6,
            uploader_w_avg_fav: 0.4,
            mix_exclusivity: 0.0,
            min_exclusivity_cooc: 2,
            exclusivity_scale: 0.5,
            exclusivity_max_tags: 15,
            mix_novelty: 0.0,
            novelty_n0: 3.0,
            novelty_use_feedback: true,
            diversity_semantic_blend: 0.0,
            diversity_pmi_threshold: 0.0,
            diversity_semantic_max_tags: 10,
            diversity_user_pmi_weight: 1.0,
            exclusivity_cross_group_weight: 0.5,
        }
    }

    fn build_idf() -> IdfIndex {
        let mut df = HashMap::new();
        df.insert("skeb".to_string(), 1000);
        df.insert("cat".to_string(), 5000);
        IdfIndex::from_df(&df, 10_000)
    }

    fn build_graph() -> TagRelationGraph {
        let mut g = TagRelationGraph::with_posts(1000);
        g.set_marginal(0, "skeb", 100);
        g.set_marginal(1, "cat", 500);
        g.insert_pair(0, "skeb", 1, "cat", 50);
        g
    }

    fn minimal_profile() -> AccountPreferenceProfile {
        AccountPreferenceProfile {
            rating: vec![],
            media: vec![],
            feedback: vec![],
            quality: AccountQualityProfile::default(),
            recency: AccountRecencyProfile::default(),
            uploaders: vec![],
            last_refreshed_at: None,
            preferred_tags: vec![],
        }
    }

    fn empty_counts() -> Vec<TagCount> {
        vec![]
    }

    // ==================================================================
    //  context_fingerprint
    // ==================================================================

    #[test]
    fn fingerprint_same_priors_produces_same_hash() {
        let a = context_fingerprint(&default_priors());
        let b = context_fingerprint(&default_priors());
        assert_eq!(a, b, "same priors → same fingerprint");
    }

    #[test]
    fn fingerprint_differs_on_idf_lambda() {
        let mut p1 = default_priors();
        let mut p2 = default_priors();
        p1.idf_lambda = 0.5;
        p2.idf_lambda = 0.6;
        assert_ne!(
            context_fingerprint(&p1),
            context_fingerprint(&p2),
            "changing idf_lambda should change fingerprint"
        );
    }

    #[test]
    fn fingerprint_differs_on_group_weight() {
        let mut p1 = default_priors();
        let mut p2 = default_priors();
        p1.group_w_artist = 1.0;
        p2.group_w_artist = 2.0;
        assert_ne!(
            context_fingerprint(&p1),
            context_fingerprint(&p2),
            "changing group_w_artist should change fingerprint"
        );
    }

    #[test]
    fn fingerprint_not_affected_by_irrelevant_fields() {
        // Fields like mix_* are not part of the fingerprint.
        let mut p1 = default_priors();
        let mut p2 = default_priors();
        p1.mix_sim = 0.5;
        p2.mix_sim = 0.8;
        // Different mix weights might give same fingerprint (they're not mixed in)
        // Actually the test is that the fingerprint is the same despite mix changes.
        assert_eq!(
            context_fingerprint(&p1),
            context_fingerprint(&p2),
            "mix weights are not part of context_fingerprint"
        );
    }

    // ==================================================================
    //  ContextBase — construction
    // ==================================================================

    #[test]
    fn context_base_construction_empty_counts() {
        let p = default_priors();
        let idf = build_idf();
        let _graph = build_graph();
        let profile = minimal_profile();
        let base = ContextBase::new(&empty_counts(), &p, &idf, &profile);
        for g in 0..GROUP_COUNT {
            assert!(base.user[g].is_empty(), "group {g} should be empty");
        }
        assert_eq!(base.user_tag_count, 0);
        assert!(close(base.u_norm, 0.0));
        assert_eq!(base.rating_total, 0);
        assert_eq!(base.media_total, 0);
        assert!(close(base.personal_confidence, 0.0));
        assert!(close(base.user_base_positive_rate, 0.5));
    }

    #[test]
    fn context_base_construction_with_counts() {
        let p = default_priors();
        let idf = build_idf();
        let _graph = build_graph();
        let profile = minimal_profile();
        let counts = vec![
            TagCount {
                name: "skeb".to_string(),
                group_type: "artist".to_string(),
                count: 10,
            },
            TagCount {
                name: "cat".to_string(),
                group_type: "character".to_string(),
                count: 20,
            },
        ];
        let base = ContextBase::new(&counts, &p, &idf, &profile);

        assert_eq!(base.user_tag_count, 2);
        assert!(!base.user[0].is_empty(), "artist map should be populated");
        assert!(
            !base.user[1].is_empty(),
            "character map should be populated"
        );
        assert!(base.u_norm > 0.0, "u_norm should be positive");
        assert!(base.fingerprint != 0, "fingerprint should not be zero");
    }

    #[test]
    fn context_base_zero_weight_groups_skipped() {
        let mut p = default_priors();
        p.group_w_artist = 0.0;
        let idf = build_idf();
        let _graph = build_graph();
        let profile = minimal_profile();
        let counts = vec![TagCount {
            name: "skeb".to_string(),
            group_type: "artist".to_string(),
            count: 10,
        }];
        let base = ContextBase::new(&counts, &p, &idf, &profile);
        assert!(
            base.user[0].is_empty(),
            "artist map should be empty when group_w_artist=0"
        );
        assert_eq!(base.user_tag_count, 0);
    }

    #[test]
    fn context_base_with_feedback_and_ratings() {
        let p = default_priors();
        let idf = build_idf();
        let _graph = build_graph();
        let profile = AccountPreferenceProfile {
            rating: vec![
                AccountRatingStat {
                    rating: "s".to_string(),
                    count: 100,
                },
                AccountRatingStat {
                    rating: "q".to_string(),
                    count: 50,
                },
            ],
            media: vec![
                AccountMediaStat {
                    media_type: "image".to_string(),
                    count: 120,
                },
                AccountMediaStat {
                    media_type: "video".to_string(),
                    count: 30,
                },
            ],
            feedback: vec![AccountTagFeedback {
                tag_name: "skeb".to_string(),
                group_type: "artist".to_string(),
                impression_count: 10,
                positive_count: 8,
                negative_count: 1,
                last_interaction_at: Utc::now().to_rfc3339(),
            }],
            quality: AccountQualityProfile::default(),
            recency: AccountRecencyProfile::default(),
            uploaders: vec![],
            last_refreshed_at: None,
            preferred_tags: vec![],
        };
        let counts = vec![TagCount {
            name: "skeb".to_string(),
            group_type: "artist".to_string(),
            count: 5,
        }];
        let base = ContextBase::new(&counts, &p, &idf, &profile);

        assert_eq!(base.rating_total, 150);
        assert_eq!(base.media_total, 150);
        assert!(close(base.personal_confidence, 150.0 / 175.0));
        assert!(close(base.user_base_positive_rate, 8.0 / 9.0));
        let fb = &base.feedback[0];
        assert!(fb.contains_key("skeb"), "feedback should contain skeb");
    }

    // ==================================================================
    //  ContextBase — fingerprint caching
    // ==================================================================

    #[test]
    fn context_base_fingerprint_matches_function() {
        let p = default_priors();
        let idf = build_idf();
        let _graph = build_graph();
        let profile = minimal_profile();
        let base = ContextBase::new(&empty_counts(), &p, &idf, &profile);
        assert_eq!(base.fingerprint(), context_fingerprint(&p));
    }

    // ==================================================================
    //  ScoringContext — final_blend
    // ==================================================================

    #[test]
    fn final_blend_identity_without_temperature() {
        let p = default_priors();
        let idf = build_idf();
        let graph = build_graph();
        let profile = minimal_profile();
        let counts = empty_counts();
        let ctx = ScoringContext::new(&counts, &p, &idf, &profile, &graph, &graph);

        // Each channel contributes proportionally to its mix weight.
        // mix_sim=0, mix_quality=1, others=0 → score = quality
        let score = ctx.final_blend(0.5, 0.7, 0.3, 0.6, 0.4, 0.2, 0.8, 0.5, 0.0, 0.0, 0.0, false);
        assert!(
            close(score, 0.7),
            "with mix_quality=1 and no temp, score should equal quality=0.7 got {score}"
        );
    }

    #[test]
    fn final_blend_temperature_sharpens() {
        let idf = build_idf();
        let graph = build_graph();
        let profile = minimal_profile();
        let counts = empty_counts();

        // Build two contexts: one with temperature=0, one with temperature=5.
        let mut p_no = default_priors();
        p_no.score_temperature = 0.0;
        let ctx_no = ScoringContext::new(&counts, &p_no, &idf, &profile, &graph, &graph);

        let mut p_yes = default_priors();
        p_yes.score_temperature = 5.0;
        let ctx_yes = ScoringContext::new(&counts, &p_yes, &idf, &profile, &graph, &graph);

        // Input score > 0.5 → temperature pushes it higher.
        let s_no = ctx_no.final_blend(0.6, 0.6, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, false);
        let s_yes =
            ctx_yes.final_blend(0.6, 0.6, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, false);
        assert!(
            s_yes >= s_no,
            "temperature should push >0.5 scores higher: {s_no} -> {s_yes}"
        );

        // Input score < 0.5 → temperature pushes it lower.
        let s_no2 =
            ctx_no.final_blend(0.3, 0.3, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, false);
        let s_yes2 =
            ctx_yes.final_blend(0.3, 0.3, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, false);
        assert!(
            s_yes2 <= s_no2,
            "temperature should push <0.5 scores lower: {s_no2} -> {s_yes2}"
        );
    }

    #[test]
    fn final_blend_veto_applies_penalty() {
        let mut p = default_priors();
        p.strong_negative_penalty = 0.25;
        let idf = build_idf();
        let graph = build_graph();
        let profile = minimal_profile();
        let counts = empty_counts();
        let ctx = ScoringContext::new(&counts, &p, &idf, &profile, &graph, &graph);

        let _without =
            ctx.final_blend(0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, false);
        let with_veto =
            ctx.final_blend(0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, true);
        // The total mix weight sums to 1.0, no temperature, so raw=1.0, score=1.0
        // With veto: 1.0 * (1 - 0.25) = 0.75
        assert!(close(with_veto, 0.75), "expected 0.75 got {with_veto}");
    }

    #[test]
    fn final_blend_mix_weights_are_normalized() {
        let mut p = default_priors();
        p.mix_sim = 0.5;
        p.mix_quality = 0.5;
        p.mix_recency = 0.0;
        p.mix_rating = 0.0;
        p.mix_media = 0.0;
        p.mix_popularity = 0.0;
        p.mix_interaction = 0.0;
        p.mix_tag_relation = 0.0;
        let idf = build_idf();
        let graph = build_graph();
        let profile = minimal_profile();
        let counts = empty_counts();
        let ctx = ScoringContext::new(&counts, &p, &idf, &profile, &graph, &graph);

        // sim=0.2, quality=0.8, mix_sim=0.5, mix_quality=0.5, normalized to 1.0
        // score = 0.5*0.2 + 0.5*0.8 = 0.5
        let s = ctx.final_blend(0.2, 0.8, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, false);
        assert!(close(s, 0.5), "expected 0.5 got {s}");
    }

    // ==================================================================
    //  ScoringContext — from_base fast-path
    // ==================================================================

    #[test]
    fn from_base_produces_same_score_as_new() {
        let p = default_priors();
        let idf = build_idf();
        let graph = build_graph();
        let profile = minimal_profile();
        let counts = vec![TagCount {
            name: "skeb".to_string(),
            group_type: "artist".to_string(),
            count: 5,
        }];

        let base = ContextBase::new(&counts, &p, &idf, &profile);
        let ctx_from_base = ScoringContext::from_base(base, &p, &idf, &profile, &graph, &graph);
        let ctx_new = ScoringContext::new(&counts, &p, &idf, &profile, &graph, &graph);

        let s1 =
            ctx_from_base.final_blend(0.3, 0.6, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, false);
        let s2 = ctx_new.final_blend(0.3, 0.6, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, false);
        assert!(
            close(s1, s2),
            "from_base and new should produce identical blends: {s1} vs {s2}"
        );
    }

    #[test]
    fn context_fingerprint_changes_with_confidence_steepness() {
        let mut p1 = default_priors();
        let mut p2 = default_priors();
        p1.confidence_steepness = 1.0;
        p2.confidence_steepness = 2.0;
        assert_ne!(context_fingerprint(&p1), context_fingerprint(&p2));
    }

    #[test]
    fn context_fingerprint_changes_with_coldstart_n0() {
        let mut p1 = default_priors();
        let mut p2 = default_priors();
        p1.coldstart_n0 = 10.0;
        p2.coldstart_n0 = 50.0;
        assert_ne!(context_fingerprint(&p1), context_fingerprint(&p2));
    }
}
