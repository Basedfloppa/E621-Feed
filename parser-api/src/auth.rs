//! Owner-token request guard with sliding-refresh cookie.
//!
//! The device token lives in an `HttpOnly; Secure; SameSite=Strict` cookie
//! that the server refreshes on every authenticated request. Saved
//! accounts persist as long as the user comes back inside the browser's
//! 400-day cap.
//!
//! `SameSite=Strict` is our primary CSRF boundary: the browser only attaches
//! a Strict cookie to same-origin requests, so it is never delivered on a
//! cross-site top-level GET navigation, form POST, or credentialed fetch —
//! regardless of what security headers the UA sends. This matters because some
//! GET routes carry side effects (browse persistence, feed-session touch,
//! visit tracking); with `Lax` such GETs still received the cookie on cross-*
//! site top-level navigation, so we had to lean on the client-settable
//! `Sec-Fetch-Site` header just for them. `Strict` removes that reliance — the
//! cookie itself is the boundary. The same-origin `csrf_origin_is_allowed`
//! check is retained as defense-in-depth.
//!
//! ## Local dev
//!
//! `trunk serve` + Rocket on different ports looks cross-origin, so the fix is
//! **Trunk's proxy** (`Trunk.toml` already configures `[[proxy]]` for `/api/`
//! → `http://127.0.0.1:8080`). With `backend_domain = ""` in
//! `static/config.js`, all API calls use relative paths — same origin (Strict
//! attaches), cookie works.

use std::collections::HashSet;
use std::sync::{OnceLock, RwLock};

use rocket::http::{Cookie, Method, SameSite, Status};
use rocket::request::{FromRequest, Outcome, Request};
use rocket::time::Duration;
use rocket_okapi::r#gen::OpenApiGenerator;
use rocket_okapi::request::{OpenApiFromRequest, RequestHeaderInput};

use crate::db;
use crate::errors::ApiError;
use crate::validation;

pub const OWNER_TOKEN_COOKIE: &str = "owner_token";
/// Browser-enforced hard cap on cookie lifetime (Chromium/Firefox/Safari ≥ 2022).
pub const OWNER_TOKEN_MAX_AGE_DAYS: i64 = 400;
/// Retention for the revocation denylist; buffer absorbs clock skew.
pub const REVOKED_TOKEN_RETENTION_SECS: i64 = (OWNER_TOKEN_MAX_AGE_DAYS + 10) * 86_400;

/// Hot in-memory mirror of `revoked_tokens` for O(1) per-request lookup.
/// Reloaded on startup and after every prune cycle.
static REVOKED_TOKENS: OnceLock<RwLock<HashSet<String>>> = OnceLock::new();

fn revoked_set() -> &'static RwLock<HashSet<String>> {
    REVOKED_TOKENS.get_or_init(|| RwLock::new(HashSet::new()))
}

/// (Re)load the in-memory denylist from disk. Idempotent.
pub fn reload_revocation_set() -> Result<(), String> {
    let tokens = db::load_all_revoked_tokens()?;
    let mut guard = revoked_set()
        .write()
        .map_err(|e| format!("revocation set lock poisoned: {e}"))?;
    *guard = tokens.into_iter().collect();
    Ok(())
}

/// Persist a revocation and update the hot set. Idempotent.
pub fn revoke(token: &str) -> Result<(), String> {
    db::revoke_token_in_db(token)?;
    let mut guard = revoked_set()
        .write()
        .map_err(|e| format!("revocation set lock poisoned: {e}"))?;
    guard.insert(token.to_string());
    Ok(())
}

