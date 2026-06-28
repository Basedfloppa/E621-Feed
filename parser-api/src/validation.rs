//! Input validation for HTTP-facing structs.
//!
//! Routes accepting `DeviceScopedAccount`, `owner_token`, or `account_id`
//! push input through here before any DB work. Limits are loose enough
//! to accept any real e621 user while rejecting obvious abuse:
//! * `id`        — strictly positive, capped above e621's current id space.
//! * `name`      — ≤ 64 chars, printable ASCII subset.
//! * `blacklist` — ≤ 16 KB.
//! * `owner_token` — 16..=128 chars from `[A-Za-z0-9_-]`.

use crate::errors::ApiError;
use crate::models::{
    BatchInteractionRequest, BlacklistPayload, DeviceScopedAccount, FeedInteractionRequest,
    PreferredTagPayload,
};

const MAX_ACCOUNT_ID: i32 = 100_000_000;
const MAX_NAME_LEN: usize = 64;
const MAX_BLACKLIST_LEN: usize = 16 * 1024;
const MIN_OWNER_TOKEN_LEN: usize = 16;
const MAX_OWNER_TOKEN_LEN: usize = 128;
const MAX_SESSION_ID_LEN: usize = 128;
/// e621 caps `posts.json?page=` at 750 for non-staff. Anything past that
/// returns `410 Gone`; reject locally first.
pub const MAX_RECOMMENDATIONS_PAGE: i32 = 750;

pub fn validate_account_id(id: i32) -> Result<(), ApiError> {
    if id <= 0 || id > MAX_ACCOUNT_ID {
        return Err(ApiError::BadRequest(format!(
            "invalid account id {id}: must be in 1..={MAX_ACCOUNT_ID}"
        )));
    }
    Ok(())
}

pub fn validate_owner_token(owner_token: &str) -> Result<(), ApiError> {
    let len = owner_token.len();
    if !(MIN_OWNER_TOKEN_LEN..=MAX_OWNER_TOKEN_LEN).contains(&len) {
        return Err(ApiError::BadRequest(format!(
            "owner_token length {len} not in {MIN_OWNER_TOKEN_LEN}..={MAX_OWNER_TOKEN_LEN}"
        )));
    }
    if !owner_token
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(ApiError::BadRequest(
            "owner_token must contain only [A-Za-z0-9_-]".into(),
        ));
    }
    Ok(())
}

fn validate_account_name(name: &str) -> Result<(), ApiError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(ApiError::BadRequest("name must not be empty".into()));
    }
    // Use char count so multi-byte glyphs don't slip past via byte length.
    let char_len = trimmed.chars().count();
    if char_len > MAX_NAME_LEN {
        return Err(ApiError::BadRequest(format!(
            "name too long ({char_len} chars, max {MAX_NAME_LEN})"
        )));
    }
    // Printable ASCII subset covering historical e621 usernames; reject
    // control bytes and non-ASCII to keep storage / display predictable.
    let allowed = |c: char| {
        c.is_ascii_alphanumeric()
            || matches!(
                c,
                '_' | '-' | '.' | '~' | ' ' | '\'' | '(' | ')' | '!' | '@'
            )
    };
    if !trimmed.chars().all(allowed) {
        return Err(ApiError::BadRequest(
            "name contains characters that are not allowed".into(),
        ));
    }
    Ok(())
}

const MAX_BLACKLIST_LINE_LEN: usize = 64;

fn validate_blacklist_text(blacklist: &str) -> Result<(), ApiError> {
    if blacklist.len() > MAX_BLACKLIST_LEN {
        return Err(ApiError::BadRequest(format!(
            "blacklist too long ({} bytes, max {MAX_BLACKLIST_LEN})",
            blacklist.len()
        )));
    }
    if blacklist.contains('\0') {
        return Err(ApiError::BadRequest(
            "blacklist must not contain NUL bytes".into(),
        ));
    }

    let lower = blacklist.to_ascii_lowercase();
    for needle in ["<script", "javascript:", "data:text/html", "vbscript:"] {
        if lower.contains(needle) {
            return Err(ApiError::BadRequest(format!(
                "blacklist contains forbidden substring '{needle}'"
            )));
        }
    }
    for c in blacklist.chars() {
        if c.is_control() && c != '\n' && c != '\r' && c != '\t' {
            return Err(ApiError::BadRequest(
                "blacklist must not contain control characters".into(),
            ));
        }
        if c == '<' || c == '>' {
            return Err(ApiError::BadRequest(
                "blacklist must not contain '<' or '>'".into(),
            ));
        }
    }
    for (idx, line) in blacklist.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.chars().count() > MAX_BLACKLIST_LINE_LEN {
            return Err(ApiError::BadRequest(format!(
                "blacklist line {} too long (max {MAX_BLACKLIST_LINE_LEN} chars)",
                idx + 1
            )));
        }
    }
    Ok(())
}

