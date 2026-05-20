//! Cached-input variants of the tag-keyed channels. Used by the calibrate
//! grid: each post's tag list is pre-resolved to (group, lc, df_raw,
//! global_tid) once at prep time, and the hot loop here skips the
//! `IdfIndex::df_for` and `TagRelationGraph::tag_id` HashMap-by-string
//! lookups.
//!
//! Math is identical to [`super::channels`]; if you change the formulas
//! there you MUST mirror them here. Channels not keyed by post tags
//! (`quality_fit`, `popularity_fit`, `recency_fit`, `rating_fit`,
//! `media_fit`) don't have cached variants — they don't benefit and the
//! calibrate fast path calls the existing `&Post` versions directly.

use super::cached::{CachedPostFeatures, CachedTag};
use super::context::ScoringContext;
use super::util::{
    blend2, blend3, ctr_score, discrete_preference_smooth, one_sided_ratio, sigmoid,
    wilson_lower_bound, PairAggregator, FEEDBACK_NEUTRAL, WILSON_Z,
};
use super::Group;
use crate::utils::tag_relation::TagId;

impl<'a> ScoringContext<'a> {
    /// Cached counterpart of `tag_similarity`. Same cosine-with-Jaccard-blend
    /// math, but each tag's IDF is reconstructed from the cached `df_raw`
    /// (no `idf.df_for(...)` HashMap probe per post×probe).
    pub fn tag_similarity_cached(&self, post: &CachedPostFeatures) -> f32 {
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

        let mut dot = 0.0f32;
        let mut p_norm_sq = 0.0f32;
        let mut overlap = 0u32;
        let mut post_tag_count = 0u32;

        for ct in &post.tags {
            let g_idx = ct.group as usize;
            let g = self.group_wts[g_idx];
            if g <= 0.0 {
                continue;
            }
            let lam = if ct.group == Group::Meta as u8 {
                lambda_meta
            } else {
                lambda
            };
            let idf_w = self
                .idf
                .idf_tempered_from_df(ct.df_raw, df_floor, idf_max, rsj, lam, alpha);
            let pw = g * idf_w;
            p_norm_sq += pw * pw;
            post_tag_count += 1;
            if let Some(&uw) = self.user[g_idx].get(ct.lc.as_str()) {
                dot += uw * pw;
                overlap += 1;
            }
        }

        let cosine = if self.u_norm <= 0.0 || p_norm_sq <= 0.0 {
            0.0
        } else {
            (dot / (self.u_norm * p_norm_sq.sqrt())).clamp(0.0, 1.0)
        };

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

    /// Cached counterpart of `interaction_fit`. The feedback HashMap
    /// itself isn't pre-resolved (one HashMap per account, rebuilt only
    /// on profile change — not per-probe), but we skip the per-tag
    /// `normalize_tag` since cached tags are already lowercased.
    pub fn interaction_fit_cached(&self, post: &CachedPostFeatures) -> (f32, bool) {
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

        // Class F: time-weighted decay — mirrors channels.rs interaction_fit.
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

        for ct in &post.tags {
            let g_idx = ct.group as usize;
            let group_weight = if ct.group == Group::Meta as u8 {
                meta_w
            } else {
                self.group_wts[g_idx]
            };
            if group_weight <= 0.0 {
                continue;
            }
            let group_feedback = &self.feedback[g_idx];
            if let Some(fb) = group_feedback.get(ct.lc.as_str()) {
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

        let score = if total_weight <= 0.0 {
            FEEDBACK_NEUTRAL
        } else {
            (weighted / total_weight).clamp(0.0, 1.0)
        };
        (score, strong_neg)
    }

    /// Cached counterpart of `tag_relation_fit`. Math mirrors the
    /// `&Post` variant — both global and per-account graphs are queried
    /// via pre-resolved `TagId`s on the cached features, eliminating
    /// the per-pair HashMap-by-string lookups.
    pub fn tag_relation_fit_cached(&self, post: &CachedPostFeatures) -> f32 {
        let w_g_cfg = self.priors.tag_relation_w_global.max(0.0);
        let w_u_cfg = self.priors.tag_relation_w_personal.max(0.0);
        if w_g_cfg + w_u_cfg <= 0.0 {
            return FEEDBACK_NEUTRAL;
        }

        // Cold-start re-routing: shrink personal weight by confidence.
        let conf = self.personal_confidence;
        let w_u = w_u_cfg * conf;
        let w_g = w_g_cfg + w_u_cfg * (1.0 - conf);

        let mut entries: Vec<(f32, Option<TagId>, Option<TagId>)> =
            Vec::with_capacity(post.tags.len());
        for ct in &post.tags {
            if ct.group == Group::Meta as u8 {
                continue;
            }
            let gw = self.group_wts[ct.group as usize];
            if gw <= 0.0 {
                continue;
            }
            entries.push((gw, ct.global_tid, ct.user_tid));
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
                let pair_score = match self.pair_aggregator {
                    PairAggregator::Mean => {
                        (active_g * global_score + active_u * user_score) / active_sum
                    }
                    PairAggregator::Max => global_score.max(user_score),
                    PairAggregator::GeoMean => {
                        let g = if global_has_signal { global_score } else { 0.5_f32 };
                        let u = if user_has_signal { user_score } else { 0.5_f32 };
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

    /// Read-only access to the post-tag-count term that the cached sim
    /// path computed. Useful for diagnostic checks; not used by scoring.
    #[allow(dead_code)]
    pub fn cached_post_tag_count(&self, post: &CachedPostFeatures) -> usize {
        post.tags.len()
    }

    /// Cached `quality_fit` — same math as [`super::channels`] but reads
    /// `score_total / fav_count / comment_count` from the prebuilt
    /// features. Lets the calibrate hot path drop the underlying `Post`
    /// from the dataset entirely.
    pub fn quality_fit_cached(&self, post: &CachedPostFeatures) -> f32 {
        let p = self.priors;
        let exp = p.one_sided_ratio_exp;
        let absolute = sigmoid(
            p.quality_a * (post.score_total.max(0) as f32).ln_1p()
                + p.quality_b * (post.fav_count.max(0) as f32).ln_1p()
                + p.quality_log_bias,
        );
        let rel_score = one_sided_ratio(
            post.score_total.max(0) as f32,
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
            let up = post.score_up.max(0) as f32;
            let down = post.score_down.max(0) as f32;
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

    /// Cached `popularity_fit`.
    pub fn popularity_fit_cached(&self, post: &CachedPostFeatures) -> f32 {
        let p = self.priors;
        let exp = p.one_sided_ratio_exp;
        let fav_fit = one_sided_ratio(
            post.fav_count.max(0) as f32,
            self.profile.quality.avg_fav_count,
            exp,
        );
        let dur_val = post.duration;
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

    /// Cached `rating_fit`.
    pub fn rating_fit_cached(&self, post: &CachedPostFeatures) -> f32 {
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

        // Baseline smoothed rate (legacy behaviour).
        let smoothed = discrete_preference_smooth(total, matched, k, alpha, self.priors.discrete_pref_floor);

        // Confidence-weighted blend with raw observed rate.
        let confidence = (matched as f32 / (matched as f32 + alpha)).sqrt();
        let raw = matched as f32 / total as f32;
        (smoothed * (1.0 - confidence) + raw * confidence).clamp(0.0, 1.0)
    }

    /// Cached `media_fit`.
    pub fn media_fit_cached(&self, post: &CachedPostFeatures) -> f32 {
        let matched = self
            .profile
            .media
            .iter()
            .find(|s| s.media_type == post.media_type)
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
}

// Touch the unused-import warning suppressors: CachedTag is part of the
// public type surface that callers need; the impl above only consumes
// CachedPostFeatures directly.
#[allow(dead_code)]
fn _phantom(_: &CachedTag) {}
