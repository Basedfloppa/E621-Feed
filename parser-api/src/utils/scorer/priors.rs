//! Scoring priors loaded from `[priors]` in `config.toml`. The struct carries
//! `#[serde(default)]`, so the section may be partial: any field left out takes
//! [`Priors::default`], whose values reproduce the legacy production
//! behaviour.
//!
//! Knob classes added in v5.2 / v5.3 are grouped at the end with inline
//! markers (`Class A` … `Class E`) so calibrate's grid is easy to mirror.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
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
    /// Meta is excluded from `tag_similarity` / `tag_relation`; only counts here.
    #[serde(default = "default_meta_interaction_weight")]
    pub meta_interaction_weight: f32,
    #[serde(default = "default_coldstart_n0")]
    pub coldstart_n0: f32,
    #[serde(default = "default_discrete_pref_floor")]
    pub discrete_pref_floor: f32,
    #[serde(default = "default_diversity_max_penalty")]
    pub diversity_max_penalty: f32,
    #[serde(default = "default_diversity_interaction_damp")]
    pub diversity_interaction_damp: f32,

    // ---- v5.2: IDF/freq promoted from top-level Config ----
    #[serde(default = "default_df_floor")]
    pub df_floor: f32,
    #[serde(default = "default_idf_max")]
    pub idf_max: f32,
    /// BM25 saturation `k`: `tf * (k+1) / (tf + k)`. Lower = saturates faster.
    #[serde(default = "default_bm25_k")]
    pub bm25_k: f32,
    /// Exponent in `one_sided_ratio`. 0.5 = legacy sqrt; 1.0 = linear.
    #[serde(default = "default_one_sided_ratio_exp")]
    pub one_sided_ratio_exp: f32,

    // ---- v5.3 Class A: previously hardcoded constants ----
    /// Multiplier on extra Laplace smoothing for cold profiles in
    /// `rating_fit` / `media_fit`. Was hardcoded at 2.0 pre-v5.3.
    #[serde(default = "default_coldstart_smoothing_boost")]
    pub coldstart_smoothing_boost: f32,
    /// Bayesian-prior strength for per-tag CTR. Was 4.0 pre-v5.3.
    #[serde(default = "default_interaction_ctr_prior_alpha")]
    pub interaction_ctr_prior_alpha: f32,
    /// Robertson-Sparck-Jones IDF smoothing `s` in `(n - dfp + s) / (dfp + s)`.
    /// Was hardcoded at 0.5 pre-v5.3.
    #[serde(default = "default_idf_rsj_smoothing")]
    pub idf_rsj_smoothing: f32,

    // ---- v5.3 Class B: per-group multipliers (replaced [group_weights]) ----
    #[serde(default = "default_group_w_artist")]
    pub group_w_artist: f32,
    #[serde(default = "default_group_w_character")]
    pub group_w_character: f32,
    #[serde(default = "default_group_w_copyright")]
    pub group_w_copyright: f32,
    #[serde(default = "default_group_w_species")]
    pub group_w_species: f32,
    #[serde(default = "default_group_w_general")]
    pub group_w_general: f32,
    #[serde(default = "default_group_w_lore")]
    pub group_w_lore: f32,

    // ---- v5.3 Class C: algorithmic shape (defaults are no-ops) ----
    /// `final = sigmoid((score - 0.5) * temperature)` if T > 0; 0 = identity.
    #[serde(default = "default_score_temperature")]
    pub score_temperature: f32,
    /// Exponent `p` in `n^p / (n^p + n0^p)`. p=1 → legacy linear curve.
    #[serde(default = "default_confidence_steepness")]
    pub confidence_steepness: f32,
    /// Exponent on redundancy in MMR penalty: `redundancy^p × gap`. p=1 = legacy.
    #[serde(default = "default_mmr_redundancy_exp")]
    pub mmr_redundancy_exp: f32,
    /// Blend on Jaccard fallback in `tag_similarity`. 0 = pure cosine.
    #[serde(default = "default_tag_sim_jaccard_blend")]
    pub tag_sim_jaccard_blend: f32,

    // ---- v5.3 Class D: point splits (NaN sentinel = "track parent") ----
    #[serde(default = "default_split_disabled")]
    pub idf_lambda_meta: f32,
    #[serde(default = "default_split_disabled")]
    pub tag_relation_pmi_scale_user: f32,
    /// 2-piece exponential kernel: posts younger than `recency_split_age_days`
    /// use this τ; older fall back to `recency_tau_days`.
    #[serde(default = "default_split_disabled")]
    pub recency_tau_recent: f32,
    #[serde(default = "default_recency_split_age_days")]
    pub recency_split_age_days: f32,

    // ---- v5.3 Class E: categorical ----
    /// "mean" | "max" | "geomean".
    #[serde(default = "default_tag_relation_pair_aggregator")]
    pub tag_relation_pair_aggregator: String,

    // ---- Class F: quality upvote-ratio component ----
    /// Weight for upvote-ratio component in `quality_fit`.
    /// `up / (up + down)` is blended in with this weight.
    /// 0 = disabled (legacy behaviour).
    #[serde(default = "default_quality_c")]
    pub quality_c: f32,

    // ---- Class F: 3-piece recency kernel ----
    /// 3rd τ for posts younger than `recency_split_age_hours`.
    /// NaN = disabled (falls back to 2-piece `recency_tau_recent` / `recency_tau_days`).
    #[serde(default = "default_recency_tau_hot")]
    pub recency_tau_hot: f32,
    /// Age boundary in hours between the "hot" and "recent" pieces.
    /// Only used when `recency_tau_hot` is not NaN.
    /// Default 24.0 = posts under 1 day get the hot kernel.
    #[serde(default = "default_recency_split_age_hours")]
    pub recency_split_age_hours: f32,

    // ---- Class F: per-group diversity weights (copyright / species) ----
    /// Jaccard weight for copyright tags in MMR redundancy.
    #[serde(default = "default_diversity_w_copyright")]
    pub diversity_w_copyright: f32,
    /// Jaccard weight for species tags in MMR redundancy.
    #[serde(default = "default_diversity_w_species")]
    pub diversity_w_species: f32,

    // ---- Class F: exploration ----
    /// ε-greedy exploration bonus. 0 = disabled (pure exploit).
    /// Applied post-scoring as `score += ε * (1 - tag_similarity)`.
    #[serde(default = "default_exploration_epsilon")]
    pub exploration_epsilon: f32,

    // ---- Class G: Cluster-PMI tag limit ----
    /// Maximum number of tags to consider in `tag_relation_fit`'s O(T²) loop.
    /// Tags are sorted by group weight and only the top K are used, reducing
    /// complexity from O(T²) to O(K²). Default 20 → 190 pairs vs 1225 at T=50.
    /// 0 = no limit (legacy behaviour, potentially slower).
    #[serde(default = "default_tag_relation_max_tags")]
    pub tag_relation_max_tags: usize,

    // ---- Class H: uploader quality channel ----
    /// Mix weight for the uploader quality channel in the final blend.
    /// 0 = disabled.
    #[serde(default = "default_mix_uploader")]
    pub mix_uploader: f32,
    /// Cold-start threshold: how many posts from an uploader are needed
    /// before the signal reaches 50% confidence.
    #[serde(default = "default_uploader_n0")]
    pub uploader_n0: f32,
    /// Weight of the `avg_score` component inside the uploader channel.
    #[serde(default = "default_uploader_w_avg_score")]
    pub uploader_w_avg_score: f32,
    /// Weight of the `avg_fav` component inside the uploader channel.
    #[serde(default = "default_uploader_w_avg_fav")]
    pub uploader_w_avg_fav: f32,

    // ---- Class I: tag exclusivity channel ----
    /// Mix weight for the tag exclusivity channel. 0 = disabled.
    #[serde(default = "default_mix_exclusivity")]
    pub mix_exclusivity: f32,
    /// Minimum co-occurrence count for a pair to be considered "not rare".
    /// Pairs with cooc < `min_exclusivity_cooc` get full exclusivity credit.
    #[serde(default = "default_min_exclusivity_cooc")]
    pub min_exclusivity_cooc: i64,
    /// Scale factor for the exclusivity sigmoid. Larger = sharper threshold.
    #[serde(default = "default_exclusivity_scale")]
    pub exclusivity_scale: f32,
    /// Maximum tags to consider in the O(T²) exclusivity loop (0 = no limit).
    #[serde(default = "default_exclusivity_max_tags")]
    pub exclusivity_max_tags: usize,
    /// Relative weight of cross-group co-occurrence vs within-group in the
    /// exclusivity channel. The effective cross-group weight is
    /// `cross_group_weight / (cross_group_weight + 1.0)`. Default 0.5 means
    /// cross-group pairs contribute ~⅓ of total weight (50% of within-group
    /// pairs). Higher values give more credit to rare multi-group tag combos.
    #[serde(default = "default_exclusivity_cross_group_weight")]
    pub exclusivity_cross_group_weight: f32,

    // ---- Class I: tag novelty channel ----
    /// Mix weight for the tag novelty channel. 0 = disabled.
    #[serde(default = "default_mix_novelty")]
    pub mix_novelty: f32,
    /// Cold-start: how many impressions needed for 50% confidence that a tag
    /// is "not novel" to the user.
    #[serde(default = "default_novelty_n0")]
    pub novelty_n0: f32,
    /// When true, uses feedback `impression_count`; when false, only checks
    /// whether the tag exists in the user's tag-counts (favourites).
    #[serde(default = "default_novelty_use_feedback")]
    pub novelty_use_feedback: bool,

    // ---- Class K: artist discovery channel ----
    /// Mix weight for the artist discovery channel. 0 = disabled.
    #[serde(default = "default_mix_artist_discovery")]
    pub mix_artist_discovery: f32,
    /// Cold-start: how many artist tags from a new artist are needed before
    /// 50% confidence that the recommendation signal is meaningful.
    #[serde(default = "default_artist_discovery_n0")]
    pub artist_discovery_n0: f32,
    /// Bonus multiplier for artists the user has never seen before.
    #[serde(default = "default_artist_discovery_novelty_bonus")]
    pub artist_discovery_novelty_bonus: f32,

    // ---- Class J: diversity semantic similarity ----
    /// Blend between Jaccard and PMI-based semantic similarity in MMR.
    /// 0.0 = pure Jaccard (legacy behaviour).
    #[serde(default = "default_diversity_semantic_blend")]
    pub diversity_semantic_blend: f32,
    /// Minimum PMI threshold for a tag pair to count as a "semantic match"
    /// in MMR similarity. Higher = only strongly-associated pairs count.
    #[serde(default = "default_diversity_pmi_threshold")]
    pub diversity_pmi_threshold: f32,
    /// Maximum tags per group for PMI-based semantic similarity in MMR
    /// (0 = no limit, though O(T²) cost scales quadratically).
    #[serde(default = "default_diversity_semantic_max_tags")]
    pub diversity_semantic_max_tags: usize,
    /// When `diversity_semantic_blend > 0`, multiply user-graph PMI by this
    /// factor over global-graph PMI. Values > 1.0 amplify per-user diversity
    /// personalization (a user who co-favorites `skeb`+`canine` gets less
    /// MMR penalty for those tags together). 0 = disable user-graph entirely
    /// even when `diversity_semantic_blend > 0`.
    #[serde(default = "default_diversity_user_pmi_weight")]
    pub diversity_user_pmi_weight: f32,

    // ---- Class L: production MMR head budget ----
    /// How many top-by-score posts take part in the O(n²) MMR pass.
    /// `0` (the default) keeps the legacy behaviour: the whole candidate
    /// list. Everything past the head keeps its raw-score order, so the
    /// bound only trims the pairwise-redundancy matrix, which dominates the
    /// cost of the production diversity pass.
    ///
    /// Only `diversify_scored_posts` (the `/recommendations` path) reads
    /// this. The offline grid derives its own head from top-K so that probes
    /// stay comparable across grid widths.
    #[serde(default = "default_diversity_head_limit")]
    pub diversity_head_limit: usize,
}