pub fn normalize_blacklist(blacklist: &str) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut lines: Vec<&str> = Vec::new();
    for raw in blacklist.split('\n') {
        let trimmed = raw.trim_matches(|c: char| c.is_whitespace());
        if trimmed.is_empty() {
            continue;
        }
        if seen.insert(trimmed.to_ascii_lowercase()) {
            lines.push(trimmed);
        }
    }
    lines.join("\n")
}

pub fn validate_device_scoped_account(acc: &DeviceScopedAccount) -> Result<(), ApiError> {
    validate_account_id(acc.id)?;
    validate_account_name(&acc.name)?;
    if let Some(bl) = acc.blacklist.as_deref() {
        validate_blacklist_text(bl)?;
    }
    Ok(())
}

pub fn validate_blacklist_payload(payload: &BlacklistPayload) -> Result<(), ApiError> {
    if let Some(bl) = payload.blacklist.as_deref() {
        validate_blacklist_text(bl)?;
    }
    Ok(())
}

/// Bound the `page` query param on `/recommendations`. `None` (caller
/// omitted the param) is the route's default, accepted as-is. Page 0 is
/// rejected because e621 uses 1-indexed pages and returns 410 Gone for 0.
/// Validate exploration mode parameter. Accepts `None` (disabled) or
/// a finite f32 clamped to `[0.0, 0.5]`. 0 = deterministic (default),
/// higher values = more exploratory / "surprise me".
pub fn validate_exploration(t: Option<f32>) -> Result<Option<f32>, ApiError> {
    let Some(t) = t else {
        return Ok(None);
    };
    if !t.is_finite() {
        return Err(ApiError::BadRequest(
            "exploration must be a finite number".into(),
        ));
    }
    Ok(Some(t.clamp(0.0, 0.5)))
}

pub fn validate_recommendations_page(page: Option<i32>) -> Result<(), ApiError> {
    let Some(p) = page else {
        return Ok(());
    };
    if !(1..=MAX_RECOMMENDATIONS_PAGE).contains(&p) {
        return Err(ApiError::BadRequest(format!(
            "page {p} out of range [1, {MAX_RECOMMENDATIONS_PAGE}]"
        )));
    }
    Ok(())
}

/// Reject NaN / ±Infinity (any comparison against them is always false,
/// silently dropping every post), then clamp into `[0.0, 1.0]`.
pub fn validate_affinity_threshold(t: Option<f32>) -> Result<Option<f32>, ApiError> {
    let Some(t) = t else {
        return Ok(None);
    };
    if !t.is_finite() {
        return Err(ApiError::BadRequest(
            "affinity_threshold must be a finite number".into(),
        ));
    }
    Ok(Some(t.clamp(0.0, 1.0)))
}

/// Validate a session token for feed continuation. Accepts UUID v4/v7
/// or any alphanumeric token 8..=128 chars (no security, just navigation).
pub fn validate_session_token(token: &str) -> Result<(), ApiError> {
    let len = token.len();
    if !(8..=128).contains(&len) {
        return Err(ApiError::BadRequest(format!(
            "session_token length {len} not in 8..=128"
        )));
    }
    if !token
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(ApiError::BadRequest(
            "session_token must contain only [A-Za-z0-9_-]".into(),
        ));
    }
    Ok(())
}

/// Validate count parameter for continuation requests.
pub fn validate_continue_count(count: Option<i32>) -> Result<i32, ApiError> {
    match count {
        None => Ok(20),
        Some(c) if (1..=100).contains(&c) => Ok(c),
        Some(c) => Err(ApiError::BadRequest(format!(
            "count {c} out of range [1, 100]"
        ))),
    }
}

