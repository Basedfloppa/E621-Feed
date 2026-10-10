//! Grid-search knob registry.
//!
//! `KnobSpec` covers numeric f32 knobs probed by additive deltas;
//! `CategoricalKnob` covers enum-valued knobs swept by enumeration.
//!
//! v5.3 expansions are grouped at the bottom of `GRID_KNOBS`:
//!  * Class A — formerly hardcoded constants
//!  * Class B — per-group multipliers (replaced `[group_weights]`)
//!  * Class C — algorithmic shape (no-op defaults)
//!  * Class D — point splits (NaN-sentinel disabled by default)
//!  * Class E — categorical (separate registry below)
//!
//! `invalidates` annotates which scoring channels a knob's delta
//! affects — used by [`crate::cache`] to skip recomputing unaffected
//! channels across grid probes (the dominant grid-run speedup).
//! `M_NONE` means the knob touches only the final mix-blend / score
//! shaping, so all 8 cached channels are reused as-is.

use e621_account_parser_api::utils::Priors;

use crate::cache::{
    M_ARTIST_DISCOVERY, M_BOOST, M_CONFIDENCE_DERIVED, M_DISCRETE, M_EXCLUSIVITY, M_GROUP_W,
    M_INTERACTION, M_NONE, M_NOVELTY, M_RATIO_EXP, M_RECENCY, M_SIM, M_TAG_RELATION, M_UPLOADER,
};

/// One tunable parameter for the grid search. `apply` mutates the trial
/// Priors in-place; `probes` are deltas, applied by each knob's own `apply`.
/// Most are absolute (added to the current value); the `mix_*` blend weights
/// are relative (`w *= 1 + v`) because the scorer L1-normalises that block, so
/// an absolute step there is a step in a meaningless unit — see
/// [`PROBES_MIX_REL`].
/// `invalidates` is the per-channel cache mask (see [`crate::cache`]).
/// `diversify_only` knobs only enter scoring under `--with-diversify`;
/// the grid filters them out otherwise so they don't waste probes on a
/// no-op delta.
pub(crate) struct KnobSpec {
    pub(crate) name: &'static str,
    pub(crate) apply: fn(&mut Priors, f32),
    pub(crate) probes: &'static [f32],
    pub(crate) invalidates: u16,
    pub(crate) diversify_only: bool,
}

/// v5.3 Class E: enum-valued knob swept by enumerating candidates.
pub(crate) struct CategoricalKnob {
    pub(crate) name: &'static str,
    pub(crate) candidates: &'static [&'static str],
    pub(crate) apply: fn(&mut Priors, &str),
    pub(crate) invalidates: u16,
}

/// Pairs known to move together. Greedy CD can miss synergies (boosting
/// `idf_lambda` is only fully useful with higher `idf_alpha`); a paired
/// sweep at the end catches those.
pub(crate) const PAIRED_KNOBS: &[(&str, &str)] = &[
    ("idf_lambda", "idf_alpha"),
    ("idf_lambda", "freq_alpha"),
    ("idf_alpha", "freq_alpha"),
    ("df_floor", "idf_max"),
    ("tag_relation_w_global", "tag_relation_w_personal"),
    ("recency_w_global", "recency_w_personal"),
    ("popularity_w_fav", "popularity_w_duration"),
    ("bm25_k", "freq_alpha"),
    // v5.6 additions:
    ("tag_relation_min_cooc", "tag_relation_cooc_ref"),
    ("tag_relation_user_min_cooc", "tag_relation_user_cooc_ref"), // v5.7
    ("recency_tau_recent", "recency_split_age_days"),
    // diversity-only — only swept when --with-diversify is on; the grid
    // helper skips paired probes whose components aren't in the active set.
    ("diversity_w_artist", "diversity_w_character"),
    ("diversity_w_copyright", "diversity_w_species"),
    ("diversity_w_general", "diversity_window"),
    ("diversity_max_penalty", "diversity_interaction_damp"),
    // v5.8 Class F: 3-piece recency hot kernel
    ("recency_tau_hot", "recency_split_age_hours"),
    // v5.11: exclusivity + novelty channel pairs
    ("mix_exclusivity", "exclusivity_scale"),
    ("mix_novelty", "novelty_n0"),
    // v5.11 Class J: diversity semantic similarity
    ("diversity_semantic_blend", "diversity_pmi_threshold"),
    ("diversity_semantic_blend", "diversity_semantic_max_tags"),
    // v5.12: exclusivity cross-group + user-graph diversity
    ("mix_exclusivity", "exclusivity_cross_group_weight"),
    ("diversity_semantic_blend", "diversity_user_pmi_weight"),
    // v5.13: artist discovery — mix weight + confidence
    ("mix_artist_discovery", "artist_discovery_n0"),
];

/// Per-pass scale on probe-deltas. Lets coarse direction emerge quickly
/// in pass 1 then refines in pass 2/3.
pub(crate) const PASS_SCALES: &[f32] = &[1.0, 0.5, 0.25];

