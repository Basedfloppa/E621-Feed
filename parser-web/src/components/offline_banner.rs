//! Global offline awareness for the app.
//!
//! Tracks the browser's `navigator.onLine` state and shows a thin themed banner
//! when the device is offline so the user understands the (cached) content they
//! are seeing may be stale and offers a Retry action once connectivity returns.
//! This is the in-app replacement for the separate offline page: while the app
//! shell (wasm + css) is cached by the service worker, this banner is what the
//! user actually sees when the network drops.

use wasm_bindgen::prelude::*;
use web_sys::{Event, window};
use yew::prelude::*;

/// Whether the device currently reports an active network connection.
fn online() -> bool {
    window().map(|w| w.navigator().on_line()).unwrap_or(true)
}

fn retry() {
    if let Some(w) = window() {
        let _ = w.location().reload();
    }
}

#[function_component(OfflineBanner)]
pub fn offline_banner() -> Html {
    let offline = use_state(|| !online());

    // Keep the banner in sync with the browser's online/offline events.
    use_effect_with((), {
        let offline = offline.clone();
        move |_| {
            let on_online = {
                let offline = offline.clone();
                Closure::<dyn FnMut(Event)>::new(move |_| offline.set(false))
            };
            let on_offline = {
                let offline = offline.clone();
                Closure::<dyn FnMut(Event)>::new(move |_| offline.set(true))
            };
            let w = window().expect("window in browser context");

            w.add_event_listener_with_callback("online", on_online.as_ref().unchecked_ref())
                .expect("add online listener");
            w.add_event_listener_with_callback("offline", on_offline.as_ref().unchecked_ref())
                .expect("add offline listener");

            move || {
                let _ = w.remove_event_listener_with_callback(
                    "online",
                    on_online.as_ref().unchecked_ref(),
                );
                let _ = w.remove_event_listener_with_callback(
                    "offline",
                    on_offline.as_ref().unchecked_ref(),
                );
            }
        }
    });

    if !*offline {
        return html! {};
    }

    html! {
        <div class="fixed bottom-4 left-1/2 -translate-x-1/2 z-[9999] w-[min(92vw,560px)]">
            <div class="alert alert-warning shadow-lg rounded-full border border-base-300 gap-3 py-2 pl-5 pr-2 text-sm">
                <span class="text-base-content font-medium leading-tight">
                    { "You're offline — showing cached content. Some actions may be unavailable." }
                </span>
                <button
                    class="btn btn-xs btn-ghost shrink-0"
                    onclick={Callback::from(|_| retry())}
                >
                    { "Retry" }
                </button>
            </div>
        </div>
    }
}
