use serde::de::DeserializeOwned;
use std::cell::Cell;
use std::rc::Rc;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen_futures::spawn_local;
use web_sys::HtmlInputElement;
use web_sys::js_sys;
use web_sys::{Request, RequestInit, RequestMode, Response, window};
use yew::prelude::*;

use crate::components::*;
use crate::models::*;
use crate::pages::UserInfo;

const PIXELS_BEFORE_REFETCH: f64 = 1000.0;
/// Stop auto-fetching after this many consecutive empty/all-duplicate pages,
/// so an exhausted catalog or a strict affinity filter can't loop forever.
const MAX_CONSECUTIVE_EMPTY_PAGES: u32 = 10;

#[derive(Clone, Copy, PartialEq, Eq)]
enum GridType {
    Auto,
    Three,
    Two,
    One,
}

impl GridType {
    fn from_storage(s: Option<String>) -> Self {
        match s.as_deref() {
            Some("3") => GridType::Three,
            Some("2") => GridType::Two,
            Some("1") => GridType::One,
            _ => GridType::Auto,
        }
    }
    fn to_storage(self) -> &'static str {
        match self {
            GridType::Auto => "auto",
            GridType::Three => "3",
            GridType::Two => "2",
            GridType::One => "1",
        }
    }
    fn col_class(self) -> &'static str {
        match self {
            GridType::Auto => {
                "col-xs-6 col-sm-5 col-md-4 col-lg-3 col-xl-2 col-xxl-1 d-flex justify-content-center"
            }
            GridType::Three => "col-4 d-flex justify-content-center",
            GridType::Two => "col-6 d-flex justify-content-center",
            GridType::One => "col-12 d-flex justify-content-center",
        }
    }
}

