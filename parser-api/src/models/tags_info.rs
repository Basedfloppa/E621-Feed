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
    pub quality: AccountQualityProfile,
    pub recency: AccountRecencyProfile,
}
