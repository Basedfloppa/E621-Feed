//! Per-channel scoring methods on `ScoringContext`.

use crate::models::Post;
use crate::utils::tag_relation::TagId;

use super::context::ScoringContext;
#[allow(unused_imports)]
use super::util::{
    blend2, blend3, confidence, ctr_score, discrete_preference_smooth, normalize_tag,
    one_sided_ratio, sigmoid, wilson_lower_bound, PairAggregator, FEEDBACK_NEUTRAL, WILSON_Z,
};
use super::Group;

impl<'a> ScoringContext<'a> {
    pub fn tag_similarity(&self, post: &Post) -> f32 {
        let mut dot = 0.0f32;
        let mut p_norm_sq = 0.0f32;
        let mut overlap = 0u32;
        let mut post_tag_count = 0u32;
        let lambda = self.priors.idf_lambda;
        let alpha = self.priors.idf_alpha;
        let lambda_meta = if self.priors.idf_lambda_meta.is_nan() {
            lambda
        } else {
            self.priors.idf_lambda_meta
        };
        let df_floor = self.priors.df_floor;
        let idf_max = self.priors.idf_max;
        let rsj = self.priors.idf_rsj_smoothing;

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
            let lam = if matches!(group, Group::Meta) {
                lambda_meta
            } else {
                lambda
            };
            let user_map = &self.user[group as usize];
            for t in tags {
                if t.is_empty() {
                    continue;
                }
                let tlc = normalize_tag(t);
                let idf_w = self.idf.idf_tempered(&tlc, df_floor, idf_max, rsj, lam, alpha);
                let pw = g * idf_w;
                p_norm_sq += pw * pw;
                post_tag_count += 1;
                if let Some(&uw) = user_map.get(tlc.as_ref()) {
                    dot += uw * pw;
                    overlap += 1;
                }
            }
        }

        let cosine = if self.u_norm <= 0.0 || p_norm_sq <= 0.0 {
            0.0
        } else {
            (dot / (self.u_norm * p_norm_sq.sqrt())).clamp(0.0, 1.0)
        };

