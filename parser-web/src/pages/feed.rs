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

/// Type alias for closure types.
type ScrollListener = Option<(web_sys::Window, Closure<dyn FnMut(Event)>)>;

const PIXELS_BEFORE_REFETCH: f64 = 1000.0;
/// Stop auto-fetching after this many consecutive empty/all-duplicate pages,
/// so an exhausted catalog or a strict per-page cutoff can't loop forever.
const MAX_CONSECUTIVE_EMPTY_PAGES: u32 = 10;
/// Stop auto-fetching after this many consecutive backend errors. Without
/// this, a scrolling user whose first request 500'd would re-trigger the
/// same broken request on every scroll frame — and each retry hits e621
/// from the admin account, accelerating any rate-limit/ban already in
/// progress. The user has to click "Retry" to resume after the threshold.
const MAX_CONSECUTIVE_ERRORS: u32 = 3;

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
    let consecutive_errors = use_mut_ref(|| Cell::new(0u32));
    let error = use_state(|| Option::<String>::None);
    let selected_user = use_state(|| Option::<UserInfo>::None);
    let session_id = use_state(|| {
        format!(
            "feed-{}-{}",
            js_sys::Date::now() as u64,
            (js_sys::Math::random() * 1_000_000_000.0) as u64
        )
    });
    let show_desc = use_state(|| {
        window()
            .and_then(|w| w.local_storage().ok().flatten())
            .and_then(|s| s.get_item("hide_post_desc").ok().flatten())
            .and_then(|v| v.parse::<bool>().ok())
            .unwrap_or(false)
    });
    let show_metadata = use_state(|| {
        window()
            .and_then(|w| w.local_storage().ok().flatten())
            .and_then(|s| s.get_item("show_file_metadata").ok().flatten())
            .and_then(|v| v.parse::<bool>().ok())
            .unwrap_or(false)
    });
    let show_breakdown = use_state(|| {
        window()
            .and_then(|w| w.local_storage().ok().flatten())
            .and_then(|s| s.get_item("show_score_breakdown").ok().flatten())
            .and_then(|v| v.parse::<bool>().ok())
            .unwrap_or(false)
    });
    // Per-page bottom-cutoff in percent (0..=95). 0 = show everything,
    // 30 = drop the bottom 30% of each fetched page by raw score, 95 =
    // keep only the top 5%. Decoupled from the model's raw `score` so
    // future scoring changes don't shift what the slider means.
    let cutoff_pct = use_state(|| {
        window()
            .and_then(|w| w.local_storage().ok().flatten())
            .and_then(|s| s.get_item("page_cutoff_pct").ok().flatten())
            .and_then(|v| v.parse::<f32>().ok())
            .unwrap_or(0.0)
            .clamp(0.0, 95.0)
    });
    let grid = use_state(|| {
        let stored = window()
            .and_then(|w| w.local_storage().ok().flatten())
            .and_then(|s| s.get_item("feed_grid_type").ok().flatten());
        GridType::from_storage(stored)
    });

    {
        let show_desc = show_desc.clone();
        use_effect_with(*show_desc, move |a: &bool| {
            if let Some(store) = window().and_then(|w| w.local_storage().ok().flatten()) {
                let _ = store.set_item("hide_post_desc", &a.to_string());
            }
            || ()
        });
    }

    {
        let show_metadata = show_metadata.clone();
        use_effect_with(*show_metadata, move |a: &bool| {
            if let Some(store) = window().and_then(|w| w.local_storage().ok().flatten()) {
                let _ = store.set_item("show_file_metadata", &a.to_string());
            }
            || ()
        });
    }

    {
        let show_breakdown = show_breakdown.clone();
        use_effect_with(*show_breakdown, move |a: &bool| {
            if let Some(store) = window().and_then(|w| w.local_storage().ok().flatten()) {
                let _ = store.set_item("show_score_breakdown", &a.to_string());
            }
            || ()
        });
    }

    {
        let cutoff_pct = cutoff_pct.clone();
        use_effect_with(*cutoff_pct, move |a: &f32| {
            if let Some(store) = window().and_then(|w| w.local_storage().ok().flatten()) {
                let _ = store.set_item("page_cutoff_pct", &a.to_string());
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
        let cutoff_pct = cutoff_pct.clone();
        let session_id = session_id.clone();
        let inflight = inflight.clone();
        let exhausted = exhausted.clone();
        let consecutive_empty = consecutive_empty.clone();
        let consecutive_errors = consecutive_errors.clone();

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

            let Some(cfg) = read_config_from_head() else {
                error.set(Some(
                    "App configuration failed to load — please reload the page.".to_string(),
                ));
                return;
            };
            let url = format!(
                "{}/recommendations/{}?page={}&session={}",
                cfg.backend_domain,
                user.id,
                *page,
                urlencoding::encode(session_id.as_str())
            );

            // Captured for the per-page cutoff filter that runs after fetch.
            let cutoff_value = *cutoff_pct;

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
            let consecutive_errors = consecutive_errors.clone();

            spawn_local(async move {
                let done = || {
                    is_loading.set(false);
                    inflight_done.borrow().set(false);
                };

                match fetch_json::<Vec<ScoredPost>>(&url).await {
                    Ok(mut new_items) => {
                        use std::collections::HashSet;
                        // Per-page bottom cutoff: drop the bottom
                        // `cutoff_value`% of this page by raw score.
                        // Computed against this page's distribution
                        // only, so the slider stays meaningful even
                        // when the model gets more discriminative and
                        // pushes scores towards the extremes.
                        if cutoff_value > 0.0 && new_items.len() >= 2 {
                            let mut sorted: Vec<f32> =
                                new_items.iter().map(|p| p.score).collect();
                            sorted.sort_by(|a, b| {
                                a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
                            });
                            let frac = (cutoff_value / 100.0).clamp(0.0, 0.99);
                            let idx =
                                ((sorted.len() as f32 - 1.0) * frac).round() as usize;
                            let threshold = sorted[idx.min(sorted.len() - 1)];
                            new_items.retain(|p| p.score >= threshold);
                        }
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
                        // A successful round-trip clears the error budget,
                        consecutive_errors.borrow().set(0);
                        page.set(*page + 1);

                        done();
                    }
                    Err(e) => {
                        web_sys::console::log_1(&e.clone().into());
                        let n = consecutive_errors.borrow().get() + 1;
                        consecutive_errors.borrow().set(n);
                        if n >= MAX_CONSECUTIVE_ERRORS {
                            exhausted.set(true);
                            error.set(Some(format!(
                                "{e} — auto-retry suspended after {n} failures. Click Retry to resume."
                            )));
                        } else {
                            error.set(Some(e));
                        }
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
        let consecutive_errors = consecutive_errors.clone();
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
                    consecutive_errors.borrow().set(0);
                    fetch_page.emit(());
                }
                || ()
            },
        );
    }

    // Lowering the per-page cutoff should let scrolling fetch again
    // even if we previously hit the empty-page cap with a stricter
    // filter setting.
    {
        let exhausted = exhausted.clone();
        let consecutive_empty = consecutive_empty.clone();
        let consecutive_errors = consecutive_errors.clone();
        let error = error.clone();
        use_effect_with(*cutoff_pct, move |_| {
            exhausted.set(false);
            consecutive_empty.borrow().set(0);
            consecutive_errors.borrow().set(0);
            error.set(None);
            || ()
        });
    }

    {
        let is_loading = is_loading.clone();
        let selected_user = selected_user.clone();
        let exhausted = exhausted.clone();
        let error = error.clone();
        let fetch_page = fetch_page.clone();

        use_effect(move || {
            let mut listener: ScrollListener = None;

            if let Some(win) = window() {
                // Throttle: scroll fires at >100Hz on touchpads/wheels and
                // each call reads `scroll_height` (forces layout). Coalesce
                // to one check per frame via rAF.
                let scroll_pending: Rc<Cell<bool>> = Rc::new(Cell::new(false));

                let is_loading_cb = is_loading.clone();
                let selected_user_cb = selected_user.clone();
                let exhausted_cb = exhausted.clone();
                let error_cb = error.clone();
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
                    let error_inner = error_cb.clone();
                    let fetch_inner = fetch_page_cb.clone();

                    let raf_cb = Closure::once_into_js(move |_: f64| {
                        scroll_pending_inner.set(false);
                        // `error.is_none()` gate: once a request fails, don't
                        // let scrolling re-trigger the same broken request.
                        if (*selected_user_inner).is_some()
                            && !*is_loading_inner
                            && !*exhausted_inner
                            && (*error_inner).is_none()
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
                    && (*error).is_none()
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
                        show_desc={*show_desc.clone()}
                        show_metadata={*show_metadata.clone()}
                        show_breakdown={*show_breakdown.clone()}
                    />
                </div>
            }
        })
        .collect();

    html! {
        <div class="container my-4 gap-2 feed-page">
            <h1 class="h2 mb-3">{ "Latest Posts" }</h1>

            <div id="feed-account">
                <SavedAccountsSelect
                    selected_user={selected_user.clone()}
                    is_loading={is_loading.clone()}
                />
            </div>

            <div class="row g-3 align-items-center feed-toolbar">
                <div class="col-auto feed-affinity-col" id="feed-affinity">
                    <label for="feed-affinity-input" class="form-label mb-1 d-block">
                        {"Per-page cutoff"}
                        <small class="text-muted ms-1">
                            { "(% of worst posts to drop)" }
                        </small>
                    </label>
                    <div class="d-flex align-items-center gap-2">
                        <input
                            id="feed-affinity-input"
                            type="number"
                            class="form-control"
                            style="max-width: 8rem"
                            value={cutoff_pct.to_string()}
                            step="5"
                            min="0"
                            max="95"
                            oninput={{
                                let cutoff_pct = cutoff_pct.clone();
                                Callback::from(move |e: InputEvent| {
                                    if let Some(target) = e.target()
                                        && let Ok(input) = target.dyn_into::<HtmlInputElement>()
                                            && let Ok(v) = input.value().parse::<f32>() {
                                                cutoff_pct.set(v.clamp(0.0, 95.0));
                                            }
                                })
                            }}
                        />
                        <div class="btn-group btn-group-sm" role="group" aria-label="Cutoff preset">
                            {
                                [
                                    ("Wide", 0.0f32, "Show every post on the page — good for discovery."),
                                    ("Balanced", 30.0f32, "Drop the weakest 30% per page — recommended starting point."),
                                    ("Strict", 60.0f32, "Drop the weakest 60% per page — keep only the top matches."),
                                ].iter().map(|(label, value, tip)| {
                                    let val = *value;
                                    let active = (*cutoff_pct - val).abs() < 0.5;
                                    let cutoff_pct = cutoff_pct.clone();
                                    html! {
                                        <button
                                            type="button"
                                            class={classes!("btn", "btn-outline-secondary", active.then_some("active"))}
                                            title={ *tip }
                                            aria-pressed={active.to_string()}
                                            onclick={Callback::from(move |_| cutoff_pct.set(val))}
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
                            type="button"
                            class={classes!("btn","btn-outline-secondary", if *grid == GridType::Auto { "active" } else { "" })}
                            aria-pressed={(*grid == GridType::Auto).to_string()}
                            aria-label="Auto grid (responsive)"
                            title="Auto grid (responsive)"
                            onclick={{
                                let grid = grid.clone();
                                Callback::from(move |_| grid.set(GridType::Auto))
                            }}
                        >
                            <i class="bi bi-water" aria-hidden="true"></i>
                        </button>

                        <button
                            type="button"
                            class={classes!("btn","btn-outline-secondary", if *grid == GridType::Three { "active" } else { "" })}
                            aria-pressed={(*grid == GridType::Three).to_string()}
                            aria-label="Three-column grid"
                            title="Three-column grid"
                            onclick={{
                                let grid = grid.clone();
                                Callback::from(move |_| grid.set(GridType::Three))
                            }}
                        >
                            <i class="bi bi-grid-3x3-gap-fill" aria-hidden="true"></i>
                        </button>

                        <button
                            type="button"
                            class={classes!("btn","btn-outline-secondary", if *grid == GridType::Two { "active" } else { "" })}
                            aria-pressed={(*grid == GridType::Two).to_string()}
                            aria-label="Two-column grid"
                            title="Two-column grid"
                            onclick={{
                                let grid = grid.clone();
                                Callback::from(move |_| grid.set(GridType::Two))
                            }}
                        >
                            <i class="bi bi-grid-fill" aria-hidden="true"></i>
                        </button>

                        <button
                            type="button"
                            class={classes!("btn","btn-outline-secondary", if *grid == GridType::One { "active" } else { "" })}
                            aria-pressed={(*grid == GridType::One).to_string()}
                            aria-label="Single-column list"
                            title="Single-column list"
                            onclick={{
                                let grid = grid.clone();
                                Callback::from(move |_| grid.set(GridType::One))
                            }}
                        >
                            <i class="bi bi-square-fill" aria-hidden="true"></i>
                        </button>
                    </div>
                </div>

                <div class="col-auto feed-post-text-col" id="feed-post-text">
                    <span class="d-block">{"Show post text"}</span>
                    <input 
                        id="show-post-text"
                        type="checkbox" 
                        class="form-check-input"
                        checked={*show_desc}
                        oninput={{
                                    let show_desc = show_desc.clone();
                                    Callback::from(move |e: InputEvent| {
                                        let input: HtmlInputElement = e.target_unchecked_into();
                                        show_desc.set(input.checked());
                                    })
                                }} 
                    />
                </div>

                <div class="col-auto feed-post-text-col" id="feed-file-metadata">
                    <span class="d-block">{"Show file metadata"}</span>
                    <input
                        id="show-file-metadata"
                        type="checkbox"
                        class="form-check-input"
                        checked={*show_metadata}
                        oninput={{
                                    let show_metadata = show_metadata.clone();
                                    Callback::from(move |e: InputEvent| {
                                        let input: HtmlInputElement = e.target_unchecked_into();
                                        show_metadata.set(input.checked());
                                    })
                                }}
                    />
                </div>

                <div class="col-auto feed-post-text-col" id="feed-score-breakdown">
                    <span class="d-block">{"Score breakdown"}</span>
                    <input
                        id="show-score-breakdown"
                        type="checkbox"
                        class="form-check-input"
                        checked={*show_breakdown}
                        oninput={{
                                    let show_breakdown = show_breakdown.clone();
                                    Callback::from(move |e: InputEvent| {
                                        let input: HtmlInputElement = e.target_unchecked_into();
                                        show_breakdown.set(input.checked());
                                    })
                                }}
                    />
                </div>
            </div>

            <div class="position-fixed bottom-0 start-50 translate-middle-x w-100 d-flex justify-content-between z-1 feed-statusbar">
                {
                    if let Some(u) = &*selected_user {
                        html! { <span class="small m-3 bg-body-tertiary bg-opacity-75 text-body-emphasis rounded-pill badge shadow-sm">{ format!("User: {} (ID: {})", u.name, u.id) }</span> }
                    } else {
                        html! { <span class="small m-3 bg-body-tertiary bg-opacity-75 text-body-emphasis rounded-pill badge shadow-sm">{ "No user selected" }</span> }
                    }
                }
                {
                    if !(*is_loading) && (*error).is_none() {
                        let label = if *exhausted {
                            format!("Loaded {} posts — nothing left to fetch above the per-page cutoff", posts.len())
                        } else {
                            format!("Loaded {} posts", posts.len())
                        };
                        html! { <span class="small m-3 bg-body-tertiary bg-opacity-75 text-body-emphasis rounded-pill badge shadow-sm" aria-live="polite">{ label }</span> }
                    } else { html!{ <span class="small m-3 bg-body-tertiary bg-opacity-75 text-body-emphasis rounded-pill badge shadow-sm" aria-live="polite"><span class="spinner-border spinner-border-sm me-1" role="status"></span>{"Loading..."}</span>} }
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
                                    let error = error.clone();
                                    let exhausted = exhausted.clone();
                                    let consecutive_errors = consecutive_errors.clone();
                                    // Reset the error/exhaust gates set by
                                    // the consecutive-error threshold, then
                                    // re-emit the fetch. Without these
                                    // resets the fetch_page guard would
                                    // short-circuit on `*exhausted`.
                                    Callback::from(move |_| {
                                        consecutive_errors.borrow().set(0);
                                        exhausted.set(false);
                                        error.set(None);
                                        fetch_page.emit(());
                                    })
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
    opts.set_credentials(web_sys::RequestCredentials::Include);

    let request = Request::new_with_str_and_init(url, &opts)
        .map_err(|e| format!("Failed to create request: {e:?}"))?;

    let resp_value = wasm_bindgen_futures::JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(|e| format!("Fetch promise rejected: {e:?}"))?;

    let resp: Response = resp_value
        .dyn_into()
        .map_err(|_| "Failed to cast Response".to_string())?;

    let text_promise = resp
        .text()
        .map_err(|e| format!("Failed to read response text: {e:?}"))?;
    let text_js = wasm_bindgen_futures::JsFuture::from(text_promise)
        .await
        .map_err(|e| format!("Text promise rejected: {e:?}"))?;
    let text = text_js
        .as_string()
        .ok_or("Response text not a string".to_string())?;

    if !resp.ok() {
        return Err(humanize_error_body(resp.status(), &text));
    }

    serde_json::from_str::<T>(&text).map_err(|e| format!("JSON parse error: {e}"))
}
