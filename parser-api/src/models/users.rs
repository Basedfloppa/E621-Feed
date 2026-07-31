use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Current e621 user API format (as of 2025-07).
/// e621 now returns a unified user format through /users/{id}.json —
/// no more FullUser vs FullCurrentUser distinction.
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserApiResponse {
    FullUser(Box<E621User>),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct E621User {
    pub id: i32,
    pub created_at: DateTime<Utc>,
    pub name: String,
    pub level: i32,
    pub base_upload_limit: i32,
    pub post_upload_count: i32,
    pub post_update_count: i32,
    pub note_update_count: i32,
    pub is_banned: bool,
    pub can_approve_posts: bool,
    pub can_upload_free: bool,
    pub level_string: String,
    pub avatar_id: Option<i32>,
    pub wiki_page_version_count: i32,
    pub artist_version_count: i32,
    pub pool_version_count: i32,
    pub forum_post_count: i32,
    pub comment_count: i32,
    pub flag_count: i32,
    pub favorite_count: i32,
    pub positive_feedback_count: i32,
    pub neutral_feedback_count: i32,
    pub negative_feedback_count: i32,
    pub profile_about: String,
    pub profile_artinfo: String,
    pub is_verified: bool,
    pub has_cropped_avatar: bool,
    #[serde(default)]
    pub upload_slots: i32,
    #[serde(default)]
    pub upload_karma: i32,
    #[serde(default)]
    pub upload_karma_free: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
#[serde(crate = "rocket::serde")]
pub struct TruncatedAccount {
    pub id: i32,
    pub name: String,
    pub blacklist: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
#[serde(crate = "rocket::serde")]
pub struct DeviceScopedAccount {
    pub id: i32,
    pub name: String,
    /// Optional. `None` (or omitted from the JSON body) signals "use the
    /// server-side default `tag_blacklist` from `config.toml`". Frontend
    /// must omit the field — not send empty string — to opt into the
    /// default; an empty string is also treated as "use default" at DB
    /// write for backwards-compat with older clients.
    #[serde(default)]
    pub blacklist: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
#[serde(crate = "rocket::serde")]
pub struct BlacklistPayload {
    /// Same semantics as `DeviceScopedAccount.blacklist` — `None` or empty
    /// resets the account to the server-side default at write time.
    #[serde(default)]
    pub blacklist: Option<String>,
}

/// A single preferred tag with a boost weight for the scoring system.
/// `weight` ∈ [0.1, 10.0]; blacklist takes priority over preferred_tags.
#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
#[serde(crate = "rocket::serde")]
pub struct PreferredTag {
    pub tag: String,
    pub group: String,
    pub weight: f32,
}

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
#[serde(crate = "rocket::serde")]
pub struct PreferredTagPayload {
    /// Max 50 tags per account. Replaces the entire list on write
    /// (like blacklist).
    pub preferred_tags: Vec<PreferredTag>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct UserSearchResult {
    pub id: i32,
    pub name: String,
    #[serde(default)]
    pub level: i32,
    #[serde(default)]
    pub post_upload_count: i32,
    #[serde(default)]
    pub is_banned: bool,
}
