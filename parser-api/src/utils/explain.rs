//! Concise human-readable "why this post?" explanations for recommendations.
//!
//! The `ScoreBreakdown` exposed to the frontend is a flat set of channel
//! scores (tag similarity, artist discovery, …). This module turns those
//! signals — plus a small set of the account's high-signal tags — into 1–3
//! short phrases a user can actually read, e.g.:
//!
//! * "New artist near your tastes: <artist>"
//! * "Matches your “<tag>“ taste"
//! * "Similar to posts you've interacted with"
//! * "Exploration pick — stepping outside your usual picks"
//!
//! Reasons are deliberately best-effort: a post with no strong signal can
//! come back with zero reasons, and the UI then falls back to its own
//! generic channel labels.

use std::collections::HashSet;

use crate::models::{Post, ScoredPost};

/// Populate `reasons` on each scored post with concise explanations.
/// `user_tags` should be the account's tag names (lowercased); it is used
/// only to name a specific tag that the post shares with the account's
/// taste, never to re-score.
pub fn explain_scored_posts(
    scored: &mut [ScoredPost],
    user_tags: &HashSet<String>,
    exploration_active: bool,
) {
    for sp in scored {
        sp.reasons = explain_one(
            &sp.post,
            sp.breakdown.as_ref(),
            user_tags,
            exploration_active,
        );
    }
}

fn explain_one(
    post: &Post,
    breakdown: Option<&crate::models::ScoreBreakdown>,
    user_tags: &HashSet<String>,
    exploration_active: bool,
) -> Vec<String> {
    let mut reasons: Vec<String> = Vec::with_capacity(3);
    let Some(b) = breakdown else {
        return reasons;
    };

    // Most specific / personal reasons first; each names the concrete entity
    // involved (a tag, an artist, an uploader) rather than a raw channel name.

    // 1. Artist discovery — the post features an artist new to the user.
    if b.artist_discovery_fit > 0.1
        && let Some(artist) = post.tags.artist.first().filter(|a| !a.is_empty())
    {
        reasons.push(format!("New artist near your tastes: {artist}"));
    }

    // 2. Taste match — name a specific tag the post shares with the account
    //    (prefer character/species/general over copyright/meta noise).
    if b.tag_similarity > 0.05 {
        for tag in user_taste_tags(post) {
            if user_tags.contains(tag) {
                reasons.push(format!("Matches your “{tag}” taste"));
                break;
            }
        }
    }

    // 3. Interaction similarity.
    if b.interaction_fit > 0.3 {
        reasons.push("Similar to posts you've interacted with".to_string());
    }

    // 4. Name the uploader when uploader affinity drives the pick.
    if reasons.len() < 2
        && b.uploader_fit > 0.2
        && let Some(name) = post.uploader_name.as_deref().filter(|n| !n.is_empty())
    {
        reasons.push(format!("From {name} — an uploader you tend to favourite"));
    }

    // 5. Exploration / novelty pick.
    if exploration_active && reasons.len() < 3 && b.novelty_fit > 0.2 {
        reasons.push("Exploration pick — stepping outside your usual picks".to_string());
    }

    // 6. Honest fallbacks so a concrete reason is present even when the post
    //    carries no personal signal (popular/new feeds), instead of letting
    //    the generic frontend sentence stand alone.
    if reasons.is_empty() {
        if b.recency_fit > 0.15 {
            reasons.push("Freshly posted".to_string());
        } else if b.popularity_fit > 0.3 {
            reasons.push("Popular with others right now".to_string());
        } else if b.quality_fit > 0.3 {
            reasons.push("High-quality post".to_string());
        }
    }

    reasons.truncate(3);
    reasons
}

