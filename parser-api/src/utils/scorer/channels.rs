//! Per-channel scoring methods on `ScoringContext`.

use crate::models::Post;
use crate::utils::tag_relation::TagId;

use super::context::ScoringContext;
use super::util::{
    blend2, blend3, ctr_score, discrete_preference_smooth, normalize_tag, one_sided_ratio, sigmoid,
    wilson_lower_bound, PairAggregator, FEEDBACK_NEUTRAL, WILSON_Z,
};
use super::Group;

impl<'a> ScoringContext<'a> {
    pub(super) fn tag_similarity(&self, post: &Post) -> f32 {
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

    pub(super) fn quality_fit(&self, post: &Post) -> f32 {
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
        blend3(
            absolute,
            p.quality_w_absolute,
            rel_score,
            p.quality_w_relative_score,
            rel_comments,
            p.quality_w_relative_comments,
        )
    }

    pub(super) fn popularity_fit(&self, post: &Post) -> f32 {
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

    pub(super) fn rating_fit(&self, post: &Post) -> f32 {
        let rating = post.rating.to_string();
        let matched = self
            .profile
            .rating
            .iter()
            .find(|s| s.rating == rating)
            .map(|s| s.count.max(0))
            .unwrap_or(0);
        let k = self.profile.rating.len().max(3);
        let boost = self.priors.coldstart_smoothing_boost.max(0.0);
        let alpha = self.priors.discrete_smoothing_alpha
            * (1.0 + (1.0 - self.personal_confidence) * boost);
        discrete_preference_smooth(
            self.rating_total,
            matched,
            k,
            alpha,
            self.priors.discrete_pref_floor,
        )
    }

    pub(super) fn media_fit(&self, post: &Post) -> f32 {
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

    pub(super) fn interaction_fit(&self, post: &Post) -> (f32, bool) {
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
                    let conf = (pos + neg + imp).ln_1p();
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

    pub(super) fn tag_relation_fit(&self, post: &Post) -> f32 {
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

    pub(super) fn recency_fit(&self, age_days: f32) -> f32 {
        let p = self.priors;
        // Class D v5.3: 2-piece kernel.
        let tau = if !p.recency_tau_recent.is_nan()
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
