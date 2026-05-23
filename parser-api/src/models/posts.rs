use std::fmt::{self, Display, Formatter};

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
pub struct PostsApiResponse {
    pub posts: Vec<Post>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct Post {
    pub id: i64,
    #[schemars(with = "String", description = "RFC3339 timestamp")]
    pub created_at: DateTime<Utc>,
    #[schemars(with = "String", description = "RFC3339 timestamp")]
    pub updated_at: DateTime<Utc>,
    pub file: Option<FileInfo>,
    pub preview: Option<Preview>,
    pub sample: Option<Sample>,
    pub score: Score,
    pub tags: Tags,
    pub locked_tags: Option<Vec<String>>,
    pub change_seq: f64,
    pub flags: Flags,
    pub rating: Rating,
    pub fav_count: i64,
    pub sources: Vec<String>,
    pub pools: Vec<i64>,
    pub relationships: Relationships,
    pub approver_id: Option<i64>,
    pub uploader_id: i64,
    pub description: Option<String>,
    pub comment_count: i64,
    pub is_favorited: bool,
    pub has_notes: bool,
    pub duration: Option<f64>,
}

impl Post {
    pub fn media_type(&self) -> &'static str {
        let ext = self
            .file
            .as_ref()
            .and_then(|file| file.ext.as_deref())
            .map(|ext| ext.to_ascii_lowercase());

        match ext.as_deref() {
            Some("webm") | Some("mp4") => "video",
            Some("gif") => "animated",
            _ if self.duration.unwrap_or(0.0) > 0.0 => "video",
            _ => "image",
        }
    }