/// Post tag names in a preferred order for the "matches your taste" reason:
/// concrete categories first, generics/meta last.
fn user_taste_tags(post: &Post) -> impl Iterator<Item = &String> {
    post.tags
        .character
        .iter()
        .chain(post.tags.species.iter())
        .chain(post.tags.general.iter())
        .chain(post.tags.copyright.iter())
        .chain(post.tags.lore.iter())
        .chain(post.tags.artist.iter())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        Files, Flags, Has, Post, Rating, Relationships, ScoreBreakdown, Stats, Tags,
    };
    use chrono::Utc;

    fn post(artist: Option<&str>, general: Vec<String>) -> Post {
        Post {
            id: 1,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            change_seq: 0.0,
            files: Files::default(),
            uploader_id: 0,
            uploader_name: None,
            approver_id: None,
            stats: Stats::default(),
            flags: Flags::default(),
            has: Has::default(),
            relationships: Relationships::default(),
            pools: vec![],
            rating: Rating::S,
            locked_tags: vec![],
            sources: vec![],
            description: None,
            tags: Tags {
                artist: artist.map(str::to_string).into_iter().collect(),
                general,
                ..Tags::default()
            },
        }
    }

    fn breakdown() -> ScoreBreakdown {
        ScoreBreakdown {
            tag_similarity: 0.0,
            quality_fit: 0.0,
            recency_fit: 0.0,
            rating_fit: 0.0,
            media_fit: 0.0,
            popularity_fit: 0.0,
            interaction_fit: 0.0,
            tag_relation_fit: 0.0,
            uploader_fit: 0.0,
            exclusivity_fit: 0.0,
            novelty_fit: 0.0,
            artist_discovery_fit: 0.0,
        }
    }

    fn scored(post: Post, bd: Option<ScoreBreakdown>) -> ScoredPost {
        ScoredPost {
            post,
            score: 0.5,
            breakdown: bd,
            reasons: Vec::new(),
        }
    }

    #[test]
    fn no_breakdown_yields_empty() {
        let mut sp = scored(post(None, vec![]), None);
        explain_scored_posts(std::slice::from_mut(&mut sp), &HashSet::new(), true);
        assert!(sp.reasons.is_empty());
    }

    #[test]
    fn artist_discovery_names_new_artist() {
        let mut bd = breakdown();
        bd.artist_discovery_fit = 0.8;
        let mut sp = scored(post(Some("test_artist"), vec![]), Some(bd));
        explain_scored_posts(std::slice::from_mut(&mut sp), &HashSet::new(), true);
        assert!(sp.reasons.iter().any(|r| r.contains("test_artist")));
    }

    #[test]
    fn taste_match_names_user_tag() {
        let mut bd = breakdown();
        bd.tag_similarity = 0.9;
        let mut user = HashSet::new();
        user.insert("dragon".to_string());
        let mut sp = scored(
            post(Some("a"), vec!["dragon".to_string(), "scalie".to_string()]),
            Some(bd),
        );
        explain_scored_posts(std::slice::from_mut(&mut sp), &user, true);
        assert!(sp.reasons.iter().any(|r| r.contains("dragon")));
    }

    #[test]
    fn exploration_pick_when_weighted() {
        let mut bd = breakdown();
        bd.novelty_fit = 0.9;
        let mut sp = scored(post(Some("a"), vec![]), Some(bd));
        explain_scored_posts(std::slice::from_mut(&mut sp), &HashSet::new(), true);
        assert!(sp.reasons.iter().any(|r| r.contains("Exploration pick")));
    }

    #[test]
    fn reasons_capped_at_three() {
        let mut bd = breakdown();
        bd.artist_discovery_fit = 0.8;
        bd.tag_similarity = 0.9;
        bd.interaction_fit = 0.9;
        bd.novelty_fit = 0.9;
        let mut user = HashSet::new();
        user.insert("dragon".to_string());
        let mut sp = scored(post(Some("artist_a"), vec!["dragon".to_string()]), Some(bd));
        explain_scored_posts(std::slice::from_mut(&mut sp), &user, true);
        assert_eq!(sp.reasons.len(), 3);
    }

    #[test]
    fn uploader_reason_names_uploader() {
        let mut bd = breakdown();
        bd.uploader_fit = 0.9;
        let mut p = post(Some("a"), vec![]);
        p.uploader_name = Some("big_artist".to_string());
        let mut sp = scored(p, Some(bd));
        explain_scored_posts(std::slice::from_mut(&mut sp), &HashSet::new(), true);
        assert!(sp.reasons.iter().any(|r| r.contains("big_artist")));
    }

    #[test]
    fn popularity_fallback_when_no_personal_signal() {
        let mut bd = breakdown();
        bd.popularity_fit = 0.8;
        bd.rating_fit = 0.7;
        bd.quality_fit = 0.6;
        let mut sp = scored(post(Some("a"), vec![]), Some(bd));
        explain_scored_posts(std::slice::from_mut(&mut sp), &HashSet::new(), false);
        assert!(!sp.reasons.is_empty());
        assert!(sp.reasons.iter().any(|r| r.contains("Popular with others")));
    }
}