/// Blend weights are ratio-valued: the scorer L1-normalises the whole `mix_*`
/// block against its own total (`MixWeights::from_priors`), so only shares
/// matter and an absolute step is meaningless. On the shipped defaults
/// (0.017–0.10) the old `-0.10` probe clamped a channel straight to zero on its
/// very first try, so the grid only ever compared "keep the default" against
/// "turn it off" and never measured the interior — every `mix_*` knob was
/// accepted exactly once, at the clamp. These probes are *relative*
/// (`w *= 1 + v`), fine-to-coarse and sign-alternating, so a single miss never
/// blocks the opposite direction; `-1.0` is the exact-off option.
const PROBES_MIX_REL: &[f32] = &[-0.25, 0.25, -0.5, 0.5, -1.0, 1.0];
const PROBES_FRACTION: &[f32] = &[-0.10, -0.05, 0.05, 0.10];
const PROBES_SMALL_FRACTION: &[f32] = &[-0.05, -0.02, 0.02, 0.05];
const PROBES_WEIGHT: &[f32] = &[-0.20, -0.10, 0.10, 0.20];
const PROBES_DAYS: &[f32] = &[-5.0, -2.0, 2.0, 5.0];
const PROBES_LOG_BIAS: &[f32] = &[-1.0, -0.5, 0.5, 1.0];
const PROBES_SMOOTHING: &[f32] = &[-0.5, -0.2, 0.2, 0.5];
const PROBES_PMI: &[f32] = &[-1.0, -0.5, 0.5, 1.0];
const PROBES_COLDSTART: &[f32] = &[-10.0, -5.0, 5.0, 10.0];
const PROBES_COOC_REF: &[f32] = &[-5.0, -2.0, 2.0, 5.0];
const PROBES_DF_FLOOR: &[f32] = &[-0.30, -0.10, 0.10, 0.30];
const PROBES_IDF_MAX: &[f32] = &[-20.0, -5.0, 5.0, 20.0];
const PROBES_BM25_K: &[f32] = &[-0.50, -0.20, 0.20, 0.50];
const PROBES_RATIO_EXP: &[f32] = &[-0.20, -0.05, 0.05, 0.20];
const PROBES_GROUP_W: &[f32] = &[-0.20, -0.10, 0.10, 0.20];
const PROBES_GROUP_W_LORE: &[f32] = &[-0.10, -0.05, 0.05, 0.10];
const PROBES_RSJ: &[f32] = &[-0.20, -0.05, 0.05, 0.20];
const PROBES_BOOST: &[f32] = &[-0.50, -0.20, 0.20, 0.50];
const PROBES_CTR_PRIOR: &[f32] = &[-1.50, -0.50, 0.50, 1.50];
const PROBES_TEMP: &[f32] = &[-0.50, 0.50, 1.50, 3.0];
const PROBES_STEEP: &[f32] = &[-0.30, -0.10, 0.10, 0.30];
const PROBES_MMR_EXP: &[f32] = &[-0.30, -0.10, 0.10, 0.30];
const PROBES_JAC_BLEND: &[f32] = &[-0.05, 0.05, 0.10, 0.20];
const PROBES_TAU_RECENT: &[f32] = &[-2.0, -1.0, 1.0, 2.0];
const PROBES_TAU_HOT: &[f32] = &[-0.5, -0.2, 0.2, 0.5];
const PROBES_HOURS: &[f32] = &[-6.0, -2.0, 2.0, 6.0];
// v5.6 additions:
const PROBES_INT_TINY: &[f32] = &[-1.0, 1.0]; // for integer thresholds
const PROBES_DIV_W: &[f32] = &[-0.05, -0.02, 0.02, 0.05];
const PROBES_DIV_PENALTY: &[f32] = &[-0.10, -0.05, 0.05, 0.10];
const PROBES_DIV_WINDOW: &[f32] = &[-8.0, -4.0, 4.0, 8.0];
const PROBES_SPLIT_AGE: &[f32] = &[-10.0, -5.0, 5.0, 10.0];

// Channel masks shared by the `quality_*` knob block. Quality-internal
// reweights only touch the quality channel; same for popularity.
const M_QUALITY: u16 = crate::cache::M_QUALITY;
const M_POPULARITY: u16 = crate::cache::M_POPULARITY;