    pub fn is_animated(&self) -> bool {
        self.media_type() != "image"
    }
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct FileInfo {
    pub width: i64,
    pub height: i64,
    pub ext: Option<String>,
    pub size: i64,
    pub md5: Option<String>,
    pub url: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct Preview {
    pub width: i64,
    pub height: i64,
    pub url: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct Sample {
    pub has: Option<bool>,
    pub height: Option<i64>,
    pub width: Option<i64>,
    pub url: Option<String>,
    pub alternates: Option<Alternates>,
    pub variants: Option<Variants>,
    pub samples: Option<Samples>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct PostSampleAlternate {
    pub fps: f32,
    pub codec: Option<String>,
    pub size: i64,
    pub width: i64,
    pub height: i64,
    pub url: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct Alternates {
    pub has: Option<bool>,
    pub original: Option<PostSampleAlternate>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct Variants {
    pub webm: PostSampleAlternate,
    pub mp4: PostSampleAlternate,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct Samples {
    #[serde(rename = "480p")]
    pub p480: PostSampleAlternate,
    #[serde(rename = "720p")]
    pub p720: PostSampleAlternate,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct Score {
    pub up: i64,
    pub down: i64,
    pub total: i64,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct Tags {
    pub general: Vec<String>,
    pub artist: Vec<String>,
    pub copyright: Vec<String>,
    pub character: Vec<String>,
    pub species: Vec<String>,
    pub invalid: Vec<String>,
    pub meta: Vec<String>,
    pub lore: Vec<String>,
    pub contributor: Vec<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Default)]
pub struct Flags {
    pub pending: bool,
    pub flagged: bool,
    pub note_locked: bool,
    pub status_locked: bool,
    pub rating_locked: bool,
    pub deleted: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
#[serde(rename_all = "lowercase")]
pub enum Rating {
    S,
    Q,
    E,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema, Clone, PartialEq, Eq)]
#[serde(crate = "rocket::serde", rename_all = "snake_case")]
pub enum FeedInteractionType {
    QualifiedImpression,
    Open,
    Hide,

    #[serde(other)]
    Unknown,
}

impl Display for FeedInteractionType {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            FeedInteractionType::QualifiedImpression => write!(f, "qualified_impression"),
            FeedInteractionType::Open => write!(f, "open"),
            FeedInteractionType::Hide => write!(f, "hide"),
            FeedInteractionType::Unknown => write!(f, "unknown"),
        }
    }
}

impl Display for Rating {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Rating::S => write!(f, "s"),
            Rating::Q => write!(f, "q"),
            Rating::E => write!(f, "e"),
        }
    }
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct Relationships {
    pub parent_id: Option<i64>,
    pub has_children: bool,
    pub has_active_children: bool,
    pub children: Vec<i64>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct ScoreBreakdown {
    pub tag_similarity: f32,
    pub quality_fit: f32,
    pub recency_fit: f32,
    pub rating_fit: f32,
    pub media_fit: f32,
    pub popularity_fit: f32,
    pub interaction_fit: f32,
    #[serde(default)]
    pub tag_relation_fit: f32,
    /// Uploader quality channel (0 when disabled). Added in v5.10.
    #[serde(default)]
    pub uploader_fit: f32,
    /// Tag exclusivity channel (0 when disabled). Added in v5.11.
    #[serde(default)]
    pub exclusivity_fit: f32,
    /// Tag novelty channel (0 when disabled). Added in v5.11.
    #[serde(default)]
    pub novelty_fit: f32,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema, Clone)]
#[serde(crate = "rocket::serde")]
pub struct FeedInteractionRequest {
    pub account_id: i32,
    pub post_id: i64,
    pub event_type: FeedInteractionType,
    pub position: i32,
    pub session_id: String,
}

/// Batch interaction submission for offline sync. Max 100 interactions
/// per batch to avoid overloading the server.
#[derive(Debug, Serialize, Deserialize, JsonSchema, Clone)]
#[serde(crate = "rocket::serde")]
pub struct BatchInteractionRequest {
    pub interactions: Vec<FeedInteractionRequest>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct ScoredPost {
    pub post: Post,
    pub score: f32,
    pub breakdown: Option<ScoreBreakdown>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use rocket::serde::json::serde_json;

    /// Build a `Post` whose only meaningful fields for media-type tests are
    /// the file extension and duration; everything else is neutral filler.
    fn post_with(ext: Option<&str>, duration: Option<f64>) -> Post {
        Post {
            id: 1,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            file: ext.map(|e| FileInfo {
                width: 1,
                height: 1,
                ext: Some(e.to_string()),
                size: 1,
                md5: None,
                url: None,
            }),
            preview: None,
            sample: None,
            score: Score {
                up: 0,
                down: 0,
                total: 0,
            },
            tags: Tags {
                general: vec![],
                artist: vec![],
                copyright: vec![],
                character: vec![],
                species: vec![],
                invalid: vec![],
                meta: vec![],
                lore: vec![],
                contributor: vec![],
            },
            locked_tags: None,
            change_seq: 0.0,
            flags: Flags {
                pending: false,
                flagged: false,
                note_locked: false,
                status_locked: false,
                rating_locked: false,
                deleted: false,
            },
            rating: Rating::S,
            fav_count: 0,
            sources: vec![],
            pools: vec![],
            relationships: Relationships {
                parent_id: None,
                has_children: false,
                has_active_children: false,
                children: vec![],
            },
            approver_id: None,
            uploader_id: 0,
            description: None,
            comment_count: 0,
            is_favorited: false,
            has_notes: false,
            duration,
        }
    }

    #[test]
    fn media_type_from_extension() {
        assert_eq!(post_with(Some("webm"), None).media_type(), "video");
        assert_eq!(post_with(Some("mp4"), None).media_type(), "video");
        assert_eq!(post_with(Some("gif"), None).media_type(), "animated");
        assert_eq!(post_with(Some("png"), None).media_type(), "image");
        assert_eq!(post_with(Some("jpg"), None).media_type(), "image");
        // Extension match is case-insensitive.
        assert_eq!(post_with(Some("WEBM"), None).media_type(), "video");
    }

    #[test]
    fn media_type_falls_back_to_duration() {
        // No usable extension but a positive duration → treat as video.
        assert_eq!(post_with(None, Some(12.5)).media_type(), "video");
        assert_eq!(post_with(None, Some(0.0)).media_type(), "image");
        assert_eq!(post_with(None, None).media_type(), "image");
    }

    #[test]
    fn is_animated_tracks_media_type() {
        assert!(post_with(Some("gif"), None).is_animated());
        assert!(post_with(Some("webm"), None).is_animated());
        assert!(!post_with(Some("png"), None).is_animated());
        assert!(!post_with(None, None).is_animated());
    }

    #[test]
    fn rating_display() {
        assert_eq!(Rating::S.to_string(), "s");
        assert_eq!(Rating::Q.to_string(), "q");
        assert_eq!(Rating::E.to_string(), "e");
    }

    #[test]
    fn feed_interaction_type_display() {
        assert_eq!(
            FeedInteractionType::QualifiedImpression.to_string(),
            "qualified_impression"
        );
        assert_eq!(FeedInteractionType::Open.to_string(), "open");
        assert_eq!(FeedInteractionType::Hide.to_string(), "hide");
        assert_eq!(FeedInteractionType::Unknown.to_string(), "unknown");
    }

    #[test]
    fn feed_interaction_type_deserialization() {
        let parse = |s: &str| serde_json::from_str::<FeedInteractionType>(s).unwrap();
        assert_eq!(parse(r#""open""#), FeedInteractionType::Open);
        assert_eq!(parse(r#""hide""#), FeedInteractionType::Hide);
        assert_eq!(
            parse(r#""qualified_impression""#),
            FeedInteractionType::QualifiedImpression
        );
        // Unrecognised event types fall through to Unknown (#[serde(other)]).
        assert_eq!(parse(r#""brand_new_event""#), FeedInteractionType::Unknown);
    }
}
