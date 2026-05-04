//! Owner-token request guard with sliding-refresh cookie.
//!
//! Audit #3: the device token used to travel in `?owner_token=…`
//! (leaked into nginx access logs and browser history) and live in
//! `localStorage` (readable by any XSS / browser extension). It now
//! lives in an `HttpOnly; Secure; SameSite=Lax` cookie that the
//! server refreshes on every authenticated request, so as long as
//! the user comes back inside the browser-imposed 400-day cap their
//! saved accounts persist indefinitely.
//!
//! `SameSite=Lax` is sufficient CSRF protection here because every
//! mutating route is `POST/PATCH/DELETE`, and Lax does not attach
//! cookies to cross-origin requests of those methods. GET routes are
//! all read-only. We intentionally skip a separate CSRF token to
//! avoid the migration overhead it would impose.

use rocket::http::{Cookie, SameSite, Status};
use rocket::request::{FromRequest, Outcome, Request};
use rocket::time::Duration;
use rocket_okapi::r#gen::OpenApiGenerator;
use rocket_okapi::request::{OpenApiFromRequest, RequestHeaderInput};

use crate::errors::ApiError;
use crate::validation;

pub const OWNER_TOKEN_COOKIE: &str = "owner_token";
/// Hard cap that all major browsers enforce on cookie lifetime
/// (Chromium, Firefox, Safari ≥ 2022). Setting anything larger gets
/// silently clamped to this value.
pub const OWNER_TOKEN_MAX_AGE_DAYS: i64 = 400;

pub struct OwnerToken(pub String);

impl OwnerToken {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Build the canonical `Set-Cookie` for the owner token. Centralised so
/// the bootstrap endpoint and the per-request sliding refresh stay in
/// sync — flipping `Secure` or changing `SameSite` only needs to happen
/// in one place.
pub fn build_owner_cookie(token: String) -> Cookie<'static> {
    Cookie::build((OWNER_TOKEN_COOKIE, token))
        .http_only(true)
        // `Secure` would block the cookie over plain HTTP in `trunk
        // serve` (dev). Tie it to release builds so dev still works
        // without a self-signed certificate.
        .secure(cfg!(not(debug_assertions)))
        .same_site(SameSite::Lax)
        .path("/api")
        .max_age(Duration::days(OWNER_TOKEN_MAX_AGE_DAYS))
        .build()
}

/// Build a `Set-Cookie` that immediately expires the token. Used by
/// `DELETE /api/session` and on validation failure of an inbound
/// cookie value (so a bad cookie can't pin a 400 forever).
pub fn build_owner_cookie_clear() -> Cookie<'static> {
    Cookie::build((OWNER_TOKEN_COOKIE, String::new()))
        .http_only(true)
        .secure(cfg!(not(debug_assertions)))
        .same_site(SameSite::Lax)
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
            // Drop the bad cookie so the next request gets a clean
            // 401 + bootstrap rather than another 400 against the
            // same value.
            req.cookies().add(build_owner_cookie_clear());
            return Outcome::Error((Status::BadRequest, e));
        }

        // Sliding refresh: rewrite the cookie on every authenticated
        // request so the 400-day expiry resets while the user is
        // active. This costs a single `Set-Cookie` header per
        // response — the browser silently no-ops if the value is
        // unchanged.
        req.cookies().add(build_owner_cookie(token.clone()));

        Outcome::Success(OwnerToken(token))
    }
}

/// Tell rocket-okapi that this guard is consumed implicitly (cookie
/// header) — no OpenAPI parameter to surface, but routes that use it
/// can still document a 401 response.
impl<'r> OpenApiFromRequest<'r> for OwnerToken {
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
