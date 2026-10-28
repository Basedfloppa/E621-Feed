//! Concise human-readable "why this post?" explanations for recommendations.
//!
//! A [`ScoreBreakdown`] is a set of per-channel fits (tag similarity, artist
//! discovery, …). The reason a post landed where it did is the **weighted
//! contribution** of each channel — `mix_* × fit`, the same product
//! `blend_score` sums — so the reasons are ranked by that contribution instead
//! of by a hard-coded priority list. With the shipped weights tag similarity
//! carries ~88 % of the blend, which is why "matches your «…» taste" is usually
//! the first reason, while a channel with a large fit but a tiny weight (say
//! recency at 0.6 %) can no longer claim to have decided the pick.
//!
//! Reasons stay best-effort: a post with no breakdown, or one whose every
//! channel contributes nothing, comes back with zero reasons and the UI falls
//! back to its own generic labels.

use std::collections::HashMap;

use super::scorer::{MixWeights, Priors};
use crate::models::{Post, ScoredPost};
use crate::utils::IdfIndex;

/// One channel that can be named in a reason.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Signal {
    ArtistDiscovery,
    Taste,
    Uploader,
    Novelty,
    Exclusivity,
    TagRelation,
    Recency,
    Popularity,
    Quality,
}

/// Populate `reasons` on each scored post with concise explanations, most
/// contributing channel first.
///
/// `user_tags` maps the account's lowercased tag names to their occurrence
/// count; it is used only to name the tag that carries the most weight in the
/// account's taste, never to re-score.
pub fn explain_scored_posts(
    scored: &mut [ScoredPost],
    user_tags: &HashMap<String, f32>,
    priors: &Priors,
    idf: &IdfIndex,
    exploration_active: bool,
) {
    let weights = MixWeights::from_priors(priors);
    for sp in scored {
        sp.reasons = explain_one(
            &sp.post,
            sp.breakdown.as_ref(),
            user_tags,
            &weights,
            priors,
            idf,
            exploration_active,
        );
    }
}

fn explain_one(
    post: &Post,
    breakdown: Option<&crate::models::ScoreBreakdown>,
    user_tags: &HashMap<String, f32>,
    w: &MixWeights,
    priors: &Priors,
    idf: &IdfIndex,
    exploration_active: bool,
) -> Vec<String> {
    let Some(b) = breakdown else {
        return Vec::new();
    };

    // Contribution of each channel to the score, exactly as `blend_score`
    // computes it. Channels without a sentence of their own (rating, media) are
    // left out: they carry weight, but there is nothing concrete to name.
    let mut ranked: [(f32, Signal); 9] = [
        (
            w.artist_discovery * b.artist_discovery_fit,
            Signal::ArtistDiscovery,
        ),
        (w.sim * b.tag_similarity, Signal::Taste),
        (w.uploader * b.uploader_fit, Signal::Uploader),
        (w.novelty * b.novelty_fit, Signal::Novelty),
        (w.exclusivity * b.exclusivity_fit, Signal::Exclusivity),
        (w.tag_relation * b.tag_relation_fit, Signal::TagRelation),
        (w.recency * b.recency_fit, Signal::Recency),
        (w.popularity * b.popularity_fit, Signal::Popularity),
        (w.quality * b.quality_fit, Signal::Quality),
    ];
    ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    let strongest = ranked[0].0.max(0.0);
    // Drop contributors under 1 % of the strongest one: they are noise in the
    // ordering, not reasons a reader would recognise.
    let floor = (strongest * 0.01).max(1e-5);

    let mut reasons: Vec<String> = Vec::with_capacity(3);
    for (contribution, signal) in ranked {
        if reasons.len() == 3 || contribution <= floor {
            break;
        }
        if let Some(text) = reason_text(signal, post, user_tags, priors, idf, exploration_active)
            && !reasons.contains(&text)
        {
            reasons.push(text);
        }
    }
    reasons
}

