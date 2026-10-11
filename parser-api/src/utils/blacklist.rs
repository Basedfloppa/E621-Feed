//! Account-blacklist parsing, shared by the live e621 query builder and the
//! in-process filters.
//!
//! A blacklist entry names what to hide. A leading `-` is decoration, not
//! negation: e621 hands such entries back through `blacklisted_tags`, and a pasted
//! blob of the shape `--fav:me -4_balls -id:123` is what used to 422 the API.
//! Reading that dash as negation would turn every one of those exclusions into a
//! *requirement* and rebuild the feed around a handful of tags — for exactly the
//! accounts that filter most. So the dash is stripped and the tag stays excluded.
//!
//! The one thing an exclusion cannot express is "hide posts that LACK a tag",
//! which is what the post card's "Block static images" needs: e621 has no static
//! tag to exclude. `require:X` is that form. e621 has no negative-inclusion
//! syntax, so a requirement becomes a bare *positive* tag in the live query and is
//! re-checked in-process — see [`query_terms`].

use std::collections::HashSet;

use crate::models::{Post, Rating};

/// Marks a *requirement* — "hide posts that lack this tag" — rather than the
/// usual "hide posts that carry it".
const REQUIRE_PREFIX: &str = "require:";

/// The media metatags `require:` is defined for.
///
/// Deliberately only these two. A requirement reaches e621 as a bare positive
/// tag, so an arbitrary payload would silently rebuild the whole request around
/// one tag, and a typo would do it without a trace. The allow-list is why
/// `require:groe` is ignored rather than obeyed.
const REQUIRE_TAGS: [&str; 2] = ["video", "animated"];

/// A blacklist split into the two things the live e621 request needs from it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct QueryTerms {
    /// Tags the query must carry as positives, so e621 drops the posts lacking
    /// them. The only way an exclusion engine can express a requirement.
    pub required: Vec<String>,
    /// Tags the query must exclude.
    pub excluded: Vec<String>,
}

/// Blacklist rules: one whitespace-separated token each, comments and blank runs
/// dropped, leading `-` stripped.
///
/// Token-split rather than line-split because the e621 request does the same with
/// the same text — a line holding two tags has to mean the same thing to the local
/// filter as it does to the query.
fn rules(blacklist: &str) -> impl Iterator<Item = &str> {
    blacklist
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .flat_map(str::split_whitespace)
        .map(|rule| rule.trim_start_matches('-'))
        .filter(|rule| !rule.is_empty())
}

/// The payload of any `require:` rule, supported or not.
fn require_payload(rule: &str) -> Option<&str> {
    let prefix = rule.get(..REQUIRE_PREFIX.len())?;
    prefix
        .eq_ignore_ascii_case(REQUIRE_PREFIX)
        .then(|| &rule[REQUIRE_PREFIX.len()..])
}

/// `Some(tag)` when `rule` is a *supported* `require:` rule.
fn require_tag(rule: &str) -> Option<&'static str> {
    let payload = require_payload(rule)?;
    REQUIRE_TAGS
        .iter()
        .copied()
        .find(|tag| payload.eq_ignore_ascii_case(tag))
}

fn has_tag_in(post: &Post, tag: &str) -> bool {
    let t = &post.tags;
    t.general
        .iter()
        .chain(&t.artist)
        .chain(&t.copyright)
        .chain(&t.character)
        .chain(&t.species)
        .chain(&t.invalid)
        .chain(&t.meta)
        .chain(&t.lore)
        .chain(&t.contributor)
        .any(|value| value.eq_ignore_ascii_case(tag))
}

fn has_meta_tag(post: &Post, tag: &str) -> bool {
    post.tags
        .meta
        .iter()
        .any(|value| value.eq_ignore_ascii_case(tag))
}

/// True when `post` matches any blacklist rule that can be evaluated locally.
///
/// This is the authoritative filter, not a fallback for cached posts: e621
/// receives at most `e621_tag_limit` of the exclusions, so live posts must be
/// re-checked against the full text here. Rules whose meaning depends on e621's
/// own engine (`fav:me`, `id:123`, an unknown metatag) cannot be evaluated and are
/// ignored — and `-id:123` in particular must not reach the tag arm below, where
/// "not the tag `id:123`" would match every post and empty the feed.
pub fn matches(post: &Post, blacklist: &str) -> bool {
    matches_impl(post, blacklist, true)
}

