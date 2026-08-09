//! "Storage / Offline" card for the settings page.
//!
//! Makes the app's local caching transparent: shows how much space the service
//! worker cache and the site's overall storage (IndexedDB + caches) occupy on
//! this device, and offers a one-click "Clear offline data" action that wipes
//! the caches and the offline IndexedDB database.

use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::{Cache, CacheQueryOptions, CustomEvent, Request, Response, window};
use yew::prelude::*;

fn format_bytes(mut b: u64) -> String {
    let units = ["B", "KB", "MB", "GB"];
    let mut i = 0;
    while b >= 1024 && i < units.len() - 1 {
        b /= 1024;
        i += 1;
    }
    format!("{b} {}", units[i])
}

/// Total bytes held in all service-worker caches for this origin.
async fn sum_caches() -> u64 {
    let caches = match window().map(|w| w.caches()) {
        Some(Ok(c)) => c,
        _ => return 0,
    };
    let Ok(names_value) = JsFuture::from(caches.keys()).await else {
        return 0;
    };
    let names = js_sys::Array::from(&names_value);
    let mut total: u64 = 0;
    for i in 0..names.length() {
        let Some(name) = names.get(i).as_string() else {
            continue;
        };
        let Ok(cache_value) = JsFuture::from(caches.open(&name)).await else {
            continue;
        };
        let cache: Cache = cache_value.unchecked_into();
        let Ok(reqs_value) = JsFuture::from(cache.keys()).await else {
            continue;
        };
        let reqs = js_sys::Array::from(&reqs_value);
        for j in 0..reqs.length() {
            let req = reqs.get(j);
            let Some(req_obj) = req.dyn_ref::<Request>() else {
                continue;
            };
            let Ok(maybe_value) = JsFuture::from(
                cache.match_with_request_and_options(req_obj, &CacheQueryOptions::new()),
            )
            .await
            else {
                continue;
            };
            let Some(resp) = maybe_value.dyn_ref::<Response>() else {
                continue;
            };
            let Ok(ab_promise) = resp.array_buffer() else {
                continue;
            };
            let Ok(ab_value) = JsFuture::from(ab_promise).await else {
                continue;
            };
            let ab: js_sys::ArrayBuffer = ab_value.unchecked_into();
            total += ab.byte_length() as u64;
        }
    }
    total
}

/// Total storage used by this origin (caches + IndexedDB + other).
async fn sum_total() -> u64 {
    let storage = match window().map(|w| w.navigator().storage()) {
        Some(s) => s,
        None => return 0,
    };
    let Ok(promise) = storage.estimate() else {
        return 0;
    };
    let Ok(value) = JsFuture::from(promise).await else {
        return 0;
    };
    let usage = js_sys::Reflect::get(&value, &JsValue::from_str("usage"))
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    usage as u64
}

// ── Install-as-app settings ──────────────────────────────────────────
// These read/write the same localStorage flags pwa.js uses, so the settings
// page can control the one-shot install prompt and trigger the native one.

fn read_flag(key: &str, def: bool) -> bool {
    window()
        .and_then(|w| w.local_storage().ok())
        .flatten()
        .and_then(|s| s.get_item(key).ok())
        .flatten()
        .map(|v| v == "1")
        .unwrap_or(def)
}

fn write_flag(key: &str, val: bool) {
    if let Some(s) = window().and_then(|w| w.local_storage().ok()).flatten() {
        let _ = s.set_item(key, if val { "1" } else { "0" });
    }
}

fn dispatch_pwa_event(name: &str) {
    let Some(window) = window() else {
        return;
    };
    let Ok(evt) = CustomEvent::new(name) else {
        return;
    };
    let _ = window.dispatch_event(evt.unchecked_ref());
}

