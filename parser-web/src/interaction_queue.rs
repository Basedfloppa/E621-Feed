//! Buffer feed interactions and submit them via the batched
//! `/interaction/batch` endpoint instead of one request per event.
//!
//! The backend's SQLite writer is single-threaded: every write serializes on
//! one global mutex, so one request per interaction means one write
//! transaction (and one fsync) per event — which is what inflates latency
//! when a feed scroll bursts out many impressions. Collapsing many events
//! into one request = one backend write transaction per batch.
//!
//! Strategy:
//! - Impressions (`qualified_impression`) are delay-tolerant: the buffer
//!   drains on a debounce timer and once a size threshold is hit, so a
//!   scroll-back burst coalesces into a handful of requests.
//! - Explicit actions (open / like / strong_like / hide) flush the queue
//!   immediately so user feedback reaches the server without a visible delay.

use std::cell::RefCell;

use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::spawn_local;
use web_sys::{Request, RequestCredentials, RequestInit, window};

use crate::models::{
    BatchInteractionRequest, FeedInteractionRequest, FeedInteractionType, api_post,
};

/// Flush once this many events are buffered, even before the debounce fires.
/// Well under the backend's per-batch cap of 100.
const FLUSH_AT: usize = 60;
/// Max time an impression can sit in the buffer before being sent.
const DEBOUNCE_MS: i32 = 800;

struct Pending {
    backend_url: String,
    interactions: Vec<FeedInteractionRequest>,
}

thread_local! {
    static TASK: RefCell<Option<Pending>> = const { RefCell::new(None) };
    /// Handle of the currently scheduled debounce timer (0 = none).
    static TIMER_ID: RefCell<i32> = const { RefCell::new(0) };
}

/// Enqueue one interaction. Batches with others and submits to
/// `/interaction/batch`. Explicit events flush the queue immediately.
pub fn push(backend_url: String, req: FeedInteractionRequest) {
    let explicit = !matches!(req.event_type, FeedInteractionType::QualifiedImpression);

    let flush_now = TASK.with(|cell| {
        let mut task = cell.borrow_mut();
        let pending = task.get_or_insert_with(|| Pending {
            backend_url: backend_url.clone(),
            interactions: Vec::new(),
        });
        pending.interactions.push(req);
        explicit || pending.interactions.len() >= FLUSH_AT
    });

    if flush_now {
        flush();
    } else {
        schedule_debounce();
    }
}

/// Drain the buffer and submit one batched request. Safe to call when empty.
fn flush() {
    clear_timer();
    let pending = TASK.with(|cell| cell.borrow_mut().take());
    if let Some(pending) = pending
        && !pending.interactions.is_empty()
    {
        send_batch(
            pending.backend_url,
            BatchInteractionRequest {
                interactions: pending.interactions,
            },
        );
    }
}

fn send_batch(backend_url: String, payload: BatchInteractionRequest) {
    let future = async move {
        let body = match serde_json::to_string(&payload) {
            Ok(body) => body,
            Err(err) => {
                web_sys::console::warn_1(
                    &format!("failed to encode interaction batch: {err}").into(),
                );
                return;
            }
        };

        if let Err(err) = api_post(&format!("{backend_url}/interaction/batch"))
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
        {
            web_sys::console::warn_1(&format!("failed to send interaction batch: {err}").into());
        }
    };
    spawn_local(future);
}

/// Flush the buffer while the document is going away.
///
/// A plain `fetch` is cancelled when the page unloads, and that is exactly when
/// a reload fires — and every redeploy is a reload. `keepalive` lets the
/// request outlive the document, so the impressions sitting in the debounce
/// window are not the ones that get lost: they are the only evidence the
/// backend has that a card was actually on screen (see
/// `migrations/V36__feed_shown_posts.sql`).
///
/// Fire-and-forget on purpose: there is no time to await a response here, and
/// the batch is idempotent on the server (`INSERT OR IGNORE`), so a duplicated
/// send is harmless.
fn flush_keepalive() {
    clear_timer();
    let Some(pending) = TASK.with(|cell| cell.borrow_mut().take()) else {
        return;
    };
    if pending.interactions.is_empty() {
        return;
    }
    let Ok(body) = serde_json::to_string(&BatchInteractionRequest {
        interactions: pending.interactions,
    }) else {
        return;
    };
    let Some(win) = window() else {
        return;
    };

    let opts = RequestInit::new();
    opts.set_method("POST");
    opts.set_credentials(RequestCredentials::Include);
    opts.set_body(&JsValue::from_str(&body));
    let headers = js_sys::Object::new();
    let _ = js_sys::Reflect::set(
        &headers,
        &JsValue::from_str("Content-Type"),
        &JsValue::from_str("application/json"),
    );
    opts.set_headers_record_from_str_to_str(&headers);
    // `keepalive` is in the Fetch spec but not in the web-sys build this crate
    // compiles against (0.3.103 has no `RequestInit::keepalive`). At runtime
    // `RequestInit` *is* a plain JS dictionary, so setting the member directly
    // is exactly what the missing setter would do.
    let init: JsValue = opts.clone().into();
    let _ = js_sys::Reflect::set(&init, &JsValue::from_str("keepalive"), &JsValue::TRUE);

    let url = format!("{}/interaction/batch", pending.backend_url);
    let Ok(request) = Request::new_with_str_and_init(&url, &opts) else {
        return;
    };
    let _ = win.fetch_with_request(&request);
}

/// Install the unload handlers once, at startup.
///
/// Two events because neither covers everything: `pagehide` fires on
/// navigation, reload and close, while `visibilitychange` also fires when the
/// browser backgrounds the tab or the OS discards it — and on mobile it is
/// often the last callback a page gets at all.
pub fn install_unload_flush() {
    let Some(win) = window() else {
        return;
    };
    let on_pagehide = Closure::<dyn FnMut()>::new(flush_keepalive);
    let _ = win.add_event_listener_with_callback("pagehide", on_pagehide.as_ref().unchecked_ref());
    on_pagehide.forget();

    let Some(doc) = win.document() else {
        return;
    };
    let doc_for_callback = doc.clone();
    let on_hidden = Closure::<dyn FnMut()>::new(move || {
        if doc_for_callback.hidden() {
            flush_keepalive();
        }
    });
    let _ = doc
        .add_event_listener_with_callback("visibilitychange", on_hidden.as_ref().unchecked_ref());
    on_hidden.forget();
}

/// Schedule a single debounced flush; a no-op if one is already pending.
fn schedule_debounce() {
    let already = TIMER_ID.with(|cell| *cell.borrow() != 0);
    if already {
        return;
    }
    let callback = Closure::once_into_js(move || {
        TIMER_ID.with(|cell| *cell.borrow_mut() = 0);
        flush();
    });
    let Some(win) = window() else {
        flush();
        return;
    };
    match win.set_timeout_with_callback_and_timeout_and_arguments_0(
        callback.as_ref().unchecked_ref(),
        DEBOUNCE_MS,
    ) {
        Ok(id) => {
            TIMER_ID.with(|cell| *cell.borrow_mut() = id);
        }
        Err(_) => flush(),
    }
}

/// Cancel a pending debounce timer, if any.
fn clear_timer() {
    let id = TIMER_ID.with(|cell| {
        let id = *cell.borrow();
        *cell.borrow_mut() = 0;
        id
    });
    if id != 0
        && let Some(win) = window()
    {
        win.clear_timeout_with_handle(id);
    }
}