/// The part of [`matches`] that needs no per-post tag walk beyond the meta group:
/// requirements plus the media/rating/uploader rules. The digest's first pass uses
/// this to keep its historical breadth, leaving plain tags to [`simple_tags`].
pub fn matches_metatag(post: &Post, blacklist: &str) -> bool {
    matches_impl(post, blacklist, false)
}

fn matches_impl(post: &Post, blacklist: &str, all_tags: bool) -> bool {
    rules(blacklist).any(|rule| {
        if let Some(tag) = require_tag(rule) {
            return !has_meta_tag(post, tag);
        }
        // `uploader:!<id>` is this project's own spelling for "hide this
        // uploader" — written by the post card, and the only rule e621 cannot
        // evaluate for us. The `!` belongs to the id, not to a negation.
        if let Some(id) = rule.strip_prefix("uploader:!") {
            return id.parse::<i64>().is_ok_and(|id| id == post.uploader_id);
        }
        match rule {
            "rating:s" | "rating:safe" => return matches!(post.rating, Rating::S),
            "rating:q" | "rating:questionable" => return matches!(post.rating, Rating::Q),
            "rating:e" | "rating:explicit" => return matches!(post.rating, Rating::E),
            // e621 files both as meta tags; checked directly so this pass needs no
            // full tag walk.
            "video" | "animated" => return has_meta_tag(post, rule),
            _ => {}
        }
        if !all_tags || rule.contains(':') {
            return false;
        }
        has_tag_in(post, rule)
    })
}

/// Split a blacklist into what the live e621 request has to say about it.
///
/// An unsupported `require:` payload is dropped outright rather than forwarded as
/// `-require:x`, which e621 cannot parse and which would contradict the marker.
pub fn query_terms(blacklist: &str) -> QueryTerms {
    let mut terms = QueryTerms::default();
    for rule in rules(blacklist) {
        if let Some(tag) = require_tag(rule) {
            terms.required.push((*tag).to_string());
        } else if require_payload(rule).is_none() {
            terms.excluded.push(rule.to_string());
        }
    }
    terms
}

