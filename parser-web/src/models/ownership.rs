//! Whether this browser owns anything on this instance.
//!
//! There is no login here: the `HttpOnly` owner cookie *is* the identity, and a
//! fresh browser starts with one that owns nothing. The local catalog is a view
//! over the saved posts of the accounts this browser owns (`accounts_post`), so
//! its nav entry only means something once `owned_accounts > 0`. Every
//! `/catalog/*` route is already owner-gated server-side (`guard_catalog`,
//! `read_guard`, `write_guard`); this module is what keeps the UI from
//! advertising a door that only answers "select an account" — with nothing to
//! select.
//!
//! `GET /session/state` is the same endpoint the settings card reads, so the
//! contract lives here once. The listener on `ACCOUNT_LIST_CHANGED_EVENT`
//! re-reads it after a link, unlink or claim.

use serde::Deserialize;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen_futures::spawn_local;
use web_sys::Event;
use yew::prelude::*;

use super::api_client::api_get;
use super::events::ACCOUNT_LIST_CHANGED_EVENT;

/// `GET /session/state`.
#[derive(Clone, Deserialize, PartialEq)]
pub struct OwnershipState {
    pub instance_locked: bool,
    pub owned_accounts: i64,
    pub adoptable_accounts: i64,
}

/// Is the local catalog this browser's to open?
///
/// Pure, and deliberately about *ownership* rather than about the `[catalog]`
/// toggles: the library is collected unconditionally, so one linked account is
/// exactly what makes the catalog a view over your own posts.
pub fn owns_catalog(state: &OwnershipState) -> bool {
    state.owned_accounts > 0
}

/// Whether the Catalog nav entry belongs in the menu.
///
/// Unknown ownership stays hidden: the read can only fail if the API itself is
/// unreachable (same origin, same cookie as every other call), and a nav entry
/// that appears for a visitor who owns nothing is precisely what this gate is
/// for. Callers therefore treat `None` as "no".
pub fn catalog_entry_visible(state: &Option<OwnershipState>) -> bool {
    state.as_ref().is_some_and(owns_catalog)
}

/// Read the state once. `None` means "could not tell", never "owns nothing".
pub async fn fetch_ownership(backend_url: &str) -> Option<OwnershipState> {
    let resp = api_get(&format!("{backend_url}/session/state"))
        .send()
        .await
        .ok()?;
    if !resp.ok() {
        return None;
    }
    resp.json::<OwnershipState>().await.ok()
}

/// Live ownership state: `None` until the first read lands.
#[hook]
pub fn use_ownership(backend_url: &str) -> UseStateHandle<Option<OwnershipState>> {
    let state = use_state(|| None::<OwnershipState>);
    let backend = backend_url.to_string();
    {
        let state = state.clone();
        use_effect_with(backend, move |backend| {
            // Cloned per caller only to make the closure `Fn`: the effect body
            // calls it immediately and the event listener calls it later.
            let fetch = {
                let state = state.clone();
                let backend = backend.clone();
                move || {
                    let state = state.clone();
                    let backend = backend.clone();
                    spawn_local(async move {
                        if let Some(s) = fetch_ownership(&backend).await {
                            state.set(Some(s));
                        }
                    });
                }
            };
            fetch();

            let on_change = fetch.clone();
            let listener = Closure::<dyn FnMut(Event)>::new(move |_: Event| on_change());
            let win = web_sys::window();
            if let Some(w) = win.as_ref() {
                let _ = w.add_event_listener_with_callback(
                    ACCOUNT_LIST_CHANGED_EVENT,
                    listener.as_ref().unchecked_ref(),
                );
            }
            move || {
                if let Some(w) = win.as_ref() {
                    let _ = w.remove_event_listener_with_callback(
                        ACCOUNT_LIST_CHANGED_EVENT,
                        listener.as_ref().unchecked_ref(),
                    );
                }
            }
        });
    }
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(owned: i64) -> OwnershipState {
        OwnershipState {
            instance_locked: true,
            owned_accounts: owned,
            adoptable_accounts: 0,
        }
    }

    #[test]
    fn one_linked_account_is_what_makes_the_catalog_visible() {
        assert!(!catalog_entry_visible(&None));
        assert!(!catalog_entry_visible(&Some(state(0))));
        assert!(catalog_entry_visible(&Some(state(1))));
        assert!(catalog_entry_visible(&Some(state(900))));
    }

    /// The instance having *adoptable* accounts must not reveal the entry: on a
    /// claimed instance those belong to the owner, and this browser has not
    /// proved anything yet.
    #[test]
    fn adoptable_accounts_do_not_count_as_owned() {
        let waiting = OwnershipState {
            instance_locked: true,
            owned_accounts: 0,
            adoptable_accounts: 900,
        };
        assert!(!catalog_entry_visible(&Some(waiting.clone())));
        assert!(!owns_catalog(&waiting));
    }
}