pub fn validate_similar_posts_limit(limit: Option<i32>) -> Result<i64, ApiError> {
    match limit {
        None => Ok(20),
        Some(l) if (1..=100).contains(&l) => Ok(l as i64),
        Some(l) => Err(ApiError::BadRequest(format!(
            "limit {l} out of range [1, 100]"
        ))),
    }
}

pub fn validate_similar_posts_min_overlap(overlap: Option<i32>) -> Result<i32, ApiError> {
    match overlap {
        None => Ok(2),
        Some(o) if (1..=20).contains(&o) => Ok(o),
        Some(o) => Err(ApiError::BadRequest(format!(
            "min_overlap {o} out of range [1, 20]"
        ))),
    }
}

pub fn validate_similar_posts_page(page: Option<i32>) -> Result<i64, ApiError> {
    match page {
        None => Ok(1),
        Some(p) if (1..=100).contains(&p) => Ok(p as i64),
        Some(p) => Err(ApiError::BadRequest(format!(
            "page {p} out of range [1, 100]"
        ))),
    }
}

pub fn validate_feed_interaction(req: &FeedInteractionRequest) -> Result<(), ApiError> {
    validate_account_id(req.account_id)?;
    if req.session_id.len() > MAX_SESSION_ID_LEN {
        return Err(ApiError::BadRequest(format!(
            "session_id too long ({} chars, max {MAX_SESSION_ID_LEN})",
            req.session_id.len()
        )));
    }
    Ok(())
}

pub fn validate_batch_interaction(req: &BatchInteractionRequest) -> Result<(), ApiError> {
    if req.interactions.is_empty() {
        return Err(ApiError::BadRequest(
            "batch must contain at least 1 interaction".into(),
        ));
    }
    if req.interactions.len() > 100 {
        return Err(ApiError::BadRequest(
            "batch max 100 interactions per request".into(),
        ));
    }
    for interaction in &req.interactions {
        validate_feed_interaction(interaction)?;
    }
    Ok(())
}

const MAX_PREFERRED_TAGS: usize = 50;
const MAX_TAG_NAME_LEN: usize = 64;
/// Whitelist of tag groups the scoring pipeline knows how to weight.
/// Mirrors the keys used in `account_tag_counts.group_type` and the
/// `Group` enum inside the scorer. Storing anything else just produces
/// rows that downstream code silently skips — better to reject upfront.
const VALID_TAG_GROUPS: &[&str] = &[
    "artist",
    "character",
    "copyright",
    "general",
    "lore",
    "meta",
    "species",
];

