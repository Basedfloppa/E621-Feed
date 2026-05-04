use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserApiResponse {
    FullCurrentUser(Box<FullCurrentUser>),
    FullUser(FullUser),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FullUser {
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
    pub upload_limit: i32,
    pub profile_about: String,
    pub profile_artinfo: String,
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
    pub blacklist: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
#[serde(crate = "rocket::serde")]
pub struct BlacklistPayload {
    pub blacklist: String,
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

#[derive(Debug, Serialize, Deserialize)]
pub struct FullCurrentUser {
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
    pub blacklist_users: bool,
    pub description_collapsed_initially: bool,
    pub hide_comments: bool,
    pub show_hidden_comments: bool,
    pub show_post_statistics: bool,
    pub receive_email_notifications: bool,
    pub enable_keyboard_navigation: bool,
    pub enable_privacy_mode: bool,
    pub style_usernames: bool,
    pub enable_auto_complete: bool,
    pub disable_cropped_thumbnails: bool,
    pub enable_safe_mode: bool,
    pub disable_responsive_mode: bool,
    pub no_flagging: bool,
    pub disable_user_dmails: bool,
    pub enable_compact_uploader: bool,
    pub replacements_beta: bool,
    pub updated_at: DateTime<Utc>,
    pub email: String,
    pub last_logged_in_at: String,
    pub last_forum_read_at: String,
    pub recent_tags: String,
    pub comment_threshold: i32,
    pub default_image_size: String, // Enum option possible
    pub favorite_tags: String,
    pub blacklisted_tags: String,
    pub time_zone: String,
    pub per_page: i32,
    pub custom_style: String,
    pub favorite_count: i32,
    pub api_regen_multiplier: i32,
    pub api_burst_limit: i32,
    pub remaining_api_limit: i32,
    pub statement_timeout: i32,
    pub favorite_limit: i32,
    pub tag_query_limit: i32,
    pub has_mail: bool,
    pub forum_notification_dot: bool,
    pub wiki_page_version_count: i32,
    pub artist_version_count: i32,
    pub pool_version_count: i32,
    pub forum_post_count: i32,
    pub comment_count: i32,
    pub flag_count: i32,
    pub positive_feedback_count: i32,
    pub neutral_feedback_count: i32,
    pub negative_feedback_count: i32,
    pub upload_limit: i32,
    pub profile_about: String,
    pub profile_artinfo: String,
}