pub(crate) const GRID_KNOBS: &[KnobSpec] = &[
    // -- mix_* (12) — final blend only, no channel recompute --
    KnobSpec {
        name: "mix_sim",
        apply: |p, v| p.mix_sim = (p.mix_sim * (1.0 + v)).max(0.0),
        probes: PROBES_MIX_REL,
        invalidates: M_NONE,
        diversify_only: false,
    },
    KnobSpec {
        name: "mix_quality",
        apply: |p, v| p.mix_quality = (p.mix_quality * (1.0 + v)).max(0.0),
        probes: PROBES_MIX_REL,
        invalidates: M_NONE,
        diversify_only: false,
    },
    KnobSpec {
        name: "mix_recency",
        apply: |p, v| p.mix_recency = (p.mix_recency * (1.0 + v)).max(0.0),
        probes: PROBES_MIX_REL,
        invalidates: M_NONE,
        diversify_only: false,
    },
    KnobSpec {
        name: "mix_rating",
        apply: |p, v| p.mix_rating = (p.mix_rating * (1.0 + v)).max(0.0),
        probes: PROBES_MIX_REL,
        invalidates: M_NONE,
        diversify_only: false,
    },
    KnobSpec {
        name: "mix_media",
        apply: |p, v| p.mix_media = (p.mix_media * (1.0 + v)).max(0.0),
        probes: PROBES_MIX_REL,
        invalidates: M_NONE,
        diversify_only: false,
    },
    KnobSpec {
        name: "mix_popularity",
        apply: |p, v| p.mix_popularity = (p.mix_popularity * (1.0 + v)).max(0.0),
        probes: PROBES_MIX_REL,
        invalidates: M_NONE,
        diversify_only: false,
    },
    KnobSpec {
        name: "mix_interaction",
        apply: |p, v| p.mix_interaction = (p.mix_interaction * (1.0 + v)).max(0.0),
        probes: PROBES_MIX_REL,
        invalidates: M_NONE,
        diversify_only: false,
    },
    KnobSpec {
        name: "mix_tag_relation",
        apply: |p, v| p.mix_tag_relation = (p.mix_tag_relation * (1.0 + v)).max(0.0),
        probes: PROBES_MIX_REL,
        invalidates: M_NONE,
        diversify_only: false,
    },
    KnobSpec {
        name: "mix_uploader",
        apply: |p, v| p.mix_uploader = (p.mix_uploader * (1.0 + v)).max(0.0),
        probes: PROBES_MIX_REL,
        invalidates: M_NONE,
        diversify_only: false,
    },
    KnobSpec {
        name: "mix_exclusivity",
        apply: |p, v| p.mix_exclusivity = (p.mix_exclusivity * (1.0 + v)).max(0.0),
        probes: PROBES_MIX_REL,
        invalidates: M_NONE,
        diversify_only: false,
    },
    KnobSpec {
        name: "mix_novelty",
        apply: |p, v| p.mix_novelty = (p.mix_novelty * (1.0 + v)).max(0.0),
        probes: PROBES_MIX_REL,
        invalidates: M_NONE,
        diversify_only: false,
    },
    // -- Artist discovery knob (only mix weight here — other priors below) --
    KnobSpec {
        name: "mix_artist_discovery",
        apply: |p, v| p.mix_artist_discovery = (p.mix_artist_discovery * (1.0 + v)).max(0.0),
        probes: PROBES_MIX_REL,
        invalidates: M_NONE,
        diversify_only: false,
    },
    // -- IDF / freq shaping (7) — all touch tag_similarity --
    KnobSpec {
        name: "idf_lambda",
        apply: |p, v| p.idf_lambda = (p.idf_lambda + v).clamp(0.0, 1.5),
        probes: PROBES_FRACTION,
        invalidates: M_SIM,
        diversify_only: false,
    },
    KnobSpec {
        name: "idf_alpha",
        apply: |p, v| p.idf_alpha = (p.idf_alpha + v).clamp(0.0, 1.5),
        probes: PROBES_FRACTION,
        invalidates: M_SIM,
        diversify_only: false,
    },
    KnobSpec {
        name: "freq_alpha",
        apply: |p, v| p.freq_alpha = (p.freq_alpha + v).clamp(0.0, 1.5),
        probes: PROBES_FRACTION,
        invalidates: M_SIM,
        diversify_only: false,
    },
    KnobSpec {
        name: "df_floor",
        apply: |p, v| p.df_floor = (p.df_floor + v).clamp(0.05, 5.0),
        probes: PROBES_DF_FLOOR,
        invalidates: M_SIM,
        diversify_only: false,
    },
    KnobSpec {
        name: "idf_max",
        apply: |p, v| p.idf_max = (p.idf_max + v).clamp(1.0, 200.0),
        probes: PROBES_IDF_MAX,
        invalidates: M_SIM,
        diversify_only: false,
    },
    KnobSpec {
        name: "bm25_k",
        apply: |p, v| p.bm25_k = (p.bm25_k + v).clamp(0.1, 10.0),
        probes: PROBES_BM25_K,
        invalidates: M_SIM,
        diversify_only: false,
    },
    KnobSpec {
        name: "one_sided_ratio_exp",
        apply: |p, v| p.one_sided_ratio_exp = (p.one_sided_ratio_exp + v).clamp(0.05, 3.0),
        probes: PROBES_RATIO_EXP,
        invalidates: M_RATIO_EXP,
        diversify_only: false,
    },
    // -- Quality channel (6) --
    KnobSpec {
        name: "quality_a",
        apply: |p, v| p.quality_a = (p.quality_a + v).max(0.0),
        probes: PROBES_SMALL_FRACTION,
        invalidates: M_QUALITY,
        diversify_only: false,
    },
    KnobSpec {
        name: "quality_b",
        apply: |p, v| p.quality_b = (p.quality_b + v).max(0.0),
        probes: PROBES_SMALL_FRACTION,
        invalidates: M_QUALITY,
        diversify_only: false,
    },
    KnobSpec {
        name: "quality_log_bias",
        apply: |p, v| p.quality_log_bias += v,
        probes: PROBES_LOG_BIAS,
        invalidates: M_QUALITY,
        diversify_only: false,
    },
    KnobSpec {
        name: "quality_w_absolute",
        apply: |p, v| p.quality_w_absolute = (p.quality_w_absolute + v).max(0.0),
        probes: PROBES_WEIGHT,
        invalidates: M_QUALITY,
        diversify_only: false,
    },
    KnobSpec {
        name: "quality_w_relative_score",
        apply: |p, v| p.quality_w_relative_score = (p.quality_w_relative_score + v).max(0.0),
        probes: PROBES_WEIGHT,
        invalidates: M_QUALITY,
        diversify_only: false,
    },
    KnobSpec {
        name: "quality_w_relative_comments",
        apply: |p, v| p.quality_w_relative_comments = (p.quality_w_relative_comments + v).max(0.0),
        probes: PROBES_WEIGHT,
        invalidates: M_QUALITY,
        diversify_only: false,
    },
    // -- v5.8 Class F: quality upvote-ratio component --
    KnobSpec {
        name: "quality_c",
        apply: |p, v| p.quality_c = (p.quality_c + v).clamp(0.0, 1.0),
        probes: PROBES_SMALL_FRACTION,
        invalidates: M_QUALITY,
        diversify_only: false,
    },
    // -- Recency channel (4) --
    KnobSpec {
        name: "recency_tau_days",
        apply: |p, v| p.recency_tau_days = (p.recency_tau_days + v).max(1.0),
        probes: PROBES_DAYS,
        invalidates: M_RECENCY,
        diversify_only: false,
    },
    KnobSpec {
        name: "recency_w_global",
        apply: |p, v| p.recency_w_global = (p.recency_w_global + v).max(0.0),
        probes: PROBES_WEIGHT,
        invalidates: M_RECENCY,
        diversify_only: false,
    },
    KnobSpec {
        name: "recency_w_personal",
        apply: |p, v| p.recency_w_personal = (p.recency_w_personal + v).max(0.0),
        probes: PROBES_WEIGHT,
        invalidates: M_RECENCY,
        diversify_only: false,
    },
    KnobSpec {
        name: "recency_personal_floor_frac",
        apply: |p, v| {
            p.recency_personal_floor_frac = (p.recency_personal_floor_frac + v).clamp(0.0, 2.0);
        },
        probes: PROBES_SMALL_FRACTION,
        invalidates: M_RECENCY,
        diversify_only: false,
    },
    // -- Popularity channel (2) --
    KnobSpec {
        name: "popularity_w_fav",
        apply: |p, v| p.popularity_w_fav = (p.popularity_w_fav + v).max(0.0),
        probes: PROBES_WEIGHT,
        invalidates: M_POPULARITY,
        diversify_only: false,
    },
    KnobSpec {
        name: "popularity_w_duration",
        apply: |p, v| p.popularity_w_duration = (p.popularity_w_duration + v).max(0.0),
        probes: PROBES_WEIGHT,
        invalidates: M_POPULARITY,
        diversify_only: false,
    },
    // -- Uploader channel (3) --
    KnobSpec {
        name: "uploader_n0",
        apply: |p, v| p.uploader_n0 = (p.uploader_n0 + v).max(1.0),
        probes: PROBES_COLDSTART,
        invalidates: M_UPLOADER,
        diversify_only: false,
    },
    KnobSpec {
        name: "uploader_w_avg_score",
        apply: |p, v| p.uploader_w_avg_score = (p.uploader_w_avg_score + v).max(0.0),
        probes: PROBES_WEIGHT,
        invalidates: M_UPLOADER,
        diversify_only: false,
    },
    KnobSpec {
        name: "uploader_w_avg_fav",
        apply: |p, v| p.uploader_w_avg_fav = (p.uploader_w_avg_fav + v).max(0.0),
        probes: PROBES_WEIGHT,
        invalidates: M_UPLOADER,
        diversify_only: false,
    },
    // -- Exclusivity channel (3) --
    KnobSpec {
        name: "min_exclusivity_cooc",
        apply: |p, v| {
            let next = (p.min_exclusivity_cooc as f32 + v).round();
            p.min_exclusivity_cooc = next.clamp(0.0, 20.0) as i64;
        },
        probes: PROBES_INT_TINY,
        invalidates: M_EXCLUSIVITY,
        diversify_only: false,
    },
    KnobSpec {
        name: "exclusivity_scale",
        apply: |p, v| p.exclusivity_scale = (p.exclusivity_scale + v).clamp(0.1, 5.0),
        probes: PROBES_SMALL_FRACTION,
        invalidates: M_EXCLUSIVITY,
        diversify_only: false,
    },
    KnobSpec {
        name: "exclusivity_max_tags",
        apply: |p, v| {
            let next = (p.exclusivity_max_tags as f32 + v).round();
            p.exclusivity_max_tags = next.clamp(1.0, 50.0) as usize;
        },
        probes: &[-5.0, -2.0, 2.0, 5.0],
        invalidates: M_EXCLUSIVITY,
        diversify_only: false,
    },
    // -- Novelty channel (2) --
    KnobSpec {
        name: "novelty_n0",
        apply: |p, v| p.novelty_n0 = (p.novelty_n0 + v).max(1.0),
        probes: PROBES_COLDSTART,
        invalidates: M_NOVELTY,
        diversify_only: false,
    },
    // -- Artist discovery n0 (v5.13) --
    KnobSpec {
        name: "artist_discovery_n0",
        apply: |p, v| p.artist_discovery_n0 = (p.artist_discovery_n0 + v).max(1.0),
        probes: PROBES_COLDSTART,
        invalidates: M_ARTIST_DISCOVERY,
        diversify_only: false,
    },
    KnobSpec {
        name: "artist_discovery_novelty_bonus",
        apply: |p, v| {
            p.artist_discovery_novelty_bonus =
                (p.artist_discovery_novelty_bonus + v).clamp(0.0, 1.0);
        },
        probes: PROBES_SMALL_FRACTION,
        invalidates: M_ARTIST_DISCOVERY,
        diversify_only: false,
    },
    // -- Discrete preference (2) + cold-start (1) --
    KnobSpec {
        name: "discrete_smoothing_alpha",
        apply: |p, v| p.discrete_smoothing_alpha = (p.discrete_smoothing_alpha + v).max(0.0),
        probes: PROBES_SMOOTHING,
        invalidates: M_DISCRETE,
        diversify_only: false,
    },
    KnobSpec {
        name: "discrete_pref_floor",
        apply: |p, v| p.discrete_pref_floor = (p.discrete_pref_floor + v).clamp(0.0, 0.5),
        probes: PROBES_SMALL_FRACTION,
        invalidates: M_DISCRETE,
        diversify_only: false,
    },
    KnobSpec {
        name: "coldstart_n0",
        apply: |p, v| p.coldstart_n0 = (p.coldstart_n0 + v).max(1.0),
        probes: PROBES_COLDSTART,
        invalidates: M_CONFIDENCE_DERIVED,
        diversify_only: false,
    },
    // -- Tag-relation (5) --
    KnobSpec {
        name: "tag_relation_pmi_scale",
        apply: |p, v| p.tag_relation_pmi_scale = (p.tag_relation_pmi_scale + v).max(1.0),
        probes: PROBES_PMI,
        invalidates: M_TAG_RELATION,
        diversify_only: false,
    },
    KnobSpec {
        name: "tag_relation_w_global",
        apply: |p, v| p.tag_relation_w_global = (p.tag_relation_w_global + v).max(0.0),
        probes: PROBES_WEIGHT,
        invalidates: M_TAG_RELATION,
        diversify_only: false,
    },
    KnobSpec {
        name: "tag_relation_w_personal",
        apply: |p, v| p.tag_relation_w_personal = (p.tag_relation_w_personal + v).max(0.0),
        probes: PROBES_WEIGHT,
        invalidates: M_TAG_RELATION,
        diversify_only: false,
    },
    KnobSpec {
        name: "tag_relation_cooc_ref",
        apply: |p, v| p.tag_relation_cooc_ref = (p.tag_relation_cooc_ref + v).max(1.0),
        probes: PROBES_COOC_REF,
        invalidates: M_TAG_RELATION,
        diversify_only: false,
    },
    KnobSpec {
        name: "tag_relation_user_cooc_ref",
        apply: |p, v| p.tag_relation_user_cooc_ref = (p.tag_relation_user_cooc_ref + v).max(1.0),
        probes: PROBES_COOC_REF,
        invalidates: M_TAG_RELATION,
        diversify_only: false,
    },
    // -- Class G: Cluster-PMI tag limit --
    KnobSpec {
        name: "tag_relation_max_tags",
        apply: |p, v| {
            let next = (p.tag_relation_max_tags as f32 + v).round();
            p.tag_relation_max_tags = next.clamp(5.0, 50.0) as usize;
        },
        probes: &[-5.0, -2.0, 2.0, 5.0],
        invalidates: M_TAG_RELATION,
        diversify_only: false,
    },
    // -- v5.3 Class A: 3 constants previously hardcoded --
    KnobSpec {
        name: "idf_rsj_smoothing",
        apply: |p, v| p.idf_rsj_smoothing = (p.idf_rsj_smoothing + v).clamp(0.05, 5.0),
        probes: PROBES_RSJ,
        invalidates: M_SIM,
        diversify_only: false,
    },
    KnobSpec {
        name: "coldstart_smoothing_boost",
        apply: |p, v| {
            p.coldstart_smoothing_boost = (p.coldstart_smoothing_boost + v).clamp(0.0, 10.0);
        },
        probes: PROBES_BOOST,
        invalidates: M_BOOST,
        diversify_only: false,
    },
    KnobSpec {
        name: "interaction_ctr_prior_alpha",
        apply: |p, v| p.interaction_ctr_prior_alpha = (p.interaction_ctr_prior_alpha + v).max(0.1),
        probes: PROBES_CTR_PRIOR,
        invalidates: M_INTERACTION,
        diversify_only: false,
    },
    // -- v5.3 Class B: per-group multipliers (sim+interaction+tag_relation) --
    KnobSpec {
        name: "group_w_artist",
        apply: |p, v| p.group_w_artist = (p.group_w_artist + v).max(0.0),
        probes: PROBES_GROUP_W,
        invalidates: M_GROUP_W,
        diversify_only: false,
    },
    KnobSpec {
        name: "group_w_character",
        apply: |p, v| p.group_w_character = (p.group_w_character + v).max(0.0),
        probes: PROBES_GROUP_W,
        invalidates: M_GROUP_W,
        diversify_only: false,
    },
    KnobSpec {
        name: "group_w_copyright",
        apply: |p, v| p.group_w_copyright = (p.group_w_copyright + v).max(0.0),
        probes: PROBES_GROUP_W,
        invalidates: M_GROUP_W,
        diversify_only: false,
    },
    KnobSpec {
        name: "group_w_species",
        apply: |p, v| p.group_w_species = (p.group_w_species + v).max(0.0),
        probes: PROBES_GROUP_W,
        invalidates: M_GROUP_W,
        diversify_only: false,
    },
    KnobSpec {
        name: "group_w_general",
        apply: |p, v| p.group_w_general = (p.group_w_general + v).max(0.1),
        probes: PROBES_GROUP_W_LORE,
        invalidates: M_GROUP_W,
        diversify_only: false,
    },
    KnobSpec {
        name: "group_w_lore",
        apply: |p, v| p.group_w_lore = (p.group_w_lore + v).max(0.0),
        probes: PROBES_GROUP_W_LORE,
        invalidates: M_GROUP_W,
        diversify_only: false,
    },
    // -- v5.3 Class C: algorithmic shape --
    KnobSpec {
        name: "score_temperature",
        apply: |p, v| p.score_temperature = (p.score_temperature + v).max(0.0),
        probes: PROBES_TEMP,
        // Final-blend post-shape only.
        invalidates: M_NONE,
        diversify_only: false,
    },
    KnobSpec {
        name: "confidence_steepness",
        apply: |p, v| p.confidence_steepness = (p.confidence_steepness + v).clamp(0.1, 5.0),
        probes: PROBES_STEEP,
        invalidates: M_CONFIDENCE_DERIVED,
        diversify_only: false,
    },
    KnobSpec {
        name: "mmr_redundancy_exp",
        apply: |p, v| p.mmr_redundancy_exp = (p.mmr_redundancy_exp + v).clamp(0.1, 5.0),
        probes: PROBES_MMR_EXP,
        // Only matters under --with-diversify (no-op otherwise).
        invalidates: M_NONE,
        diversify_only: true,
    },
    KnobSpec {
        name: "tag_sim_jaccard_blend",
        apply: |p, v| p.tag_sim_jaccard_blend = (p.tag_sim_jaccard_blend + v).clamp(0.0, 1.0),
        probes: PROBES_JAC_BLEND,
        invalidates: M_SIM,
        diversify_only: false,
    },
    // -- v5.8 Class F: exploration bonus --
    KnobSpec {
        name: "exploration_epsilon",
        apply: |p, v| p.exploration_epsilon = (p.exploration_epsilon + v).clamp(0.0, 0.5),
        probes: PROBES_SMALL_FRACTION,
        // Final-blend post-shape only — applied after scoring.
        invalidates: M_NONE,
        diversify_only: false,
    },
    // -- v5.3 Class D: point splits (NaN sentinel = disabled; first probe seeds from parent) --
    KnobSpec {
        name: "idf_lambda_meta",
        apply: |p, v| {
            if p.idf_lambda_meta.is_nan() {
                p.idf_lambda_meta = p.idf_lambda;
            }
            p.idf_lambda_meta = (p.idf_lambda_meta + v).clamp(0.0, 1.5);
        },
        probes: PROBES_FRACTION,
        invalidates: M_SIM,
        diversify_only: false,
    },
    KnobSpec {
        name: "recency_tau_recent",
        apply: |p, v| {
            if p.recency_tau_recent.is_nan() {
                p.recency_tau_recent = p.recency_tau_days;
            }
            p.recency_tau_recent = (p.recency_tau_recent + v).max(0.5);
        },
        probes: PROBES_TAU_RECENT,
        invalidates: M_RECENCY,
        diversify_only: false,
    },
    KnobSpec {
        name: "tag_relation_pmi_scale_user",
        apply: |p, v| {
            if p.tag_relation_pmi_scale_user.is_nan() {
                p.tag_relation_pmi_scale_user = p.tag_relation_pmi_scale;
            }
            p.tag_relation_pmi_scale_user = (p.tag_relation_pmi_scale_user + v).max(1.0);
        },
        probes: PROBES_PMI,
        invalidates: M_TAG_RELATION,
        diversify_only: false,
    },
    // -- v5.8 Class F: 3-piece recency hot kernel (NaN-sentinel, probes seed from recency_tau_recent) --
    KnobSpec {
        name: "recency_tau_hot",
        apply: |p, v| {
            if p.recency_tau_hot.is_nan() {
                p.recency_tau_hot = if p.recency_tau_recent.is_nan() {
                    p.recency_tau_days
                } else {
                    p.recency_tau_recent
                };
            }
            p.recency_tau_hot = (p.recency_tau_hot + v).max(0.1);
        },
        probes: PROBES_TAU_HOT,
        invalidates: M_RECENCY,
        diversify_only: false,
    },
    KnobSpec {
        name: "recency_split_age_hours",
        apply: |p, v| p.recency_split_age_hours = (p.recency_split_age_hours + v).clamp(1.0, 168.0),
        probes: PROBES_HOURS,
        invalidates: M_RECENCY,
        diversify_only: false,
    },
    // -- v5.6: tag-relation co-occurrence thresholds --
    // (skipped: `strong_negative_*`, `feedback_decay_*`,
    //  `meta_interaction_weight` — no `feed_interactions` rows in the
    //  synthetic split; tune online once real feedback flows in.)
    // Lower clamp = 2: per-account user_relation prunes cooc<2 pairs at
    // build time (memory diet). Probing min_cooc=1 would lie — the graph
    // simply doesn't have those pairs anymore.
    KnobSpec {
        name: "tag_relation_min_cooc",
        apply: |p, v| {
            let next = (p.tag_relation_min_cooc as f32 + v).round();
            p.tag_relation_min_cooc = next.clamp(2.0, 20.0) as i64;
        },
        probes: PROBES_INT_TINY,
        invalidates: M_TAG_RELATION,
        diversify_only: false,
    },
    // v5.7: re-enabled now that fixtures carry a per-account user-graph.
    KnobSpec {
        name: "tag_relation_user_min_cooc",
        apply: |p, v| {
            let next = (p.tag_relation_user_min_cooc as f32 + v).round();
            p.tag_relation_user_min_cooc = next.clamp(2.0, 10.0) as i64;
        },
        probes: PROBES_INT_TINY,
        invalidates: M_TAG_RELATION,
        diversify_only: false,
    },
    // -- v5.6: 2-piece recency split boundary (only useful when
    //          recency_tau_recent is engaged) --
    KnobSpec {
        name: "recency_split_age_days",
        apply: |p, v| p.recency_split_age_days = (p.recency_split_age_days + v).clamp(1.0, 365.0),
        probes: PROBES_SPLIT_AGE,
        invalidates: M_RECENCY,
        diversify_only: false,
    },
    // -- v5.6: diversity / MMR (6) — gated to --with-diversify --
    KnobSpec {
        name: "diversity_window",
        apply: |p, v| {
            let next = (p.diversity_window as f32 + v).round();
            p.diversity_window = next.clamp(1.0, 256.0) as usize;
        },
        probes: PROBES_DIV_WINDOW,
        // Diversify pass reads this directly; channel cache untouched.
        invalidates: M_NONE,
        diversify_only: true,
    },
    KnobSpec {
        name: "diversity_w_artist",
        apply: |p, v| p.diversity_w_artist = (p.diversity_w_artist + v).clamp(0.0, 1.0),
        probes: PROBES_DIV_W,
        invalidates: M_NONE,
        diversify_only: true,
    },
    KnobSpec {
        name: "diversity_w_character",
        apply: |p, v| p.diversity_w_character = (p.diversity_w_character + v).clamp(0.0, 1.0),
        probes: PROBES_DIV_W,
        invalidates: M_NONE,
        diversify_only: true,
    },
    // -- v5.8 Class F: per-group diversity copyright/species --
    KnobSpec {
        name: "diversity_w_copyright",
        apply: |p, v| p.diversity_w_copyright = (p.diversity_w_copyright + v).clamp(0.0, 10.0),
        probes: PROBES_DIV_W,
        invalidates: M_NONE,
        diversify_only: true,
    },
    KnobSpec {
        name: "diversity_w_species",
        apply: |p, v| p.diversity_w_species = (p.diversity_w_species + v).clamp(0.0, 10.0),
        probes: PROBES_DIV_W,
        invalidates: M_NONE,
        diversify_only: true,
    },
    KnobSpec {
        name: "diversity_w_general",
        apply: |p, v| p.diversity_w_general = (p.diversity_w_general + v).clamp(0.0, 1.0),
        probes: PROBES_DIV_W,
        invalidates: M_NONE,
        diversify_only: true,
    },
    KnobSpec {
        name: "diversity_max_penalty",
        apply: |p, v| p.diversity_max_penalty = (p.diversity_max_penalty + v).clamp(0.0, 1.0),
        probes: PROBES_DIV_PENALTY,
        invalidates: M_NONE,
        diversify_only: true,
    },
    KnobSpec {
        name: "diversity_interaction_damp",
        apply: |p, v| {
            p.diversity_interaction_damp = (p.diversity_interaction_damp + v).clamp(0.0, 1.0);
        },
        probes: PROBES_DIV_PENALTY,
        invalidates: M_NONE,
        diversify_only: true,
    },
    // -- v5.11 Class J: diversity semantic similarity (3) --
    KnobSpec {
        name: "diversity_semantic_blend",
        apply: |p, v| p.diversity_semantic_blend = (p.diversity_semantic_blend + v).clamp(0.0, 1.0),
        probes: &[-0.10, -0.05, 0.05, 0.10],
        invalidates: M_NONE,
        diversify_only: true,
    },
    KnobSpec {
        name: "diversity_pmi_threshold",
        apply: |p, v| p.diversity_pmi_threshold = (p.diversity_pmi_threshold + v).clamp(0.0, 5.0),
        probes: &[-0.5, -0.2, 0.2, 0.5],
        invalidates: M_NONE,
        diversify_only: true,
    },
    KnobSpec {
        name: "diversity_semantic_max_tags",
        apply: |p, v| {
            let next = (p.diversity_semantic_max_tags as f32 + v).round();
            p.diversity_semantic_max_tags = next.clamp(1.0, 50.0) as usize;
        },
        probes: &[-3.0, -1.0, 1.0, 3.0],
        invalidates: M_NONE,
        diversify_only: true,
    },
    // -- v5.12: exclusivity cross-group weight + user-graph PMI amplification --
    KnobSpec {
        name: "exclusivity_cross_group_weight",
        apply: |p, v| {
            p.exclusivity_cross_group_weight =
                (p.exclusivity_cross_group_weight + v).clamp(0.1, 10.0);
        },
        probes: &[0.25, 0.5, 1.0, 2.0, 5.0],
        invalidates: M_EXCLUSIVITY,
        diversify_only: false,
    },
    KnobSpec {
        name: "diversity_user_pmi_weight",
        apply: |p, v| {
            p.diversity_user_pmi_weight = (p.diversity_user_pmi_weight + v).clamp(0.1, 10.0);
        },
        probes: &[0.5, 1.0, 2.0, 5.0],
        invalidates: M_NONE,
        diversify_only: true,
    },
];