/// Every field's default, in one place. The per-field `#[serde(default = …)]`
/// attributes take precedence where they exist; this impl fills the fields that
/// have none, and is what makes a partial `[priors]` section deserialisable at
/// all — before it, the 25 fields without an attribute (starting with `now`)
/// made every partial section fail to parse.
///
/// `now` is the evaluation clock rather than a knob: it defaults to
/// deserialisation time so a config that omits it still works.
impl Default for Priors {
    fn default() -> Self {
        Self {
            now: Utc::now(),
            recency_tau_days: 10.0,
            quality_a: 0.5,
            quality_b: 0.2,
            quality_log_bias: -3.0,
            mix_sim: 0.72,
            mix_quality: 0.02,
            mix_recency: 0.02,
            mix_rating: 0.04,
            mix_media: 0.05,
            mix_popularity: 0.02,
            mix_interaction: 0.10,
            mix_tag_relation: 0.08,
            mix_uploader: 0.05,
            mix_exclusivity: 0.0,
            mix_novelty: 0.0,
            idf_lambda: 1.0,
            idf_alpha: 1.05,
            freq_alpha: 0.95,
            df_floor: 0.4,
            idf_max: 100.0,
            bm25_k: 2.25,
            idf_rsj_smoothing: 0.35,
            one_sided_ratio_exp: 0.5,
            quality_w_absolute: 0.55,
            quality_w_relative_score: 0.30,
            quality_w_relative_comments: 0.15,
            quality_c: 0.3,
            recency_w_global: 0.4,
            recency_w_personal: 0.6,
            recency_personal_floor_frac: 1.0,
            recency_log_personal: true,
            popularity_w_fav: 0.8,
            popularity_w_duration: 0.2,
            tag_relation_w_global: 0.4,
            tag_relation_w_personal: 0.6,
            tag_relation_pmi_scale: 3.5,
            tag_relation_min_cooc: 2,
            tag_relation_user_min_cooc: 1,
            tag_relation_cooc_ref: 16.0,
            tag_relation_user_cooc_ref: 5.0,
            tag_relation_max_tags: 20,
            group_w_artist: 2.4,
            group_w_character: 2.0,
            group_w_copyright: 1.45,
            group_w_species: 1.3,
            group_w_general: 0.7,
            group_w_lore: 0.4,
            coldstart_smoothing_boost: 2.0,
            interaction_ctr_prior_alpha: 4.0,
            coldstart_n0: 25.0,
            discrete_pref_floor: 0.05,
            diversity_window: 32,
            diversity_w_artist: 0.22,
            diversity_w_character: 0.16,
            diversity_w_copyright: 1.8,
            diversity_w_species: 1.5,
            diversity_w_general: 0.08,
            diversity_max_penalty: 0.45,
            diversity_interaction_damp: 0.35,
            strong_negative_count: 3,
            strong_negative_penalty: 0.4,
            strong_negative_wilson_threshold: 0.55,
            discrete_smoothing_alpha: 1.0,
            feedback_decay_half_life_days: 90.0,
            meta_interaction_weight: 0.3,
            tag_relation_pair_aggregator: "mean".to_string(),
            score_temperature: 0.0,
            confidence_steepness: 1.0,
            mmr_redundancy_exp: 1.0,
            tag_sim_jaccard_blend: 0.0,
            idf_lambda_meta: f32::NAN,
            tag_relation_pmi_scale_user: f32::NAN,
            recency_tau_recent: f32::NAN,
            recency_split_age_days: 30.0,
            recency_tau_hot: f32::NAN,
            recency_split_age_hours: 24.0,
            exploration_epsilon: 0.0,
            uploader_n0: 5.0,
            uploader_w_avg_score: 0.6,
            uploader_w_avg_fav: 0.4,
            min_exclusivity_cooc: 2,
            exclusivity_scale: 0.5,
            exclusivity_max_tags: 15,
            novelty_n0: 3.0,
            novelty_use_feedback: true,
            diversity_semantic_blend: 0.0,
            diversity_pmi_threshold: 0.0,
            diversity_semantic_max_tags: 10,
            diversity_user_pmi_weight: 1.0,
            diversity_head_limit: 0,
            exclusivity_cross_group_weight: 0.5,
            mix_artist_discovery: 0.0,
            artist_discovery_n0: 3.0,
            artist_discovery_novelty_bonus: 0.2,
        }
    }
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
fn default_strong_negative_penalty() -> f32 {
    0.40
}
fn default_recency_personal_floor_frac() -> f32 {
    1.0
}
fn default_mix_tag_relation() -> f32 {
    0.08
}
fn default_tag_relation_w_global() -> f32 {
    0.4
}
fn default_tag_relation_w_personal() -> f32 {
    0.6
}
fn default_tag_relation_pmi_scale() -> f32 {
    3.5
}
fn default_tag_relation_min_cooc() -> i64 {
    2
}
fn default_tag_relation_user_min_cooc() -> i64 {
    1
}
fn default_tag_relation_cooc_ref() -> f32 {
    16.0
}
fn default_tag_relation_user_cooc_ref() -> f32 {
    5.0
}
fn default_strong_negative_wilson_threshold() -> f32 {
    0.55
}
fn default_recency_log_personal() -> bool {
    true
}
fn default_feedback_decay_half_life_days() -> f32 {
    90.0
}
fn default_meta_interaction_weight() -> f32 {
    0.3
}
fn default_coldstart_n0() -> f32 {
    25.0
}
fn default_discrete_pref_floor() -> f32 {
    0.05
}
fn default_diversity_max_penalty() -> f32 {
    0.45
}
fn default_diversity_interaction_damp() -> f32 {
    0.35
}
fn default_df_floor() -> f32 {
    0.4
}
fn default_idf_max() -> f32 {
    100.0
}
fn default_bm25_k() -> f32 {
    2.25
}
fn default_one_sided_ratio_exp() -> f32 {
    0.5
}
fn default_coldstart_smoothing_boost() -> f32 {
    2.0
}
fn default_interaction_ctr_prior_alpha() -> f32 {
    4.0
}
fn default_idf_rsj_smoothing() -> f32 {
    0.35
}
fn default_group_w_artist() -> f32 {
    2.40
}
fn default_group_w_character() -> f32 {
    2.00
}
fn default_group_w_copyright() -> f32 {
    1.45
}
fn default_group_w_species() -> f32 {
    1.30
}
fn default_group_w_general() -> f32 {
    0.70
}
fn default_group_w_lore() -> f32 {
    0.40
}
fn default_score_temperature() -> f32 {
    0.0
}
fn default_confidence_steepness() -> f32 {
    1.0
}
fn default_mmr_redundancy_exp() -> f32 {
    1.0
}
fn default_tag_sim_jaccard_blend() -> f32 {
    0.0
}
fn default_split_disabled() -> f32 {
    f32::NAN
}
fn default_recency_split_age_days() -> f32 {
    30.0
}
fn default_tag_relation_pair_aggregator() -> String {
    "mean".to_string()
}
fn default_quality_c() -> f32 {
    0.3
}
fn default_recency_tau_hot() -> f32 {
    f32::NAN
}
fn default_recency_split_age_hours() -> f32 {
    24.0
}
fn default_diversity_w_copyright() -> f32 {
    1.8
}
fn default_diversity_w_species() -> f32 {
    1.5
}
fn default_exploration_epsilon() -> f32 {
    0.0
}
fn default_tag_relation_max_tags() -> usize {
    20
}
fn default_mix_uploader() -> f32 {
    0.05
}
fn default_uploader_n0() -> f32 {
    5.0
}
fn default_uploader_w_avg_score() -> f32 {
    0.6
}
fn default_uploader_w_avg_fav() -> f32 {
    0.4
}

// ---- Class I defaults ----
fn default_mix_exclusivity() -> f32 {
    0.0
}
fn default_min_exclusivity_cooc() -> i64 {
    2
}
fn default_exclusivity_scale() -> f32 {
    0.5
}
fn default_exclusivity_max_tags() -> usize {
    15
}
fn default_exclusivity_cross_group_weight() -> f32 {
    0.5
}
fn default_mix_novelty() -> f32 {
    0.0
}
fn default_novelty_n0() -> f32 {
    3.0
}
fn default_novelty_use_feedback() -> bool {
    true
}

// ---- Class J defaults ----
fn default_diversity_semantic_blend() -> f32 {
    0.0
}
fn default_diversity_pmi_threshold() -> f32 {
    0.0
}
fn default_diversity_semantic_max_tags() -> usize {
    10
}
fn default_diversity_user_pmi_weight() -> f32 {
    1.0
}

// ---- Class K defaults ----
fn default_mix_artist_discovery() -> f32 {
    0.0
}
fn default_artist_discovery_n0() -> f32 {
    3.0
}
fn default_artist_discovery_novelty_bonus() -> f32 {
    0.2
}

// ---- Class L defaults ----
fn default_diversity_head_limit() -> usize {
    0
}

#[cfg(test)]
mod tests {
    use super::Priors;

    /// `Priors::default()` and the per-field `#[serde(default = …)]` attributes
    /// are two encodings of the same defaults. An empty override is
    /// deserialised through the attributes (and through `Default` for the
    /// fields that have none), so comparing the two serialised forms proves
    /// they agree on every field at once. Drift here is how a partial
    /// `[priors]` section silently stops parsing.
    #[test]
    fn serde_defaults_match_priors_default() {
        let from_serde: Priors =
            serde_json::from_value(serde_json::json!({})).expect("empty object is a valid Priors");
        // `now` is the evaluation clock: each path captured it at a different
        // instant, so only this field is allowed to differ.
        let from_default = Priors {
            now: from_serde.now,
            ..Priors::default()
        };

        assert_eq!(
            serde_json::to_value(&from_serde).unwrap(),
            serde_json::to_value(&from_default).unwrap(),
            "Priors::default() must agree with the serde defaults"
        );
    }
}
