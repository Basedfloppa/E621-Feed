//! Cross-component window events.
//!
//! The dropdown of saved accounts (`SavedAccountsSelect`) is mounted on
//! /home and /feed, while account creation/removal happens on /account.
//! Routing remounts the dropdown on navigation, but a user who deletes
//! an account and stays on the same route would see stale data — the
//! audit (#16) called this out. Rather than threading a global store
//! through every component, we publish a `CustomEvent` on `window` and
//! let the dropdown subscribe.
//!
//! Keep the constant centralised so emitter and listener can't drift.

use wasm_bindgen::JsCast;

pub const ACCOUNT_LIST_CHANGED_EVENT: &str = "e621parser:account-list-changed";

/// Fire the event so any mounted dropdown / list refetches.
pub fn dispatch_account_list_changed() {
    let Some(window) = web_sys::window() else {
        return;
    };
    let Ok(event) = web_sys::CustomEvent::new(ACCOUNT_LIST_CHANGED_EVENT) else {
        return;
    };
    let _ = window.dispatch_event(event.unchecked_ref());
}