/// Plain tag names in `blacklist` — the subset usable for the DB-side pre-filter
/// and the scoring-level soft filter.
///
/// Every metatag carries a `:`, which also covers `require:`: a requirement is not
/// a tag to down-weight, and a name no post carries would only dilute the IDF
/// prior.
pub fn simple_tags(blacklist: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    rules(blacklist)
        .filter(|rule| !rule.contains(':'))
        .filter_map(|rule| {
            let tag = rule.to_lowercase();
            seen.insert(tag.clone()).then_some(tag)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Files, Flags, Has, Relationships, Score, Stats, Tags};
    use chrono::Utc;

    fn post(tags: Tags, rating: Rating, uploader_id: i64) -> Post {
        Post {
            id: 1,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            change_seq: 0.0,
            files: Files::default(),
            uploader_id,
            uploader_name: None,
            approver_id: None,
            stats: Stats {
                score: Score::default(),
                ..Default::default()
            },
            flags: Flags::default(),
            has: Has::default(),
            relationships: Relationships::default(),
            pools: vec![],
            rating,
            locked_tags: vec![],
            sources: vec![],
            description: None,
            tags,
        }
    }

    fn static_post() -> Post {
        post(
            Tags {
                general: vec!["solo".into()],
                ..Default::default()
            },
            Rating::S,
            7,
        )
    }

    fn animated_post() -> Post {
        post(
            Tags {
                general: vec!["gore".into()],
                meta: vec!["animated".into()],
                ..Default::default()
            },
            Rating::E,
            9,
        )
    }

    #[test]
    fn a_leading_dash_is_decoration_not_negation() {
        // Both spellings exclude the tag. The negated spelling comes from e621's
        // own `blacklisted_tags` and from pasted blobs; reading it as a
        // requirement would hide every post *without* the tag instead, which is
        // how a single line used to empty a feed.
        assert!(matches(&animated_post(), "gore"));
        assert!(matches(&animated_post(), "-gore"));
        assert!(!matches(&static_post(), "gore"));
        assert!(!matches(&static_post(), "-gore"));
    }

    #[test]
    fn require_hides_what_lacks_the_media_metatag() {
        assert!(matches(&static_post(), "require:animated"));
        assert!(!matches(&animated_post(), "require:animated"));
        // The dash in front of the marker is decoration like everywhere else.
        assert!(matches(&static_post(), "-require:animated"));
        // Case does not matter on either the marker or the payload.
        assert!(matches(&static_post(), "REQUIRE:Animated"));
    }

    #[test]
    fn require_video_hides_everything_that_is_not_video() {
        assert!(matches(&static_post(), "require:video"));
        assert!(matches(&animated_post(), "require:video"));
        let video = post(
            Tags {
                meta: vec!["video".into()],
                ..Default::default()
            },
            Rating::S,
            1,
        );
        assert!(!matches(&video, "require:video"));
    }

    #[test]
    fn an_unsupported_require_payload_is_ignored() {
        // `require:` is an allow-list, not a parser: an unknown payload must not
        // turn into "hide everything that lacks this tag" for an arbitrary tag.
        assert!(!matches(&static_post(), "require:gore"));
        assert!(!matches(&animated_post(), "require:gore"));
        assert!(!matches(&static_post(), "require:"));
    }

    #[test]
    fn an_unknown_metatag_is_ignored_rather_than_matched_as_a_tag() {
        for rule in ["id:123", "-id:123", "fav:me", "-fav:me", "foo:bar"] {
            assert!(!matches(&static_post(), rule), "{rule} must not match");
            assert!(!matches(&animated_post(), rule), "{rule} must not match");
        }
    }

    #[test]
    fn rating_and_uploader_rules_match_their_post() {
        assert!(matches(&static_post(), "rating:s"));
        assert!(matches(&static_post(), "rating:safe"));
        assert!(!matches(&animated_post(), "rating:s"));
        assert!(matches(&animated_post(), "rating:e"));
        assert!(matches(&static_post(), "uploader:!7"));
        assert!(!matches(&static_post(), "uploader:!9"));
        // A non-numeric uploader id is not a match, and not a panic.
        assert!(!matches(&static_post(), "uploader:!me"));
    }

    #[test]
    fn both_media_metatags_hide_their_own_kind() {
        assert!(matches(&animated_post(), "animated"));
        assert!(!matches(&static_post(), "animated"));
        assert!(!matches(&animated_post(), "video"));
    }

    #[test]
    fn every_token_of_a_line_is_a_rule_and_comments_are_skipped() {
        assert!(matches(&static_post(), "solo gore"));
        assert!(matches(&static_post(), "gore solo"));
        assert!(!matches(&static_post(), "# solo\nnope"));
        assert!(matches(&static_post(), "# comment\nsolo"));
        // A dash-only token carries no tag and is dropped, not treated as empty.
        assert!(!matches(&static_post(), "---"));
    }

    #[test]
    fn query_terms_separate_requirements_from_exclusions() {
        let terms = query_terms("gore\n-animated\nrequire:animated\n--fav:me");
        assert_eq!(terms.required, vec!["animated"]);
        assert_eq!(terms.excluded, vec!["gore", "animated", "fav:me"]);
    }

    #[test]
    fn query_terms_drop_an_unsupported_require_payload() {
        // Neither required nor excluded: sending `-require:gore` to e621 would be
        // a tag name it cannot resolve.
        let terms = query_terms("require:gore");
        assert_eq!(terms, QueryTerms::default());
    }

    #[test]
    fn simple_tags_skip_metatags_and_requirements() {
        // `-animated` narrows to the plain tag exactly as the query does, and
        // `require:animated` is dropped with every other metatag: a requirement
        // is not a name to down-weight in the IDF prior.
        let tags = simple_tags("gore\n-animated\nrating:s\nuploader:!7\nrequire:animated\nGORE");
        assert_eq!(tags, vec!["gore", "animated"]);
    }

    #[test]
    fn the_media_pass_keeps_its_narrow_breadth() {
        // The digest's first pass must not start hiding plain tags: it leaves them
        // to its own simple-tag check, and widening it here would silently drop
        // posts from every digest.
        assert!(!matches_metatag(&animated_post(), "gore"));
        assert!(matches_metatag(&animated_post(), "animated"));
        assert!(matches_metatag(&static_post(), "require:animated"));
        assert!(matches_metatag(&static_post(), "rating:s"));
        assert!(matches_metatag(&static_post(), "uploader:!7"));
    }
}