        // Class C v5.3: optional Jaccard fallback blend.
        let blend = self.priors.tag_sim_jaccard_blend.clamp(0.0, 1.0);
        if blend <= 1e-3 || post_tag_count == 0 {
            return cosine;
        }
        let union = (self.user_tag_count + post_tag_count).saturating_sub(overlap);
        let jaccard = if union == 0 {
            0.0
        } else {
            (overlap as f32) / (union as f32)
        };
        ((1.0 - blend) * cosine + blend * jaccard).clamp(0.0, 1.0)
    }

    pub fn quality_fit(&self, post: &Post) -> f32 {
        let p = self.priors;
        let exp = p.one_sided_ratio_exp;
        let absolute = sigmoid(
            p.quality_a * (post.score.total.max(0) as f32).ln_1p()
                + p.quality_b * (post.fav_count.max(0) as f32).ln_1p()
                + p.quality_log_bias,
        );
        let rel_score = one_sided_ratio(
            post.score.total.max(0) as f32,
            self.profile.quality.avg_score_total,
            exp,
        );
        let rel_comments = one_sided_ratio(
            post.comment_count.max(0) as f32,
            self.profile.quality.avg_comment_count,
            exp,
        );
        let mut score = blend3(
            absolute,
            p.quality_w_absolute,
            rel_score,
            p.quality_w_relative_score,
            rel_comments,
            p.quality_w_relative_comments,
        );
        // Class F: blend in upvote ratio if quality_c > 0.
        if p.quality_c > 1e-3 {
            let up = post.score.up.max(0) as f32;
            let down = post.score.down.max(0) as f32;
            let upvote_ratio = if up + down > 0.0 {
                up / (up + down)
            } else {
                0.5
            };
            let w_sum = p.quality_w_absolute
                + p.quality_w_relative_score
                + p.quality_w_relative_comments;
            if w_sum > 0.0 {
                score = (score * w_sum + upvote_ratio * p.quality_c) / (w_sum + p.quality_c);
            } else {
                score = upvote_ratio;
            }
        }
        score
    }

    pub fn popularity_fit(&self, post: &Post) -> f32 {
        let p = self.priors;
        let exp = p.one_sided_ratio_exp;
        let fav_fit = one_sided_ratio(
            post.fav_count.max(0) as f32,
            self.profile.quality.avg_fav_count,
            exp,
        );
        let dur_val = post.duration.unwrap_or(0.0) as f32;
        let dur_base = self.profile.quality.avg_duration;
        let duration_fit = if dur_val > 0.0 || dur_base > 0.0 {
            one_sided_ratio(dur_val, dur_base, exp)
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

    pub fn rating_fit(&self, post: &Post) -> f32 {
        let rating = post.rating.to_string();
        let matched = self
            .profile
            .rating
            .iter()
            .find(|s| s.rating == rating)
            .map(|s| s.count.max(0))
            .unwrap_or(0);
        let total = self.rating_total.max(1);
        let k = self.profile.rating.len().max(3);
        let boost = self.priors.coldstart_smoothing_boost.max(0.0);
        let alpha = self.priors.discrete_smoothing_alpha
            * (1.0 + (1.0 - self.personal_confidence) * boost);

        // Baseline smoothed rate (legacy behaviour, kept as the conservative
        // anchor for cold/ambiguous profiles).
        let smoothed = discrete_preference_smooth(total, matched, k, alpha, self.priors.discrete_pref_floor);

        // Confidence-weighted blend with the raw observed rate. When the
        // user has a strong preference for a rating (e.g. 500 S vs 50 Q),
        // the raw rate pulls the score toward the true ratio; when the
        // preference is weak or noisy, smoothed dominates.
        let confidence = (matched as f32 / (matched as f32 + alpha)).sqrt();
        let raw = matched as f32 / total as f32;
        (smoothed * (1.0 - confidence) + raw * confidence).clamp(0.0, 1.0)
    }

    pub fn media_fit(&self, post: &Post) -> f32 {
        let media = post.media_type();
        let matched = self
            .profile
            .media
            .iter()
            .find(|s| s.media_type == media)
            .map(|s| s.count.max(0))
            .unwrap_or(0);
        let k = self.profile.media.len().max(3);
        let boost = self.priors.coldstart_smoothing_boost.max(0.0);
        let alpha = self.priors.discrete_smoothing_alpha
            * (1.0 + (1.0 - self.personal_confidence) * boost);
        discrete_preference_smooth(
            self.media_total,
            matched,
            k,
            alpha,
            self.priors.discrete_pref_floor,
        )
    }

    pub fn interaction_fit(&self, post: &Post) -> (f32, bool) {
        let mut total_weight = 0.0f32;
        let mut weighted = 0.0f32;
        let mut strong_neg = false;

        let strong_min = self.priors.strong_negative_count.max(1) as f32;
        let wilson_threshold = self
            .priors
            .strong_negative_wilson_threshold
            .clamp(0.05, 0.99);
        let p0 = self.user_base_positive_rate;
        let meta_w = self.priors.meta_interaction_weight.max(0.0);

        // Class F: time-weighted decay — supplementary decay since the last
        // profile refresh. If the profile hasn't been refreshed recently,
        // the feedback counts are stale and should carry less weight.
        let staleness = match self.profile.last_refreshed_at {
            Some(last) => {
                let elapsed_days =
                    (self.priors.now - last).num_seconds() as f32 / 86_400.0;
                if elapsed_days > 0.0 {
                    (-std::f32::consts::LN_2 * elapsed_days
                        / self.priors.feedback_decay_half_life_days.max(1.0))
                    .exp()
                } else {
                    1.0
                }
            }
            None => 1.0,
        };

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
                    let conf = (pos + neg + imp).ln_1p() * staleness;
                    if conf <= 0.0 {
                        continue;
                    }
                    let w = group_weight * conf;
                    total_weight += w;
                    weighted +=
                        w * ctr_score(pos, neg, imp, p0, self.priors.interaction_ctr_prior_alpha);

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

    pub fn tag_relation_fit(&self, post: &Post) -> f32 {
        let w_g_cfg = self.priors.tag_relation_w_global.max(0.0);
        let w_u_cfg = self.priors.tag_relation_w_personal.max(0.0);
        if w_g_cfg + w_u_cfg <= 0.0 {
            return FEEDBACK_NEUTRAL;
        }

        // Cold-start re-routing: shrink personal weight by confidence.
        let conf = self.personal_confidence;
        let w_u = w_u_cfg * conf;
        let w_g = w_g_cfg + w_u_cfg * (1.0 - conf);

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
        let pmi_scale_user = if self.priors.tag_relation_pmi_scale_user.is_nan() {
            pmi_scale
        } else {
            self.priors.tag_relation_pmi_scale_user.max(1e-3)
        };
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

                // Global channel: PMI in [0,1]; boost-only.
                let (global_score, global_has_signal) = match (gi_global, gj_global) {
                    (Some(a), Some(b)) => {
                        let c = self.global_relation.cooc_by_id(a, b);
                        let gj_df = self.global_relation.marginal_by_id(b).max(0) as f32;
                        if c >= min_cooc_global && gi_df > 0.0 && gj_df > 0.0 {
                            let denom = gi_df * gj_df / ng;
                            if denom > 0.0 {
                                let lift = (c as f32) / denom;
                                let raw_pmi = (lift.max(1e-6).ln() / pmi_scale).clamp(0.0, 1.0);
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

                // Personal channel: signed PMI mapped to [0,1].
                let (user_score, user_has_signal) = match (gi_user, gj_user) {
                    (Some(a), Some(b)) if nu > 0.0 => {
                        let c = self.user_relation.cooc_by_id(a, b);
                        let gj_um = self.user_relation.marginal_by_id(b).max(0) as f32;
                        if c >= min_cooc_user && gi_um > 0.0 && gj_um > 0.0 {
                            let denom = gi_um * gj_um / nu;
                            if denom > 0.0 {
                                let lift = (c as f32) / denom;
                                let signed_pmi =
                                    (lift.max(1e-6).ln() / pmi_scale_user).clamp(-1.0, 1.0);
                                let conf_user =
                                    ((c as f32 + 1.0).ln() / user_cooc_ref_log).clamp(0.0, 1.0);
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

                let active_g = if global_has_signal { w_g } else { 0.0 };
                let active_u = if user_has_signal { w_u } else { 0.0 };
                let active_sum = active_g + active_u;
                if active_sum <= 0.0 {
                    continue;
                }
                // Class E v5.3: aggregator choice.
                let pair_score = match self.pair_aggregator {
                    PairAggregator::Mean => {
                        (active_g * global_score + active_u * user_score) / active_sum
                    }
                    PairAggregator::Max => global_score.max(user_score),
                    PairAggregator::GeoMean => {
                        let g = if global_has_signal { global_score } else { 0.5 };
                        let u = if user_has_signal { user_score } else { 0.5 };
                        (g.max(0.0) * u.max(0.0)).sqrt()
                    }
                };

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

    pub fn uploader_fit(&self, post: &Post) -> f32 {
        let Some(stats) = self.uploader_map.get(&post.uploader_id) else {
            return FEEDBACK_NEUTRAL;
        };
        let conf = confidence(
            stats.post_count as f32,
            self.priors.uploader_n0.max(1.0),
            self.priors.confidence_steepness,
        );
        if conf <= 1e-6 {
            return FEEDBACK_NEUTRAL;
        }
        let score_fit = one_sided_ratio(
            stats.avg_score,
            self.profile.quality.avg_score_total,
            self.priors.one_sided_ratio_exp,
        );
        let fav_fit = one_sided_ratio(
            stats.avg_fav,
            self.profile.quality.avg_fav_count,
            self.priors.one_sided_ratio_exp,
        );
        let raw = blend2(
            score_fit,
            self.priors.uploader_w_avg_score,
            fav_fit,
            self.priors.uploader_w_avg_fav,
        );
        // Confidence-weighted blend with neutral.
        FEEDBACK_NEUTRAL * (1.0 - conf) + raw * conf
    }

    pub fn recency_fit(&self, age_days: f32) -> f32 {
        let p = self.priors;
        // Class D v5.3: 2-piece kernel + Class F: 3-piece kernel.
        // Hot piece: posts younger than `recency_split_age_hours`.
        let age_hours = age_days * 24.0;
        let tau = if !p.recency_tau_hot.is_nan()
            && age_hours <= p.recency_split_age_hours.max(0.0)
        {
            p.recency_tau_hot.max(1e-3)
        } else if !p.recency_tau_recent.is_nan()
            && age_days <= p.recency_split_age_days.max(0.0)
        {
            p.recency_tau_recent.max(1e-3)
        } else {
            p.recency_tau_days.max(1e-3)
        };
        let global = (-age_days.max(0.0) / tau).exp().clamp(0.0, 1.0);
        let avg_age = self.profile.recency.avg_age_days;
        if avg_age <= 0.0 {
            return global;
        }
        let floor = tau * p.recency_personal_floor_frac.max(0.0);
        let spread = self.profile.recency.avg_abs_dev_days.max(floor).max(1.0);

        let personal = if p.recency_log_personal {
            let log_age = age_days.max(0.0).ln_1p();
            let log_avg = avg_age.max(0.0).ln_1p();
            let log_spread = (spread / avg_age.max(1.0)).max(0.05);
            (-((log_age - log_avg).abs()) / log_spread)
                .exp()
                .clamp(0.0, 1.0)
        } else {
            (-((age_days - avg_age).abs()) / spread)
                .exp()
                .clamp(0.0, 1.0)
        };

        let conf = self.personal_confidence;
        let w_p = p.recency_w_personal * conf;
        let w_g = p.recency_w_global + p.recency_w_personal * (1.0 - conf);
        blend2(global, w_g, personal, w_p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        AccountMediaStat, AccountPreferenceProfile, AccountQualityProfile,
        AccountRatingStat, AccountRecencyProfile, AccountTagFeedback,
        AccountUploaderStat, Flags, Post, Rating, Relationships, Score, TagCount, Tags,
    };
    use crate::utils::idf::IdfIndex;
    use crate::utils::scorer::context::ScoringContext;
    use crate::utils::scorer::priors::Priors;
    use crate::utils::tag_relation::TagRelationGraph;
    use chrono::{Duration as ChronoDuration, Utc};
    use std::collections::HashMap;

    // ------------------------------------------------------------------
    //  Fixture helpers — return owned data so each test owns its stack.
    //  A macro creates the context inline, avoiding borrow-from-local
    //  issues when ScoringContext borrows its inputs.
    // ------------------------------------------------------------------

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
            mix_uploader: 0.0,
            uploader_n0: 5.0,
            uploader_w_avg_score: 0.6,
            uploader_w_avg_fav: 0.4,
        }
    }

    fn build_idf() -> IdfIndex {
        let mut df = HashMap::new();
        df.insert("skeb".to_string(), 1000);
        df.insert("cat".to_string(), 5000);
        df.insert("dog".to_string(), 3000);
        df.insert("furry".to_string(), 8000);
        df.insert("original_character".to_string(), 2000);
        df.insert("commission".to_string(), 1500);
        df.insert("detailed_background".to_string(), 500);
        IdfIndex::from_df(&df, 10_000)
    }

    /// Build a global graph using public API only (set_marginal + insert_pair
    /// both call intern internally).
    fn build_global_graph() -> TagRelationGraph {
        // n_posts=1000, so expected cooc under independence for skeb(100)*cat(500)/1000 = 50.
        // Use cooc=100 to give lift=2.0 → raw PMI > 0.
        let mut g = TagRelationGraph::with_posts(1000);
        g.set_marginal(0, "skeb", 100);
        g.set_marginal(1, "cat", 500);
        g.set_marginal(1, "dog", 300);
        g.set_marginal(2, "original_character", 200);
        g.insert_pair(0, "skeb", 1, "cat", 100);
        g.insert_pair(0, "skeb", 1, "dog", 30);
        g.insert_pair(1, "cat", 1, "dog", 10);
        g
    }

    fn build_user_graph() -> TagRelationGraph {
        let mut g = TagRelationGraph::with_posts(50);
        g.set_marginal(0, "skeb", 20);
        g.set_marginal(1, "cat", 30);
        g.set_marginal(1, "dog", 10);
        g.insert_pair(0, "skeb", 1, "cat", 15);
        g.insert_pair(0, "skeb", 1, "dog", 5);
        g
    }

    fn default_profile() -> AccountPreferenceProfile {
        AccountPreferenceProfile {
            rating: vec![
                AccountRatingStat { rating: "s".to_string(), count: 500 },
                AccountRatingStat { rating: "q".to_string(), count: 100 },
                AccountRatingStat { rating: "e".to_string(), count: 50 },
            ],
            media: vec![
                AccountMediaStat {
                    media_type: "image".to_string(),
                    count: 600,
                },
                AccountMediaStat {
                    media_type: "video".to_string(),
                    count: 50,
                },
            ],
            feedback: vec![
                AccountTagFeedback {
                    tag_name: "skeb".to_string(),
                    group_type: "artist".to_string(),
                    impression_count: 100,
                    positive_count: 80,
                    negative_count: 5,
                },
                AccountTagFeedback {
                    tag_name: "cat".to_string(),
                    group_type: "character".to_string(),
                    impression_count: 50,
                    positive_count: 40,
                    negative_count: 2,
                },
            ],
            quality: AccountQualityProfile {
                avg_score_total: 100.0,
                avg_fav_count: 50.0,
                avg_comment_count: 10.0,
                avg_duration: 0.0,
            },
            recency: AccountRecencyProfile {
                avg_age_days: 30.0,
                avg_abs_dev_days: 15.0,
            },
            uploaders: vec![],
            last_refreshed_at: None,
        }
    }

    fn default_tag_counts() -> Vec<TagCount> {
        vec![
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
            TagCount {
                name: "dog".to_string(),
                group_type: "character".to_string(),
                count: 5,
            },
            TagCount {
                name: "original_character".to_string(),
                group_type: "copyright".to_string(),
                count: 15,
            },
            TagCount {
                name: "furry".to_string(),
                group_type: "general".to_string(),
                count: 50,
            },
            TagCount {
                name: "commission".to_string(),
                group_type: "general".to_string(),
                count: 30,
            },
        ]
    }

    fn make_post(tags: Tags, score_total: i64, fav_count: i64, rating: Rating) -> Post {
        Post {
            id: 1,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            file: None,
            preview: None,
            sample: None,
            score: Score {
                up: score_total.max(0),
                down: 0,
                total: score_total,
            },
            tags,
            locked_tags: None,
            change_seq: 0.0,
            flags: Flags::default(),
            rating,
            fav_count,
            sources: vec![],
            pools: vec![],
            relationships: Relationships {
                parent_id: None,
                has_children: false,
                has_active_children: false,
                children: vec![],
            },
            approver_id: None,
            uploader_id: 0,
            description: None,
            comment_count: 0,
            is_favorited: false,
            has_notes: false,
            duration: None,
        }
    }

    fn make_empty_tags() -> Tags {
        Tags {
            general: vec![],
            artist: vec![],
            copyright: vec![],
            character: vec![],
            species: vec![],
            invalid: vec![],
            meta: vec![],
            lore: vec![],
            contributor: vec![],
        }
    }

    /// Inline setup macro — creates variables on the test's own stack frame
    /// so ScoringContext's borrows are valid for the test's body.
    macro_rules! setup {
        ($ctx:ident) => {
            let priors = default_priors();
            let idf = build_idf();
            let global_graph = build_global_graph();
            let user_graph = build_user_graph();
            let profile = default_profile();
            let counts = default_tag_counts();
            let mut $ctx = ScoringContext::new(
                &counts,
                &priors,
                &idf,
                &profile,
                &global_graph,
                &user_graph,
            );
        };
        ($ctx:ident, $priors:expr) => {
            let priors = $priors;
            let idf = build_idf();
            let global_graph = build_global_graph();
            let user_graph = build_user_graph();
            let profile = default_profile();
            let counts = default_tag_counts();
            let mut $ctx = ScoringContext::new(
                &counts,
                &priors,
                &idf,
                &profile,
                &global_graph,
                &user_graph,
            );
        };
    }

    // ==================================================================
    //  tag_similarity
    // ==================================================================

    #[test]
    fn tag_similarity_full_overlap() {
        setup!(ctx);
        let mut tags = make_empty_tags();
        tags.artist.push("skeb".to_string());
        tags.character.push("cat".to_string());
        tags.character.push("dog".to_string());
        tags.general.push("furry".to_string());
        tags.general.push("commission".to_string());
        let post = make_post(tags, 0, 0, Rating::S);
        let sim = ctx.tag_similarity(&post);
        // IDF weights differ between user (BM25-saturated) and post (pure IDF)
        // so full tag overlap doesn't give cosine = 1.0, but should be high.
        assert!(sim > 0.90, "expected > 0.9 got {sim}");
    }

    // ==================================================================
    //  Tests using inline setup! macro (no build_context function)
    // ==================================================================

    #[test]
    fn tag_similarity_no_overlap() {
        setup!(ctx);
        let mut tags = make_empty_tags();
        tags.artist.push("nonexistent_artist".to_string());
        tags.character.push("nobody".to_string());
        let post = make_post(tags, 0, 0, Rating::S);
        let sim = ctx.tag_similarity(&post);
        // No tags in common → dot product = 0 → cosine = 0.
        assert!(close(sim, 0.0), "expected 0.0 got {sim}");
    }

    #[test]
    fn tag_similarity_partial_overlap() {
        setup!(ctx);
        let mut tags = make_empty_tags();
        tags.artist.push("skeb".to_string()); // user has this
        tags.character.push("nobody".to_string()); // user doesn't
        let post = make_post(tags, 0, 0, Rating::S);
        let sim = ctx.tag_similarity(&post);
        // Partial overlap → score strictly between 0 and 1.
        assert!(sim > 0.0 && sim < 1.0, "expected 0<sim<1 got {sim}");
    }

    #[test]
    fn tag_similarity_zero_user_norm() {
        // A user with zero tag counts → u_norm = 0 → cosine = 0.
        let priors = default_priors();
        let idf = build_idf();
        let global_graph = build_global_graph();
        let user_graph = build_user_graph();
        let profile = default_profile();
        let empty_counts = vec![];
        let ctx = ScoringContext::new(&empty_counts, &priors, &idf, &profile, &global_graph, &user_graph);
        let mut tags = make_empty_tags();
        tags.artist.push("skeb".to_string());
        let post = make_post(tags, 0, 0, Rating::S);
        let sim = ctx.tag_similarity(&post);
        assert!(close(sim, 0.0), "expected 0.0 got {sim}");
    }

    #[test]
    fn tag_similarity_jaccard_blend() {
        // When jaccard_blend > 0, the result should differ from pure cosine.
        let mut priors_cos = default_priors();
        priors_cos.tag_sim_jaccard_blend = 0.0;
        let mut priors_jac = default_priors();
        priors_jac.tag_sim_jaccard_blend = 1.0;

        let idf = build_idf();
        let global_graph = build_global_graph();
        let user_graph = build_user_graph();
        let profile = default_profile();
        let counts = default_tag_counts();
        let ctx_cos = ScoringContext::new(&counts, &priors_cos, &idf, &profile, &global_graph, &user_graph);
        let ctx_jac = ScoringContext::new(&counts, &priors_jac, &idf, &profile, &global_graph, &user_graph);

        let mut tags = make_empty_tags();
        tags.artist.push("skeb".to_string());
        tags.character.push("cat".to_string());
        tags.general.push("detailed_background".to_string());
        let post = make_post(tags, 0, 0, Rating::S);

        let sim_cos = ctx_cos.tag_similarity(&post);
        let sim_jac = ctx_jac.tag_similarity(&post);
        assert!(
            (sim_cos - sim_jac).abs() > 1e-4,
            "Jaccard blend should differ from cosine: cos={sim_cos} jac={sim_jac}"
        );
    }

    // ==================================================================
    //  quality_fit
    // ==================================================================

    #[test]
    fn quality_fit_absolute_only() {
        setup!(ctx);
        let post = make_post(make_empty_tags(), 50, 10, Rating::S);
        let q = ctx.quality_fit(&post);
        // sigmoid(ln_1p(50) + ln_1p(10) - 3) = sigmoid(3.33) ≈ 0.9654
        assert!(close(q, 0.9654), "expected ~0.965 got {q}");
    }

    #[test]
    fn quality_fit_with_upvote_ratio() {
        let mut priors = default_priors();
        priors.quality_c = 0.5;
        priors.quality_w_absolute = 1.0;
        priors.quality_w_relative_score = 0.0;
        priors.quality_w_relative_comments = 0.0;
        let idf = build_idf();
        let global_graph = build_global_graph();
        let user_graph = build_user_graph();
        let profile = default_profile();
        let counts = default_tag_counts();
        let ctx = ScoringContext::new(&counts, &priors, &idf, &profile, &global_graph, &user_graph);

        let post = Post {
            score: Score { up: 40, down: 10, total: 50 },
            // Note: struct update overwrites fav_count to 0 from make_post defaults.
            ..make_post(make_empty_tags(), 0, 0, Rating::S)
        };
        let q = ctx.quality_fit(&post);
        // abs = sigmoid(ln_1p(50) + ln_1p(0) - 3) = sigmoid(3.932 - 3) = sigmoid(0.932) ≈ 0.717
        // upvote_ratio = 40/50 = 0.8
        // blend: (0.717 * 1.0 + 0.8 * 0.5) / (1.0 + 0.5) = 1.117 / 1.5 ≈ 0.745
        assert!(close(q, 0.745), "expected ~0.745 got {q}");
    }

    #[test]
    fn quality_fit_relative_blend() {
        let mut priors = default_priors();
        priors.quality_w_absolute = 0.5;
        priors.quality_w_relative_score = 0.5;
        priors.quality_w_relative_comments = 0.0;
        priors.quality_c = 0.0;
        let idf = build_idf();
        let global_graph = build_global_graph();
        let user_graph = build_user_graph();
        let profile = default_profile();
        let counts = default_tag_counts();
        let ctx = ScoringContext::new(&counts, &priors, &idf, &profile, &global_graph, &user_graph);

        let post = make_post(make_empty_tags(), 50, 10, Rating::S);
        let q = ctx.quality_fit(&post);
        // abs ≈ 0.965, rel_score = (50/100)^0.5 = 0.707
        // blend: (0.965*0.5 + 0.707*0.5) / 1.0 = 0.836
        assert!(close(q, 0.8363), "expected ~0.8363 got {q}");
    }

    // ==================================================================
    //  popularity_fit
    // ==================================================================

    #[test]
    fn popularity_fit_fav_only() {
        setup!(ctx);
        let post = make_post(make_empty_tags(), 0, 25, Rating::S);
        let pop = ctx.popularity_fit(&post);
        // one_sided_ratio(25, 50, 0.5) = 0.7071
        assert!(close(pop, std::f32::consts::FRAC_1_SQRT_2), "expected ~0.7071 got {pop}");
    }

    #[test]
    fn popularity_fit_blend_with_duration() {
        let mut priors = default_priors();
        priors.popularity_w_fav = 0.5;
        priors.popularity_w_duration = 0.5;
        let idf = build_idf();
        let global_graph = build_global_graph();
        let user_graph = build_user_graph();
        let profile = default_profile();
        let counts = default_tag_counts();
        let ctx = ScoringContext::new(&counts, &priors, &idf, &profile, &global_graph, &user_graph);

        let post = Post {
            duration: Some(60.0),
            ..make_post(make_empty_tags(), 0, 25, Rating::S)
        };
        let pop = ctx.popularity_fit(&post);
        // fav_fit = 0.707, dur_fit = 1.0 (baseline 0 → >0 → full marks)
        // blend: (0.707*0.5 + 1.0*0.5)/1.0 = 0.8535
        assert!(close(pop, 0.8535), "expected ~0.854 got {pop}");
    }

    // ==================================================================
    //  rating_fit
    // ==================================================================

    #[test]
    fn rating_fit_matches_top_category() {
        setup!(ctx);
        let post = make_post(make_empty_tags(), 0, 0, Rating::S);
        let r = ctx.rating_fit(&post);
        assert!(r > 0.5, "expected S rating fit > 0.5 got {r}");
    }

    #[test]
    fn rating_fit_unknown_rating() {
        let mut priors = default_priors();
        priors.discrete_smoothing_alpha = 0.0;
        priors.coldstart_smoothing_boost = 0.0;
        let idf = build_idf();
        let global_graph = build_global_graph();
        let user_graph = build_user_graph();
        let profile = default_profile();
        let counts = default_tag_counts();
        let ctx = ScoringContext::new(&counts, &priors, &idf, &profile, &global_graph, &user_graph);
        let post = make_post(make_empty_tags(), 0, 0, Rating::Q);
        let r = ctx.rating_fit(&post);
        // profile has Q count = 100 / total 650 → raw = 0.1538
        assert!(close(r, 100.0 / 650.0), "expected {} got {r}", 100.0 / 650.0);
    }

    // ==================================================================
    //  media_fit
    // ==================================================================

    #[test]
    fn media_fit_image() {
        setup!(ctx);
        let post = make_post(make_empty_tags(), 0, 0, Rating::S);
        let m = ctx.media_fit(&post);
        assert!(m > 0.8, "expected image fit > 0.8 got {m}");
    }

    #[test]
    fn media_fit_unknown_type() {
        let mut priors = default_priors();
        priors.discrete_smoothing_alpha = 0.0;
        priors.coldstart_smoothing_boost = 0.0;
        let idf = build_idf();
        let global_graph = build_global_graph();
        let user_graph = build_user_graph();
        let profile = default_profile();
        let counts = default_tag_counts();
        let ctx = ScoringContext::new(&counts, &priors, &idf, &profile, &global_graph, &user_graph);

        let post = Post {
            file: Some(crate::models::FileInfo {
                ext: Some("swf".to_string()),
                width: 0, height: 0, size: 0, md5: None, url: None,
            }),
            ..make_post(make_empty_tags(), 0, 0, Rating::S)
        };
        let m = ctx.media_fit(&post);
        assert!(m >= 0.0 && m <= 1.0, "media fit out of range: {m}");
    }

    // ==================================================================
    //  recency_fit
    // ==================================================================

    #[test]
    fn recency_fit_global_only() {
        setup!(ctx);
        assert!(close(ctx.recency_fit(0.0), 1.0), "age 0 should give 1.0");
    }

    #[test]
    fn recency_fit_global_decay() {
        setup!(ctx);
        // Age = 60 days → decay = exp(-60/60) = exp(-1) ≈ 0.3679
        let r = ctx.recency_fit(60.0);
        assert!(close(r, 0.3679), "expected ~0.368 got {r}");
    }

    #[test]
    fn recency_fit_personal_blend() {
        let mut priors = default_priors();
        priors.recency_w_global = 0.5;
        priors.recency_w_personal = 0.5;
        let idf = build_idf();
        let global_graph = build_global_graph();
        let user_graph = build_user_graph();
        let profile = default_profile();
        let counts = default_tag_counts();
        let ctx = ScoringContext::new(&counts, &priors, &idf, &profile, &global_graph, &user_graph);
        let r = ctx.recency_fit(30.0);
        assert!(r > 0.5, "expected personal blend > 0.5 got {r}");
    }

    // ==================================================================
    //  interaction_fit
    // ==================================================================

    #[test]
    fn interaction_fit_no_feedback() {
        setup!(ctx);
        let mut tags = make_empty_tags();
        tags.artist.push("unknown_artist".to_string());
        let post = make_post(tags, 0, 0, Rating::S);
        let (score, veto) = ctx.interaction_fit(&post);
        assert!(close(score, FEEDBACK_NEUTRAL), "expected neutral, got {score}");
        assert!(!veto, "expected no veto");
    }

    #[test]
    fn interaction_fit_positive_feedback() {
        setup!(ctx);
        let mut tags = make_empty_tags();
        tags.artist.push("skeb".to_string());
        let post = make_post(tags, 0, 0, Rating::S);
        let (score, veto) = ctx.interaction_fit(&post);
        // Bayesian CTR ≈ 0.769 (p0≈0.945 from profile feedback, alpha=4.0)
        assert!(close(score, 0.7686), "expected ~0.769 got {score}");
        assert!(!veto, "expected no veto");
    }

    #[test]
    fn interaction_fit_triggers_veto() {
        let mut profile = default_profile();
        profile.feedback.push(AccountTagFeedback {
            tag_name: "hated_tag".to_string(),
            group_type: "general".to_string(),
            impression_count: 10,
            positive_count: 0,
            negative_count: 10,
        });
        let priors = default_priors();
        let idf = build_idf();
        let global_graph = build_global_graph();
        let user_graph = build_user_graph();
        let counts = default_tag_counts();
        let ctx = ScoringContext::new(&counts, &priors, &idf, &profile, &global_graph, &user_graph);

        let mut tags = make_empty_tags();
        tags.general.push("hated_tag".to_string());
        let post = make_post(tags, 0, 0, Rating::S);
        let (_score, veto) = ctx.interaction_fit(&post);
        assert!(veto, "expected veto for strongly negative tag");
    }

    #[test]
    fn interaction_fit_staleness_decay() {
        let mut profile = default_profile();
        profile.last_refreshed_at = Some(Utc::now() - ChronoDuration::days(180));
        let mut priors = default_priors();
        priors.feedback_decay_half_life_days = 90.0;
        let idf = build_idf();
        let global_graph = build_global_graph();
        let user_graph = build_user_graph();
        let counts = default_tag_counts();
        let ctx = ScoringContext::new(&counts, &priors, &idf, &profile, &global_graph, &user_graph);

        let mut tags = make_empty_tags();
        tags.artist.push("skeb".to_string());
        let post = make_post(tags, 0, 0, Rating::S);
        let (score, _veto) = ctx.interaction_fit(&post);
        // staleness = exp(-ln2 * 180/90) = 0.25 → score should be < fresh case
        assert!(score > 0.0 && score < 0.921, "expected decayed score < 0.921 got {score}");
    }

    // ==================================================================
    //  tag_relation_fit
    // ==================================================================

    #[test]
    fn tag_relation_fit_disabled_when_weights_zero() {
        setup!(ctx); // w_global=0, w_personal=0 by default
        let mut tags = make_empty_tags();
        tags.artist.push("skeb".to_string());
        tags.character.push("cat".to_string());
        let post = make_post(tags, 0, 0, Rating::S);
        let t = ctx.tag_relation_fit(&post);
        assert!(close(t, FEEDBACK_NEUTRAL), "expected neutral, got {t}");
    }

    #[test]
    fn tag_relation_fit_fewer_than_two_tags() {
        let mut priors = default_priors();
        priors.tag_relation_w_global = 1.0;
        let idf = build_idf();
        let global_graph = build_global_graph();
        let user_graph = build_user_graph();
        let profile = default_profile();
        let counts = default_tag_counts();
        let ctx = ScoringContext::new(&counts, &priors, &idf, &profile, &global_graph, &user_graph);
        let post = make_post(make_empty_tags(), 0, 0, Rating::S);
        let t = ctx.tag_relation_fit(&post);
        assert!(close(t, FEEDBACK_NEUTRAL), "expected neutral, got {t}");
    }

    #[test]
    fn tag_relation_fit_global_only() {
        let mut priors = default_priors();
        priors.tag_relation_w_global = 1.0;
        priors.tag_relation_w_personal = 0.0;
        let idf = build_idf();
        let global_graph = build_global_graph();
        let user_graph = build_user_graph();
        let profile = default_profile();
        let counts = default_tag_counts();
        let ctx = ScoringContext::new(&counts, &priors, &idf, &profile, &global_graph, &user_graph);
        let mut tags = make_empty_tags();
        tags.artist.push("skeb".to_string());
        tags.character.push("cat".to_string());
        tags.character.push("dog".to_string());
        let post = make_post(tags, 0, 0, Rating::S);
        let t = ctx.tag_relation_fit(&post);
        assert!(t > 0.0, "expected above zero (global PMI signal), got {t}");
        assert!(t <= 1.0, "expected <=1.0, got {t}");
    }

    #[test]
    fn tag_relation_fit_personal_only() {
        let mut priors = default_priors();
        priors.tag_relation_w_global = 0.0;
        priors.tag_relation_w_personal = 1.0;
        let idf = build_idf();
        let global_graph = build_global_graph();
        let user_graph = build_user_graph();
        let profile = default_profile();
        let counts = default_tag_counts();
        let ctx = ScoringContext::new(&counts, &priors, &idf, &profile, &global_graph, &user_graph);
        let mut tags = make_empty_tags();
        tags.artist.push("skeb".to_string());
        tags.character.push("cat".to_string());
        let post = make_post(tags, 0, 0, Rating::S);
        let t = ctx.tag_relation_fit(&post);
        assert!(t > FEEDBACK_NEUTRAL, "expected above neutral, got {t}");
    }

    #[test]
    fn tag_relation_fit_max_aggregator() {
        let mut priors = default_priors();
        priors.tag_relation_w_global = 1.0;
        priors.tag_relation_w_personal = 1.0;
        priors.tag_relation_pair_aggregator = "max".to_string();
        let idf = build_idf();
        let global_graph = build_global_graph();
        let user_graph = build_user_graph();
        let profile = default_profile();
        let counts = default_tag_counts();
        let ctx = ScoringContext::new(&counts, &priors, &idf, &profile, &global_graph, &user_graph);
        let mut tags = make_empty_tags();
        tags.artist.push("skeb".to_string());
        tags.character.push("cat".to_string());
        let post = make_post(tags, 0, 0, Rating::S);
        let t = ctx.tag_relation_fit(&post);
        assert!(t > 0.0 && t <= 1.0, "expected 0 < t <= 1, got {t}");
    }

    // ==================================================================
    //  full score path
    // ==================================================================

    #[test]
    fn score_in_range() {
        setup!(ctx);
        let mut tags = make_empty_tags();
        tags.artist.push("skeb".to_string());
        tags.character.push("cat".to_string());
        let post = make_post(tags, 50, 10, Rating::S);
        let (score, breakdown) = ctx.score(&post);
        assert!(score >= 0.0 && score <= 1.0, "score out of range: {score}");
        assert!(breakdown.tag_similarity >= 0.0 && breakdown.tag_similarity <= 1.0);
        assert!(breakdown.quality_fit >= 0.0 && breakdown.quality_fit <= 1.0);
        assert!(breakdown.recency_fit >= 0.0 && breakdown.recency_fit <= 1.0);
        assert!(breakdown.rating_fit >= 0.0 && breakdown.rating_fit <= 1.0);
        assert!(breakdown.media_fit >= 0.0 && breakdown.media_fit <= 1.0);
        assert!(breakdown.popularity_fit >= 0.0 && breakdown.popularity_fit <= 1.0);
        assert!(breakdown.interaction_fit >= 0.0 && breakdown.interaction_fit <= 1.0);
        assert!(breakdown.tag_relation_fit >= 0.0 && breakdown.tag_relation_fit <= 1.0);
        assert!(breakdown.uploader_fit >= 0.0 && breakdown.uploader_fit <= 1.0);
    }

    #[test]
    fn score_temperature_sharpens() {
        let mut priors_no = default_priors();
        priors_no.score_temperature = 0.0;
        priors_no.quality_w_absolute = 1.0;
        let mut priors_yes = default_priors();
        priors_yes.score_temperature = 5.0;
        priors_yes.quality_w_absolute = 1.0;

        let idf = build_idf();
        let global_graph = build_global_graph();
        let user_graph = build_user_graph();
        let profile = default_profile();
        let counts = default_tag_counts();
        let ctx_no = ScoringContext::new(&counts, &priors_no, &idf, &profile, &global_graph, &user_graph);
        let ctx_yes = ScoringContext::new(&counts, &priors_yes, &idf, &profile, &global_graph, &user_graph);

        let post = make_post(make_empty_tags(), 50, 0, Rating::S);
        let (score_no, _) = ctx_no.score(&post);
        let (score_yes, _) = ctx_yes.score(&post);
        if score_no > 0.5 {
            assert!(score_yes >= score_no, "temperature should push high scores higher: {score_no} -> {score_yes}");
        } else {
            assert!(score_yes <= score_no, "temperature should push low scores lower: {score_no} -> {score_yes}");
        }
    }

    #[test]
    fn veto_applies_penalty() {
        let mut priors = default_priors();
        priors.strong_negative_count = 1;
        priors.strong_negative_wilson_threshold = 0.1;
        priors.strong_negative_penalty = 0.5;

        let mut profile = default_profile();
        profile.feedback.push(AccountTagFeedback {
            tag_name: "bad".to_string(),
            group_type: "general".to_string(),
            impression_count: 1,
            positive_count: 0,
            negative_count: 5,
        });

        let idf = build_idf();
        let global_graph = build_global_graph();
        let user_graph = build_user_graph();
        let counts = default_tag_counts();
        let ctx = ScoringContext::new(&counts, &priors, &idf, &profile, &global_graph, &user_graph);

        let mut tags = make_empty_tags();
        tags.general.push("bad".to_string());
        tags.artist.push("skeb".to_string());
        let post = make_post(tags, 100, 10, Rating::S);
        let (score, _) = ctx.score(&post);
        // quality_fit = sigmoid(ln1p(100) + ln1p(10) - 3) = sigmoid(4.013) ≈ 0.982
        // With veto: 0.982 * (1 - 0.5) ≈ 0.491
        assert!(close(score, 0.4911), "expected ~0.4911 with veto, got {score}");
    }

    // ==================================================================
    //  uploader_fit
    // ==================================================================

    #[test]
    fn uploader_fit_no_uploader_map() {
        setup!(ctx);
        let post = make_post(make_empty_tags(), 0, 0, Rating::S);
        let u = ctx.uploader_fit(&post);
        // No uploader_map → FEEDBACK_NEUTRAL.
        assert!(close(u, FEEDBACK_NEUTRAL), "expected neutral, got {u}");
    }

    #[test]
    fn uploader_fit_unknown_uploader() {
        // Profile with an uploader_map, but the post's uploader is not in it.
        let mut profile = default_profile();
        profile.uploaders = vec![AccountUploaderStat {
            uploader_id: 42,
            post_count: 10,
            avg_score: 200.0,
            avg_fav: 100.0,
        }];
        let priors = default_priors();
        let idf = build_idf();
        let global_graph = build_global_graph();
        let user_graph = build_user_graph();
        let counts = default_tag_counts();
        let ctx = ScoringContext::new(&counts, &priors, &idf, &profile, &global_graph, &user_graph);

        // Post has uploader_id = 99 (not in map)
        let post = Post {
            uploader_id: 99,
            ..make_post(make_empty_tags(), 0, 0, Rating::S)
        };
        let u = ctx.uploader_fit(&post);
        assert!(close(u, FEEDBACK_NEUTRAL), "expected neutral, got {u}");
    }

    #[test]
    fn uploader_fit_known_uploader_high_confidence() {
        let mut profile = default_profile();
        profile.uploaders = vec![AccountUploaderStat {
            uploader_id: 42,
            post_count: 100,       // well above n0=5 → high confidence
            avg_score: 200.0,      // vs profile avg 100 → one_sided_ratio(200,100,0.5)=1.0
            avg_fav: 80.0,         // vs profile avg 50  → one_sided_ratio(80,50,0.5)=1.0
        }];
        let mut priors = default_priors();
        priors.uploader_n0 = 5.0;
        priors.uploader_w_avg_score = 0.6;
        priors.uploader_w_avg_fav = 0.4;
        // Enable the channel in the mix so final_blend includes it.
        priors.mix_uploader = 0.0; // keep it out of final blend for pure channel test

        let idf = build_idf();
        let global_graph = build_global_graph();
        let user_graph = build_user_graph();
        let counts = default_tag_counts();
        let ctx = ScoringContext::new(&counts, &priors, &idf, &profile, &global_graph, &user_graph);

        let post = Post {
            uploader_id: 42,
            ..make_post(make_empty_tags(), 0, 0, Rating::S)
        };
        let u = ctx.uploader_fit(&post);
        // conf = 100/(100+5) = 0.952
        // score_fit = 1.0 (200/100 saturated), fav_fit = (80/50)^0.5 ≈ 1.0 (saturated)
        // raw = 0.6*1.0 + 0.4*1.0 = 1.0
        // final = 0.5*(1-0.952) + 1.0*0.952 ≈ 0.976
        // Note: conf = 100/(100+5) = 0.95238... with steepness=1.0
        // Actually confidence(n, n0, p) = n/(n+n0) when p=1
        // conf = 100/(100+5) = 0.95238
        // result = 0.5*(1-0.95238) + 1.0*0.95238 = 0.02381 + 0.95238 = 0.97619
        assert!(close(u, 0.9762), "expected ~0.9762, got {u}");
    }

    #[test]
    fn uploader_fit_low_confidence_near_neutral() {
        let mut profile = default_profile();
        profile.uploaders = vec![AccountUploaderStat {
            uploader_id: 7,
            post_count: 1,         // below n0=5 → low confidence
            avg_score: 200.0,
            avg_fav: 80.0,
        }];
        let mut priors = default_priors();
        priors.uploader_n0 = 5.0;
        priors.uploader_w_avg_score = 0.6;
        priors.uploader_w_avg_fav = 0.4;

        let idf = build_idf();
        let global_graph = build_global_graph();
        let user_graph = build_user_graph();
        let counts = default_tag_counts();
        let ctx = ScoringContext::new(&counts, &priors, &idf, &profile, &global_graph, &user_graph);

        let post = Post {
            uploader_id: 7,
            ..make_post(make_empty_tags(), 0, 0, Rating::S)
        };
        let u = ctx.uploader_fit(&post);
        // conf = 1/(1+5) = 0.1667
        // raw = 1.0 (saturated ratios)
        // result = 0.5*(1-0.1667) + 1.0*0.1667 = 0.4167 + 0.1667 = 0.5833
        assert!(close(u, 0.5833), "expected ~0.5833, got {u}");
    }

    #[test]
    fn uploader_fit_poor_uploader() {
        let mut profile = default_profile();
        profile.uploaders = vec![AccountUploaderStat {
            uploader_id: 7,
            post_count: 100,       // high confidence
            avg_score: 10.0,       // vs profile avg 100 → one_sided_ratio(10,100,0.5)=0.316
            avg_fav: 5.0,          // vs profile avg 50  → one_sided_ratio(5,50,0.5)=0.316
        }];
        let mut priors = default_priors();
        priors.uploader_n0 = 5.0;
        priors.uploader_w_avg_score = 0.6;
        priors.uploader_w_avg_fav = 0.4;

        let idf = build_idf();
        let global_graph = build_global_graph();
        let user_graph = build_user_graph();
        let counts = default_tag_counts();
        let ctx = ScoringContext::new(&counts, &priors, &idf, &profile, &global_graph, &user_graph);

        let post = Post {
            uploader_id: 7,
            ..make_post(make_empty_tags(), 0, 0, Rating::S)
        };
        let u = ctx.uploader_fit(&post);
        // score_fit = (10/100)^0.5 = 0.3162
        // fav_fit = (5/50)^0.5 = 0.3162
        // raw = 0.6*0.3162 + 0.4*0.3162 = 0.3162
        // conf = 100/(100+5) = 0.9524
        // result = 0.5*(1-0.9524) + 0.3162*0.9524 = 0.0238 + 0.3012 = 0.3250
        assert!(close(u, 0.3250), "expected ~0.3250, got {u}");
    }
}
