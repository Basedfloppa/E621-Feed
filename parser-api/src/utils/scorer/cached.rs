//! Pre-resolved post features for the calibrate fast path.
//!
//! Each post's tags are walked once at prep time; for every (group, tag)
//! we resolve the per-tag bits the hot scoring loop needs (lowercase
//! string, raw IDF document-frequency, and the global TagRelationGraph
//! `TagId`) into a flat `Vec<CachedTag>`. Subsequent grid probes then
//! score against `CachedPostFeatures` without calling `IdfIndex::df_for`
//! or `TagRelationGraph::tag_id` HashMap-by-string lookups.
//!
//! Memory: ~50–80 bytes per tag (lc string + 16 bytes of ints). Negligible
//! next to the post data itself; see `docs/calibration.md` for sizing.
//!
//! Design note: this lives in the lib (rather than `bin/calibrate`) so
//! `ScoringContext` can take `&CachedPostFeatures` directly. The prod
//! `/recommendations` path still uses `&Post`; cached features are only
//! built where the same post is scored many times (calibrate grid probes).

use chrono::{DateTime, Utc};

use crate::models::{Post, Rating};

use super::super::idf::IdfIndex;
use super::super::tag_relation::{TagId, TagRelationGraph};
use super::Group;

/// One tag occurrence on a post, with everything the hot scoring loop
/// needs to evaluate it under any priors. Built once at prep time.
#[derive(Clone)]
pub struct CachedTag {
    /// `Group` enum value cast to `u8` (0..=6).
    pub group: u8,
    /// Lowercased, trimmed tag name.
    pub lc: String,
    /// Raw document-frequency from `IdfIndex` at prep time. Used to
    /// reconstruct IDF without re-looking-up the HashMap on every probe.
    pub df_raw: i64,
    /// Pre-resolved global-graph TagId, if the tag was present in the
    /// graph at prep time. `None` → tag is unknown to the global graph
    /// (typically a brand-new tag) and PMI contribution will be skipped.
    pub global_tid: Option<TagId>,
    /// Pre-resolved per-account user-graph TagId. `None` for the prod
    /// fast path (no user graph supplied) or when the tag wasn't in the
    /// caller's train_posts.
    pub user_tid: Option<TagId>,
}

/// Pre-resolved scoring features for one post in the eval dataset.
/// Mirrors the subset of `Post` fields the scoring loop actually reads;
/// the original `Post` is still kept alongside it for the diversify
/// pass (which needs the full struct).
pub struct CachedPostFeatures {
    pub id: i64,
    pub created_at: DateTime<Utc>,
    pub score_total: i64,
    pub fav_count: i64,
    pub comment_count: i64,
    pub duration: f32,
    pub rating: Rating,
    pub media_type: &'static str,
    pub tags: Vec<CachedTag>,
}

impl CachedPostFeatures {
    /// Build per-post features against a global tag-relation graph and
    /// no personal graph (`user_tid = None` for every tag). Used by
    /// callers that don't have a per-account graph available.
    pub fn from_post(post: &Post, idf: &IdfIndex, global_relation: &TagRelationGraph) -> Self {
        Self::from_post_with_user(post, idf, global_relation, None)
    }

    /// Build per-post features with both global and per-account
    /// (personal) tag-relation graphs pre-resolved into TagIds. The
    /// calibrate harness builds one fixture per account with its own
    /// `user_relation` from train_posts; pass that here to give the
    /// personal `tag_relation_fit` channel real signal.
    pub fn from_post_with_user(
        post: &Post,
        idf: &IdfIndex,
        global_relation: &TagRelationGraph,
        user_relation: Option<&TagRelationGraph>,
    ) -> Self {
        let mut tags = Vec::with_capacity(32);
        let groups: [(Group, &Vec<String>); 7] = [
            (Group::Artist, &post.tags.artist),
            (Group::Character, &post.tags.character),
            (Group::Copyright, &post.tags.copyright),
            (Group::Species, &post.tags.species),
            (Group::General, &post.tags.general),
            (Group::Lore, &post.tags.lore),
            (Group::Meta, &post.tags.meta),
        ];
        for (group, raw_tags) in groups {
            let g = group as u8;
            for raw in raw_tags {
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let lc: String = if trimmed.bytes().any(|b| b.is_ascii_uppercase()) {
                    trimmed.to_ascii_lowercase()
                } else {
                    trimmed.to_owned()
                };
                let df_raw = idf.df_for(&lc);
                let global_tid = global_relation.tag_id(g, lc.as_str());
                let user_tid = user_relation.and_then(|gr| gr.tag_id(g, lc.as_str()));
                tags.push(CachedTag {
                    group: g,
                    lc,
                    df_raw,
                    global_tid,
                    user_tid,
                });
            }
        }
        Self {
            id: post.id,
            created_at: post.created_at,
            score_total: post.score.total,
            fav_count: post.fav_count,
            comment_count: post.comment_count,
            duration: post.duration.unwrap_or(0.0) as f32,
            rating: post.rating.clone(),
            media_type: post.media_type(),
            tags,
        }
    }
}
