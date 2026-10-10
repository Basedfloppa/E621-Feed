//! Pure helpers shared between channel implementations.
//! Crate-internal — only `scorer/context.rs` and `scorer/diversify.rs` consume them.

use std::borrow::Cow;

use super::priors::Priors;

/// Mathematical 95% one-sided Wilson lower-bound z-score.
pub(super) const WILSON_Z: f32 = 1.96;
pub(super) const FEEDBACK_NEUTRAL: f32 = 0.5;

#[inline]
pub(super) fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

/// Width of a [`blend_score`] weight / channel vector.
///
/// Public because the analytic blend fit in `bin/calibrate/fit.rs` builds
/// these vectors itself. The count and the *order* are one contract: the
/// cache stores channels as named fields, and the fit flattens them with
/// [`crate::utils::ChannelScores::values`] — a new channel must be added in
/// the same position on all three sides or the fit silently optimises a
/// permutation of the blend the scorer actually runs.
pub const BLEND_CHANNEL_COUNT: usize = 12;

/// Channel order for [`blend_score`], matching the `ChannelScores` layout in
/// `bin/calibrate/cache.rs` and `MixWeights` field order.
pub const BLEND_CHANNEL_NAMES: [&str; BLEND_CHANNEL_COUNT] = [
    "sim",
    "quality",
    "recency",
    "rating",
    "media",
    "popularity",
    "interaction",
    "tag_relation",
    "uploader",
    "exclusivity",
    "novelty",
    "artist_discovery",
];

/// The final blend for an explicit weight vector:
/// `Σ w_c·s_c` → clamp → optional sigmoid sharpening → optional veto
/// penalty → clamp.
///
/// Single implementation shared by the production scorer
/// ([`crate::utils::ScoringContext::final_blend`], which derives the weights
/// from `Priors`) and the analytic mix fit (`bin/calibrate/fit.rs`, which
/// solves for the weights directly from the cached channel matrix). The fit
/// *must* optimise this exact function — a second copy would make the fitted
/// weights optimal for a different formula than the one the grid measures,
/// and the divergence would only surface as offline metrics that production
/// does not reproduce.
///
/// `w` is deliberately not normalised here: the scorer always passes simplex
/// weights (`MixWeights::from_priors`) and the fit's solver keeps them there
/// by construction. Normalisation is *not* a no-op for the score itself —
/// once `raw` is clamped, scaling the weights changes the result — so callers
/// converting `Priors` must go through [`normalised_mix_weights`].
#[must_use]
pub fn blend_score(
    w: [f32; BLEND_CHANNEL_COUNT],
    channels: [f32; BLEND_CHANNEL_COUNT],
    score_temperature: f32,
    strong_negative_penalty: f32,
    veto: bool,
) -> f32 {
    let mut raw = 0.0f32;
    for (wc, sc) in w.iter().zip(channels.iter()) {
        raw += wc * sc;
    }
    let mut score = raw.clamp(0.0, 1.0);
    // Class C v5.3: optional sigmoid sharpening on the final blend.
    if score_temperature > 1e-3 {
        score = sigmoid((score - 0.5) * score_temperature).clamp(0.0, 1.0);
    }
    if veto {
        score *= 1.0 - strong_negative_penalty.clamp(0.0, 1.0);
    }
    score.clamp(0.0, 1.0)
}

#[inline]
pub(super) fn normalize_tag(t: &str) -> Cow<'_, str> {
    let trimmed = t.trim();
    if trimmed.bytes().any(|b| b.is_ascii_uppercase()) {
        Cow::Owned(trimmed.to_ascii_lowercase())
    } else {
        Cow::Borrowed(trimmed)
    }
}

/// Maps `value/baseline` ∈ [0,∞) onto [0,1] via `min(r,1)^exp`.
/// `exp = 0.5` reproduces the legacy `.sqrt()` shape.
#[inline]
pub(super) fn one_sided_ratio(value: f32, baseline: f32, exp: f32) -> f32 {
    if baseline <= 1e-6 {
        return if value > 0.0 { 1.0 } else { FEEDBACK_NEUTRAL };
    }
    let r = value.max(0.0) / baseline;
    let exp = exp.max(1e-3);
    r.min(1.0).powf(exp).clamp(0.0, 1.0)
}