pub(crate) fn csrf_origin_is_allowed(req: &Request<'_>) -> bool {
    // Defense-in-depth alongside the `SameSite=Strict` owner cookie (the
    // primary boundary). `Sec-Fetch-Site` is sent by browsers on every fetch
    // and navigation; a UA that omits/weakens it must not be able to drive
    // cross-site state changes, and for cross-site requests the Strict cookie
    // is anyway never attached. Same-origin app requests (served via the /api
    // proxy) are `same-origin`/`same-site`, so this never blocks the first
    // party.
    if let Some(site) = req.headers().get_one("Sec-Fetch-Site")
        && site.eq_ignore_ascii_case("cross-site")
    {
        return false;
    }

    if matches!(req.method(), Method::Get | Method::Head | Method::Options) {
        return true;
    }

    let Some(origin) = req.headers().get_one("Origin") else {
        // CLI/API clients are useful in local development; production browser
        // requests include Origin for unsafe methods and must prove same-origin.
        return cfg!(debug_assertions);
    };

    if cfg!(debug_assertions) {
        return matches!(
            origin,
            "http://localhost:8000"
                | "http://127.0.0.1:8000"
                | "http://localhost:8080"
                | "http://127.0.0.1:8080"
        );
    }

    let origin_host = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))
        .and_then(|value| value.split('/').next());
    origin_host.is_some_and(|origin_host| req.host().is_some_and(|host| origin_host == host))
}

/// Request guard that rejects cross-origin state-changing requests.
///
/// `session/bootstrap` mints/rotates the device `owner_token` cookie, which is
/// the sole account-ownership identity, but it deliberately does not require an
/// existing `OwnerToken`. Adding this guard to such routes applies the same
/// same-origin (CSRF) check that the `OwnerToken` guard uses on every other
/// mutating route, so a cross-site form auto-submit can no longer silently
/// replace a victim's device token and strand their linked accounts.
#[derive(Clone, Copy)]
pub struct SameOrigin;

#[rocket::async_trait]
impl<'r> FromRequest<'r> for SameOrigin {
    type Error = ApiError;

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        if csrf_origin_is_allowed(req) {
            Outcome::Success(SameOrigin)
        } else {
            Outcome::Error((
                Status::Forbidden,
                ApiError::Forbidden("cross-origin state-changing request rejected".into()),
            ))
        }
    }
}

impl OpenApiFromRequest<'_> for SameOrigin {
    fn from_request_input(
        _gen: &mut OpenApiGenerator,
        _name: String,
        _required: bool,
    ) -> rocket_okapi::Result<RequestHeaderInput> {
        Ok(RequestHeaderInput::None)
    }

    fn get_responses(
        _gen: &mut OpenApiGenerator,
    ) -> rocket_okapi::Result<rocket_okapi::okapi::openapi3::Responses> {
        Ok(rocket_okapi::okapi::openapi3::Responses::default())
    }
}

fn is_revoked(token: &str) -> bool {
    // Fail closed on poisoned lock — refusing one request beats accepting
    // a token that may have been logged out.
    match revoked_set().read() {
        Ok(g) => g.contains(token),
        Err(_) => true,
    }
}

pub struct OwnerToken(pub String);

impl OwnerToken {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Canonical `Set-Cookie` for the owner token. Centralised so bootstrap
/// and the per-request sliding refresh stay in sync.
#[must_use]
pub fn build_owner_cookie(token: String) -> Cookie<'static> {
    Cookie::build((OWNER_TOKEN_COOKIE, token))
        .http_only(true)
        // `Secure` would block the cookie over plain HTTP in `trunk serve`.
        .secure(cfg!(not(debug_assertions)))
        // `Strict` (not `Lax`): the frontend is served same-origin by this
        // binary, so Strict never blocks the first party, and it stops
        // cross-site top-level GET navigations (CSRF on side-effecting GETs)
        // from ever receiving the cookie. See the module docs.
        .same_site(SameSite::Strict)
        .path("/api")
        .max_age(Duration::days(OWNER_TOKEN_MAX_AGE_DAYS))
        .build()
}

