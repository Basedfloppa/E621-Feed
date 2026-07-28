//! One IntersectionObserver instance shared across the whole feed.
//!
//! Each PostCard previously created its own observer + closure pair. With 200
//! cards rendered, that's 200 native observers and 200 boxed closures pinned
//! in JS land — a measurable cost during scroll. The shared observer routes
//! entries back to per-card callbacks via a `data-feed-card-id` attribute we
//! attach on registration.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen::prelude::Closure;
use web_sys::js_sys;
use web_sys::{Element, IntersectionObserver, IntersectionObserverEntry, IntersectionObserverInit};

pub type CardCallback = Box<dyn Fn(&IntersectionObserverEntry)>;

const ID_ATTR: &str = "data-feed-card-id";

thread_local! {
    static REGISTRY: RefCell<HashMap<u64, CardCallback>> = RefCell::new(HashMap::new());
    static OBSERVER: RefCell<Option<IntersectionObserver>> = const { RefCell::new(None) };
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn get_or_init_observer() -> IntersectionObserver {
    OBSERVER.with(|cell| {
        if let Some(obs) = cell.borrow().clone() {
            return obs;
        }

        let cb = Closure::<dyn FnMut(js_sys::Array, IntersectionObserver)>::wrap(Box::new(
            |entries: js_sys::Array, _obs: IntersectionObserver| {
                let len = entries.length();
                for i in 0..len {
                    let Some(entry) = entries
                        .get(i)
                        .dyn_ref::<IntersectionObserverEntry>()
                        .cloned()
                    else {
                        continue;
                    };
                    let target = entry.target();
                    let Some(id_str) = target.get_attribute(ID_ATTR) else {
                        continue;
                    };
                    let Ok(id) = id_str.parse::<u64>() else {
                        continue;
                    };
                    // Borrow the registry briefly, find the callback, drop the
                    // borrow, then call it. The callback may itself touch
                    // hooks/state that would re-enter the registry on unmount.
                    let cb_opt: Option<*const CardCallback> = REGISTRY.with(|r| {
                        let map = r.borrow();
                        map.get(&id).map(|c| c as *const _)
                    });
                    if let Some(ptr) = cb_opt {
                        // Safety: the registry HashMap owns the boxed callback
                        // for the lifetime of the registration, and we hold no
                        // borrow while calling. The callback won't free its
                        // own slot mid-call.
                        let cb_ref = unsafe { &*ptr };
                        cb_ref(&entry);
                    }
                }
            },
        ));

        let opts = IntersectionObserverInit::new();
        opts.set_threshold(&JsValue::from_f64(0.5));
        let obs = IntersectionObserver::new_with_options(cb.as_ref().unchecked_ref(), &opts)
            .expect("create shared IntersectionObserver");
        cb.forget();
        *cell.borrow_mut() = Some(obs.clone());
        obs
    })
}

/// Register `element` with the shared observer; `on_entry` runs when the
/// element crosses the threshold. Returns the registration id; pass it to
/// `unobserve` on cleanup.
pub fn observe(element: &Element, on_entry: CardCallback) -> u64 {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let _ = element.set_attribute(ID_ATTR, &id.to_string());
    REGISTRY.with(|r| r.borrow_mut().insert(id, on_entry));
    let observer = get_or_init_observer();
    observer.observe(element);
    id
}

pub fn unobserve(element: &Element, id: u64) {
    REGISTRY.with(|r| {
        r.borrow_mut().remove(&id);
    });
    OBSERVER.with(|cell| {
        if let Some(obs) = cell.borrow().as_ref() {
            obs.unobserve(element);
        }
    });
}
