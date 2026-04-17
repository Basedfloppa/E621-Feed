use serde::de::DeserializeOwned;
use std::cell::Cell;
use std::rc::Rc;
use web_sys::js_sys;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen_futures::spawn_local;
use web_sys::HtmlInputElement;
use web_sys::{Request, RequestInit, RequestMode, Response, window};
use yew::prelude::*;

use crate::components::*;
use crate::models::*;
use crate::pages::UserInfo;

const PIXELS_BEFORE_REFETCH: f64 = 1000.0;

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

        Callback::from(move |_| {
            if inflight.borrow().get() {
                return;
            }
            if *is_loading {
                return;
            }

            let Some(user) = (*selected_user).clone() else {
                error.set(Some("Select an account to load the feed.".to_string()));
                return;
            };

            let cfg = read_config_from_head().unwrap();
            let mut url = format!(
                "{}/recommendations/{}?page={}",
                cfg.backend_domain, user.id, *page
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
                            page.set(*page + 1);
                        }

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
        let fetch_page = fetch_page.clone();

        use_effect_with(
            (*selected_user).clone(),
            move |selected: &Option<UserInfo>| {
                if selected.is_some() {
                    posts.set(Vec::new());
                    page.set(1);
                    error.set(None);
                    is_loading.set(false);
                    fetch_page.emit(());
                }
                || ()
            },
        );
    }

    {
        let is_loading = is_loading.clone();
        let selected_user = selected_user.clone();
        let fetch_page = fetch_page.clone();

        use_effect(move || {
            let mut listener: Option<(web_sys::Window, Closure<dyn FnMut(Event)>)> = None;

            if let Some(win) = window() {
                let is_loading_cb = is_loading.clone();
                let selected_user_cb = selected_user.clone();
                let fetch_page_cb = fetch_page.clone();

                let win_for_cb = win.clone();
                let on_scroll = Closure::<dyn FnMut(Event)>::wrap(Box::new(move |_e: Event| {
                    if (*selected_user_cb).is_some() && !*is_loading_cb {
                        let scroll_y = win_for_cb.scroll_y().unwrap_or(0.0);
                        let inner_h = win_for_cb
                            .inner_height()
                            .ok()
                            .and_then(|h| h.as_f64())
                            .unwrap_or(0.0);

                        let doc = match win_for_cb.document() {
                            Some(d) => d,
                            None => return,
                        };
                        let scroll_h = if let Some(el) = doc.document_element() {
                            el.scroll_height() as f64
                        } else if let Some(body) = doc.body() {
                            body.scroll_height() as f64
                        } else {
                            0.0
                        };

                        if scroll_y + inner_h + PIXELS_BEFORE_REFETCH >= scroll_h {
                            fetch_page_cb.emit(());
                        }
                    }
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

    html! {
        <div class="container my-4 gap-2">
            <h2 class="mb-3">{ "Latest Posts" }</h2>

            <div id="feed-account">
                <SavedAccountsSelect
                    selected_user={selected_user.clone()}
                    is_loading={is_loading.clone()}
                />
            </div>

            <div class="row g-3 align-items-center">
                <div class="col-auto" id="feed-affinity">
                    <label>{"Minimal affinity"}
                        <input
                            type="number"
                            class="form-control"
                            value={affinity.to_string()}
                            step="0.01"
                            oninput={{
                                let affinity = affinity.clone();
                                Callback::from(move |e: InputEvent| {
                                    if let Some(target) = e.target() {
                                        if let Ok(input) = target.dyn_into::<HtmlInputElement>() {
                                            if let Ok(v) = input.value().parse::<f32>() {
                                                affinity.set(v);
                                            }
                                        }
                                    }
                                })
                            }}
                        />
                    </label>
                </div>

                <div class="col-auto" id="feed-grid">
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

            <div class="position-fixed bottom-0 start-50 translate-middle-x w-100 d-flex justify-content-between z-1">
                {
                    if let Some(u) = &*selected_user {
                        html! { <span class="text-muted small m-3 bg-dark bg-opacity-50 rounded-pill badge">{ format!("User: {} (ID: {})", u.name, u.id) }</span> }
                    } else {
                        html! { <span class="text-muted small m-3 bg-dark bg-opacity-50 rounded-pill badge">{ "No user selected" }</span> }
                    }
                }
                {
                    if !(*is_loading) && (*error).is_none() {
                        html! { <span class="text-muted small m-3 bg-dark bg-opacity-50 rounded-pill badge" aria-live="polite">{ format!("Loaded {} posts", posts.len()) }</span> }
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

            <div class="row g-3 m-3" aria-busy={(*is_loading).to_string()}>
                {
                    posts.iter().enumerate().map(|(idx, sp)| {
                        let sp = sp.clone();
                        let position = (idx + 1) as i32;
                        let backend_url = read_config_from_head()
                            .map(|cfg| cfg.backend_domain)
                            .unwrap_or_default();
                        html! {
                            <div key={sp.post.id} class={ (*grid).col_class() } style="min-width: 200px">
                                <PostCard
                                    affinity={sp.score}
                                    post={Rc::new(sp.post.clone())}
                                    backend_url={backend_url}
                                    account_id={selected_user.as_ref().map(|u| u.id as i32).unwrap_or_default()}
                                    session_id={(*session_id).clone()}
                                    position={position}
                                    breakdown={sp.breakdown.clone()}
                                />
                            </div>
                        }
                    }).collect::<Html>()
                }
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
