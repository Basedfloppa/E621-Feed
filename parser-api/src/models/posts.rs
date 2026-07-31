use std::fmt::{self, Display, Formatter};

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

// ── Canonical Post model (e621 API v2) ─────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct Post {
    pub id: i64,
    #[schemars(with = "String", description = "RFC3339 timestamp")]
    pub created_at: DateTime<Utc>,
    #[schemars(with = "String", description = "RFC3339 timestamp")]
    pub updated_at: DateTime<Utc>,
    pub files: Files,
    pub change_seq: f64,
    pub uploader_id: i64,
    pub uploader_name: Option<String>,
    pub approver_id: Option<i64>,
    pub stats: Stats,
    pub flags: Flags,
    pub has: Has,
    pub relationships: Relationships,
    pub pools: Vec<i64>,
    pub rating: Rating,
    #[serde(default)]
    pub locked_tags: Vec<String>,
    #[serde(default)]
    pub sources: Vec<String>,
    pub description: Option<String>,
    #[serde(default, deserialize_with = "deserialize_tags")]
    pub tags: Tags,
}

impl Post {
    pub fn media_type(&self) -> &'static str {
        let ext = self
            .files
            .meta
            .ext
            .as_deref()
            .map(|ext| ext.to_ascii_lowercase());

        match ext.as_deref() {
            Some("webm") | Some("mp4") => "video",
            Some("gif") => "animated",
            _ if self.files.meta.duration.unwrap_or(0.0) > 0.0 => "video",
            _ => "image",
        }
    }

    pub fn is_animated(&self) -> bool {
        self.media_type() != "image"
    }

    /// Group flat tags into the legacy categorized structure.
    /// v2 API returns a flat list — we put everything in "general",
    /// which is the scorer's primary signal channel.
    pub fn tag_groups(&self) -> Tags {
        self.tags.clone()
    }
}

/// Deserializes `Tags` from either a categorized object (`mode=extended`)
/// or a flat `Vec<String>` (v2 default), putting everything in `general`.
fn deserialize_tags<'de, D: Deserializer<'de>>(d: D) -> Result<Tags, D::Error> {
    use serde::de;

    struct TagsVisitor;
    impl<'de> de::Visitor<'de> for TagsVisitor {
        type Value = Tags;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a tag map {general: [...], artist: [...], ...} or a flat array of strings")
        }

        fn visit_map<M: de::MapAccess<'de>>(self, mut map: M) -> Result<Tags, M::Error> {
            let mut tags = Tags::default();
            while let Some(key) = map.next_key::<String>()? {
                let vals: Vec<String> = map.next_value()?;
                match key.as_str() {
                    "general" => tags.general = vals,
                    "artist" => tags.artist = vals,
                    "character" => tags.character = vals,
                    "copyright" => tags.copyright = vals,
                    "species" => tags.species = vals,
                    "lore" => tags.lore = vals,
                    "meta" => tags.meta = vals,
                    "invalid" => tags.invalid = vals,
                    "contributor" => tags.contributor = vals,
                    _ => {} // ignore unknown groups
                }
            }
            Ok(tags)
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Tags, A::Error> {
            let mut general = Vec::new();
            while let Some(s) = seq.next_element::<String>()? {
                general.push(s);
            }
            Ok(Tags {
                general,
                ..Tags::default()
            })
        }
    }

    d.deserialize_any(TagsVisitor)
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Default)]
pub struct Files {
    #[serde(default)]
    pub meta: FileMeta,
    #[serde(default)]
    pub original: FileOriginal,
    #[serde(default)]
    pub preview: FilePreview,
    #[serde(default)]
    pub sample: FileSample,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Default)]
pub struct FileMeta {
    pub md5: Option<String>,
    pub ext: Option<String>,
    #[serde(default)]
    pub size: i64,
    pub duration: Option<f64>,
    #[serde(default)]
    pub has_sample: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Default)]
pub struct FileOriginal {
    #[serde(default)]
    pub width: i64,
    #[serde(default)]
    pub height: i64,
    pub url: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Default)]
pub struct FilePreview {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub alt: Option<String>,
    #[serde(default)]
    pub width: i64,
    #[serde(default)]
    pub height: i64,
    pub jpg: Option<String>,
    pub webp: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Default)]
pub struct FileSample {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub alt: Option<String>,
    #[serde(default)]
    pub width: i64,
    #[serde(default)]
    pub height: i64,
    pub jpg: Option<String>,
    pub webp: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Default)]
pub struct Stats {
    #[serde(default)]
    pub score: Score,
    #[serde(default)]
    pub fav_count: i64,
    #[serde(default)]
    pub is_favorited: bool,
    #[serde(default)]
    pub vote: i64,
    #[serde(default)]
    pub comment_count: i64,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Default)]
pub struct Has {
    #[serde(default)]
    pub parent: bool,
    #[serde(default)]
    pub children: bool,
    #[serde(default)]
    pub active_children: bool,
    #[serde(default)]
    pub notes: bool,
    #[serde(default)]
    pub sample: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Default)]
pub struct Relationships {
    pub parent_id: Option<i64>,
    #[serde(default)]
    pub children: Vec<i64>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Default)]
pub struct Score {
    pub up: i64,
    pub down: i64,
    pub total: i64,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Default, Debug, PartialEq, Eq)]
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
    Like,
    StrongLike,
    Hide,

    #[serde(other)]
    Unknown,
}

impl Display for FeedInteractionType {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            FeedInteractionType::QualifiedImpression => write!(f, "qualified_impression"),
            FeedInteractionType::Open => write!(f, "open"),
            FeedInteractionType::Like => write!(f, "like"),
            FeedInteractionType::StrongLike => write!(f, "strong_like"),
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
    /// Artist discovery channel (0 when disabled). Added in v5.12.
    #[serde(default)]
    pub artist_discovery_fit: f32,
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

/// Response wrapper for session-based feed continuation.
/// `fresh_start` is true when the session expired or didn't exist,
/// signalling the client to discard the old session token.
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
#[serde(crate = "rocket::serde")]
pub struct ContinueResponse {
    pub posts: Vec<ScoredPost>,
    pub fresh_start: bool,
}

/// Query parameters for `/posts/<id>/similar`.
#[derive(Deserialize, JsonSchema, Clone)]
#[serde(crate = "rocket::serde")]
pub struct SimilarPostsQuery {
    pub account_id: i32,
    pub limit: Option<i32>,
    /// Minimum number of overlapping tags (default 2).
    pub min_overlap: Option<i32>,
    pub page: Option<i32>,
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
            change_seq: 0.0,
            files: Files {
                meta: FileMeta {
                    md5: None,
                    ext: ext.map(|e| e.to_string()),
                    size: 1,
                    duration,
                    has_sample: false,
                },
                original: FileOriginal {
                    width: 1,
                    height: 1,
                    url: None,
                },
                ..Default::default()
            },
            uploader_id: 0,
            uploader_name: None,
            approver_id: None,
            stats: Stats {
                score: Score {
                    up: 0,
                    down: 0,
                    total: 0,
                },
                ..Default::default()
            },
            flags: Flags::default(),
            has: Has::default(),
            relationships: Relationships::default(),
            pools: vec![],
            rating: Rating::S,
            locked_tags: vec![],
            sources: vec![],
            description: None,
            tags: Tags::default(),
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