/// The sentence for one channel, or `None` when the post cannot substantiate
/// it (no artist name, no shared tag, …). A missing sentence must not push a
/// weaker channel into its place, so callers skip the entry instead of
/// inventing a generic line.
fn reason_text(
    signal: Signal,
    post: &Post,
    user_tags: &HashMap<String, f32>,
    priors: &Priors,
    idf: &IdfIndex,
    exploration_active: bool,
) -> Option<String> {
    match signal {
        Signal::ArtistDiscovery => post
            .tags
            .artist
            .first()
            .filter(|a| !a.is_empty())
            .map(|artist| format!("New artist near your tastes: {artist}")),
        // Tag similarity is a cosine over tag weights: without a shared tag it
        // can only come from the relation graph, which the next reason covers —
        // do not emit a vague duplicate.
        // An artist name reads badly as a "taste": say what it is instead.
        Signal::Taste => best_shared_tag(post, user_tags, priors, idf).map(|(tag, artist)| {
            if artist {
                format!("From {tag} — an artist you favourite")
            } else {
                format!("Matches your “{tag}” taste")
            }
        }),
        Signal::Uploader => post
            .uploader_name
            .as_deref()
            .filter(|n| !n.is_empty())
            .map(|name| format!("From {name} — an uploader you tend to favourite")),
        Signal::Novelty => Some(if exploration_active {
            "Exploration pick — stepping outside your usual picks".to_string()
        } else {
            "Tags you have not saved before".to_string()
        }),
        Signal::Exclusivity => Some("Unusual tag combination".to_string()),
        Signal::TagRelation => Some("Related to tags you like".to_string()),
        Signal::Recency => Some("Freshly posted".to_string()),
        Signal::Popularity => Some("Popular with others right now".to_string()),
        Signal::Quality => Some("High-quality post".to_string()),
    }
}