/// `Set-Cookie` that immediately expires the token. Used by
/// `DELETE /api/session` and on validation failure of an inbound cookie.
#[must_use]
pub fn build_owner_cookie_clear() -> Cookie<'static> {
    Cookie::build((OWNER_TOKEN_COOKIE, String::new()))
        .http_only(true)
        .secure(cfg!(not(debug_assertions)))
        .same_site(SameSite::Strict)
        .path("/api")
        .max_age(Duration::ZERO)
        .build()
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for OwnerToken {
    type Error = ApiError;

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let Some(token) = req
            .cookies()
            .get(OWNER_TOKEN_COOKIE)
            .map(|c| c.value().to_string())
        else {
            return Outcome::Error((
                Status::Unauthorized,
                ApiError::Unauthorized("missing owner token".into()),
            ));
        };

        if let Err(e) = validation::validate_owner_token(&token) {
            // Drop the bad cookie so the next request gets a clean 401.
            req.cookies().add(build_owner_cookie_clear());
            return Outcome::Error((Status::BadRequest, e));
        }

        if !csrf_origin_is_allowed(req) {
            return Outcome::Error((
                Status::Forbidden,
                ApiError::Forbidden("cross-origin state-changing request rejected".into()),
            ));
        }

        if is_revoked(&token) {
            req.cookies().add(build_owner_cookie_clear());
            return Outcome::Error((
                Status::Unauthorized,
                ApiError::Unauthorized("session revoked; please log in again".into()),
            ));
        }

        // Sliding refresh: reset the 400-day expiry while the user is active.
        req.cookies().add(build_owner_cookie(token.clone()));

        Outcome::Success(OwnerToken(token))
    }
}

impl OpenApiFromRequest<'_> for OwnerToken {
    fn from_request_input(
        _gen: &mut OpenApiGenerator,
        _name: String,
        _required: bool,
    ) -> rocket_okapi::Result<RequestHeaderInput> {
        Ok(RequestHeaderInput::None)
    }

    fn get_responses(
        _gen: &mut OpenApiGenerator,
    ) -> rocket_okapi::Result<rocket_okapi::okapi::openapi3::Responses> {
        Ok(rocket_okapi::okapi::openapi3::Responses::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_owner_cookie_properties() {
        let token = "test-token-abc123";
        let cookie = build_owner_cookie(token.to_string());

        assert_eq!(cookie.name(), OWNER_TOKEN_COOKIE);
        assert_eq!(cookie.value(), token);
        assert_eq!(cookie.http_only(), Some(true));
        assert_eq!(cookie.same_site(), Some(SameSite::Strict));
        assert_eq!(cookie.path(), Some("/api"));
        assert_eq!(
            cookie.max_age(),
            Some(Duration::days(OWNER_TOKEN_MAX_AGE_DAYS))
        );
    }

    #[test]
    fn build_owner_cookie_clear_has_zero_max_age() {
        let cookie = build_owner_cookie_clear();

        assert_eq!(cookie.name(), OWNER_TOKEN_COOKIE);
        assert!(cookie.value().is_empty());
        assert_eq!(cookie.max_age(), Some(Duration::ZERO));
    }

    #[test]
    fn build_owner_cookie_in_debug_secure_false() {
        // `build_owner_cookie` uses `cfg!(not(debug_assertions))` for Secure.
        // Under `cargo test` (debug profile) it must be false.
        let cookie = build_owner_cookie("x".into());
        assert_eq!(
            cookie.secure(),
            Some(false),
            "Secure must be false in debug/test profile"
        );
    }

    #[test]
    fn is_revoked_known_and_unknown() {
        let set = revoked_set();
        // Recover from any prior poison and reset.
        let mut g = set
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.clear();
        g.insert("known_revoked".into());
        drop(g);

        assert!(is_revoked("known_revoked"));
        assert!(!is_revoked("unknown_token"));
    }

    #[test]
    fn is_revoked_empty_set() {
        let set = revoked_set();
        match set.write() {
            Ok(mut g) => g.clear(),
            Err(p) => {
                // Recover from poisoned lock left by a prior test.
                let mut g = p.into_inner();
                g.clear();
            }
        }
        assert!(!is_revoked("any_token"));
        assert!(!is_revoked(""));
    }
}
