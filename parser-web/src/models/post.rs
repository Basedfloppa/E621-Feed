use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize, Clone, PartialEq, Default)]
pub struct Post {
    pub id: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub change_seq: f64,
    pub files: Files,
    pub uploader_id: i64,
    #[serde(default)]
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
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub tags: Tags,
}

#[derive(Deserialize, Serialize, Clone, PartialEq, Default)]
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

#[derive(Deserialize, Serialize, Clone, PartialEq, Default)]
pub struct FileMeta {
    #[serde(default)]
    pub md5: Option<String>,
    #[serde(default)]
    pub ext: Option<String>,
    #[serde(default)]
    pub size: i64,
    #[serde(default)]
    pub duration: Option<f64>,
    #[serde(default)]
    pub has_sample: bool,
}

#[derive(Deserialize, Serialize, Clone, PartialEq, Default)]
pub struct FileOriginal {
    #[serde(default)]
    pub width: i64,
    #[serde(default)]
    pub height: i64,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, PartialEq, Default)]
pub struct FilePreview {
    /// e621 v2 primary/alternate preview URLs. Legacy `jpg`/`webp` fields
    /// remain for previously cached v1-shaped responses.
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub alt: Option<String>,
    #[serde(default)]
    pub width: i64,
    #[serde(default)]
    pub height: i64,
    #[serde(default)]
    pub jpg: Option<String>,
    #[serde(default)]
    pub webp: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, PartialEq, Default)]
pub struct FileSample {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub alt: Option<String>,
    #[serde(default)]
    pub width: i64,
    #[serde(default)]
    pub height: i64,
    #[serde(default)]
    pub jpg: Option<String>,
    #[serde(default)]
    pub webp: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, PartialEq, Default)]
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

#[derive(Deserialize, Serialize, Clone, PartialEq, Default)]
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

#[derive(Deserialize, Serialize, Clone, PartialEq, Default)]
pub struct Relationships {
    pub parent_id: Option<i64>,
    #[serde(default)]
    pub children: Vec<i64>,
}

#[derive(Deserialize, Serialize, Clone, PartialEq, Default)]
pub struct Score {
    pub up: i64,
    pub down: i64,
    pub total: i64,
}

#[derive(Deserialize, Serialize, Clone, PartialEq, Default)]
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

#[derive(Deserialize, Serialize, Clone, PartialEq, Default)]
pub struct Flags {
    pub pending: bool,
    pub flagged: bool,
    pub note_locked: bool,
    pub status_locked: bool,
    pub rating_locked: bool,
    pub deleted: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Rating {
    #[default]
    S,
    Q,
    E,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FeedInteractionType {
    QualifiedImpression,
    Open,
    Like,
    StrongLike,
    Hide,
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
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
    #[serde(default)]
    pub uploader_fit: f32,
    #[serde(default)]
    pub exclusivity_fit: f32,
    #[serde(default)]
    pub novelty_fit: f32,
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct FeedInteractionRequest {
    pub account_id: i32,
    pub post_id: i64,
    pub event_type: FeedInteractionType,
    pub position: i32,
    pub session_id: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct ScoredPost {
    pub post: Post,
    pub score: f32,
    pub breakdown: Option<ScoreBreakdown>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct TagRelationNode {
    pub name: String,
    pub group_type: String,
    pub count: i64,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct TagRelationEdge {
    pub source: usize,
    pub target: usize,
    pub user_cooc: i64,
    pub global_cooc: i64,
    pub global_lift: f32,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
pub struct TagRelationScoring {
    pub w_global: f32,
    pub w_personal: f32,
    pub pmi_scale: f32,
    pub cooc_ref: f32,
    pub user_cooc_ref: f32,
    pub min_cooc_global: i64,
    pub min_cooc_user: i64,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
pub struct TagRelationGraphPayload {
    pub nodes: Vec<TagRelationNode>,
    pub edges: Vec<TagRelationEdge>,
    pub catalog_post_count: i64,
    pub account_post_count: i64,
    #[serde(default)]
    pub scoring: TagRelationScoring,
}

#[derive(Deserialize, Serialize, Clone, PartialEq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ProcessJobPhase {
    Running,
    Done,
    Failed,
}

#[derive(Deserialize, Serialize, Clone, PartialEq, Debug)]
pub struct JobPhaseRecord {
    pub name: String,
    pub elapsed_ms: f64,
}

#[derive(Deserialize, Serialize, Clone, PartialEq, Debug)]
pub struct ProcessJobState {
    pub account_id: i32,
    pub phase: ProcessJobPhase,
    pub pages_total: i32,
    pub pages_done: i32,
    #[serde(default)]
    pub error: Option<String>,
    pub started_at: String,
    #[serde(default)]
    pub finished_at: Option<String>,
    #[serde(default)]
    pub phases: Vec<JobPhaseRecord>,
    pub elapsed_secs: f64,
}

// ── (reserved for tag alias resolution if needed) ──────────────────
