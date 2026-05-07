//! Input validation for HTTP-facing structs.
//!
//! All routes that accept a `DeviceScopedAccount`, an `owner_token`, or
//! an `account_id` push their input through here before any DB work. The
//! audit (H-1) showed that without this layer anonymous POSTs could
//! create rows with `id = -1`, 100 KB names, 50 KB blacklists, and even
//! null-byte tokens — and once a row exists the prefetcher will start
//! making admin-authenticated e621 requests on its behalf.
//!
//! Limits are deliberately loose enough to accept any real e621 user
//! while rejecting obvious abuse:
//! * `id`        — strictly positive, capped above e621's current id
//!                 space with margin.
//! * `name`      — ≤ 64 chars; e621 caps usernames at much less but
//!                 we don't want to track that exactly. Restricts to
//!                 a printable ASCII subset that covers historical
//!                 username conventions.
//! * `blacklist` — ≤ 16 KB. e621's own UI caps it at a similar size;
//!                 anything larger is almost certainly a flood.
//! * `owner_token` — 16..=128 chars from `[A-Za-z0-9_-]`. The current
//!                 generator (`owner-<ts>-<r>-<r>`) lands at ~38 chars
//!                 so existing tokens pass, but null bytes / massive
//!                 strings are rejected.

use crate::errors::ApiError;
use crate::models::{BlacklistPayload, DeviceScopedAccount, FeedInteractionRequest};

const MAX_ACCOUNT_ID: i32 = 100_000_000;
const MAX_NAME_LEN: usize = 64;
const MAX_BLACKLIST_LEN: usize = 16 * 1024;
const MIN_OWNER_TOKEN_LEN: usize = 16;
const MAX_OWNER_TOKEN_LEN: usize = 128;
const MAX_SESSION_ID_LEN: usize = 128;

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
    // Allow the printable ASCII subset that covers e621 historical
    // usernames: letters/digits, the punctuation users often pick, and
    // space. Reject control bytes (incl. NUL — H-1 PoC #6) and anything
    // outside ASCII to keep storage / display predictable.
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