/// `mix_*`-only subset for fast iteration. All `M_NONE` — entire grid is
/// pure final-blend reshuffling.
pub(crate) const MIX_ONLY_KNOBS: &[KnobSpec] = &[
    KnobSpec {
        name: "mix_sim",
        apply: |p, v| p.mix_sim = (p.mix_sim * (1.0 + v)).max(0.0),
        probes: PROBES_MIX_REL,
        invalidates: M_NONE,
        diversify_only: false,
    },
    KnobSpec {
        name: "mix_quality",
        apply: |p, v| p.mix_quality = (p.mix_quality * (1.0 + v)).max(0.0),
        probes: PROBES_MIX_REL,
        invalidates: M_NONE,
        diversify_only: false,
    },
    KnobSpec {
        name: "mix_recency",
        apply: |p, v| p.mix_recency = (p.mix_recency * (1.0 + v)).max(0.0),
        probes: PROBES_MIX_REL,
        invalidates: M_NONE,
        diversify_only: false,
    },
    KnobSpec {
        name: "mix_rating",
        apply: |p, v| p.mix_rating = (p.mix_rating * (1.0 + v)).max(0.0),
        probes: PROBES_MIX_REL,
        invalidates: M_NONE,
        diversify_only: false,
    },
    KnobSpec {
        name: "mix_media",
        apply: |p, v| p.mix_media = (p.mix_media * (1.0 + v)).max(0.0),
        probes: PROBES_MIX_REL,
        invalidates: M_NONE,
        diversify_only: false,
    },
    KnobSpec {
        name: "mix_popularity",
        apply: |p, v| p.mix_popularity = (p.mix_popularity * (1.0 + v)).max(0.0),
        probes: PROBES_MIX_REL,
        invalidates: M_NONE,
        diversify_only: false,
    },
    KnobSpec {
        name: "mix_interaction",
        apply: |p, v| p.mix_interaction = (p.mix_interaction * (1.0 + v)).max(0.0),
        probes: PROBES_MIX_REL,
        invalidates: M_NONE,
        diversify_only: false,
    },
    KnobSpec {
        name: "mix_tag_relation",
        apply: |p, v| p.mix_tag_relation = (p.mix_tag_relation * (1.0 + v)).max(0.0),
        probes: PROBES_MIX_REL,
        invalidates: M_NONE,
        diversify_only: false,
    },
    KnobSpec {
        name: "mix_uploader",
        apply: |p, v| p.mix_uploader = (p.mix_uploader * (1.0 + v)).max(0.0),
        probes: PROBES_MIX_REL,
        invalidates: M_NONE,
        diversify_only: false,
    },
    KnobSpec {
        name: "mix_exclusivity",
        apply: |p, v| p.mix_exclusivity = (p.mix_exclusivity * (1.0 + v)).max(0.0),
        probes: PROBES_MIX_REL,
        invalidates: M_NONE,
        diversify_only: false,
    },
    KnobSpec {
        name: "mix_novelty",
        apply: |p, v| p.mix_novelty = (p.mix_novelty * (1.0 + v)).max(0.0),
        probes: PROBES_MIX_REL,
        invalidates: M_NONE,
        diversify_only: false,
    },
    KnobSpec {
        name: "mix_artist_discovery",
        apply: |p, v| p.mix_artist_discovery = (p.mix_artist_discovery * (1.0 + v)).max(0.0),
        probes: PROBES_MIX_REL,
        invalidates: M_NONE,
        diversify_only: false,
    },
];

