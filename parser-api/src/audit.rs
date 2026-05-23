//! Backend-behaviour audit log.
//!
//! Emits exactly one line per user-facing event so an operator can
//! eyeball the live system without wading through the verbose
//! debug/info stream from Rocket, the scorer, and the prefetcher.
//!
//! Each line goes through the normal `log` facade at `info!` level
//! with a stable `[AUDIT]` prefix and pipe-separated key=value
//! fields. Two operational use-cases:
//!
//! ```text
//! # live tail of just the audit stream:
//! journalctl -u e621-parser-api -f | grep '\[AUDIT\]'
//! # …or under Docker:
//! docker compose logs -f app | grep AUDIT
//!
//! # filter to a single account:
//! ... | grep '\[AUDIT\]' | grep 'account_id=658288'
//!
//! # all failed /process runs in the last hour:
//! ... | grep 'process.failed'
//! ```
//!
//! Format: `[AUDIT] <event.kind>|key1=v1|key2=v2|...`
//!
//! Key naming convention: `dotted.scoped.kind` for the event, snake_case
//! for fields. Field values are escaped just enough to keep `|` and `=`
//! from breaking the parse — anything richer (full error bodies, JSON,
//! multi-line) goes to the regular log stream, not here.
//!
//! Hot-path safety: the builder is lazy — fields are only formatted on
//! `emit()`, so adding `.field(...)` calls to a code path that never
//! reaches `emit()` has zero cost.

use std::fmt::{Display, Write};

/// Audit-event builder. Call `audit::event("kind").field(...).emit()`.
/// Drop without `emit()` is intentional and free — no line is logged.
pub struct Event {
    kind: &'static str,
    buf: String,
}

impl Event {
    fn new(kind: &'static str) -> Self {
        Self {
            kind,
            buf: String::with_capacity(64),
        }
    }

    /// Append a `|key=value` pair. `value`'s `Display` is used and the
    /// rendered text is sanitised so it can't break the pipe-separated
    /// format (newlines and pipes become spaces; `=` is kept since it
    /// only matters in the FIRST `=` of each field).
    pub fn field(mut self, key: &str, value: impl Display) -> Self {
        let rendered = format!("{value}");
        let escaped: String = rendered
            .chars()
            .map(|c| match c {
                '\n' | '\r' | '|' => ' ',
                other => other,
            })
            .collect();
        // Ignore the write_str result — String never errors.
        let _ = write!(self.buf, "|{key}={escaped}");
        self
    }

    /// Append a key only when `value` is `Some`. Saves a couple of
    /// `if let Some(v) = ...` lines at call sites that want to log
    /// optional context (session_id, page, etc.).
    pub fn field_opt(self, key: &str, value: Option<impl Display>) -> Self {
        match value {
            Some(v) => self.field(key, v),
            None => self,
        }
    }

    /// Write the line to the log stream.
    pub fn emit(self) {
        info!("[AUDIT] {}{}", self.kind, self.buf);
    }
}

/// Open an audit event for one of the documented kinds. Using a fn
/// rather than a free macro keeps the kind a `&'static str` (no
/// accidental formatted strings) and gives one obvious place to
/// document the list.
///
/// ## Event catalogue
///
/// **Account lifecycle**
/// * `account.set`       — `account_id`, `name`
/// * `account.deleted`   — `account_id`, `removed_links`
///
/// **/process pipeline**
/// * `process.start`     — `account_id`
/// * `process.already`   — `account_id` (try_begin returned AlreadyRunning)
/// * `process.phase`     — `account_id`, `phase`, `ms`
/// * `process.done`      — `account_id`, `favs`, `pages`, `ms`
/// * `process.failed`    — `account_id`, `error`
///
/// **Feed routes**
/// * `feed.recommend`    — `account_id`, `returned`, `page`
/// * `feed.continue`     — `account_id`, `session_state` (fresh/active/expired), `returned`
/// * `feed.similar`      — `account_id`, `post_id`, `returned`
/// * `feed.digest`       — `account_id`, `mode` (personalized/generic), `returned`, `cache_hit`
/// * `feed.interaction`  — `account_id`, `post_id`, `event` (open/hide/qualified_impression)
/// * `feed.batch`        — `account_id`, `count`
///
/// **Auth / sessions**
/// * `session.bootstrap` — `minted` (true/false)
/// * `session.cleared`   — (no fields; cookie was wiped)
pub fn event(kind: &'static str) -> Event {
    Event::new(kind)
}
