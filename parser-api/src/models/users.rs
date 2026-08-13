use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::models::AccountPreferenceProfile;
use crate::models::deserialize_nullable_dt;

/// Current e621 user API format (as of 2025-07).
/// e621 now returns a unified user format through /users/{id}.json —
/// no more `FullUser` vs `FullCurrentUser` distinction.
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserApiResponse {
    FullUser(Box<E621User>),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct E621User {
    pub id: i32,
    #[serde(deserialize_with = "deserialize_nullable_dt")]
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
/// `weight` ∈ [0.1, 10.0]; blacklist takes priority over `preferred_tags`.
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

/// Consolidated feed/recommendation settings for an account.
/// Returned by `GET /account/<id>/feed_settings`.
#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
#[serde(crate = "rocket::serde")]
pub struct AccountFeedSettings {
    /// The effective blacklist text (device-specific with global fallback).
    #[serde(default)]
    pub blacklist: Option<String>,
    /// Per-account preferred tags for scoring.
    #[serde(default)]
    pub preferred_tags: Vec<PreferredTag>,
    /// A/B experiment bucket assignment (read-only after creation).
    pub experiment_bucket: Option<String>,
}

/// Partial update payload for `PATCH /account/<id>/feed_settings`.
/// Every field is optional — only present fields are updated.
#[derive(Debug, Deserialize, Clone, JsonSchema)]
#[serde(crate = "rocket::serde")]
pub struct AccountFeedSettingsPatch {
    /// Replace the device-scoped blacklist. `None` = no change;
    /// `Some("")` = reset to server-side default at write time.
    pub blacklist: Option<String>,
    /// Replace the full preferred-tags list. `None` = no change.
    pub preferred_tags: Option<Vec<PreferredTag>>,
}

/// Minimal account identity carried in an export snapshot.
#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
#[serde(crate = "rocket::serde")]
pub struct ExportAccountSummary {
    pub id: i32,
    pub name: String,
}

/// Full account data snapshot for backup / migration.
///
/// `profile` is included for archival/diagnostic value but is **not
/// importable** — it is derived state recomputed by `/process` from the
/// account's favourites and the local catalog. Import restores only the
/// user-settable fields (`blacklist`, `preferred_tags`).
#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
#[serde(crate = "rocket::serde")]
pub struct AccountDataExport {
    pub account: ExportAccountSummary,
    /// Effective blacklist text (device-scoped, falls back to the server
    /// default blacklist when empty).
    #[serde(default)]
    pub blacklist: Option<String>,
    #[serde(default)]
    pub preferred_tags: Vec<PreferredTag>,
    /// A/B experiment bucket assignment (read-only).
    #[serde(default)]
    pub experiment_bucket: Option<String>,
    pub profile: AccountPreferenceProfile,
}

/// Import payload for `POST /account/<id>/import`.
///
/// Only user-settable fields are accepted; `profile` from an export is
/// intentionally ignored (recomputed by `/process`). `None` = leave that
/// field untouched; `Some("")` for blacklist resets to the server default.
#[derive(Debug, Deserialize, Clone, JsonSchema)]
#[serde(crate = "rocket::serde")]
pub struct AccountDataImport {
    /// Replace the device-scoped blacklist. `None` = no change;
    /// `Some("")` = reset to server-side default at write time.
    #[serde(default)]
    pub blacklist: Option<String>,
    /// Replace the full preferred-tags list. `None` = no change.
    #[serde(default)]
    pub preferred_tags: Option<Vec<PreferredTag>>,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The /settings page expects the exact `snake_case` wire format the
    /// backend emits (no camelCase aliases). Lock it in so a rename can't
    /// silently break the frontend again.
    #[test]
    fn account_feed_settings_uses_snake_case_wire_format() {
        let body = r#"{
            "blacklist": "gore\nyoung",
            "preferred_tags": [
                {"tag": "wolf", "group": "general", "weight": 2.0},
                {"tag": "canine", "group": "species", "weight": 1.5}
            ],
            "experiment_bucket": "B"
        }"#;
        let settings: AccountFeedSettings =
            serde_json::from_str(body).expect("GET body must parse with snake_case field names");
        assert_eq!(settings.blacklist.as_deref(), Some("gore\nyoung"));
        assert_eq!(settings.preferred_tags.len(), 2);
        assert_eq!(settings.preferred_tags[0].tag, "wolf");
        assert_eq!(settings.preferred_tags[0].weight, 2.0);
        assert_eq!(settings.experiment_bucket.as_deref(), Some("B"));

        // PATCH body arrives with snake_case field names; absent fields -> None.
        let patch: AccountFeedSettingsPatch = serde_json::from_str(
            r#"{"blacklist":"gore","preferred_tags":[{"tag":"wolf","group":"general","weight":2.0}]}"#,
        )
        .expect("PATCH body must parse with snake_case field names");
        assert_eq!(patch.blacklist.as_deref(), Some("gore"));
        assert_eq!(patch.preferred_tags.as_ref().map(Vec::len), Some(1));

        // Partial update: absent fields deserialize as None.
        let empty: AccountFeedSettingsPatch =
            serde_json::from_str(r#"{"blacklist": null}"#).expect("partial PATCH parses");
        assert!(empty.preferred_tags.is_none());
    }
}