#[inline]
pub(super) fn discrete_preference_smooth(
    total: i64,
    matched: i64,
    k: usize,
    alpha: f32,
    floor: f32,
) -> f32 {
    if total <= 0 {
        return FEEDBACK_NEUTRAL;
    }
    let k = k.max(1) as f32;
    let a = alpha.max(0.0);
    let num = matched.max(0) as f32 + a;
    let den = total as f32 + a * k;
    (num / den).sqrt().clamp(floor, 1.0)
}

#[inline]
pub(super) fn blend2(a: f32, wa: f32, b: f32, wb: f32) -> f32 {
    let sum = wa + wb;
    if sum <= 0.0 {
        return 0.0;
    }
    ((wa * a + wb * b) / sum).clamp(0.0, 1.0)
}

#[inline]
pub(super) fn blend3(a: f32, wa: f32, b: f32, wb: f32, c: f32, wc: f32) -> f32 {
    let sum = wa + wb + wc;
    if sum <= 0.0 {
        return 0.0;
    }
    ((wa * a + wb * b + wc * c) / sum).clamp(0.0, 1.0)
}

/// `n^p / (n^p + n0^p)`. `p == 1.0` is the legacy linear curve;
/// >1 sharpens the warm-up; <1 flattens it (Class C v5.3).
#[inline]
pub(super) fn confidence(n: f32, n0: f32, p: f32) -> f32 {
    let nn = n.max(0.0);
    let nn0 = n0.max(1e-3);
    if (p - 1.0).abs() < 1e-3 {
        return nn / (nn + nn0);
    }
    let p = p.clamp(0.1, 5.0);
    let np = nn.powf(p);
    let n0p = nn0.powf(p);
    np / (np + n0p)
}

#[inline]
pub(super) fn wilson_lower_bound(positives: f32, total: f32, z: f32) -> f32 {
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

/// CTR with Bayesian prior pulling low-volume tags toward `p0`.
#[inline]
pub(super) fn ctr_score(pos: f32, neg: f32, imp: f32, p0: f32, alpha: f32) -> f32 {
    let strong = pos + neg;
    let exposure = (imp - pos).max(0.0);
    let denom = strong + exposure + alpha;
    if denom <= 0.0 {
        return p0.clamp(0.0, 1.0);
    }
    ((pos + alpha * p0) / denom).clamp(0.0, 1.0)
}

#[derive(Default, Clone, Copy)]
pub(super) struct CompactFeedback {
    pub positive: i64,
    pub negative: i64,
    pub impressions: i64,
    /// When this tag was last interacted with. Used for per-tag staleness
    /// decay in the interaction channel — replaces the old global
    /// `last_refreshed_at` approach so each tag's feedback age is tracked
    /// independently.
    pub last_interaction_secs: Option<f64>,
}

/// Class E v5.3: how to combine global+user PMI inside a single tag pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PairAggregator {
    Mean,
    Max,
    GeoMean,
}

impl PairAggregator {
    pub(super) fn from_str(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "max" => Self::Max,
            "geomean" | "geo_mean" => Self::GeoMean,
            _ => Self::Mean,
        }
    }
}

#[derive(Default, Clone, Copy)]
pub(super) struct MixWeights {
    pub sim: f32,
    pub quality: f32,
    pub recency: f32,
    pub rating: f32,
    pub media: f32,
    pub popularity: f32,
    pub interaction: f32,
    pub tag_relation: f32,
    pub uploader: f32,
    pub exclusivity: f32,
    pub novelty: f32,
    pub artist_discovery: f32,
}