/// The shared tag that did the most to make this post similar to the profile.
///
/// The weight is the same term the tag-similarity cosine sums —
/// `saturate(count)^freq_alpha × group_weight × idf` — so the sentence names
/// the tag that actually carried the match instead of the account's most
/// frequent one. Ranking by raw frequency alone picked `anthro` (1000+
/// favourites, present on most of the site) over `bear` (26 favourites,
/// distinctive) even though the specific tag is what identifies the post.
///
/// Ties keep the category order of the group list (character and species before
/// copyright and meta noise), because a specific tag reads better in the
/// sentence than the franchise it belongs to.
fn best_shared_tag<'a>(
    post: &'a Post,
    user_tags: &HashMap<String, f32>,
    priors: &Priors,
    idf: &IdfIndex,
) -> Option<(&'a str, bool)> {
    let groups: [(&[String], f32, bool); 6] = [
        (&post.tags.character, priors.group_w_character, false),
        (&post.tags.species, priors.group_w_species, false),
        (&post.tags.general, priors.group_w_general, false),
        (&post.tags.copyright, priors.group_w_copyright, false),
        (&post.tags.lore, priors.group_w_lore, false),
        (&post.tags.artist, priors.group_w_artist, true),
    ];
    let mut best: Option<(&'a str, f32, bool)> = None;
    for (tags, group_weight, is_artist) in groups {
        if group_weight <= 0.0 {
            continue;
        }
        for tag in tags {
            let lc = tag.trim().to_lowercase();
            let Some(&count) = user_tags.get(&lc) else {
                continue;
            };
            let idf_weight = idf.idf_tempered(
                &lc,
                priors.df_floor,
                priors.idf_max,
                priors.idf_rsj_smoothing,
                priors.idf_lambda,
                priors.idf_alpha,
            );
            // The weight `ContextBase` puts on this tag, term for term:
            // BM25-saturated count^freq_alpha × group weight × idf. Saturation
            // matters — 1000 and 26 favourites differ by 1.7x here, not 38x —
            // and it is what lets a distinctive tag outrank a generic one that
            // is merely frequent.
            let k = priors.bm25_k.max(1e-3);
            let tf = count.max(0.0);
            let saturated = (tf * (k + 1.0)) / (tf + k);
            let contribution = saturated.powf(priors.freq_alpha) * group_weight * idf_weight;
            if best.is_none_or(|(_, best_weight, _)| contribution > best_weight) {
                best = Some((tag.as_str(), contribution, is_artist));
            }
        }
    }
    best.map(|(tag, _, is_artist)| (tag, is_artist))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        Files, Flags, Has, Post, Rating, Relationships, ScoreBreakdown, Stats, Tags,
    };
    use crate::utils::scorer::Priors;

    fn post(artist: Option<&str>, general: Vec<&str>) -> Post {
        Post {
            id: 1,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
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
                general: general.into_iter().map(str::to_string).collect(),
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

    fn tags(pairs: &[(&str, f32)]) -> HashMap<String, f32> {
        pairs
            .iter()
            .map(|(name, weight)| ((*name).to_string(), *weight))
            .collect()
    }

    /// Every channel carries a real weight, so "the channel with the largest
    /// fit" is never confused with "the channel the blend actually listens
    /// to". `Priors::default()` has several `mix_*` at zero, which would make
    /// those channels silently unnameable.
    fn test_priors() -> Priors {
        Priors {
            mix_sim: 1.0,
            mix_quality: 0.1,
            mix_recency: 0.1,
            mix_rating: 0.1,
            mix_media: 0.1,
            mix_popularity: 0.1,
            mix_tag_relation: 0.1,
            mix_uploader: 0.1,
            mix_exclusivity: 0.1,
            mix_novelty: 0.1,
            mix_artist_discovery: 0.1,
            ..Priors::default()
        }
    }

    /// A DF table that makes `anthro` generic (60 % of the catalog) and
    /// `bear`/`dragon`/`scalie` specific, so the tag ranking can be exercised.
    fn test_idf() -> IdfIndex {
        let mut df = HashMap::new();
        df.insert("anthro".to_string(), 6000);
        df.insert("male".to_string(), 5000);
        df.insert("dragon".to_string(), 100);
        df.insert("scalie".to_string(), 200);
        df.insert("bear".to_string(), 200);
        IdfIndex::from_df(&df, 10_000)
    }

    fn explain(sp: &mut ScoredPost, user: &HashMap<String, f32>, exploration: bool) -> Vec<String> {
        explain_with(sp, user, exploration, &test_priors())
    }

    fn explain_with(
        sp: &mut ScoredPost,
        user: &HashMap<String, f32>,
        exploration: bool,
        priors: &Priors,
    ) -> Vec<String> {
        let idf = test_idf();
        explain_scored_posts(std::slice::from_mut(sp), user, priors, &idf, exploration);
        sp.reasons.clone()
    }

    #[test]
    fn no_breakdown_yields_empty() {
        let mut sp = scored(post(None, vec![]), None);
        assert!(explain(&mut sp, &tags(&[]), true).is_empty());
    }

    #[test]
    fn artist_discovery_names_new_artist() {
        let mut bd = breakdown();
        bd.artist_discovery_fit = 0.8;
        let mut sp = scored(post(Some("test_artist"), vec![]), Some(bd));
        let reasons = explain(&mut sp, &tags(&[]), true);
        assert!(reasons.iter().any(|r| r.contains("test_artist")));
    }

    #[test]
    fn taste_match_names_user_tag() {
        let mut bd = breakdown();
        bd.tag_similarity = 0.9;
        let mut sp = scored(post(Some("a"), vec!["dragon", "scalie"]), Some(bd));
        let reasons = explain(&mut sp, &tags(&[("dragon", 5.0)]), true);
        assert!(reasons.iter().any(|r| r.contains("dragon")));
    }

    #[test]
    fn taste_match_prefers_the_heaviest_shared_tag() {
        let mut bd = breakdown();
        bd.tag_similarity = 0.9;
        let mut sp = scored(post(Some("a"), vec!["dragon", "scalie"]), Some(bd));
        let reasons = explain(&mut sp, &tags(&[("dragon", 1.0), ("scalie", 50.0)]), true);
        assert!(
            reasons.iter().any(|r| r.contains("scalie")),
            "the tag with the higher account weight must be named, got {reasons:?}"
        );
    }

    /// The sentence must name the tag that carried the match, not the account's
    /// most frequent one: `anthro` is favourited 20x more often than `bear`, but
    /// it appears on most of the catalog, so `bear` is what makes this post
    /// recognisably "yours".
    #[test]
    fn taste_match_prefers_the_specific_tag_over_a_generic_one() {
        let mut bd = breakdown();
        bd.tag_similarity = 0.9;
        let mut sp = scored(post(Some("a"), vec!["anthro", "male", "bear"]), Some(bd));
        let reasons = explain(
            &mut sp,
            &tags(&[("anthro", 1000.0), ("male", 900.0), ("bear", 26.0)]),
            false,
        );
        assert!(
            reasons.iter().any(|r| r.contains("bear")),
            "a specific shared tag must beat a generic heavier one, got {reasons:?}"
        );
    }

    #[test]
    fn exploration_pick_when_weighted() {
        let mut bd = breakdown();
        bd.novelty_fit = 0.9;
        let mut sp = scored(post(Some("a"), vec![]), Some(bd));
        let reasons = explain(&mut sp, &tags(&[]), true);
        assert!(reasons.iter().any(|r| r.contains("Exploration pick")));
    }

    #[test]
    fn novelty_is_not_called_exploration_when_the_knob_is_off() {
        let mut bd = breakdown();
        bd.novelty_fit = 0.9;
        let mut sp = scored(post(Some("a"), vec![]), Some(bd));
        let reasons = explain(&mut sp, &tags(&[]), false);
        assert!(reasons.iter().any(|r| r.contains("not saved before")));
        assert!(!reasons.iter().any(|r| r.contains("Exploration pick")));
    }

    #[test]
    fn reasons_capped_at_three() {
        let mut bd = breakdown();
        bd.artist_discovery_fit = 0.8;
        bd.tag_similarity = 0.9;
        bd.novelty_fit = 0.9;
        let mut sp = scored(post(Some("artist_a"), vec!["dragon"]), Some(bd));
        let reasons = explain(&mut sp, &tags(&[("dragon", 1.0)]), true);
        assert_eq!(reasons.len(), 3);
    }

    #[test]
    fn uploader_reason_names_uploader() {
        let mut bd = breakdown();
        bd.uploader_fit = 0.9;
        let mut p = post(Some("a"), vec![]);
        p.uploader_name = Some("big_artist".to_string());
        let mut sp = scored(p, Some(bd));
        let reasons = explain(&mut sp, &tags(&[]), true);
        assert!(reasons.iter().any(|r| r.contains("big_artist")));
    }

    #[test]
    fn popularity_fallback_when_no_personal_signal() {
        let mut bd = breakdown();
        bd.popularity_fit = 0.8;
        bd.rating_fit = 0.7;
        bd.quality_fit = 0.6;
        let mut sp = scored(post(Some("a"), vec![]), Some(bd));
        let reasons = explain(&mut sp, &tags(&[]), false);
        assert!(!reasons.is_empty());
        assert!(reasons.iter().any(|r| r.contains("Popular with others")));
    }

    /// The reason list is ordered by weighted contribution, not by a fixed
    /// priority list. A small but perfectly-fitting channel (tag similarity at
    /// 0.9) must rank above a large but lightly weighted one (novelty at 1.0
    /// with `mix_novelty ≈ 0.008` of the blend).
    #[test]
    fn reasons_are_ordered_by_contribution() {
        let mut bd = breakdown();
        bd.tag_similarity = 0.9;
        bd.novelty_fit = 1.0;
        let mut sp = scored(post(Some("a"), vec!["dragon"]), Some(bd));
        let reasons = explain(&mut sp, &tags(&[("dragon", 3.0)]), false);
        assert!(reasons[0].contains("dragon"), "got {reasons:?}");
        assert!(
            reasons.iter().any(|r| r.contains("not saved before")),
            "the novelty runner-up should still be named, got {reasons:?}"
        );
    }

    /// A channel with a huge fit but a weight that rounds to nothing is not a
    /// reason: recency at `mix_recency = 0.017/2.575` contributes ~0.6 % of the
    /// score even at fit 1.0, so it must not be presented as the explanation of
    /// a strong tag match.
    #[test]
    fn a_negligible_weight_is_not_used_as_the_reason() {
        let mut bd = breakdown();
        bd.tag_similarity = 0.9;
        bd.recency_fit = 1.0;
        let mut sp = scored(post(Some("a"), vec!["dragon"]), Some(bd));
        let mut priors = test_priors();
        priors.mix_recency = 1e-4;
        let reasons = explain_with(&mut sp, &tags(&[("dragon", 3.0)]), false, &priors);
        assert_eq!(reasons.len(), 1, "got {reasons:?}");
        assert!(reasons[0].contains("dragon"));
    }
}
