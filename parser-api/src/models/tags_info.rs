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

impl AccountTagFeedback {
    pub fn interaction_score(&self) -> f32 {
        let impressions = self.impression_count.max(0) as f32;
        let positives = self.positive_count.max(0) as f32;
        let negatives = self.negative_count.max(0) as f32;

        let strong_total = positives + negatives;
        if impressions == 0.0 && strong_total == 0.0 {
            return 0.5;
        }

        let positive_rate = (positives + 1.0) / (strong_total + 2.0);
        let exposure_penalty = if impressions > 0.0 {
            ((impressions - positives).max(0.0) / (impressions + 1.0)).min(1.0)
        } else {
            0.0
        };

        (positive_rate * (1.0 - 0.35 * exposure_penalty)).clamp(0.0, 1.0)
    }
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

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
pub struct TagRelationGraphPayload {
    pub nodes: Vec<TagRelationNode>,
    pub edges: Vec<TagRelationEdge>,
    pub catalog_post_count: i64,
    pub account_post_count: i64,
}