impl MixWeights {
    pub(super) fn from_priors(p: &Priors) -> Self {
        // No sign restriction. A channel may carry anti-signal, and the offline
        // evidence for the small channels is confounded (see
        // `docs/calibration.md`), so clamping a negative weight to zero here
        // would silently narrow the model that the calibrator and the online
        // bucket overrides are able to express.
        let raw = [
            p.mix_sim,
            p.mix_quality,
            p.mix_recency,
            p.mix_rating,
            p.mix_media,
            p.mix_popularity,
            p.mix_interaction,
            p.mix_tag_relation,
            p.mix_uploader,
            p.mix_exclusivity,
            p.mix_novelty,
            p.mix_artist_discovery,
        ];
        // L1 rather than the signed sum: with mixed signs the signed sum passes
        // through zero and the shares blow up next to it, while L1 stays
        // bounded and coincides with the old normalisation whenever every
        // weight is non-negative.
        let sum: f32 = raw.iter().map(|x| x.abs()).sum();
        if sum.is_nan() || sum <= f32::EPSILON {
            return Self::default();
        }
        Self {
            sim: raw[0] / sum,
            quality: raw[1] / sum,
            recency: raw[2] / sum,
            rating: raw[3] / sum,
            media: raw[4] / sum,
            popularity: raw[5] / sum,
            interaction: raw[6] / sum,
            tag_relation: raw[7] / sum,
            uploader: raw[8] / sum,
            exclusivity: raw[9] / sum,
            novelty: raw[10] / sum,
            artist_discovery: raw[11] / sum,
        }
    }

    /// Flatten to the [`blend_score`] channel order.
    #[must_use]
    pub(super) fn as_array(self) -> [f32; BLEND_CHANNEL_COUNT] {
        [
            self.sim,
            self.quality,
            self.recency,
            self.rating,
            self.media,
            self.popularity,
            self.interaction,
            self.tag_relation,
            self.uploader,
            self.exclusivity,
            self.novelty,
            self.artist_discovery,
        ]
    }
}