/// v5.3 Class E: enum-valued knob registry.
pub(crate) const CATEGORICAL_KNOBS: &[CategoricalKnob] = &[
    CategoricalKnob {
        name: "tag_relation_pair_aggregator",
        candidates: &["mean", "max", "geomean"],
        apply: |p, v| p.tag_relation_pair_aggregator = v.to_string(),
        invalidates: M_TAG_RELATION,
    },
    // v5.12: novelty channel — switch on user-feedback mode.
    // Uses feed_interactions data; skip if no synthetic feedback rows
    // exist (same rationale as feedback_decay / meta_interaction_weight).
    CategoricalKnob {
        name: "novelty_use_feedback",
        candidates: &["true", "false"],
        apply: |p, v| p.novelty_use_feedback = v == "true",
        invalidates: M_NOVELTY,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The values the grid actually starts from (`config.toml`), used here so
    /// the regression below is measured against shipped numbers rather than
    /// `Priors::default()`, which ships three of these at zero.
    fn shipped_priors() -> Priors {
        Priors {
            mix_sim: 0.603,
            mix_quality: 0.017,
            mix_recency: 0.017,
            mix_rating: 0.034,
            mix_media: 0.042,
            mix_popularity: 0.017,
            mix_interaction: 0.084,
            mix_tag_relation: 0.067,
            mix_uploader: 0.05,
            mix_exclusivity: 0.02,
            mix_novelty: 0.02,
            mix_artist_discovery: 0.03,
            ..Priors::default()
        }
    }

    fn mix_value(p: &Priors, name: &str) -> f32 {
        match name {
            "mix_sim" => p.mix_sim,
            "mix_quality" => p.mix_quality,
            "mix_recency" => p.mix_recency,
            "mix_rating" => p.mix_rating,
            "mix_media" => p.mix_media,
            "mix_popularity" => p.mix_popularity,
            "mix_interaction" => p.mix_interaction,
            "mix_tag_relation" => p.mix_tag_relation,
            "mix_uploader" => p.mix_uploader,
            "mix_exclusivity" => p.mix_exclusivity,
            "mix_novelty" => p.mix_novelty,
            "mix_artist_discovery" => p.mix_artist_discovery,
            other => panic!("{other} is not a mix_* knob"),
        }
    }

    /// Both registries, mix knobs only.
    fn mix_knobs() -> Vec<&'static KnobSpec> {
        GRID_KNOBS
            .iter()
            .chain(MIX_ONLY_KNOBS.iter())
            .filter(|k| k.name.starts_with("mix_"))
            .collect()
    }

    #[test]
    fn both_registries_still_expose_all_twelve_mix_knobs() {
        assert_eq!(
            mix_knobs().len(),
            24,
            "12 in GRID_KNOBS + 12 in MIX_ONLY_KNOBS"
        );
    }

    /// The regression this change is about. With the old absolute probe set the
    /// gentlest `mix_*` probe was `-0.10` — larger than every shipped weight
    /// (0.017 … 0.084) — so the very first probe clamped the channel to zero.
    /// The grid then only ever compared "keep the default" against "turn it
    /// off", accepted the clamp, moved on, and never measured the interior:
    /// every `mix_*` knob was accepted exactly once, at the floor.
    #[test]
    fn the_gentlest_probe_never_zeroes_a_shipped_mix_weight() {
        let gentlest = PROBES_MIX_REL[0] * PASS_SCALES[0];
        assert!(gentlest < 0.0, "the gentlest probe shrinks, got {gentlest}");
        for k in mix_knobs() {
            let mut p = shipped_priors();
            let before = mix_value(&p, k.name);
            assert!(before > 0.0, "{} ships at zero", k.name);
            (k.apply)(&mut p, gentlest);
            let after = mix_value(&p, k.name);
            assert!(
                after > 0.0,
                "{} was clamped to zero by the gentlest probe ({before} -> {after})",
                k.name
            );
            assert!(
                after < before,
                "{} should shrink under a negative probe ({before} -> {after})",
                k.name
            );
        }
    }

    #[test]
    fn no_mix_knob_still_uses_the_absolute_probe_set() {
        for k in mix_knobs() {
            assert_eq!(k.probes, PROBES_MIX_REL, "{} uses stale probes", k.name);
        }
    }

    /// Only the explicit `-1.0` probe may reach zero. Every other probe has to
    /// leave the weight strictly positive, so no single relative step can be
    /// the whole "on or off" decision the way the old `-0.10` was.
    #[test]
    fn only_the_explicit_off_probe_can_reach_zero() {
        for k in mix_knobs() {
            for &probe in k.probes {
                if probe == -1.0 {
                    continue;
                }
                let mut p = shipped_priors();
                (k.apply)(&mut p, probe);
                assert!(
                    mix_value(&p, k.name) > 0.0,
                    "{} reached zero on probe {probe}",
                    k.name
                );
            }
        }
    }

    /// The first two probes go opposite ways, so the grid's "2 consecutive
    /// misses" early exit cannot fire after a single rejection and starve the
    /// opposite direction.
    #[test]
    fn the_two_gentlest_probes_go_opposite_ways() {
        assert!(PROBES_MIX_REL[0] < 0.0, "gentlest probe shrinks");
        assert!(PROBES_MIX_REL[1] > 0.0, "second probe grows");
    }
}
