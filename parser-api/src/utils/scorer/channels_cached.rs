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
    PairAggregator, FEEDBACK_NEUTRAL, WILSON_Z, ctr_score, wilson_lower_bound,
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

        let score = if total_weight <= 0.0 {
            FEEDBACK_NEUTRAL
        } else {
            (weighted / total_weight).clamp(0.0, 1.0)
        };
        (score, strong_neg)
    }

    /// Cached counterpart of `tag_relation_fit`. Big win: the per-tag
    /// `global_relation.tag_id(...)` HashMap probe is pre-resolved into
    /// `CachedTag.global_tid`, dropping ~2T HashMap-by-string lookups
    /// to zero. The user-relation graph is per-account / empty in the
    /// calibrate dataset, so we skip it entirely on the fast path.
    pub fn tag_relation_fit_cached(&self, post: &CachedPostFeatures) -> f32 {
        let w_g_cfg = self.priors.tag_relation_w_global.max(0.0);
        let w_u_cfg = self.priors.tag_relation_w_personal.max(0.0);
        if w_g_cfg + w_u_cfg <= 0.0 {
            return FEEDBACK_NEUTRAL;
        }

        // Personal weight is shrunk by confidence; with an empty user
        // graph the personal channel never has signal, but we still
        // honour the cold-start re-routing of weight toward global.
        // (The user-side `w_u` is therefore folded into `w_g`.)
        let conf = self.personal_confidence;
        let w_g = w_g_cfg + w_u_cfg * (1.0 - conf);

        // Same scratch layout as the &Post variant: (group_weight, global_tid).
        // User-side TagId is always None on the calibrate fast path.
        let mut entries: Vec<(f32, Option<TagId>)> = Vec::with_capacity(post.tags.len());
        for ct in &post.tags {
            // tag_relation excludes meta — mirror channels.rs
            if ct.group == Group::Meta as u8 {
                continue;
            }
            let gw = self.group_wts[ct.group as usize];
            if gw <= 0.0 {
                continue;
            }
            entries.push((gw, ct.global_tid));
        }
        if entries.len() < 2 {
            return FEEDBACK_NEUTRAL;
        }

        let ng = self.global_relation.n_posts().max(1) as f32;
        let pmi_scale = self.priors.tag_relation_pmi_scale.max(1e-3);
        let min_cooc_global = self.priors.tag_relation_min_cooc.max(1);
        let cooc_ref = self.priors.tag_relation_cooc_ref.max(1.0);
        let cooc_ref_log = (cooc_ref + 1.0).ln().max(1e-3);

        let mut num = 0.0f32;
        let mut den = 0.0f32;

        for (i, entry_i) in entries.iter().enumerate() {
            let (gi_w, gi_global) = *entry_i;
            let gi_df = gi_global
                .map(|id| self.global_relation.marginal_by_id(id).max(0) as f32)
                .unwrap_or(0.0);

            for entry_j in &entries[i + 1..] {
                let (gj_w, gj_global) = *entry_j;
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

                let active_g = if global_has_signal { w_g } else { 0.0 };
                let active_u = 0.0; // empty user graph on the calibrate fast path
                let active_sum = active_g + active_u;
                if active_sum <= 0.0 {
                    continue;
                }
                let pair_score = match self.pair_aggregator {
                    PairAggregator::Mean => {
                        (active_g * global_score) / active_sum
                    }
                    PairAggregator::Max => global_score,
                    PairAggregator::GeoMean => {
                        let g = if global_has_signal { global_score } else { 0.5_f32 };
                        let u = 0.5_f32;
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
}

// Touch the unused-import warning suppressors: CachedTag is part of the
// public type surface that callers need; the impl above only consumes
// CachedPostFeatures directly.
#[allow(dead_code)]
fn _phantom(_: &CachedTag) {}
