//! Bootstraps the device session.
//!
//! `POST /api/session/bootstrap` once at app start ensures the
//! `HttpOnly` cookie is in place before any component fires its first
//! authenticated request — the server refreshes or mints as needed.
//!
//! Failures are logged and swallowed: the next API call surfaces the
//! real error rather than blocking app render on a network blip.

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
