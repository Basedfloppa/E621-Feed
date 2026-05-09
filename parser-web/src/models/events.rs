//! Cross-component window events.
//!
//! `SavedAccountsSelect` mounts on /home and /feed; mutations happen on
//! /account. Without a notification, deleting an account and staying on
//! the same route shows stale data. Instead of a global store, we
//! publish a `CustomEvent` on `window` and let the dropdown subscribe.

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