#[function_component(FeedPage)]
pub fn feed_page() -> Html {
    let posts = use_state(Vec::<ScoredPost>::new);
    let page = use_state(|| 1usize);
    let is_loading = use_state(|| false);
    let inflight = use_mut_ref(|| Cell::new(false));
    let exhausted = use_state(|| false);
    let consecutive_empty = use_mut_ref(|| Cell::new(0u32));
    let error = use_state(|| Option::<String>::None);
    let selected_user = use_state(|| Option::<UserInfo>::None);
    let session_id = use_state(|| {
        format!(
            "feed-{}-{}",
            js_sys::Date::now() as u64,
            (js_sys::Math::random() * 1_000_000_000.0) as u64
        )
    });
    let affinity = use_state(|| {
        window()
            .and_then(|w| w.local_storage().ok().flatten())
            .and_then(|s| s.get_item("affinity_threshold").ok().flatten())
            .and_then(|v| v.parse::<f32>().ok())
            .unwrap_or(0.0)
    });
    let grid = use_state(|| {
        let stored = window()
            .and_then(|w| w.local_storage().ok().flatten())
            .and_then(|s| s.get_item("feed_grid_type").ok().flatten());
        GridType::from_storage(stored)
    });

    {
        let affinity = affinity.clone();
        use_effect_with(*affinity, move |a: &f32| {
            if let Some(store) = window().and_then(|w| w.local_storage().ok().flatten()) {
                let _ = store.set_item("affinity_threshold", &a.to_string());
            }
            || ()
        });
    }

    {
        let grid = grid.clone();
        use_effect_with(*grid, move |g: &GridType| {
            if let Some(store) = window().and_then(|w| w.local_storage().ok().flatten()) {
                let _ = store.set_item("feed_grid_type", g.to_storage());
            }
            || ()
        });
    }

    let fetch_page = {
        let posts = posts.clone();
        let page = page.clone();
        let is_loading = is_loading.clone();
        let error = error.clone();
        let selected_user = selected_user.clone();
        let affinity = affinity.clone();
        let inflight = inflight.clone();
        let exhausted = exhausted.clone();
        let consecutive_empty = consecutive_empty.clone();

        Callback::from(move |_| {
            if inflight.borrow().get() {
                return;
            }
            if *is_loading || *exhausted {
                return;
            }

            let Some(user) = (*selected_user).clone() else {
                error.set(Some("Select an account to load the feed.".to_string()));
                return;
            };

            let Some(owner_token) = get_or_create_owner_token() else {
                error.set(Some("Missing device token".to_string()));
                return;
            };

            let cfg = read_config_from_head().unwrap();
            let mut url = format!(
                "{}/recommendations/{}?owner_token={}&page={}",
                cfg.backend_domain,
                user.id,
                urlencoding::encode(&owner_token),
                *page
            );

            let value = *affinity;
            if value > 0.0 {
                url.push_str(&format!("&affinity_threshold={value}"));
            }

            inflight.borrow().set(true);
            is_loading.set(true);
            error.set(None);

            let posts = posts.clone();
            let page = page.clone();
            let is_loading = is_loading.clone();
            let inflight_done = inflight.clone();
            let error = error.clone();
            let exhausted = exhausted.clone();
            let consecutive_empty = consecutive_empty.clone();

            spawn_local(async move {
                let done = || {
                    is_loading.set(false);
                    inflight_done.borrow().set(false);
                };

                match fetch_json::<Vec<ScoredPost>>(&url).await {
                    Ok(mut new_items) => {
                        use std::collections::HashSet;
                        let mut merged: Vec<ScoredPost> = (*posts).clone();
                        let mut seen: HashSet<i64> = merged.iter().map(|p| p.post.id).collect();
                        new_items.retain(|p| seen.insert(p.post.id));

                        web_sys::console::log_1(
                            &format!("Received recommendation page with {:?}", &new_items.len())
                                .into(),
                        );
                        let added = new_items.len();
                        if added > 0 {
                            merged.extend(new_items);
                            posts.set(merged);
                            consecutive_empty.borrow().set(0);
                        } else {
                            // Empty/all-duplicates: count it but still advance
                            // the cursor so the next scroll-trigger doesn't
                            // refetch the same page forever.
                            let next = consecutive_empty.borrow().get() + 1;
                            consecutive_empty.borrow().set(next);
                            if next >= MAX_CONSECUTIVE_EMPTY_PAGES {
                                exhausted.set(true);
                            }
                        }
                        page.set(*page + 1);

                        done();
                    }
                    Err(e) => {
                        web_sys::console::log_1(&e.clone().into());
                        error.set(Some(e));
                        done();
                    }
                }
            });
        })
    };

    {
        let posts = posts.clone();
        let page = page.clone();
        let error = error.clone();
        let is_loading = is_loading.clone();
        let exhausted = exhausted.clone();
        let consecutive_empty = consecutive_empty.clone();
        let fetch_page = fetch_page.clone();

        use_effect_with(
            (*selected_user).clone(),
            move |selected: &Option<UserInfo>| {
                if selected.is_some() {
                    posts.set(Vec::new());
                    page.set(1);
                    error.set(None);
                    is_loading.set(false);
                    exhausted.set(false);
                    consecutive_empty.borrow().set(0);
                    fetch_page.emit(());
                }
                || ()
            },
        );
    }

    // Lowering the affinity threshold should let scrolling fetch again even
    // if we previously hit the empty-page cap with a stricter filter.
    {
        let exhausted = exhausted.clone();
        let consecutive_empty = consecutive_empty.clone();
        use_effect_with(*affinity, move |_| {
            exhausted.set(false);
            consecutive_empty.borrow().set(0);
            || ()
        });
    }

    {
        let is_loading = is_loading.clone();
        let selected_user = selected_user.clone();
        let exhausted = exhausted.clone();
        let fetch_page = fetch_page.clone();

        use_effect(move || {
            let mut listener: Option<(web_sys::Window, Closure<dyn FnMut(Event)>)> = None;

            if let Some(win) = window() {
                // Throttle: scroll fires at >100Hz on touchpads/wheels and
                // each call reads `scroll_height` (forces layout). Coalesce
                // to one check per frame via rAF.
                let scroll_pending: Rc<Cell<bool>> = Rc::new(Cell::new(false));

                let is_loading_cb = is_loading.clone();
                let selected_user_cb = selected_user.clone();
                let exhausted_cb = exhausted.clone();
                let fetch_page_cb = fetch_page.clone();

                let win_for_cb = win.clone();
                let scroll_pending_cb = scroll_pending.clone();
                let on_scroll = Closure::<dyn FnMut(Event)>::wrap(Box::new(move |_e: Event| {
                    if scroll_pending_cb.get() {
                        return;
                    }
                    scroll_pending_cb.set(true);

                    let scroll_pending_inner = scroll_pending_cb.clone();
                    let win_inner = win_for_cb.clone();
                    let selected_user_inner = selected_user_cb.clone();
                    let is_loading_inner = is_loading_cb.clone();
                    let exhausted_inner = exhausted_cb.clone();
                    let fetch_inner = fetch_page_cb.clone();

                    let raf_cb = Closure::once_into_js(move |_: f64| {
                        scroll_pending_inner.set(false);
                        if (*selected_user_inner).is_some()
                            && !*is_loading_inner
                            && !*exhausted_inner
                        {
                            let scroll_y = win_inner.scroll_y().unwrap_or(0.0);
                            let inner_h = win_inner
                                .inner_height()
                                .ok()
                                .and_then(|h| h.as_f64())
                                .unwrap_or(0.0);

                            let Some(doc) = win_inner.document() else {
                                return;
                            };
                            let scroll_h = if let Some(el) = doc.document_element() {
                                el.scroll_height() as f64
                            } else if let Some(body) = doc.body() {
                                body.scroll_height() as f64
                            } else {
                                0.0
                            };

                            if scroll_y + inner_h + PIXELS_BEFORE_REFETCH >= scroll_h {
                                fetch_inner.emit(());
                            }
                        }
                    });
                    let _ = win_for_cb.request_animation_frame(raf_cb.as_ref().unchecked_ref());
                }));

                let _ = win
                    .add_event_listener_with_callback("scroll", on_scroll.as_ref().unchecked_ref());
                listener = Some((win.clone(), on_scroll));

                let scroll_y = win.scroll_y().unwrap_or(0.0);
                let inner_h = win
                    .inner_height()
                    .ok()
                    .and_then(|h| h.as_f64())
                    .unwrap_or(0.0);
                let doc = win.document();
                let scroll_h = doc
                    .as_ref()
                    .and_then(|d| d.document_element())
                    .map(|el| el.scroll_height() as f64)
                    .or_else(|| {
                        doc.as_ref()
                            .and_then(|d| d.body())
                            .map(|b| b.scroll_height() as f64)
                    })
                    .unwrap_or(0.0);

                if (*selected_user).is_some()
                    && !*is_loading
                    && !*exhausted
                    && (scroll_y + inner_h + PIXELS_BEFORE_REFETCH >= scroll_h)
                {
                    fetch_page.emit(());
                }
            }

            move || {
                if let Some((win, on_scroll)) = listener {
                    let _ = win.remove_event_listener_with_callback(
                        "scroll",
                        on_scroll.as_ref().unchecked_ref(),
                    );
                }
            }
        });
    }

    let backend_url = read_config_from_head()
        .map(|cfg| cfg.backend_domain)
        .unwrap_or_default();
    let card_account_id = selected_user.as_ref().map(|u| u.id as i32).unwrap_or_default();
    let card_col_class = (*grid).col_class();
    let card_session_id = (*session_id).clone();
    let feed_cards: Html = posts
        .iter()
        .enumerate()
        .map(|(idx, sp)| {
            let position = (idx + 1) as i32;
            html! {
                <div key={sp.post.id} class={ card_col_class } style="min-width: 200px">
                    <PostCard
                        affinity={sp.score}
                        post={Rc::new(sp.post.clone())}
                        backend_url={backend_url.clone()}
                        account_id={card_account_id}
                        session_id={card_session_id.clone()}
                        position={position}
                        breakdown={sp.breakdown.clone()}
                    />
                </div>
            }
        })
        .collect();

    html! {
        <div class="container my-4 gap-2 feed-page">
            <h2 class="mb-3">{ "Latest Posts" }</h2>

            <div id="feed-account">
                <SavedAccountsSelect
                    selected_user={selected_user.clone()}
                    is_loading={is_loading.clone()}
                />
            </div>

            <div class="row g-3 align-items-center feed-toolbar">
                <div class="col-auto feed-affinity-col" id="feed-affinity">
                    <label for="feed-affinity-input" class="form-label mb-1 d-block">
                        {"Minimum affinity"}
                        <small class="text-muted ms-1">
                            { "(0 = show everything, 1 = only perfect matches)" }
                        </small>
                    </label>
                    <div class="d-flex align-items-center gap-2 flex-wrap">
                        <input
                            id="feed-affinity-input"
                            type="number"
                            class="form-control"
                            style="max-width: 8rem"
                            value={affinity.to_string()}
                            step="0.01"
                            min="0"
                            max="1"
                            oninput={{
                                let affinity = affinity.clone();
                                Callback::from(move |e: InputEvent| {
                                    if let Some(target) = e.target() {
                                        if let Ok(input) = target.dyn_into::<HtmlInputElement>() {
                                            if let Ok(v) = input.value().parse::<f32>() {
                                                affinity.set(v.clamp(0.0, 1.0));
                                            }
                                        }
                                    }
                                })
                            }}
                        />
                        <div class="btn-group btn-group-sm" role="group" aria-label="Affinity preset">
                            {
                                [
                                    ("Wide", 0.05f32, "Show most posts — good for discovery."),
                                    ("Balanced", 0.20f32, "Recommended starting point — filters out the least relevant posts."),
                                    ("Strict", 0.40f32, "Only posts that match your profile well."),
                                ].iter().map(|(label, value, tip)| {
                                    let val = *value;
                                    let active = (*affinity - val).abs() < 0.005;
                                    let affinity = affinity.clone();
                                    html! {
                                        <button
                                            type="button"
                                            class={classes!("btn", "btn-outline-secondary", active.then_some("active"))}
                                            title={ *tip }
                                            aria-pressed={active.to_string()}
                                            onclick={Callback::from(move |_| affinity.set(val))}
                                        >
                                            { *label }
                                        </button>
                                    }
                                }).collect::<Html>()
                            }
                        </div>
                    </div>
                </div>

                <div class="col-auto feed-grid-col" id="feed-grid">
                    <span class="d-block">{"Grid type"}</span>
                    <div class="btn-group" role="group" aria-label="Grid type">
                        <button
                            class={classes!("btn","btn-dark", if *grid == GridType::Auto { "active" } else { "" })}
                            aria-pressed={(*grid == GridType::Auto).to_string()}
                            onclick={{
                                let grid = grid.clone();
                                Callback::from(move |_| grid.set(GridType::Auto))
                            }}
                        >
                            <i class="bi bi-water"></i>
                        </button>

                        <button
                            class={classes!("btn","btn-dark", if *grid == GridType::Three { "active" } else { "" })}
                            aria-pressed={(*grid == GridType::Three).to_string()}
                            onclick={{
                                let grid = grid.clone();
                                Callback::from(move |_| grid.set(GridType::Three))
                            }}
                        >
                            <i class="bi bi-grid-3x3-gap-fill"></i>
                        </button>

                        <button
                            class={classes!("btn","btn-dark", if *grid == GridType::Two { "active" } else { "" })}
                            aria-pressed={(*grid == GridType::Two).to_string()}
                            onclick={{
                                let grid = grid.clone();
                                Callback::from(move |_| grid.set(GridType::Two))
                            }}
                        >
                            <i class="bi bi-grid-fill"></i>
                        </button>

                        <button
                            class={classes!("btn","btn-dark", if *grid == GridType::One { "active" } else { "" })}
                            aria-pressed={(*grid == GridType::One).to_string()}
                            onclick={{
                                let grid = grid.clone();
                                Callback::from(move |_| grid.set(GridType::One))
                            }}
                        >
                            <i class="bi bi-square-fill"></i>
                        </button>
                    </div>
                </div>
            </div>

            <div class="position-fixed bottom-0 start-50 translate-middle-x w-100 d-flex justify-content-between z-1 feed-statusbar">
                {
                    if let Some(u) = &*selected_user {
                        html! { <span class="text-muted small m-3 bg-dark bg-opacity-50 rounded-pill badge">{ format!("User: {} (ID: {})", u.name, u.id) }</span> }
                    } else {
                        html! { <span class="text-muted small m-3 bg-dark bg-opacity-50 rounded-pill badge">{ "No user selected" }</span> }
                    }
                }
                {
                    if !(*is_loading) && (*error).is_none() {
                        let label = if *exhausted {
                            format!("Loaded {} posts — no more matches above the affinity threshold", posts.len())
                        } else {
                            format!("Loaded {} posts", posts.len())
                        };
                        html! { <span class="text-muted small m-3 bg-dark bg-opacity-50 rounded-pill badge" aria-live="polite">{ label }</span> }
                    } else { html!{ <span class="text-muted small m-3 bg-dark bg-opacity-50 rounded-pill badge" aria-live="polite">{"Loading..."}</span>} }
                }
            </div>

            {
                if let Some(err) = &*error {
                    html! {
                        <div class="alert alert-danger d-flex justify-content-between align-items-center" role="alert" aria-live="polite">
                            <span>{ err }</span>
                            <button
                                class="btn btn-sm btn-outline-light"
                                type="button"
                                onclick={{
                                    let fetch_page = fetch_page.clone();
                                    Callback::from(move |_| fetch_page.emit(()))
                                }}
                            >
                                { "Retry" }
                            </button>
                        </div>
                    }
                } else { html!{} }
            }

            {
                if selected_user.is_some() && !*is_loading && error.is_none() && posts.is_empty() {
                    html! {
                        <div class="text-center text-muted my-5" aria-live="polite">
                            { "No posts yet." }
                        </div>
                    }
                } else { html!{} }
            }

            <div class="row g-3 m-3 feed-grid" aria-busy={(*is_loading).to_string()}>
                { feed_cards }
            </div>

            {
                if *is_loading && posts.is_empty() {
                    html! {
                        <div class="d-flex justify-content-center my-4">
                            <div class="spinner-border" role="status">
                                <span class="visually-hidden">{ "Loading..." }</span>
                            </div>
                        </div>
                    }
                } else { html!{} }
            }
        </div>
    }
}

async fn fetch_json<T: DeserializeOwned>(url: &str) -> Result<T, String> {
    let window = window().ok_or("No window available".to_string())?;

    let opts = RequestInit::new();
    opts.set_method("GET");
    opts.set_mode(RequestMode::Cors);

    let request = Request::new_with_str_and_init(url, &opts)
        .map_err(|e| format!("Failed to create request: {e:?}"))?;

    let resp_value = wasm_bindgen_futures::JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(|e| format!("Fetch promise rejected: {e:?}"))?;

    let resp: Response = resp_value
        .dyn_into()
        .map_err(|_| "Failed to cast Response".to_string())?;

    if !resp.ok() {
        return Err(format!(
            "HTTP error {} {}",
            resp.status(),
            resp.status_text()
        ));
    }

    let text_promise = resp
        .text()
        .map_err(|e| format!("Failed to read response text: {e:?}"))?;
    let text_js = wasm_bindgen_futures::JsFuture::from(text_promise)
        .await
        .map_err(|e| format!("Text promise rejected: {e:?}"))?;
    let text = text_js
        .as_string()
        .ok_or("Response text not a string".to_string())?;

    serde_json::from_str::<T>(&text).map_err(|e| format!("JSON parse error: {e}"))
}
