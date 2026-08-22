use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::models::AccountPreferenceProfile;
use crate::models::InteractionHistoryItem;
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
    /// The account's own private blacklist, exposed only when authenticated
    /// as that user (direct sync). e621 returns it in `blacklisted_tags` as a
    /// single `\n`-joined string of filter tags (empty/absent when none set).
    /// Optional so a missing field can't break parsing.
    #[serde(default)]
    pub blacklisted_tags: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
#[serde(crate = "rocket::serde")]
pub struct TruncatedAccount {
    pub id: i32,
    pub name: String,
    pub blacklist: String,
}

/// A single account link owned by a device (owner token), rendered within
/// [`DeviceSession`]. Contains no secrets.
#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
#[serde(crate = "rocket::serde", rename_all = "camelCase")]
pub struct DeviceAccountLink {
    pub account_id: i32,
    pub name: String,
    pub linked_at: String,
    pub last_seen_at: String,
}

/// A device (owner token) and the accounts it is linked to, as seen from the
/// requesting token's point of view. `id` is a stable, non-reversible
/// `sha256` of the owner token — the raw token is never exposed.
#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
#[serde(crate = "rocket::serde", rename_all = "camelCase")]
pub struct DeviceSession {
    /// Stable, non-reversible device identifier (`sha256` hex of the token).
    pub id: String,
    pub is_current: bool,
    pub first_seen_at: String,
    pub last_seen_at: String,
    /// Whether the device was last seen within the active window.
    pub active: bool,
    pub accounts: Vec<DeviceAccountLink>,
}

/// Payload for `POST /session/revoke`: the `deviceId` of another device
/// (as returned by `GET /session/devices`) to revoke.
#[derive(Debug, Deserialize, Clone, JsonSchema)]
#[serde(crate = "rocket::serde", rename_all = "camelCase")]
pub struct RevokeDeviceRequest {
    pub device_id: String,
}

/// Payload for `PUT /account/<id>/key`: the plaintext e621 API key to store
/// (encrypted at rest). Never returned by any endpoint.
#[derive(Debug, Deserialize, Clone, JsonSchema)]
#[serde(crate = "rocket::serde", rename_all = "camelCase")]
pub struct SetAccountKeyRequest {
    pub key: String,
}

/// State of an account's e621 API key, exposed by `GET /account/<id>/key/state`
/// (and returned by the mutating key endpoints). Contains NO key material —
/// only booleans, timestamps and the username.
#[derive(Debug, Serialize, Clone, JsonSchema)]
#[serde(crate = "rocket::serde", rename_all = "camelCase")]
pub struct AccountKeyState {
    pub account_id: i32,
    pub has_key: bool,
    /// RFC 3339 when the current key was set/rotated.
    pub added_at: Option<String>,
    /// RFC 3339 of the last successful verification against e621.
    pub verified_at: Option<String>,
    /// e621 username this key belongs to (the linked account's name).
    pub name: String,
    /// Which e621 operations currently use this key (e.g. `direct_sync`).
    pub operations: Vec<String>,
}

/// Result of `POST /account/<id>/key/test`.
#[derive(Debug, Serialize, Clone, JsonSchema)]
#[serde(crate = "rocket::serde", rename_all = "camelCase")]
pub struct KeyVerifyResult {
    pub valid: bool,
    pub name: String,
    /// RFC 3339 of this verification when `valid` is true.
    pub verified_at: Option<String>,
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
    /// Optional e621 API key for the claimed account (M2 ownership proof /
    /// direct-sync enablement). When present, `POST /api/account` verifies it
    /// against e621 and stores it encrypted at rest; when absent the account is
    /// linked without a key. Never returned in any response.
    #[serde(default)]
    pub api_key: Option<String>,
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
/// `profile` is included for archival/diagnostic value but its non-interaction
/// parts (rating/media/quality/recency/uploaders) are **not importable** —
/// they are derived state recomputed by `/process` from the account's
/// favourites and the local catalog. The interaction model (`interactions`)
/// IS importable: it is the raw preference signal that rebuilds the account's
/// tag-feedback on restore. `blacklist` and `preferred_tags` are user-settable
/// and importable. No secrets (owner-token or API keys) are ever exported.
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
    /// The account's raw interaction model (open/like/hide/… events).
    #[serde(default)]
    pub interactions: Vec<InteractionHistoryItem>,
}

/// Import payload for `POST /account/<id>/import`.
///
/// Only user-settable fields are accepted; the non-interaction part of
/// `profile` from an export is intentionally ignored (recomputed by `/process`,
/// and its non-interaction fields derive from public favourites). `interactions`
/// IS accepted and restores the interaction model (replayed into
/// `feed_interactions` + tag-feedback rebuilt), transferring the interaction-
/// derived part of the profile. `None` = leave that field untouched;
/// `Some("")` for blacklist resets to the server default.
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
    /// Restore the interaction model. `None` = no change (empty list = none).
    #[serde(default)]
    pub interactions: Option<Vec<InteractionHistoryItem>>,
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
