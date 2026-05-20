use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use schemars::JsonSchema;

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

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema, Default)]
pub struct AccountPreferenceProfile {
    pub rating: Vec<AccountRatingStat>,
    pub media: Vec<AccountMediaStat>,
    pub feedback: Vec<AccountTagFeedback>,
    pub quality: AccountQualityProfile,
    pub recency: AccountRecencyProfile,
    /// When the profile was last refreshed by `/process`.
    /// `None` = never refreshed (legacy accounts).
    #[serde(default)]
    pub last_refreshed_at: Option<DateTime<Utc>>,
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
    /// log1p(cooc_ref) reference for global confidence shrinkage.
    pub cooc_ref: f32,
    /// log1p(cooc_ref) reference for user confidence shrinkage.
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
