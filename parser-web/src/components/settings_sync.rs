//! Hook that lets a page re-read its display settings when the
//! QuickSettingsModal (or /settings) edits the unified `settings_*` keys.
//!
//! The modal dispatches a `settings-changed` CustomEvent on `window` after
//! persisting. Any page that calls `use_settings_tick()` gets a counter that
//! bumps on each event; the page body then re-reads `read_display_setting` /
//! grid / scoring from localStorage on the resulting re-render.

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;
use web_sys::{Event, window};
use yew::prelude::*;

/// Returns a `UseStateHandle<u32>` incremented whenever display settings
/// change elsewhere (quick-settings modal or the /settings page).
#[hook]
pub fn use_settings_tick() -> UseStateHandle<u32> {
    let tick = use_state(|| 0u32);
    let tick_cb = tick.clone();
    use_effect_with((), move |_| {
        let handler = Closure::<dyn FnMut(Event)>::new(move |_: Event| {
            tick_cb.set(*tick_cb + 1);
        });
        let win = window();
        if let Some(w) = win.as_ref() {
            let _ = w.add_event_listener_with_callback(
                "settings-changed",
                handler.as_ref().unchecked_ref(),
            );
        }
        move || {
            if let Some(w) = win.as_ref() {
                let _ = w.remove_event_listener_with_callback(
                    "settings-changed",
                    handler.as_ref().unchecked_ref(),
                );
            }
        }
    });
    tick
}
