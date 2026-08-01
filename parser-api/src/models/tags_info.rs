use std::collections::HashMap;

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ── Tag aliases & implications (from e621 API) ─────────────────────────

/// A single tag alias as returned by e621's `/tag_aliases.json`.
/// Maps `antecedent_name` (wrong/deprecated) → `consequent_name` (canonical).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TagAlias {
    pub id: i64,
    pub antecedent_name: String,
    pub consequent_name: String,
    pub status: String,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

/// A single tag implication as returned by e621's `/tag_implications.json`.
/// `antecedent_name` implies `consequent_name` (if X then Y).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TagImplication {
    pub id: i64,
    pub antecedent_name: String,
    pub consequent_name: String,
    pub status: String,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

/// Response for `GET /tag_relations/resolve?tag=...`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TagResolveResponse {
    /// The original tag as queried.
    pub query: String,
    /// The canonical (resolved) tag name after following the alias chain.
    /// Same as `query` if no alias exists.
    pub canonical: String,
    /// All known synonyms (antecedents) of the canonical tag, including
    /// the canonical name itself.
    pub synonyms: Vec<String>,
}

/// Request body for `POST /tag_relations/resolve-batch`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct TagResolveBatchRequest {
    pub tags: Vec<String>,
}

/// Response for `POST /tag_relations/resolve-batch`.
/// Maps each input tag to its canonical name (same as input if no alias).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TagResolveBatchResponse {
    /// Map from original tag → canonical (resolved) tag.
    pub resolved: HashMap<String, String>,
    /// Set of all unique canonical names.
    pub canonicals: Vec<String>,
}

/// A single tag within a Taste Theme, with its count and centrality.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TasteThemeTag {
    pub name: String,
    pub count: i64,
    /// `PageRank` centrality (0..1). High = more central to the theme.
    pub centrality: f32,
}

/// One community cluster in the Taste Themes v3 pipeline.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TasteTheme {
    /// Thematic name = tag with max count among all tags in community.
    pub name: String,
    /// Core tags — high-centrality non-generic tags (centrality >= median).
    pub core: Vec<TasteThemeTag>,
    /// Kink tags — low-centrality non-generic tags (centrality <= p25).
    /// These are the "unique" aspects of this theme for this user.
    pub kink: Vec<TasteThemeTag>,
    /// TF-IDF weighted importance score.
    pub importance: f32,
    /// Total tags in this community.
    pub size: usize,
}

/// Pre-computed taste profile — returned by `GET /api/account/<id>/taste-profile`.
/// Contains only Taste Themes v3 community clusters.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TasteProfileResponse {
    /// Taste Themes v3 — community clusters from Label Propagation.
    pub themes: Vec<TasteTheme>,
}

/// Request body for `POST /tag_relations/implications-batch`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct TagImplicationsBatchRequest {
    pub tags: Vec<String>,
}

/// Response for `POST /tag_relations/implications-batch`.
/// Maps each input tag to the list of tags it implies.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TagImplicationsBatchResponse {
    /// Map from tag → list of tags it implies (all active implications).
    pub implications: HashMap<String, Vec<String>>,
}

/// Response for `GET /tag_relations/implications?tag=...`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TagImplicationsResponse {
    /// The tag that was queried.
    pub tag: String,
    /// List of tags implied by the queried tag (implications where this
    /// tag is the antecedent).
    pub implies: Vec<String>,
    /// List of tags that imply this tag (implications where this tag
    /// is the consequent).
    pub implied_by: Vec<String>,
}

#[derive(Debug, Serialize, Clone, JsonSchema)]
pub struct TagCount {
    pub name: String,
    pub group_type: String,
    pub count: i64,
}

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
pub struct AccountRatingStat {
    pub rating: String,
    pub count: i64,
}

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
pub struct AccountMediaStat {
    pub media_type: String,
    pub count: i64,
}

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema, Default)]
pub struct AccountTagFeedback {
    pub tag_name: String,
    pub group_type: String,
    pub impression_count: i64,
    pub positive_count: i64,
    pub negative_count: i64,
    pub last_interaction_at: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema, Default)]
pub struct AccountQualityProfile {
    pub avg_score_total: f32,
    pub avg_fav_count: f32,
    pub avg_comment_count: f32,
    pub avg_duration: f32,
}

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema, Default)]
pub struct AccountRecencyProfile {
    pub avg_age_days: f32,
    pub avg_abs_dev_days: f32,
}

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
pub struct AccountUploaderStat {
    pub uploader_id: i64,
    pub post_count: i64,
    pub avg_score: f32,
    pub avg_fav: f32,
}

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema, Default)]
pub struct AccountPreferenceProfile {
    pub rating: Vec<AccountRatingStat>,
    pub media: Vec<AccountMediaStat>,
    pub feedback: Vec<AccountTagFeedback>,
    pub quality: AccountQualityProfile,
    pub recency: AccountRecencyProfile,
    /// Per-uploader quality stats based on user's favourited posts.
    #[serde(default)]
    pub uploaders: Vec<AccountUploaderStat>,
    /// When the profile was last refreshed by `/process`.
    /// `None` = never refreshed (legacy accounts).
    #[serde(default)]
    pub last_refreshed_at: Option<DateTime<Utc>>,
    /// Positive preferences: tags the user wants to see more of (soft boost).
    /// Applied as IDF-weight multipliers in `ScoringContext`. Blacklist takes
    /// priority over `preferred_tags`.
    #[serde(default)]
    pub preferred_tags: Vec<crate::models::PreferredTag>,
}

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
pub struct TagRelationNode {
    pub name: String,
    pub group_type: String,
    pub count: i64,
}

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
pub struct TagRelationEdge {
    pub source: usize,
    pub target: usize,
    pub user_cooc: i64,
    pub global_cooc: i64,
    /// PMI-style global lift: cooc * N / (df1 * df2). Zero when global signal is missing.
    pub global_lift: f32,
}

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema, Default)]
pub struct TagRelationScoring {
    /// Weight for the global PMI component (mirrors `tag_relation_w_global`).
    pub w_global: f32,
    /// Weight for the personal PMI component (mirrors
    /// `tag_relation_w_personal`). Cold-start aware — for users with few
    /// favourites, the backend re-routes some of this to `w_global` before
    /// emitting the payload.
    pub w_personal: f32,
    /// PMI normalisation scale (mirrors `tag_relation_pmi_scale`). Both PMI
    /// terms divide by this before clamping to [0, 1].
    pub pmi_scale: f32,
    /// `log1p(cooc_ref)` reference for global confidence shrinkage.
    pub cooc_ref: f32,
    /// `log1p(cooc_ref)` reference for user confidence shrinkage.
    pub user_cooc_ref: f32,
    /// Minimum global co-occurrence required for a pair to contribute to the
    /// global PMI component.
    pub min_cooc_global: i64,
    /// Minimum user co-occurrence required for a pair to contribute to the
    /// personal PMI component.
    pub min_cooc_user: i64,
}

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
pub struct TagRelationGraphPayload {
    pub nodes: Vec<TagRelationNode>,
    pub edges: Vec<TagRelationEdge>,
    pub catalog_post_count: i64,
    pub account_post_count: i64,
    #[serde(default)]
    pub scoring: TagRelationScoring,
}
