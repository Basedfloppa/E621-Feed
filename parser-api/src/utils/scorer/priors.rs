//! Scoring priors loaded from `[priors]` in `config.toml`. Every numeric
//! field carries a `#[serde(default)]` so the section can be partial; the
//! default values reproduce the legacy production behaviour.
//!
//! Knob classes added in v5.2 / v5.3 are grouped at the end with inline
//! markers (`Class A` … `Class E`) so calibrate's grid is easy to mirror.

use chrono::{DateTime, Utc};
use serde::Deserialize;

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
    /// Meta is excluded from tag_similarity / tag_relation; only counts here.
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
    /// rating_fit / media_fit. Was hardcoded at 2.0 pre-v5.3.
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
    /// Blend on Jaccard fallback in tag_similarity. 0 = pure cosine.
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
    /// Weight for upvote-ratio component in quality_fit.
    /// `up / (up + down)` is blended in with this weight.
    /// 0 = disabled (legacy behaviour).
    #[serde(default = "default_quality_c")]
    pub quality_c: f32,

    // ---- Class F: 3-piece recency kernel ----
    /// 3rd τ for posts younger than `recency_split_age_hours`.
    /// NaN = disabled (falls back to 2-piece recency_tau_recent / recency_tau_days).
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
    0.0
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
    0.20
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
