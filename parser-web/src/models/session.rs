//! Bootstraps the device session against the cookie-based auth.
//!
//! Audit #3: the owner token lives in an `HttpOnly` cookie that the
//! server sets/refreshes on each request. Calling
//! `POST /api/session/bootstrap` once at app start guarantees the
//! cookie is in place before any component fires its first
//! authenticated request — the server either refreshes an existing
//! cookie or mints a fresh one and installs it.
//!
//! Failures are logged and silently swallowed: the user lands in an
//! unauthenticated state and the next API call surfaces the real
//! error. We do not block app render forever on a network blip.

use super::api_client::api_post;
use super::config::read_config_from_head;

pub async fn bootstrap_session() {
    let Some(cfg) = read_config_from_head() else {
        return;
    };

    let url = format!("{}/session/bootstrap", cfg.backend_domain);
    match api_post(&url).send().await {
        Ok(r) if r.ok() => {}
        Ok(r) => {
            web_sys::console::warn_1(
                &format!("session bootstrap returned HTTP {}", r.status()).into(),
            );
        }
        Err(e) => {
            web_sys::console::warn_1(&format!("session bootstrap failed: {e}").into());
        }
    }
}
