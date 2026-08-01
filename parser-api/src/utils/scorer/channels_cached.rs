//! Cached-input variants of the tag-keyed channels. Used by the calibrate
//! grid: each post's tag list is pre-resolved to (group, lc, `df_raw`,
//! `global_tid`) once at prep time, and the hot loop here skips the
//! `IdfIndex::df_for` and `TagRelationGraph::tag_id` HashMap-by-string
//! lookups.
//!
//! Math is identical to [`super::channels`]; if you change the formulas
//! there you MUST mirror them here. Channels not keyed by post tags
//! (`quality_fit`, `popularity_fit`, `recency_fit`, `rating_fit`,
//! `media_fit`) don't have cached variants — they don't benefit and the
//! calibrate fast path calls the existing `&Post` versions directly.

use super::Group;
use super::cached::{CachedPostFeatures, CachedTag};
use super::context::ScoringContext;
use super::util::{
    FEEDBACK_NEUTRAL, PairAggregator, WILSON_Z, blend2, blend3, confidence, ctr_score,
    discrete_preference_smooth, one_sided_ratio, sigmoid, wilson_lower_bound,
};
use crate::utils::tag_relation::TagId;

impl ScoringContext<'_> {
    /// Cached counterpart of `tag_similarity`. Same cosine-with-Jaccard-blend
    /// math, but each tag's IDF is reconstructed from the cached `df_raw`
    /// (no `idf.df_for(...)` `HashMap` probe per post×probe).
    #[must_use]
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
            // Blacklist-IDF prior: skip tags that the user has blacklisted.
            if self.blacklisted_tags.contains(ct.lc.as_str()) {
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

    /// Cached counterpart of `interaction_fit`. The feedback `HashMap`
    /// itself isn't pre-resolved (one `HashMap` per account, rebuilt only
    /// on profile change — not per-probe), but we skip the per-tag
    /// `normalize_tag` since cached tags are already lowercased.
    #[must_use]
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
        let half_life = f64::from(self.priors.feedback_decay_half_life_days.max(1.0));

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

                // Per-tag staleness decay — mirrors channels.rs.
                // IMPORTANT: raw counts (pos/neg/imp) are already decayed
                // by the /process worker's decay_account_tag_feedback().
                // We only apply the half-life factor to the confidence
                // scaling (ln_1p term) — applying it to raw counts again
                // would be a double-decay bug.
                let tag_staleness = match fb.last_interaction_secs {
                    Some(secs) => {
                        let elapsed_days =
                            ((self.priors.now.timestamp() as f64 - secs) / 86_400.0).max(0.0);
                        if elapsed_days > 0.0 {
                            (-std::f32::consts::LN_2 * elapsed_days as f32 / half_life as f32).exp()
                        } else {
                            1.0
                        }
                    }
                    None => 1.0,
                };
                let conf = (pos + neg + imp).ln_1p() * tag_staleness;
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
    #[must_use]
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

        // Class G: Cluster-PMI — keep only top-K tags by group weight to
        // reduce the O(T²) pairwise loop to O(K²). 0 = no limit (legacy).
        // Mirrors the same optimization in channels.rs tag_relation_fit.
        let max_tags = self.priors.tag_relation_max_tags;
        if max_tags > 0 && entries.len() > max_tags {
            entries.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            entries.truncate(max_tags);
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
            let gi_df = gi_global.map_or(0.0, |id| {
                self.global_relation.marginal_by_id(id).max(0) as f32
            });
            let gi_um = gi_user.map_or(0.0, |id| {
                self.user_relation.marginal_by_id(id).max(0) as f32
            });

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
                        let g = if global_has_signal {
                            global_score
                        } else {
                            0.5_f32
                        };
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
    #[must_use]
    pub fn cached_post_tag_count(&self, post: &CachedPostFeatures) -> usize {
        post.tags.len()
    }

    /// Cached `quality_fit` — same math as [`super::channels`] but reads
    /// `score_total / fav_count / comment_count` from the prebuilt
    /// features. Lets the calibrate hot path drop the underlying `Post`
    /// from the dataset entirely.
    #[must_use]
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
            let w_sum =
                p.quality_w_absolute + p.quality_w_relative_score + p.quality_w_relative_comments;
            if w_sum > 0.0 {
                score = (score * w_sum + upvote_ratio * p.quality_c) / (w_sum + p.quality_c);
            } else {
                score = upvote_ratio;
            }
        }
        score
    }

    /// Cached `popularity_fit`.
    #[must_use]
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
    #[must_use]
    pub fn rating_fit_cached(&self, post: &CachedPostFeatures) -> f32 {
        let rating = post.rating.to_string();
        let matched = self
            .profile
            .rating
            .iter()
            .find(|s| s.rating == rating)
            .map_or(0, |s| s.count.max(0));
        let total = self.rating_total.max(1);
        let k = self.profile.rating.len().max(3);
        let boost = self.priors.coldstart_smoothing_boost.max(0.0);
        let alpha =
            self.priors.discrete_smoothing_alpha * (1.0 + (1.0 - self.personal_confidence) * boost);

        // Baseline smoothed rate (legacy behaviour).
        let smoothed =
            discrete_preference_smooth(total, matched, k, alpha, self.priors.discrete_pref_floor);

        // Confidence-weighted blend with raw observed rate.
        let confidence = (matched as f32 / (matched as f32 + alpha)).sqrt();
        let raw = matched as f32 / total as f32;
        (smoothed * (1.0 - confidence) + raw * confidence).clamp(0.0, 1.0)
    }

    /// Cached `media_fit`.
    #[must_use]
    pub fn media_fit_cached(&self, post: &CachedPostFeatures) -> f32 {
        let matched = self
            .profile
            .media
            .iter()
            .find(|s| s.media_type == post.media_type)
            .map_or(0, |s| s.count.max(0));
        let k = self.profile.media.len().max(3);
        let boost = self.priors.coldstart_smoothing_boost.max(0.0);
        let alpha =
            self.priors.discrete_smoothing_alpha * (1.0 + (1.0 - self.personal_confidence) * boost);
        discrete_preference_smooth(
            self.media_total,
            matched,
            k,
            alpha,
            self.priors.discrete_pref_floor,
        )
    }

    /// Cached `uploader_fit` — same math as [`super::channels`] but reads
    /// `uploader_id` from the prebuilt features.
    #[must_use]
    pub fn uploader_fit_cached(&self, post: &CachedPostFeatures) -> f32 {
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

    /// Cached counterpart of `exclusivity_fit`. Uses pre-resolved `CachedTag`
    /// fields (`global_tid`, `lc`, `group`) to skip tag-ID lookups.
    #[must_use]
    pub fn exclusivity_fit_cached(&self, post: &CachedPostFeatures) -> f32 {
        let p = self.priors;
        if p.mix_exclusivity <= 0.0 {
            return 0.0;
        }
        let min_cooc = p.min_exclusivity_cooc.max(1) as f32;
        let scale = p.exclusivity_scale.max(0.01);
        let max_tags = p.exclusivity_max_tags;

        // Collect tags per group with weights and truncated by max_tags.
        // Each entry: (idf_weight, global_tid). group_tags[g_idx] holds
        // tags from group g_idx.
        let mut group_tags: [Vec<(f32, TagId)>; 7] = Default::default();
        for ct in &post.tags {
            let g = ct.group as usize;
            let gw = self.group_wts[g];
            if gw <= 0.0 {
                continue;
            }
            let weight = gw
                * self.idf.idf_tempered_from_df(
                    ct.df_raw,
                    p.df_floor,
                    p.idf_max,
                    p.idf_rsj_smoothing,
                    p.idf_lambda,
                    p.idf_alpha,
                );
            if let Some(tid) = ct.global_tid {
                group_tags[g].push((weight, tid));
            }
        }

        // Truncate each group to top-K by weight.
        if max_tags > 0 {
            for entries in &mut group_tags {
                entries.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
                entries.truncate(max_tags);
            }
        }

        // Track within-group and cross-group pairs separately.
        let mut pairs_in = 0u32;
        let mut total_cooc_in = 0i64;
        let mut pairs_cross = 0u32;
        let mut total_cooc_cross = 0i64;

        // Within-group pairs: tag co-occurrence computed inside each group.
        for entries in &group_tags {
            for i in 0..entries.len() {
                let tid_a = entries[i].1;
                for entry_b in entries.iter().skip(i + 1) {
                    let tid_b = entry_b.1;
                    let cooc = self.global_relation.cooc_by_id(tid_a, tid_b);
                    total_cooc_in += cooc.max(0);
                    pairs_in += 1;
                }
            }
        }

        // Cross-group pairs: tag co-occurrence computed across all group
        // combinations, capturing rare multi-group patterns (e.g. an artist
        // tag + a character tag that seldom appear together).
        for ai in 0..group_tags.len() {
            for bi in ai + 1..group_tags.len() {
                let a_tags = &group_tags[ai];
                let b_tags = &group_tags[bi];
                if a_tags.is_empty() || b_tags.is_empty() {
                    continue;
                }

                for a_tag in a_tags {
                    let tid_a = a_tag.1;
                    for b_tag in b_tags {
                        let tid_b = b_tag.1;
                        let cooc = self.global_relation.cooc_by_id(tid_a, tid_b);
                        total_cooc_cross += cooc.max(0);
                        pairs_cross += 1;
                    }
                }
            }
        }

        if pairs_in + pairs_cross == 0 {
            return 0.0;
        }

        // Blend: cross-group pairs get weighted by `exclusivity_cross_group_weight`.
        // Default 0.5 → cross-group pairs contribute ~⅓ of total.
        let cw = p.exclusivity_cross_group_weight;
        // Keep pair counts as f32 so fractional cross-group weights don't
        // truncate to 0 when there are few cross-group pairs (which would
        // cause division-by-zero → NaN downstream).
        let weighted_pairs = (pairs_in as f32) + (pairs_cross as f32) * cw;
        let avg_cooc = (total_cooc_in as f32 + (total_cooc_cross as f32) * cw) / weighted_pairs;
        1.0 - sigmoid(avg_cooc / scale - min_cooc)
    }

    /// Cached counterpart of `artist_discovery_fit`. Uses pre-resolved
    /// cached tags to skip `normalize_tag` and IDF lookups.
    #[must_use]
    pub fn artist_discovery_fit_cached(&self, post: &CachedPostFeatures) -> f32 {
        let p = &self.priors;
        if p.mix_artist_discovery <= 0.0 {
            return 0.0;
        }

        // Collect artist tags from the cached features.
        let artist_tags: Vec<_> = post
            .tags
            .iter()
            .filter(|ct| ct.group == Group::Artist as u8)
            .collect();

        // Early exit: no artist tags → no discovery signal.
        if artist_tags.is_empty() {
            return 0.0;
        }

        let known_artists = &self.user[Group::Artist as usize];
        let mut new_artist_count: u32 = 0;
        for ct in &post.tags {
            if ct.group != Group::Artist as u8 {
                continue;
            }
            if !known_artists.contains_key(ct.lc.as_str()) {
                new_artist_count += 1;
            }
        }

        if new_artist_count == 0 {
            return 0.0;
        }

        // Compute tag_similarity excluding artist tags.
        let lambda = p.idf_lambda;
        let alpha = p.idf_alpha;
        let lambda_meta = if p.idf_lambda_meta.is_nan() {
            lambda
        } else {
            p.idf_lambda_meta
        };
        let df_floor = p.df_floor;
        let idf_max = p.idf_max;
        let rsj = p.idf_rsj_smoothing;

        let mut dot = 0.0f32;
        let mut p_norm_sq = 0.0f32;

        for ct in &post.tags {
            let g_idx = ct.group as usize;
            // Skip artist tags and meta.
            if ct.group == Group::Artist as u8 || ct.group == Group::Meta as u8 {
                continue;
            }
            let g = self.group_wts[g_idx];
            if g <= 0.0 {
                continue;
            }
            if self.blacklisted_tags.contains(ct.lc.as_str()) {
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
            if let Some(&uw) = self.user[g_idx].get(ct.lc.as_str()) {
                dot += uw * pw;
            }
        }

        let profile_sim = if self.u_norm <= 0.0 || p_norm_sq <= 0.0 {
            0.0
        } else {
            (dot / (self.u_norm * p_norm_sq.sqrt())).clamp(0.0, 1.0)
        };

        // Confidence from number of new artists.
        let n0 = p.artist_discovery_n0.max(0.5);
        let conf = (new_artist_count as f32) / (new_artist_count as f32 + n0);

        // Novelty bonus: the more new artists, the more we boost.
        let bonus = 1.0 + p.artist_discovery_novelty_bonus * (1.0 - conf);

        (profile_sim * conf * bonus).clamp(0.0, 1.0)
    }

    /// Cached counterpart of `novelty_fit`. Uses pre-resolved `ct.lc` to
    /// skip `normalize_tag`. Checks against `self.user` and `self.feedback`
    /// `HashMaps` (same data as the uncached path).
    #[must_use]
    pub fn novelty_fit_cached(&self, post: &CachedPostFeatures) -> f32 {
        let p = self.priors;
        if p.mix_novelty <= 0.0 {
            return 0.0;
        }
        let n0 = p.novelty_n0.max(0.5);

        let mut total = 0u32;
        let mut novel_weight = 0.0f32;

        for ct in &post.tags {
            let g = ct.group as usize;
            if self.group_wts[g] <= 0.0 {
                continue;
            }
            total += 1;

            // Known from favourites → not novel.
            if self.user[g].contains_key(ct.lc.as_str()) {
                continue;
            }

            // Check feedback impressions if enabled.
            if p.novelty_use_feedback
                && let Some(fb) = self.feedback[g].get(ct.lc.as_str())
                && fb.impressions > 0
            {
                let seen = confidence(fb.impressions as f32, n0, 1.0);
                novel_weight += 1.0 - seen;
                continue;
            }

            // Tag is completely novel.
            novel_weight += 1.0;
        }

        if total == 0 {
            return 0.0;
        }
        (novel_weight / total as f32).clamp(0.0, 1.0)
    }
}

// Touch the unused-import warning suppressors: CachedTag is part of the
// public type surface that callers need; the impl above only consumes
// CachedPostFeatures directly.
#[allow(dead_code)]
fn _phantom(_: &CachedTag) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        AccountMediaStat, AccountPreferenceProfile, AccountQualityProfile, AccountRatingStat,
        AccountRecencyProfile, AccountTagFeedback, Files, Flags, Has, Post, Rating, Relationships,
        Score, Stats, TagCount, Tags,
    };
    use crate::utils::idf::IdfIndex;
    use crate::utils::scorer::cached::CachedPostFeatures;
    use crate::utils::scorer::context::ScoringContext;
    use crate::utils::scorer::priors::Priors;
    use crate::utils::tag_relation::TagRelationGraph;
    use chrono::Utc;
    use std::collections::HashMap;

    // ------------------------------------------------------------------
    //  Fixture helpers (identical to channels.rs tests)
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
            mix_sim: 1.0,
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
            mix_artist_discovery: 0.0,
            artist_discovery_n0: 3.0,
            artist_discovery_novelty_bonus: 0.2,
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

    fn build_global_graph() -> TagRelationGraph {
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
                AccountRatingStat {
                    rating: "s".to_string(),
                    count: 500,
                },
                AccountRatingStat {
                    rating: "q".to_string(),
                    count: 100,
                },
                AccountRatingStat {
                    rating: "e".to_string(),
                    count: 50,
                },
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
                    last_interaction_at: Utc::now().to_rfc3339(),
                },
                AccountTagFeedback {
                    tag_name: "cat".to_string(),
                    group_type: "character".to_string(),
                    impression_count: 50,
                    positive_count: 40,
                    negative_count: 2,
                    last_interaction_at: Utc::now().to_rfc3339(),
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
            preferred_tags: vec![],
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

    fn make_post(tags: Tags) -> Post {
        Post {
            id: 1,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            change_seq: 0.0,
            files: Files::default(),
            uploader_id: 0,
            uploader_name: None,
            approver_id: None,
            stats: Stats {
                score: Score {
                    up: 100,
                    down: 0,
                    total: 100,
                },
                fav_count: 50,
                comment_count: 5,
                ..Default::default()
            },
            flags: Flags::default(),
            has: Has::default(),
            relationships: Relationships::default(),
            pools: vec![],
            rating: Rating::S,
            locked_tags: vec![],
            sources: vec![],
            description: None,
            tags,
        }
    }

    /// Compute per-tag `CachedPostFeatures` for the same Post/IDF/graph used
    /// by the `ScoringContext`.
    fn cache_post(post: &Post, idf: &IdfIndex, global: &TagRelationGraph) -> CachedPostFeatures {
        CachedPostFeatures::from_post(post, idf, global)
    }

    // ==================================================================
    //  Equivalence tests: cached vs uncached output for identical inputs.
    //  If the math diverges, these tests catch it.
    // ==================================================================

    /// Build priors + fixtures inline; return (ctx, idf, global) so the
    /// cached-fn variants can still call `cache_post` without redundant
    /// construction.
    macro_rules! cached_setup {
        ($ctx:ident, $idf:ident, $global:ident) => {
            let priors = default_priors();
            let $idf = build_idf();
            let $global = build_global_graph();
            let user_graph = build_user_graph();
            let profile = default_profile();
            let counts = default_tag_counts();
            let $ctx =
                ScoringContext::new(&counts, &priors, &$idf, &profile, &$global, &user_graph);
        };
    }

    #[test]
    fn tag_similarity_cached_matches_uncached() {
        cached_setup!(ctx, idf, global);
        let mut tags = make_empty_tags();
        tags.artist.push("skeb".to_string());
        tags.character.push("cat".to_string());
        tags.character.push("dog".to_string());
        tags.general.push("furry".to_string());
        tags.general.push("commission".to_string());
        let post = make_post(tags);

        let uncached = ctx.tag_similarity(&post);
        let cached = ctx.tag_similarity_cached(&cache_post(&post, &idf, &global));
        assert!(
            close(uncached, cached),
            "tag_similarity mismatch: uncached={uncached} cached={cached}"
        );
    }

    #[test]
    fn interaction_fit_cached_matches_uncached() {
        cached_setup!(ctx, idf, global);
        let mut tags = make_empty_tags();
        tags.artist.push("skeb".to_string());
        tags.character.push("cat".to_string());
        let post = make_post(tags);

        let (u_score, u_veto) = ctx.interaction_fit(&post);
        let (c_score, c_veto) = ctx.interaction_fit_cached(&cache_post(&post, &idf, &global));
        assert!(
            close(u_score, c_score),
            "interaction_fit score mismatch: uncached={u_score} cached={c_score}"
        );
        assert_eq!(u_veto, c_veto, "interaction_fit veto mismatch");
    }

    #[test]
    fn tag_relation_fit_cached_matches_uncached() {
        let mut priors = default_priors();
        priors.tag_relation_w_global = 1.0;
        priors.tag_relation_w_personal = 0.0;
        let idf = build_idf();
        let global = build_global_graph();
        let user_graph = build_user_graph();
        let profile = default_profile();
        let counts = default_tag_counts();
        let ctx = ScoringContext::new(&counts, &priors, &idf, &profile, &global, &user_graph);

        let mut tags = make_empty_tags();
        tags.artist.push("skeb".to_string());
        tags.character.push("cat".to_string());
        tags.character.push("dog".to_string());
        let post = make_post(tags);

        let uncached = ctx.tag_relation_fit(&post);
        let cached = ctx.tag_relation_fit_cached(&cache_post(&post, &idf, &global));
        assert!(
            close(uncached, cached),
            "tag_relation_fit mismatch: uncached={uncached} cached={cached}"
        );
    }

    #[test]
    fn quality_fit_cached_matches_uncached() {
        cached_setup!(ctx, idf, global);
        let post = make_post(make_empty_tags());
        let u = ctx.quality_fit(&post);
        let c = ctx.quality_fit_cached(&cache_post(&post, &idf, &global));
        assert!(close(u, c), "quality_fit mismatch: uncached={u} cached={c}");
    }

    #[test]
    fn popularity_fit_cached_matches_uncached() {
        cached_setup!(ctx, idf, global);
        let post = make_post(make_empty_tags());
        let u = ctx.popularity_fit(&post);
        let c = ctx.popularity_fit_cached(&cache_post(&post, &idf, &global));
        assert!(
            close(u, c),
            "popularity_fit mismatch: uncached={u} cached={c}"
        );
    }

    #[test]
    fn rating_fit_cached_matches_uncached() {
        cached_setup!(ctx, idf, global);
        let post = make_post(make_empty_tags());
        let u = ctx.rating_fit(&post);
        let c = ctx.rating_fit_cached(&cache_post(&post, &idf, &global));
        assert!(close(u, c), "rating_fit mismatch: uncached={u} cached={c}");
    }

    #[test]
    fn media_fit_cached_matches_uncached() {
        cached_setup!(ctx, idf, global);
        let mut tags = make_empty_tags();
        tags.artist.push("skeb".to_string());
        let post = make_post(tags);
        let u = ctx.media_fit(&post);
        let c = ctx.media_fit_cached(&cache_post(&post, &idf, &global));
        assert!(close(u, c), "media_fit mismatch: uncached={u} cached={c}");
    }

    #[test]
    fn score_cached_matches_uncached() {
        cached_setup!(ctx, idf, global);
        let mut tags = make_empty_tags();
        tags.artist.push("skeb".to_string());
        tags.character.push("cat".to_string());
        let post = make_post(tags);

        let (u_score, u_brk) = ctx.score(&post);
        let (c_score, c_brk) = ctx.score_cached(&cache_post(&post, &idf, &global));
        assert!(
            close(u_score, c_score),
            "score mismatch: uncached={u_score} cached={c_score}"
        );
        assert!(
            close(u_brk.tag_similarity, c_brk.tag_similarity),
            "breakdown.tag_similarity mismatch"
        );
        assert!(
            close(u_brk.quality_fit, c_brk.quality_fit),
            "breakdown.quality_fit mismatch"
        );
        assert!(
            close(u_brk.interaction_fit, c_brk.interaction_fit),
            "breakdown.interaction_fit mismatch"
        );
    }

    // ==================================================================
    //  Cached-channel edge cases
    // ==================================================================

    #[test]
    fn tag_similarity_cached_empty_post() {
        cached_setup!(ctx, idf, global);
        let post = make_post(make_empty_tags());
        let cached = cache_post(&post, &idf, &global);
        let sim = ctx.tag_similarity_cached(&cached);
        assert!(close(sim, 0.0), "expected 0.0 got {sim}");
    }

    #[test]
    fn interaction_fit_cached_no_feedback() {
        cached_setup!(ctx, idf, global);
        let mut tags = make_empty_tags();
        tags.artist.push("unknown".to_string());
        let post = make_post(tags);
        let cached = cache_post(&post, &idf, &global);
        let (score, veto) = ctx.interaction_fit_cached(&cached);
        assert!(
            close(score, FEEDBACK_NEUTRAL),
            "expected neutral, got {score}"
        );
        assert!(!veto, "expected no veto");
    }

    #[test]
    fn tag_relation_fit_cached_disabled() {
        cached_setup!(ctx, idf, global); // w_global=0, w_personal=0
        let mut tags = make_empty_tags();
        tags.artist.push("skeb".to_string());
        tags.character.push("cat".to_string());
        let post = make_post(tags);
        let cached = cache_post(&post, &idf, &global);
        let t = ctx.tag_relation_fit_cached(&cached);
        assert!(close(t, FEEDBACK_NEUTRAL), "expected neutral, got {t}");
    }

    #[test]
    fn quality_fit_cached_upvote_ratio() {
        let mut priors = default_priors();
        priors.quality_c = 0.5;
        priors.quality_w_relative_score = 0.0;
        priors.quality_w_relative_comments = 0.0;
        let idf = build_idf();
        let global = build_global_graph();
        let user_graph = build_user_graph();
        let profile = default_profile();
        let counts = default_tag_counts();
        let ctx = ScoringContext::new(&counts, &priors, &idf, &profile, &global, &user_graph);

        let post = Post {
            stats: Stats {
                score: Score {
                    up: 40,
                    down: 10,
                    total: 50,
                },
                fav_count: 0,
                ..Default::default()
            },
            ..make_post(make_empty_tags())
        };
        let cached = cache_post(&post, &idf, &global);
        let c = ctx.quality_fit_cached(&cached);
        assert!(close(c, 0.745), "expected ~0.745 got {c}");
    }

    // ==================================================================
    //  artist_discovery_fit_cached
    // ==================================================================

    #[test]
    fn artist_discovery_fit_cached_disabled_by_default() {
        let priors = default_priors();
        let idf = build_idf();
        let global = build_global_graph();
        let user_graph = build_user_graph();
        let profile = default_profile();
        let counts = default_tag_counts();
        let ctx = ScoringContext::new(&counts, &priors, &idf, &profile, &global, &user_graph);
        let mut tags = make_empty_tags();
        tags.artist.push("unknown_artist".to_string());
        let post = make_post(tags);
        let cached = cache_post(&post, &idf, &global);
        let score = ctx.artist_discovery_fit_cached(&cached);
        assert!(close(score, 0.0), "expected 0.0 got {score}");
    }

    #[test]
    fn artist_discovery_fit_cached_new_artist() {
        let mut priors = default_priors();
        priors.mix_artist_discovery = 1.0;
        let idf = build_idf();
        let global = build_global_graph();
        let user_graph = build_user_graph();
        let profile = default_profile();
        let counts = default_tag_counts();
        let ctx = ScoringContext::new(&counts, &priors, &idf, &profile, &global, &user_graph);
        let mut tags = make_empty_tags();
        tags.artist.push("new_artist".to_string());
        tags.character.push("cat".to_string());
        tags.general.push("furry".to_string());
        let post = make_post(tags);
        let cached = cache_post(&post, &idf, &global);
        let score = ctx.artist_discovery_fit_cached(&cached);
        assert!(
            score > 0.0,
            "expected positive score for new artist, got {score}"
        );
    }

    #[test]
    fn artist_discovery_fit_cached_known_artist_zero() {
        let mut priors = default_priors();
        priors.mix_artist_discovery = 1.0;
        let idf = build_idf();
        let global = build_global_graph();
        let user_graph = build_user_graph();
        let profile = default_profile();
        let counts = default_tag_counts();
        let ctx = ScoringContext::new(&counts, &priors, &idf, &profile, &global, &user_graph);
        let mut tags = make_empty_tags();
        tags.artist.push("skeb".to_string());
        tags.character.push("cat".to_string());
        let post = make_post(tags);
        let cached = cache_post(&post, &idf, &global);
        let score = ctx.artist_discovery_fit_cached(&cached);
        assert!(
            close(score, 0.0),
            "expected 0.0 for known artist, got {score}"
        );
    }

    #[test]
    fn artist_discovery_fit_cached_no_artist_tags() {
        let mut priors = default_priors();
        priors.mix_artist_discovery = 1.0;
        let idf = build_idf();
        let global = build_global_graph();
        let user_graph = build_user_graph();
        let profile = default_profile();
        let counts = default_tag_counts();
        let ctx = ScoringContext::new(&counts, &priors, &idf, &profile, &global, &user_graph);
        let post = make_post(make_empty_tags());
        let cached = cache_post(&post, &idf, &global);
        let score = ctx.artist_discovery_fit_cached(&cached);
        assert!(
            close(score, 0.0),
            "expected 0.0 for no artist tags, got {score}"
        );
    }
}