/// Delete every service-worker cache and the offline IndexedDB database.
async fn clear_all() {
    if let Some(window) = window() {
        if let Ok(caches) = window.caches()
            && let Ok(names_value) = JsFuture::from(caches.keys()).await
        {
            let names = js_sys::Array::from(&names_value);
            for i in 0..names.length() {
                if let Some(name) = names.get(i).as_string() {
                    let _ = JsFuture::from(caches.delete(&name)).await;
                }
            }
        }
        if let Ok(factory) = window.indexed_db()
            && let Some(factory) = factory
        {
            let _ = factory.delete_database("e621-feed");
        }
    }
}

#[function_component(StorageCard)]
pub fn storage_card() -> Html {
    let cache_bytes = use_state(|| None::<u64>);
    let total_bytes = use_state(|| None::<u64>);
    let clearing = use_state(|| false);
    let install_enabled = use_state(|| read_flag("pwa_install_enabled", true));

    {
        let cache_bytes = cache_bytes.clone();
        let total_bytes = total_bytes.clone();
        use_effect_with((), move |_| {
            spawn_local(async move {
                cache_bytes.set(Some(sum_caches().await));
            });
            spawn_local(async move {
                total_bytes.set(Some(sum_total().await));
            });
            || ()
        });
    }

    let on_clear = {
        let cache_bytes = cache_bytes.clone();
        let total_bytes = total_bytes.clone();
        let clearing = clearing.clone();
        Callback::from(move |_| {
            let cache_bytes = cache_bytes.clone();
            let total_bytes = total_bytes.clone();
            let clearing = clearing.clone();
            spawn_local(async move {
                clearing.set(true);
                clear_all().await;
                cache_bytes.set(Some(sum_caches().await));
                total_bytes.set(Some(sum_total().await));
                clearing.set(false);
            });
        })
    };

    let fmt = |b: Option<u64>| match b {
        Some(v) => format_bytes(v),
        None => "…".to_string(),
    };

    let on_install_toggle = {
        let install_enabled = install_enabled.clone();
        Callback::from(move |_| {
            let new_val = !*install_enabled;
            install_enabled.set(new_val);
            write_flag("pwa_install_enabled", new_val);
            if new_val {
                // Re-enabling also clears the one-shot flag so the prompt may
                // offer again; ask pwa.js to re-evaluate installability.
                write_flag("pwa_install_dismissed", false);
                dispatch_pwa_event("pwa-installable");
            }
        })
    };

    let on_install_now = Callback::from(move |_| dispatch_pwa_event("pwa-request-install"));

    html! {
        <div id="settings-storage" class="card bg-base-200 border border-base-300 shadow-sm">
            <div class="card-body">
                <h2 class="card-title text-xl">{ "Storage / Offline" }</h2>
                <p class="text-sm text-base-content/70">
                    { "This app caches data on your device (via a service worker) so pages keep working offline. You can see how much space is used and clear it here." }
                </p>
                <div class="flex flex-col gap-1 text-sm">
                    <div class="flex justify-between">
                        <span>{ "Service-worker cache:" }</span>
                        <span class="font-medium">{ fmt(*cache_bytes) }</span>
                    </div>
                    <div class="flex justify-between">
                        <span>{ "Site storage used:" }</span>
                        <span class="font-medium">{ fmt(*total_bytes) }</span>
                    </div>
                </div>
                <div class="mt-2">
                    <button
                        class="btn btn-sm btn-error"
                        disabled={*clearing}
                        onclick={on_clear}
                    >
                        { if *clearing { "Clearing…" } else { "Clear offline data" } }
                    </button>
                </div>

                <div class="divider my-3">{ "Install as an app" }</div>
                <label class="label cursor-pointer justify-start gap-2 py-1">
                    <input
                        type="checkbox"
                        class="toggle toggle-sm"
                        checked={*install_enabled}
                        onchange={on_install_toggle}
                    />
                    <span class="label-text">{ "Offer to install as an app" }</span>
                </label>
                <button class="btn btn-sm btn-outline" onclick={on_install_now}>
                    { "Install as an app now" }
                </button>
                <p class="text-xs text-base-content/60 mt-1">
                    { "Chromium-only. The install prompt shows once; after dismissing it won't ask again unless you switch this back on." }
                </p>
            </div>
        </div>
    }
}
