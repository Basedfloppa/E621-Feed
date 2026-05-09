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
use crate::models::{BlacklistPayload, DeviceScopedAccount, FeedInteractionRequest};

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
/// omitted the param) is the route's default, accepted as-is.
pub fn validate_recommendations_page(page: Option<i32>) -> Result<(), ApiError> {
    let Some(p) = page else { return Ok(()); };
    if !(0..=MAX_RECOMMENDATIONS_PAGE).contains(&p) {
        return Err(ApiError::BadRequest(format!(
            "page {p} out of range [0, {MAX_RECOMMENDATIONS_PAGE}]"
        )));
    }
    Ok(())
}

/// Reject NaN / ±Infinity (any comparison against them is always false,
/// silently dropping every post), then clamp into `[0.0, 1.0]`.
pub fn validate_affinity_threshold(t: Option<f32>) -> Result<Option<f32>, ApiError> {
    let Some(t) = t else { return Ok(None); };
    if !t.is_finite() {
        return Err(ApiError::BadRequest(
            "affinity_threshold must be a finite number".into(),
        ));
    }
    Ok(Some(t.clamp(0.0, 1.0)))
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