pub fn validate_preferred_tag_payload(payload: &PreferredTagPayload) -> Result<(), ApiError> {
    if payload.preferred_tags.len() > MAX_PREFERRED_TAGS {
        return Err(ApiError::BadRequest(format!(
            "max {MAX_PREFERRED_TAGS} preferred tags per account"
        )));
    }
    // Track (lowercased tag, lowercased group) so the same logical entry
    // can't be sent twice in one payload and silently double its weight
    // on the next read.
    let mut seen: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::with_capacity(payload.preferred_tags.len());
    for pt in &payload.preferred_tags {
        let trimmed = pt.tag.trim();
        if trimmed.is_empty() {
            return Err(ApiError::BadRequest("tag name must not be empty".into()));
        }
        if trimmed.len() > MAX_TAG_NAME_LEN {
            return Err(ApiError::BadRequest(format!(
                "tag name too long (max {MAX_TAG_NAME_LEN} chars)"
            )));
        }
        let weight = pt.weight;
        if !weight.is_finite() || !(0.1..=10.0).contains(&weight) {
            return Err(ApiError::BadRequest(format!(
                "weight {weight} out of range [0.1, 10.0]"
            )));
        }
        let group_trimmed = pt.group.trim();
        if group_trimmed.is_empty() {
            return Err(ApiError::BadRequest("group must not be empty".into()));
        }
        let group_lc = group_trimmed.to_ascii_lowercase();
        if !VALID_TAG_GROUPS.contains(&group_lc.as_str()) {
            return Err(ApiError::BadRequest(format!(
                "group '{group_trimmed}' not one of {VALID_TAG_GROUPS:?}"
            )));
        }
        let key = (trimmed.to_ascii_lowercase(), group_lc);
        if !seen.insert(key) {
            return Err(ApiError::BadRequest(format!(
                "duplicate preferred tag: {trimmed} ({group_trimmed})"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::FeedInteractionType;

    fn is_bad<T>(r: Result<T, ApiError>) -> bool {
        matches!(r, Err(ApiError::BadRequest(_)))
    }

    #[test]
    fn account_id_bounds() {
        assert!(is_bad(validate_account_id(0)));
        assert!(is_bad(validate_account_id(-1)));
        assert!(is_bad(validate_account_id(MAX_ACCOUNT_ID + 1)));
        assert!(validate_account_id(1).is_ok());
        assert!(validate_account_id(MAX_ACCOUNT_ID).is_ok());
    }

    #[test]
    fn owner_token_length_and_charset() {
        assert!(is_bad(validate_owner_token(&"a".repeat(15))));
        assert!(validate_owner_token(&"a".repeat(16)).is_ok());
        assert!(validate_owner_token(&"a".repeat(128)).is_ok());
        assert!(is_bad(validate_owner_token(&"a".repeat(129))));
        assert!(validate_owner_token("AZaz09_-AZaz09_-").is_ok());
        assert!(is_bad(validate_owner_token("token-with-bang!!token")));
    }

    #[test]
    fn account_name_rules() {
        assert!(is_bad(validate_account_name("   ")));
        assert!(is_bad(validate_account_name("")));
        assert!(validate_account_name("valid_user-name.1").is_ok());
        assert!(validate_account_name(&"x".repeat(MAX_NAME_LEN)).is_ok());
        assert!(is_bad(validate_account_name(&"x".repeat(MAX_NAME_LEN + 1))));
        assert!(is_bad(validate_account_name("angle<bracket")));
        // Non-ASCII glyphs are rejected.
        assert!(is_bad(validate_account_name("naïve")));
    }

    #[test]
    fn blacklist_text_rejects_abuse() {
        assert!(validate_blacklist_text("rating:e\ncub").is_ok());
        assert!(is_bad(validate_blacklist_text("ok\0nul")));
        assert!(is_bad(validate_blacklist_text("<script>alert(1)</script>")));
        assert!(is_bad(validate_blacklist_text("javascript:void")));
        assert!(is_bad(validate_blacklist_text("a<b")));
        assert!(is_bad(validate_blacklist_text(
            &"x".repeat(MAX_BLACKLIST_LEN + 1)
        )));
        assert!(is_bad(validate_blacklist_text(
            &"y".repeat(MAX_BLACKLIST_LINE_LEN + 1)
        )));
    }

    #[test]
    fn normalize_blacklist_dedups_and_trims() {
        // Case-insensitive dedup, whitespace trimmed, blank lines dropped;
        // first-seen order and original casing are preserved.
        assert_eq!(normalize_blacklist("a\n  B \n\na\nb"), "a\nB");
        assert_eq!(normalize_blacklist(""), "");
        assert_eq!(normalize_blacklist("   \n  "), "");
        assert_eq!(normalize_blacklist("solo"), "solo");
    }

    #[test]
    fn recommendations_page_bounds() {
        assert!(validate_recommendations_page(None).is_ok());
        assert!(validate_recommendations_page(Some(1)).is_ok());
        assert!(validate_recommendations_page(Some(MAX_RECOMMENDATIONS_PAGE)).is_ok());
        // Page 0 is invalid — e621 is 1-indexed and returns 410 for 0.
        assert!(is_bad(validate_recommendations_page(Some(0))));
        assert!(is_bad(validate_recommendations_page(Some(-3))));
        assert!(is_bad(validate_recommendations_page(Some(
            MAX_RECOMMENDATIONS_PAGE + 1
        ))));
    }

    #[test]
    fn affinity_threshold_rejects_non_finite_and_clamps() {
        assert_eq!(validate_affinity_threshold(None).unwrap(), None);
        assert!(validate_affinity_threshold(Some(f32::NAN)).is_err());
        assert!(validate_affinity_threshold(Some(f32::INFINITY)).is_err());
        assert_eq!(validate_affinity_threshold(Some(0.5)).unwrap(), Some(0.5));
        // Out-of-range values clamp into [0, 1].
        assert_eq!(validate_affinity_threshold(Some(1.7)).unwrap(), Some(1.0));
        assert_eq!(validate_affinity_threshold(Some(-0.4)).unwrap(), Some(0.0));
    }

    #[test]
    fn device_scoped_account_validation() {
        let ok = DeviceScopedAccount {
            id: 42,
            name: "tester".to_string(),
            blacklist: Some("rating:e".to_string()),
        };
        assert!(validate_device_scoped_account(&ok).is_ok());

        let bad_id = DeviceScopedAccount {
            id: 0,
            ..ok.clone()
        };
        assert!(is_bad(validate_device_scoped_account(&bad_id)));

        let bad_name = DeviceScopedAccount {
            name: "bad<name".to_string(),
            ..ok.clone()
        };
        assert!(is_bad(validate_device_scoped_account(&bad_name)));

        // Omitted blacklist (None) is accepted — server applies its default.
        let no_blacklist = DeviceScopedAccount {
            blacklist: None,
            ..ok.clone()
        };
        assert!(validate_device_scoped_account(&no_blacklist).is_ok());
    }

    #[test]
    fn blacklist_payload_validation() {
        assert!(validate_blacklist_payload(&BlacklistPayload { blacklist: None }).is_ok());
        assert!(validate_blacklist_payload(&BlacklistPayload {
            blacklist: Some("cub".to_string()),
        })
        .is_ok());
        assert!(is_bad(validate_blacklist_payload(&BlacklistPayload {
            blacklist: Some("<script".to_string()),
        })));
    }

    #[test]
    fn feed_interaction_validation() {
        let ok = FeedInteractionRequest {
            account_id: 7,
            post_id: 1234,
            event_type: FeedInteractionType::Open,
            position: 3,
            session_id: "sess-1".to_string(),
        };
        assert!(validate_feed_interaction(&ok).is_ok());
        assert!(is_bad(validate_feed_interaction(&FeedInteractionRequest {
            account_id: 0,
            ..ok.clone()
        })));
        assert!(is_bad(validate_feed_interaction(&FeedInteractionRequest {
            session_id: "s".repeat(MAX_SESSION_ID_LEN + 1),
            ..ok.clone()
        })));
    }

    #[test]
    fn batch_interaction_validation() {
        let single = FeedInteractionRequest {
            account_id: 7,
            post_id: 1234,
            event_type: FeedInteractionType::Open,
            position: 3,
            session_id: "sess-1".to_string(),
        };
        assert!(is_bad(validate_batch_interaction(
            &BatchInteractionRequest {
                interactions: vec![],
            }
        )));
        assert!(validate_batch_interaction(&BatchInteractionRequest {
            interactions: vec![single.clone()],
        })
        .is_ok());
        let many = vec![single.clone(); 100];
        assert!(
            validate_batch_interaction(&BatchInteractionRequest { interactions: many }).is_ok()
        );
        let too_many = vec![single; 101];
        assert!(is_bad(validate_batch_interaction(
            &BatchInteractionRequest {
                interactions: too_many,
            }
        )));
    }

    #[test]
    fn preferred_tag_payload_validation() {
        use crate::models::PreferredTag;
        let ok = PreferredTagPayload {
            preferred_tags: vec![PreferredTag {
                tag: "fluffy".to_string(),
                group: "general".to_string(),
                weight: 2.0,
            }],
        };
        assert!(validate_preferred_tag_payload(&ok).is_ok());

        // Empty tag name
        let bad = PreferredTagPayload {
            preferred_tags: vec![PreferredTag {
                tag: "".to_string(),
                group: "general".to_string(),
                weight: 1.0,
            }],
        };
        assert!(is_bad(validate_preferred_tag_payload(&bad)));

        // Weight out of range
        let bad = PreferredTagPayload {
            preferred_tags: vec![PreferredTag {
                tag: "fluffy".to_string(),
                group: "general".to_string(),
                weight: 20.0,
            }],
        };
        assert!(is_bad(validate_preferred_tag_payload(&bad)));

        // Weight negative
        let bad = PreferredTagPayload {
            preferred_tags: vec![PreferredTag {
                tag: "fluffy".to_string(),
                group: "general".to_string(),
                weight: -0.1,
            }],
        };
        assert!(is_bad(validate_preferred_tag_payload(&bad)));

        // Empty group
        let bad = PreferredTagPayload {
            preferred_tags: vec![PreferredTag {
                tag: "fluffy".to_string(),
                group: "".to_string(),
                weight: 1.0,
            }],
        };
        assert!(is_bad(validate_preferred_tag_payload(&bad)));

        // Too many tags
        let many = vec![
            PreferredTag {
                tag: "a".to_string(),
                group: "general".to_string(),
                weight: 1.0,
            };
            MAX_PREFERRED_TAGS + 1
        ];
        assert!(is_bad(validate_preferred_tag_payload(
            &PreferredTagPayload {
                preferred_tags: many,
            }
        )));

        // Unknown group — rejected against the whitelist.
        let bad = PreferredTagPayload {
            preferred_tags: vec![PreferredTag {
                tag: "fluffy".to_string(),
                group: "supergroup".to_string(),
                weight: 1.0,
            }],
        };
        assert!(is_bad(validate_preferred_tag_payload(&bad)));

        // Whitelist accepts every documented group.
        for g in [
            "artist",
            "character",
            "copyright",
            "general",
            "lore",
            "meta",
            "species",
        ] {
            let payload = PreferredTagPayload {
                preferred_tags: vec![PreferredTag {
                    tag: "fluffy".to_string(),
                    group: g.to_string(),
                    weight: 1.0,
                }],
            };
            assert!(
                validate_preferred_tag_payload(&payload).is_ok(),
                "group '{g}' should be accepted"
            );
        }

        // Case-insensitive group matching.
        let ok_mixed_case = PreferredTagPayload {
            preferred_tags: vec![PreferredTag {
                tag: "fluffy".to_string(),
                group: "General".to_string(),
                weight: 1.0,
            }],
        };
        assert!(validate_preferred_tag_payload(&ok_mixed_case).is_ok());

        // Duplicate (tag, group) — rejected so the next read can't see
        // doubled weight from a single round-trip.
        let dup = PreferredTagPayload {
            preferred_tags: vec![
                PreferredTag {
                    tag: "fluffy".to_string(),
                    group: "general".to_string(),
                    weight: 1.0,
                },
                PreferredTag {
                    tag: "FLUFFY".to_string(), // case-insensitive dedup
                    group: "general".to_string(),
                    weight: 2.0,
                },
            ],
        };
        assert!(is_bad(validate_preferred_tag_payload(&dup)));

        // Same tag, different group — NOT a duplicate.
        let ok_cross_group = PreferredTagPayload {
            preferred_tags: vec![
                PreferredTag {
                    tag: "fluffy".to_string(),
                    group: "general".to_string(),
                    weight: 1.0,
                },
                PreferredTag {
                    tag: "fluffy".to_string(),
                    group: "species".to_string(),
                    weight: 1.0,
                },
            ],
        };
        assert!(validate_preferred_tag_payload(&ok_cross_group).is_ok());
    }

    #[test]
    fn session_token_validation() {
        assert!(is_bad(validate_session_token("short"))); // 7 chars
        assert!(validate_session_token("01234567").is_ok()); // 8 chars
        assert!(validate_session_token(&"a".repeat(128)).is_ok());
        assert!(is_bad(validate_session_token(&"a".repeat(129))));
        assert!(validate_session_token("550e8400-e29b-41d4-a716-446655440000").is_ok());
        assert!(is_bad(validate_session_token("hello world!")));
    }

    #[test]
    fn continue_count_validation() {
        assert_eq!(validate_continue_count(None).unwrap(), 20);
        assert!(validate_continue_count(Some(0)).is_err());
        assert!(validate_continue_count(Some(101)).is_err());
        assert_eq!(validate_continue_count(Some(50)).unwrap(), 50);
    }

    #[test]
    fn similar_posts_validation() {
        assert_eq!(validate_similar_posts_limit(None).unwrap(), 20);
        assert_eq!(validate_similar_posts_limit(Some(50)).unwrap(), 50);
        assert!(validate_similar_posts_limit(Some(0)).is_err());
        assert!(validate_similar_posts_limit(Some(101)).is_err());

        assert_eq!(validate_similar_posts_min_overlap(None).unwrap(), 2);
        assert_eq!(validate_similar_posts_min_overlap(Some(3)).unwrap(), 3);
        assert!(validate_similar_posts_min_overlap(Some(0)).is_err());

        assert_eq!(validate_similar_posts_page(None).unwrap(), 1);
        assert_eq!(validate_similar_posts_page(Some(5)).unwrap(), 5);
        assert!(validate_similar_posts_page(Some(0)).is_err());
        assert!(validate_similar_posts_page(Some(101)).is_err());
    }
}