/// L1-normalised blend weights implied by `priors`, in [`blend_score`] channel
/// order. Exposes [`MixWeights::from_priors`] to callers outside the scorer
/// module (the calibrate fit solver) without making `MixWeights` public.
#[must_use]
pub fn normalised_mix_weights(p: &Priors) -> [f32; BLEND_CHANNEL_COUNT] {
    MixWeights::from_priors(p).as_array()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn sigmoid_is_centred_and_monotone() {
        assert!(close(sigmoid(0.0), 0.5));
        assert!(sigmoid(20.0) > 0.99);
        assert!(sigmoid(-20.0) < 0.01);
        assert!(sigmoid(1.0) > sigmoid(-1.0));
    }

    #[test]
    fn normalize_tag_lowercases_and_trims() {
        assert_eq!(&*normalize_tag("Foo"), "foo");
        assert_eq!(&*normalize_tag("  Bar_Baz  "), "bar_baz");
        // Already-lowercase input is borrowed, not re-allocated.
        assert!(matches!(normalize_tag("foo"), Cow::Borrowed(_)));
        assert!(matches!(normalize_tag("Foo"), Cow::Owned(_)));
    }

    /// The blend block is normalised by its L1 norm, not by its signed sum, and
    /// a negative weight survives the trip. Both properties matter: the model
    /// allows a channel to carry anti-signal, and a signed-sum denominator
    /// passes through zero whenever a negative term cancels a positive one.
    #[test]
    fn from_priors_keeps_negative_weights_and_normalises_by_l1() {
        let p = Priors {
            mix_sim: 0.75,
            mix_quality: 0.0,
            mix_recency: 0.0,
            mix_rating: 0.0,
            mix_media: 0.0,
            mix_popularity: 0.0,
            mix_interaction: 0.0,
            mix_tag_relation: 0.0,
            mix_uploader: 0.0,
            mix_exclusivity: 0.0,
            mix_novelty: -0.25,
            mix_artist_discovery: 0.0,
            ..Priors::default()
        };

        let w = normalised_mix_weights(&p);
        assert!(w[10] < 0.0, "negative weight must survive, got {}", w[10]);
        assert!(close(w[0], 0.75), "sim share should be 0.75, got {}", w[0]);
        assert!(
            close(w[10], -0.25),
            "novelty share should be -0.25, got {}",
            w[10]
        );
        let l1: f32 = w.iter().map(|x| x.abs()).sum();
        assert!(close(l1, 1.0), "L1 norm should be 1, got {l1}");
    }

    #[test]
    fn one_sided_ratio_handles_zero_baseline() {
        // No baseline + positive value → full marks; no value → neutral.
        assert!(close(one_sided_ratio(3.0, 0.0, 0.5), 1.0));
        assert!(close(one_sided_ratio(0.0, 0.0, 0.5), FEEDBACK_NEUTRAL));
    }

    #[test]
    fn one_sided_ratio_caps_and_shapes() {
        // value >= baseline saturates at 1.0.
        assert!(close(one_sided_ratio(8.0, 4.0, 0.5), 1.0));
        // exp = 0.5 reproduces the sqrt shape: (1/4)^0.5 = 0.5.
        assert!(close(one_sided_ratio(1.0, 4.0, 0.5), 0.5));
    }

    #[test]
    fn discrete_preference_smooth_basics() {
        // No observations → neutral.
        assert!(close(
            discrete_preference_smooth(0, 0, 3, 1.0, 0.0),
            FEEDBACK_NEUTRAL
        ));
        // Perfect match, no smoothing → 1.0.
        assert!(close(
            discrete_preference_smooth(100, 100, 3, 0.0, 0.0),
            1.0
        ));
        // Zero matches honours the floor.
        assert!(close(discrete_preference_smooth(100, 0, 3, 0.0, 0.2), 0.2));
    }

    #[test]
    fn blend_returns_zero_on_nonpositive_weights() {
        assert_eq!(blend2(1.0, 0.0, 1.0, 0.0), 0.0);
        assert_eq!(blend3(1.0, 0.0, 1.0, 0.0, 1.0, 0.0), 0.0);
    }

    #[test]
    fn blend_is_weighted_average_clamped() {
        assert!(close(blend2(0.0, 1.0, 1.0, 1.0), 0.5));
        assert!(close(blend3(0.0, 1.0, 0.5, 1.0, 1.0, 1.0), 0.5));
        // Out-of-range inputs clamp into [0, 1].
        assert!(close(blend2(2.0, 1.0, 2.0, 1.0), 1.0));
    }

    #[test]
    fn confidence_linear_and_curved() {
        // p == 1 → n / (n + n0).
        assert!(close(confidence(10.0, 10.0, 1.0), 0.5));
        assert!(close(confidence(0.0, 10.0, 1.0), 0.0));
        // p != 1 path stays in [0, 1] and is monotone in n.
        assert!(close(confidence(10.0, 10.0, 2.0), 0.5));
        assert!(confidence(100.0, 10.0, 2.0) > confidence(1.0, 10.0, 2.0));
    }

    #[test]
    fn wilson_lower_bound_bounds_and_grows_with_evidence() {
        assert_eq!(wilson_lower_bound(0.0, 0.0, WILSON_Z), 0.0);
        let small = wilson_lower_bound(10.0, 10.0, WILSON_Z);
        let large = wilson_lower_bound(1000.0, 1000.0, WILSON_Z);
        // Same 100% observed rate, but more evidence → tighter (higher) bound.
        assert!(small > 0.0 && small < 1.0);
        assert!(large > small);
    }

    #[test]
    fn ctr_score_falls_back_to_prior_without_data() {
        assert!(close(ctr_score(0.0, 0.0, 0.0, 0.42, 0.0), 0.42));
        // 8 up / 2 down, 10 impressions → 8 / (10 + 2) = 0.667.
        assert!(close(ctr_score(8.0, 2.0, 10.0, 0.5, 0.0), 8.0 / 12.0));
    }

    #[test]
    fn pair_aggregator_parsing() {
        assert_eq!(PairAggregator::from_str("max"), PairAggregator::Max);
        assert_eq!(PairAggregator::from_str("  MAX "), PairAggregator::Max);
        assert_eq!(PairAggregator::from_str("geomean"), PairAggregator::GeoMean);
        assert_eq!(
            PairAggregator::from_str("geo_mean"),
            PairAggregator::GeoMean
        );
        assert_eq!(PairAggregator::from_str("mean"), PairAggregator::Mean);
        // Unknown strings fall back to the Mean default.
        assert_eq!(PairAggregator::from_str("nonsense"), PairAggregator::Mean);
    }
}
